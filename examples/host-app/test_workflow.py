import json
import os
from pathlib import Path
import tempfile
import time
import unittest
from unittest.mock import patch

from host import Host, CommandFailed, Result, digest
from workflow import Workflow


class WorkflowTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='awr 工作流 ')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.binary = Path(os.environ['AWR_TEST_BINARY']).resolve()
        self.version = os.environ['AWR_TEST_VERSION']
        (self.root/'map.toml').write_text("[project]\nname='Guide'\ncontext_profile='minimal'\n[[sources]]\ndomain='ledger'\nrole='primary'\npath='work.yaml'\nadapter='yaml-ledger-v1'\n")
        (self.root/'work.yaml').write_text('goals:\n- id: G\n  title: Deliver a useful guide\n  status: active\nwork_items:\n- id: W\n  title: Draft the guide\n  status: in_progress\n  goal: G\n  next_action: Review examples\n  acceptance: [Reviewed guide]\n')
        host = Host(self.binary, digest(self.binary), self.root, self.root/'init-receipts')
        host.ok('init','--manifest','map.toml','--accept')
        self.pid = host.ok('status')['project_id']
        self.wf = self.open_workflow('workflow/state.json')

    def open_workflow(self, state):
        return Workflow(self.binary,digest(self.binary),self.version,self.root,self.pid,self.root/state)

    def revision(self):
        return self.wf.host.ok('session','list')['project_revision']

    def begin(self):
        value = self.wf.begin('W','writer','fixture','no-model',self.revision())
        self.session = value['session']['id']
        context = self.wf.context()
        # Synthetic host consumes the actual rendered pack and validates its work/criterion.
        self.assertIn('Reviewed guide',context['work_context']['rendered_context'])
        self.assertEqual(context['work_context']['identity']['work_item_key'],'W')
        return context['work_context']['context_hash']

    def test_full_lifecycle_requires_consumption_and_preserves_real_receipts(self):
        context_hash = self.begin()
        with self.assertRaises(ValueError):
            self.wf.checkpoint(context_hash,'Reviewed draft','Deliver guide',self.revision())
        with self.assertRaises(ValueError): self.wf.acknowledge('0'*64)
        self.wf.acknowledge(context_hash)
        checkpoint = self.wf.checkpoint(context_hash,'Reviewed draft','Deliver guide',self.revision())
        source_sha = 'a'*40
        report = dict(version=1,work_item='W',source_sha=source_sha,command='Inspect the guide fixture',scope=['W'],verified_at=time.time_ns()//1000000,
                      checks=[dict(name='Guide reviewed',passed=True,details='Checked the synthetic guide artifact',criteria=['Reviewed guide'])])
        path = self.root/'report.json';path.write_text(json.dumps(report))
        draft = dict(external_key='GUIDE-E',work_item_key='W',evidence_type='completion_report',level='locally_verified',summary='Synthetic guide reviewed',locator='report.json',sha256=digest(path),source_sha=source_sha,command=report['command'],scope=['W'],branch_id=None,verified_at=report['verified_at'])
        self.wf.evidence(draft,self.revision())
        done = self.wf.finish(dict(version=1,source_sha=source_sha,acceptance=[dict(criterion='Reviewed guide',evidence=['GUIDE-E'])]),'Reviewed synthetic evidence',self.revision())
        self.assertEqual(done['session']['status'],'ended')
        self.assertEqual(self.wf.state['phase'],'ended')
        self.assertEqual(self.wf.host.ok('work','show','W')['work']['status'],'completed')
        self.assertEqual(self.wf.state['checkpoint'],checkpoint['checkpoint']['id'])
        self.assertEqual(len(self.wf.state['history']),5)
        with self.assertRaises(ValueError): self.wf.begin('W','other','fixture','none',self.revision())

    def test_lost_checkpoint_receipt_requires_inspection_and_never_replays(self):
        context_hash = self.begin();self.wf.acknowledge(context_hash)
        original = self.wf.host.call
        def lose(*args, **kwargs):
            result = original(*args,**kwargs)  # Real side effect happened; caller loses its answer.
            result.require()
            return Result(None,b'',b'',result.receipt,True)
        revision = self.revision()
        with patch.object(self.wf.host,'call',side_effect=lose) as call:
            with self.assertRaises(CommandFailed): self.wf.checkpoint(context_hash,'Review complete','Deliver guide',revision)
            self.assertEqual(call.call_count,1)  # Exactly one attempted checkpoint, with a real saved effect.
        self.assertEqual(self.wf.state['pending']['outcome'],'unknown')
        reopened = self.open_workflow('workflow/state.json')
        with self.assertRaises(ValueError): reopened.checkpoint(context_hash,'Duplicate','Deliver',self.revision())
        view = reopened.inspect();before = self.revision()
        self.assertIsNotNone(view['recovery']['checkpoint'])
        resumed = reopened.reconcile(view['inspection']['sha256'],'Observed the saved checkpoint; continue without replay')
        self.assertFalse(resumed['side_effects_replayed']);self.assertEqual(self.revision(),before)
        self.assertEqual(reopened.state['history'][-1]['resolution'],'operator_reconciled')

    def test_stale_revision_pin_and_context_tampering_are_rejected(self):
        h = self.begin();self.wf.acknowledge(h)
        with self.assertRaises(CommandFailed) as failure:
            self.wf.checkpoint(h,'Review','Next',0)
        self.assertEqual(failure.exception.result.error['code'],'RevisionConflict')
        observed = self.wf.inspect()
        self.wf.reconcile(observed['inspection']['sha256'],'Revision rejection observed; no operation replayed')
        original = self.wf.state['context']['output']
        Path(original).write_text('{}')
        with self.assertRaises(ValueError):self.wf.checkpoint(h,'Review','Next',self.revision())
        with self.assertRaises(ValueError):Workflow(self.binary,'0'*64,self.version,self.root,self.pid,self.root/'bad/state.json')
        with self.assertRaises(ValueError):Workflow(self.binary,digest(self.binary),'99.0.0',self.root,self.pid,self.root/'version/state.json')

    def test_existing_session_adoption_retains_identity(self):
        self.begin();other=self.open_workflow('other/state.json')
        adopted=other.adopt(self.session,'W')
        self.assertEqual(adopted['session']['id'],self.session)
        self.assertIsNone(other.state['context'])
        with self.assertRaises(ValueError):other.acknowledge('a'*64)

    def observation(self, **changes):
        return dict(observed_at=time.time_ns()//1000000, note='Host reviewed the fixture scope',
                    **dict(dict(single_outcome=True, bounded_scope=True, single_executor=True,
                                no_deferred_wait=True, independently_schedulable_units=1,
                                plan_valid=True, outcome_known=True), **changes))

    def test_compaction_and_guided_preparation_use_the_bound_session_without_model_calls(self):
        self.begin()
        native = dict(compaction_id='fixture-compact-1', sequence=1, observed_at=time.time_ns()//1000000,
                      trigger='automatic', model='fixture-model', source='fixture.full_request',
                      measurement_scope='full_request', measurement_basis='host_reported',
                      before_tokens=230000, after_tokens=180000, context_window_tokens=256000)
        observed = self.wf.observe_compaction(native)
        self.assertEqual(observed['state'], 'handoff_candidate')
        self.assertFalse(observed['session_switch_performed'])
        self.assertEqual(observed['session'], self.session)
        self.assertIsNone(self.wf.compaction(True)['observation']['usage'])
        with patch.object(self.wf.host, 'call', wraps=self.wf.host.call) as calls:
            prepared = self.wf.prepare(response_view='action')
        self.assertEqual(calls.call_count, 1)
        self.assertEqual(set(prepared['guidance']), {'when', 'basis', 'next_action', 'recheck'})
        self.assertIn('ask before opening', prepared['guidance']['next_action'])
        self.assertIn('Reviewed guide', prepared['context']['work_context']['rendered_context'])
        self.assertEqual(self.wf.defer_compaction(observed['observation_event_id'])['state'], 'deferred')
        recorded = self.wf.prepare(self.observation(), response_view='action')
        self.assertTrue(recorded['observation_recorded'])
        self.assertFalse(recorded['management']['record_required'])
        self.assertNotIn('record known observations', recorded['guidance']['next_action'])
        self.wf.acknowledge(recorded['context']['work_context']['context_hash'])
        with self.assertRaises(ValueError):
            self.wf.progress('Reviewed context', 'Deliver', response_view='action')
        self.wf.capabilities.discard('workflow.action_guidance')
        fallback = self.wf.prepare(response_view='action')
        self.assertEqual(fallback['response_view']['view'], 'summary')
        self.assertEqual(self.wf.state['session'], self.session)

    def test_preparation_reuses_management_but_always_delivers_fresh_context(self):
        self.wf.begin('W','writer','fixture','no-model')
        unknown=self.wf.prepare()
        self.assertEqual(unknown['management']['decision']['mode'],'undetermined')
        self.assertFalse(unknown['observation_recorded'])
        first=self.wf.prepare(self.observation())
        self.assertTrue(first['observation_recorded'])
        self.assertEqual(first['management']['decision']['mode'],'lightweight')
        h=first['context']['work_context']['context_hash']
        with self.assertRaises(ValueError): self.wf.progress('Review','Deliver')
        self.wf.acknowledge(h)
        try:
            self.wf.progress('Read the required context','Deliver the reviewed guide')
        except CommandFailed as error:
            self.fail(str(error.result.error))
        with patch.object(self.wf.host,'call',wraps=self.wf.host.call) as calls:
            second=self.wf.prepare()
        self.assertEqual(calls.call_count,1)
        self.assertEqual(calls.call_args.args[:2],('work','prepare'))
        self.assertFalse(second['observation_recorded'])
        self.assertEqual(second['management']['decision']['mode'],'lightweight')
        self.assertFalse(self.wf.state['context']['acknowledged'])
        self.assertIn('Reviewed guide',second['context']['work_context']['rendered_context'])

    def test_new_observations_upgrade_without_downgrade_or_inferred_facts(self):
        self.wf.begin('W','writer','fixture','no-model')
        self.wf.prepare(self.observation())
        upgraded=self.wf.prepare(self.observation(no_deferred_wait=False))
        self.assertEqual(upgraded['management']['decision']['mode'],'continuous')
        retained=self.wf.prepare(self.observation())
        self.assertEqual(retained['management']['decision']['mode'],'continuous')
        source=self.root/'work.yaml'
        source.write_text(source.read_text().replace('Reviewed guide','Reviewed complete guide'))
        changed=self.wf.prepare()
        self.assertIsNone(changed['management']['observation'])
        self.assertTrue(changed['management']['record_required'])
        self.assertIn('Reviewed complete guide',changed['context']['work_context']['rendered_context'])

    def test_legacy_preparation_fallback_is_chosen_before_writing(self):
        self.wf.begin('W','writer','fixture','no-model')
        self.wf.capabilities.discard('workflow.prepare')
        with patch.object(self.wf.host,'call',wraps=self.wf.host.call) as calls:
            result=self.wf.prepare(self.observation())
        self.assertEqual(result['workflow_path'],'legacy')
        self.assertFalse(result['observation_recorded'])
        self.assertEqual(calls.call_count,1)
        self.assertEqual(calls.call_args.args[:2],('context','compile'))
        self.wf.acknowledge(result['context']['work_context']['context_hash'])
        self.wf.checkpoint(self.wf.state['context']['hash'],'Reviewed','Deliver')

    def test_unknown_management_result_is_never_replayed_or_fallback_written(self):
        self.wf.begin('W','writer','fixture','no-model')
        original=self.wf.host.call
        def lose(*args, **kwargs):
            result=original(*args,**kwargs)
            if args[:2]==('work','manage'):
                result.require()
                return Result(None,b'',b'',result.receipt,True)
            return result
        with patch.object(self.wf.host,'call',side_effect=lose) as calls:
            with self.assertRaises(CommandFailed):self.wf.prepare(self.observation())
            self.assertEqual(calls.call_count,2)
        reopened=self.open_workflow('workflow/state.json')
        with self.assertRaises(ValueError):reopened.prepare(self.observation())
        view=reopened.inspect()
        reopened.reconcile(view['inspection']['sha256'],'Observed the recorded assessment; no replay')
        self.assertFalse(reopened.prepare()['observation_recorded'])

    def test_concise_preparation_keeps_context_and_legacy_default(self):
        self.wf.begin('W','writer','fixture','no-model')
        full=self.wf.prepare()
        concise=self.wf.prepare(response_view='summary')
        self.assertIn('selected_chunks',full['context']['work_context'])
        self.assertNotIn('selected_chunks',concise['context']['work_context'])
        self.assertEqual(full['context']['work_context']['rendered_context'],concise['context']['work_context']['rendered_context'])
        self.assertEqual(full['context']['completeness'],concise['context']['completeness'])
        self.wf.acknowledge(concise['context']['work_context']['context_hash'])
        changed=self.wf.progress('Review completed','Deliver guide',response_view='summary')
        self.assertEqual(changed['changes']['status'],'in_progress')
        self.assertNotIn('patch',changed['proposal'])
        self.wf.capabilities.discard('workflow.response_summary')
        legacy=self.wf.prepare(response_view='summary')
        self.assertIn('selected_chunks',legacy['context']['work_context'])
