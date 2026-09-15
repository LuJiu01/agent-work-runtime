"""A native AWR graph and lifecycle with a replaceable synthetic executor."""
import json
import os
from pathlib import Path
import tempfile
import time
import unittest

from host import Host, digest
from workflow import Workflow
from orchestrator import Orchestrator, conflicts, resources


class Driver:
    def __init__(self, root, binary, version):
        self.root, self.binary, self.version = root, binary, version
        self.host = Host(binary,digest(binary),root,root/'driver-receipts')
        self.pid = self.host.ok('status')['project_id']
        self.binding = dict(project=str(root),project_id=self.pid,binary=str(binary),sha256=digest(binary),executor='synthetic-native-fixture-v1')
        self.executions, self.calls, self.lost = {}, [], False

    def graph(self):
        return self.host.ok('work','graph')

    def dispatch(self, job):
        self.calls.append(job['id'])
        wf = Workflow(self.binary,digest(self.binary),self.version,self.root,self.pid,self.root/'workflows'/job['id']/'state.json')
        wf.begin(job['work'],'fixture-consumer','synthetic','no-model',self.host.ok('session','list')['project_revision'])
        pack = wf.context()
        # Actual synthetic context consumption; no attestation on behalf of a real model.
        if pack['work_context']['identity']['work_item_key'] != job['work']:
            raise ValueError('wrong delivered work')
        if 'Reviewed output' not in pack['work_context']['rendered_context']:
            raise ValueError('missing actual acceptance')
        wf.acknowledge(pack['work_context']['context_hash'])
        progressed = wf.perform('progress', ['work','progress',job['work'],'--session',wf.state['session'],
            '--reason','Begin the explicitly consumed fixture work','--next-action','Produce the reviewed output',
            '--expected-revision',str(self.host.ok('session','list')['project_revision'])])
        wf.completed(progressed)
        self.executions[job['id']] = dict(workflow=wf,work=job['work'],state='running')
        if self.lost:
            self.lost = False
            raise TimeoutError('synthetic loss after reservation and dispatch')

    def observe(self, job):
        execution = self.executions.get(job['id'])
        return dict(execution=execution['state'] if execution else 'unknown',receipt='synthetic-executor-query:' + job['id'])

    def complete(self, work):
        execution = next(e for e in self.executions.values() if e['work']==work)
        wf = execution['workflow']
        artifact = self.root/(work+'.txt')
        artifact.write_text('Reviewed output for '+work)
        assert artifact.read_text() == 'Reviewed output for '+work
        source_sha = 'b'*40
        report = dict(version=1,work_item=work,source_sha=source_sha,command='Review the independent synthetic output',scope=[work],verified_at=time.time_ns()//1000000,checks=[dict(name='Actual artifact reviewed',passed=True,details=artifact.name,criteria=['Reviewed output'])])
        path = self.root/(work+'-report.json'); path.write_text(json.dumps(report))
        evidence = 'PROOF-'+work
        wf.evidence(dict(external_key=evidence,work_item_key=work,evidence_type='completion_report',level='locally_verified',summary='Independent synthetic output reviewed',locator=path.name,sha256=digest(path),source_sha=source_sha,command=report['command'],scope=[work],branch_id=None,verified_at=report['verified_at']),self.host.ok('session','list')['project_revision'])
        wf.finish(dict(version=1,source_sha=source_sha,acceptance=[dict(criterion='Reviewed output',evidence=[evidence])]),'Synthetic output verified',self.host.ok('session','list')['project_revision'])
        execution['state'] = 'stopped'


class OrchestratorTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='awr orchestration ')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        binary = Path(os.environ['AWR_TEST_BINARY']).resolve()
        version = os.environ['AWR_TEST_VERSION']
        (self.root/'map.toml').write_text("[project]\nname='Parallel fixture'\ncontext_profile='minimal'\n[[sources]]\ndomain='ledger'\nrole='primary'\npath='work.yaml'\nadapter='yaml-ledger-v1'\n")
        source = 'goals:\n- id: G\n  title: Deliver independent reviewed outputs\n  status: active\nwork_items:\n'
        for key, deps, path in [('A',[], 'a'),('B',[], 'b'),('C',['A','B'],'c'),('D',[],'a/subpath')]:
            source += f'- id: {key}\n  title: Review {key}\n  status: planned\n  goal: G\n  next_action: Review output\n  acceptance: [Reviewed output]\n  paths: [{path}]\n  depends_on: {json.dumps(deps)}\n'
        (self.root/'work.yaml').write_text(source)
        h = Host(binary,digest(binary),self.root,self.root/'init-receipts')
        h.ok('init','--manifest','map.toml','--accept')
        self.driver = Driver(self.root,binary,version)
        self.state = self.root/'dispatch/state.json'

    def test_bounded_parallel_dependency_release_and_lost_response_resume(self):
        self.driver.lost = True
        host = Orchestrator(self.driver,self.state,parallelism=2)
        first = host.step()
        self.assertEqual([j['work'] for j in first['jobs']],['A','B'])
        self.assertEqual(len(self.driver.calls),2)
        self.assertEqual(first['jobs'][0]['execution'],'unknown')
        # New orchestrator process state resumes through observation; dispatch is not repeated.
        host = Orchestrator(self.driver,self.state,parallelism=2)
        self.assertEqual(host.step()['dispatched'],[])
        self.driver.complete('B')
        self.assertEqual(host.step()['dispatched'],[])  # D conflicts with A; C still depends on A.
        self.driver.complete('A')
        next_step = host.step()
        self.assertEqual([j['work'] for j in next_step['jobs'] if j['execution']=='running'],['C','D'])
        self.assertEqual(len(self.driver.calls),4)
        self.assertEqual(len(set(self.driver.calls)),4)
        for key in ['C','D']: self.driver.complete(key)
        self.assertEqual(host.step()['dispatched'],[])
        self.assertTrue(all(n['status']=='completed' for n in self.driver.graph()['nodes']))

    def test_unknown_execution_retains_capacity_and_path_conflicts_are_conservative(self):
        host = Orchestrator(self.driver,self.state,parallelism=1)
        host.step()
        self.driver.executions.clear()  # External state is unavailable; it is not a failed run.
        self.assertEqual(host.step()['dispatched'],[])
        self.assertEqual(len(self.driver.calls),1)
        self.assertTrue(conflicts(resources([]),resources(['separate'])))
        self.assertTrue(conflicts(resources(['src']),resources(['src/module'])))
        self.assertTrue(conflicts(resources(['src/**']),resources(['docs'])))
        self.assertFalse(conflicts(resources(['src']),resources(['src-other'])))
