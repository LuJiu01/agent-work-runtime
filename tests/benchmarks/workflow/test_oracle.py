import json
from pathlib import Path
import tempfile
import unittest

from oracle import CRITERIA, GOAL, RULE, UPGRADE_CRITERION, check_context, check_artifact, check_final
from run import metrics, cost_segments


class OracleTest(unittest.TestCase):
    def test_cost_segments_keep_onboarding_and_rejection_probes_in_total(self):
        phases=['onboarding','protocol','catalog','context','maintenance','guard','verification','recovery']
        rows=[dict(phase=p,tool_call=p not in ('protocol','catalog'),tool_text_bytes=10+i,
                   elapsed_ms=i+1,wire_input_bytes=1,wire_output_bytes=2) for i,p in enumerate(phases)]
        total=metrics(rows,100)
        segments=cost_segments(rows)
        self.assertEqual(segments['project_onboarding']['tool_calls'],1)
        self.assertEqual(segments['independent_verification']['tool_calls'],2)
        self.assertEqual(segments['normal_workflow']['tool_calls'],3)
        for field in ('tool_calls','tool_text_bytes'):
            self.assertEqual(sum(v[field] for v in segments.values()),total[field])

    def test_missing_criteria_rules_or_wrong_identity_cannot_pass_context(self):
        pack = dict(completeness=dict(complete=True),work_context=dict(identity=dict(work_item_key='W'),context_hash='real-test-hash',rendered_context='\n'.join([*CRITERIA,GOAL,RULE])))
        self.assertEqual(check_context(pack,'W',CRITERIA),'real-test-hash')
        for missing in [*CRITERIA,GOAL,RULE]:
            broken = json.loads(json.dumps(pack))
            broken['work_context']['rendered_context'] = broken['work_context']['rendered_context'].replace(missing,'')
            with self.assertRaises(AssertionError): check_context(broken,'W',CRITERIA)
        with self.assertRaises(AssertionError): check_context(pack,'ANOTHER',CRITERIA)

    def test_completed_status_cannot_replace_real_artifact_coverage_or_single_execution(self):
        with tempfile.TemporaryDirectory() as path:
            root = Path(path)
            (root/'W-artifact.json').write_text(json.dumps(dict(work='W',result='A reviewed conclusion',limitations='Fixture only')))
            report = dict(work_item='W',checks=[dict(passed=True,criteria=CRITERIA)])
            (root/'W-report.json').write_text(json.dumps(report))
            work = dict(work=dict(id='original',status='completed'),acceptance=CRITERIA)
            self.assertEqual(len(check_final(root,'W','original',CRITERIA,work,{'W':1})),64)
            with self.assertRaises(AssertionError): check_final(root,'W','original',CRITERIA,work,{'W':2})
            with self.assertRaises(AssertionError): check_final(root,'W','replacement',CRITERIA,work,{'W':1})
            report['checks'][0]['criteria'] = CRITERIA[:1]
            (root/'W-report.json').write_text(json.dumps(report))
            with self.assertRaises(AssertionError): check_final(root,'W','original',CRITERIA,work,{'W':1})
            with self.assertRaises(FileNotFoundError): check_artifact(root,'W',CRITERIA+[UPGRADE_CRITERION])

    def test_cost_accounting_includes_failures_recovery_and_real_replays(self):
        rows = [
            dict(name='write',phase='maintenance',wire_input_bytes=20,wire_output_bytes=30,elapsed_ms=4,tool_call=True,rpc=True,tool_text_bytes=12,error=True,expected_rejection=True,mutation_identity=['write','same']),
            dict(name='write',phase='recovery',wire_input_bytes=20,wire_output_bytes=40,elapsed_ms=8,tool_call=True,rpc=True,tool_text_bytes=16,error=False,mutation_identity=['write','same']),
            dict(name='artifact',phase='business',wire_input_bytes=100,wire_output_bytes=0,elapsed_ms=2,host_operation=True),
            dict(name='report',phase='maintenance',wire_input_bytes=60,wire_output_bytes=0,elapsed_ms=3,host_operation=True),
            dict(name='catalog',phase='catalog',wire_input_bytes=5,wire_output_bytes=1000,elapsed_ms=1,rpc=True),
        ]
        measured = metrics(rows,20)
        self.assertEqual(measured['tool_calls'],2)
        self.assertEqual(measured['wire_input_bytes'],45)
        self.assertEqual(measured['wire_output_bytes'],1070)
        self.assertEqual(measured['maintenance_bytes'],110)
        self.assertEqual(measured['maintenance_ms'],7)
        self.assertEqual(measured['recovery_calls'],1)
        self.assertEqual(measured['expected_rejections'],1)
        self.assertEqual(measured['explicit_replays'],1)
        rows[0]['expected_rejection'] = False
        self.assertEqual(metrics(rows,20)['unexpected_errors'],1)


if __name__=='__main__': unittest.main()
