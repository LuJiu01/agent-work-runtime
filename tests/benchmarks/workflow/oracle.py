"""Independent business assertions, separate from recording and transport."""
import hashlib
import json

CRITERIA = ['Reviewed artifact exists', 'Limitations are stated']
UPGRADE_CRITERION = 'Tradeoffs are documented separately'
RULE = 'Never claim an unverified deliverable.'
GOAL = 'Deliver a useful reviewed handoff'


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def check_context(context, key, criteria):
    require(context['completeness']['complete'], 'required context is incomplete')
    pack = context['work_context']
    require(pack['identity']['work_item_key'] == key, 'wrong work identity')
    require(all(text in pack['rendered_context'] for text in [*criteria, RULE, GOAL]),
            'a required criterion, hard rule or goal was dropped')
    require(bool(pack['context_hash']), 'missing delivered context identity')
    return pack['context_hash']


def check_artifact(root, key, criteria):
    value = json.loads((root / (key + '-artifact.json')).read_text())
    require(value['work'] == key and bool(value['result']), 'missing independent work result')
    require(bool(value['limitations']), 'limitations absent from actual artifact')
    if UPGRADE_CRITERION in criteria:
        tradeoffs = json.loads((root / (key + '-tradeoffs.json')).read_text())
        require(tradeoffs['work'] == key and bool(tradeoffs['tradeoffs']), 'missing separate tradeoffs')


def check_final(root, key, identity, criteria, work_result, executions):
    require(work_result['work']['id'] == identity, 'work identity changed')
    require(work_result['work']['status'] == 'completed', 'source completion was not recorded')
    require(work_result['acceptance'] == criteria, 'completion criteria changed unexpectedly')
    check_artifact(root,key,criteria)
    report_bytes = (root / (key + '-report.json')).read_bytes()
    report = json.loads(report_bytes)
    require(report['work_item'] == key, 'report belongs to another work')
    covered = set()
    for check in report['checks']:
        if check['passed']:
            covered.update(check['criteria'])
    require(covered == set(criteria), 'actual report does not cover exact current criteria')
    require(executions[key] == 1, 'business executor ran more than once')
    return hashlib.sha256(report_bytes).hexdigest()
