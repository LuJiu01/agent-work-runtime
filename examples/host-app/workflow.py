"""Explicit AWR work lifecycle over the pinned public CLI (Python 3.11+)."""
import argparse
from contextlib import contextmanager
from functools import wraps
import hashlib
import json
import os
from pathlib import Path
import sys
import uuid

from host import Host, CommandFailed, digest, protected_file
from execution_reports import ExecutionReports


def atomic_json(path, value):
    temporary = protected_file(path.parent, '.tmp', json.dumps(value, ensure_ascii=False, indent=2).encode())
    try:
        # Windows fsync requires a writable descriptor; do not truncate the payload.
        with temporary.open('r+b') as stream:
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        if os.name == 'posix':
            fd = os.open(path.parent, os.O_RDONLY)
            try:
                os.fsync(fd)
            finally:
                os.close(fd)
    finally:
        temporary.unlink(missing_ok=True)


def serialized(method):
    @wraps(method)
    def wrapped(self, *args, **kwargs):
        with self.guard():
            self.state = json.loads(self.path.read_text())
            if self.state['binding'] != self.binding:
                raise ValueError('Workflow program/project binding changed')
            return method(self, *args, **kwargs)
    return wrapped


class Workflow:
    """Caller-driven protocol: deliver context, consume it, acknowledge, then save.

    An acknowledgement is a caller attestation, not proof of model comprehension.
    Only run() explicitly dispatches caller-supplied argv through AWR. Report
    preparation never executes commands or automatically retries uncertain writes.
    """
    def __init__(self, binary, sha256, version, project, project_id, state_path):
        self.path = Path(state_path).absolute()
        if self.path.is_symlink():
            raise ValueError('Workflow state must not be a symlink')
        self.path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        self.binding = dict(binary=str(Path(binary).resolve(strict=True)), sha256=sha256,
                            version=version, project=str(Path(project).resolve(strict=True)),
                            project_id=project_id)
        if digest(self.binding['binary']) != sha256:
            raise ValueError('Executable differs from the trusted pinned checksum')
        self.host = Host(binary, sha256, project,
                         self.path.parent / 'receipts' / str(uuid.uuid4()))
        catalog = self.host.discover(version, ['session.checkpoint', 'context.compile', 'evidence.read_write', 'completion.engineering'])
        self.capabilities = {c['id'] for c in catalog['capabilities'] if c['available']}
        with self.guard():
            if self.path.exists():
                self.state = json.loads(self.path.read_text())
                if self.state.get('version') != 1 or self.state['binding'] != self.binding:
                    raise ValueError('Workflow program/project binding differs; use its original pin')
            else:
                status = self.host.ok('status')
                if status['project_id'] != project_id:
                    raise ValueError('Project identity differs from the selected binding')
                self.state = dict(version=1, binding=self.binding, session=None, work=None,
                                  phase='new', context=None, pending=None, history=[],
                                  last_revision=status['project_revision'])
                self.save()

    @contextmanager
    def guard(self):
        """OS lock releases on process exit; the durable pending record survives."""
        lock = self.path.with_suffix(self.path.suffix + '.lock')
        fd = os.open(lock, os.O_CREAT | os.O_RDWR, 0o600)
        try:
            if os.name == 'posix':
                import fcntl
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            else:
                import msvcrt
                if os.fstat(fd).st_size == 0:
                    os.write(fd, b'0')
                os.lseek(fd, 0, os.SEEK_SET)
                msvcrt.locking(fd, msvcrt.LK_NBLCK, 1)
            yield
        finally:
            os.close(fd)

    def save(self):
        atomic_json(self.path, self.state)

    def available(self):
        if self.state['pending']:
            raise ValueError('Unknown or failed operation remains; inspect and explicitly reconcile first')

    def active(self):
        self.available()
        if self.state['phase'] != 'active' or not self.state['session']:
            raise ValueError('An active, explicitly selected session is required')

    def revision(self, expected_revision):
        revision = self.state.get('last_revision') if expected_revision is None else expected_revision
        if revision is None:
            raise ValueError('Read current state before choosing an expected revision')
        return revision  # Optimistic concurrency still rejects intervening writes.

    def perform(self, operation, args, value=None):
        self.available()
        self.state['pending'] = dict(id=str(uuid.uuid4()), operation=operation, args=args,
                                     receipts=str(self.host.receipts), outcome='pending')
        self.save()  # Before invoking any possible side effect.
        result = self.host.call(*args) if value is None else self.host.input(args, value)
        self.state['pending'].update(receipt=str(result.receipt),
                                     outcome='unknown' if result.outcome_unknown else 'returned',
                                     exit_code=result.exit_code)
        self.save()
        return result.require()  # Error/timeout retains the pending operation.

    def completed(self, value, **updates):
        self.state['history'].append(self.state['pending'])
        self.state.update(updates, pending=None, last_revision=value['project_revision'])
        self.save()
        return value

    def session_identity(self, session, work):
        value = self.host.ok('session', 'show', session)
        item = self.host.ok('work', 'show', work)
        if value['session']['work_item_id'] != item['work']['id']:
            raise ValueError('Selected session belongs to another work item')
        if value['session']['project_id'] != self.binding['project_id']:
            raise ValueError('Selected session belongs to another project')
        return value, item

    @serialized
    def begin(self, work, agent, provider, model, expected_revision=None):
        self.available()
        if self.state['phase'] != 'new':
            raise ValueError('Workflow already has a session; inspect or adopt it explicitly')
        value = self.perform('begin', ['session', 'start', '--work', work, '--agent', agent,
            '--provider', provider, '--model', model, '--claim', '--expected-revision', str(self.revision(expected_revision))])
        return self.completed(value, session=value['session']['id'], work=work, phase='active')

    @serialized
    def adopt(self, session, work):
        self.available()
        if self.state['phase'] != 'new':
            raise ValueError('Adoption requires a new workflow state')
        value, _ = self.session_identity(session, work)
        if value['session']['status'] != 'active':
            raise ValueError('Only an active session can be adopted')
        self.state.update(session=session, work=work, phase='active', last_revision=value['project_revision'])
        self.save()
        return value

    @serialized
    def context(self):
        self.active()
        return self.fetch_context(prepared=False)

    def response_args(self, view, prepared=False):
        if view == 'action' and prepared:
            if 'workflow.action_guidance' in self.capabilities:
                return ['--response-view', 'action']
            view = 'summary'  # Negotiate before execution; no fallback write.
        if view not in ('full', 'summary'):
            raise ValueError('response_view must be full or summary; action is preparation-only')
        return (['--response-view', view] if view == 'summary' and
                'workflow.response_summary' in self.capabilities else [])

    def fetch_context(self, prepared, goals=(), response_view='full'):
        self.state['context'] = None
        self.save()
        args = (['work', 'prepare', self.state['work']] if prepared else
                ['context', 'compile', '--work', self.state['work']])
        args += ['--session', self.state['session']]
        if prepared:
            args += self.response_args(response_view, prepared=True)
        for goal in goals:
            args += ['--goal', goal]
        result = self.host.call(*args)
        value = result.require()
        context = value['context'] if prepared else value
        if not context['completeness']['complete'] or not context.get('work_context'):
            raise ValueError('Context is incomplete; do not execute')
        if (context['session_id'] != self.state['session'] or
                context['work_context']['identity']['project_id'] != self.binding['project_id'] or
                self.state['work'] not in (context['work_context']['identity']['work_item_key'],
                                          context['work_context']['identity']['work_item_id'])):
            raise ValueError('Prepared context does not match this workflow')
        receipt = json.loads(result.receipt.read_text())
        output = result.receipt.parent / receipt['stdout']
        self.state['context'] = dict(hash=context['work_context']['context_hash'], output=str(output),
                                    sha256=digest(output), revision=value['project_revision'],
                                    prepared=prepared, acknowledged=False)
        self.state['last_revision'] = value['project_revision']
        self.save()
        return value  # Full rendered context must actually reach the caller.

    @serialized
    def prepare(self, observation=None, goals=(), response_view='full'):
        """Fresh preparation on every call; never cache context or invent observations."""
        self.active()
        self.response_args(response_view, prepared=True)  # Validate before any call or mutation.
        if 'workflow.prepare' not in self.capabilities:
            return dict(context=self.fetch_context(False, goals), workflow_path='legacy',
                        management_available=False, observation_recorded=False)
        value = self.fetch_context(True, goals, response_view)
        value['workflow_path'] = 'prepared'
        value['observation_recorded'] = False
        assessment = value['management']
        # An explicit new observation is recorded even when the contract is unchanged.
        # With no new observation, only a runtime-requested reassessment is persisted.
        if 'work.management' in self.capabilities:
            selected = observation if observation is not None else assessment['observation']
            if selected is not None and (observation is not None or assessment['record_required']):
                request = dict(work=self.state['work'], session=self.state['session'],
                               expected_revision=value['project_revision'], request_key=str(uuid.uuid4()),
                               contract_fingerprint=assessment['contract_fingerprint'], observation=selected)
                recorded = self.perform('manage', ['work', 'manage'], request)
                self.completed(recorded)
                value.update(management=recorded['assessment'], observation_recorded=True,
                             project_revision=recorded['project_revision'])
                if response_view == 'action' and 'workflow.action_guidance' in self.capabilities:
                    # The mutation changed the assessment: never return an obsolete instruction.
                    value = self.fetch_context(True, goals, response_view)
                    value.update(workflow_path='prepared', observation_recorded=True)
        value['management_available'] = 'work.management' in self.capabilities
        return value

    @serialized
    def observe_compaction(self, observation, policy=None, expected_revision=None):
        """Called by a host after native compaction completes, never by a round timer.

        The adapter must supply stable event identity/sequence and actual telemetry.
        Missing measurements stay absent. This performs no model call or window change.
        """
        self.active()
        if 'client.compaction' not in self.capabilities:
            raise ValueError('Pinned AWR does not support native compaction observations')
        request = dict(session=self.state['session'], expected_revision=self.revision(expected_revision),
                       observation=observation)
        if policy is not None:
            request['policy'] = policy
        value = self.perform('observe_compaction', ['session', 'compaction', 'observe'], request)
        return self.completed(value)

    @serialized
    def compaction(self, include_observation=False):
        # Read-only inspection remains available after a lost write response.
        if not self.state['session']:
            raise ValueError('Select a session before inspecting compaction')
        args = ['session', 'compaction', 'inspect', '--session', self.state['session']]
        if include_observation:
            args.append('--include-observation')
        return self.host.ok(*args)

    @serialized
    def defer_compaction(self, observation_event_id, expected_revision=None):
        """Caller invokes this after the user postpones; it does not grant approval."""
        self.active()
        value = self.perform('defer_compaction', ['session', 'compaction', 'defer', '--session',
            self.state['session'], '--observation-event-id', observation_event_id,
            '--expected-revision', str(self.revision(expected_revision))])
        return self.completed(value)

    @serialized
    def progress(self, reason, next_action, expected_revision=None, response_view='full'):
        self.active()
        self.require_consumed()
        value = self.perform('progress', ['work', 'progress', self.state['work'],
            '--session', self.state['session'], '--reason', reason, '--next-action', next_action,
            '--expected-revision', str(self.revision(expected_revision)), *self.response_args(response_view)])
        return self.completed(value)

    def delivered(self, consumed_hash):
        context = self.state.get('context')
        if not context or context['hash'] != consumed_hash or digest(context['output']) != context['sha256']:
            raise ValueError('Hash does not identify this workflow\'s intact delivered context')
        value = json.loads(Path(context['output']).read_text())
        if context.get('prepared'):
            value = value['context']
        if value['session_id'] != self.state['session'] or value['work_context']['context_hash'] != consumed_hash:
            raise ValueError('Delivered context/session mismatch')
        return context

    def require_consumed(self):
        context = self.state.get('context')
        if not context or not self.delivered(context['hash'])['acknowledged']:
            raise ValueError('Consume and acknowledge context before continuing work')

    @serialized
    def acknowledge(self, consumed_hash):
        self.active()
        context = self.delivered(consumed_hash)
        context['acknowledged'] = True
        context['acknowledgement_basis'] = 'caller attests that this exact context was consumed'
        self.save()
        return context

    @serialized
    def checkpoint(self, consumed_hash, digest_text, next_action, expected_revision=None):
        self.active()
        if not self.delivered(consumed_hash)['acknowledged']:
            raise ValueError('Explicit consumption acknowledgement is required before checkpointing')
        value = self.perform('checkpoint', ['session', 'checkpoint', '--session', self.state['session'],
            '--context-hash', consumed_hash, '--digest', digest_text, '--next-action', next_action,
            '--expected-revision', str(self.revision(expected_revision))])
        return self.completed(value, checkpoint=value['checkpoint']['id'])

    @serialized
    def evidence(self, draft, expected_revision=None):
        self.active()
        if draft.get('work_item_key') != self.state['work']:
            raise ValueError('Evidence must reference the selected work item')
        value = self.perform('evidence', ['evidence', 'add', '--expected-revision', str(self.revision(expected_revision))], draft)
        return self.completed(value)

    @serialized
    def run(self, key, purpose, command, source_paths, artifact_paths):
        return ExecutionReports(self).run(key, purpose, command, source_paths, artifact_paths)

    @serialized
    def collect_run(self, key):
        return ExecutionReports(self).collect(key)

    @serialized
    def prepare_report(self, key, checks, reviewer, evidence_key):
        return ExecutionReports(self).prepare(key, checks, reviewer, evidence_key)

    @serialized
    def finish_report(self, report_id, reason, expected_revision=None, response_view='full'):
        return ExecutionReports(self).finish(report_id, reason, expected_revision, response_view)

    @serialized
    def finish(self, completion, reason, expected_revision=None, response_view='full'):
        return self._finish(completion, reason, expected_revision, response_view)

    def _finish(self, completion, reason, expected_revision=None, response_view='full'):
        self.available()
        expected_revision = self.revision(expected_revision)
        display = self.response_args(response_view)
        if self.state['phase'] == 'active':
            context = self.state.get('context')
            if not context or not self.delivered(context['hash'])['acknowledged']:
                raise ValueError('Consume and acknowledge context before completing work')
            value = self.perform('complete', ['work', 'complete', self.state['work'], '--session', self.state['session'],
                '--reason', reason, '--expected-revision', str(expected_revision), *display], completion)
            self.completed(value, phase='work_completed')
            expected_revision = value['project_revision']
        if self.state['phase'] != 'work_completed':
            raise ValueError('Work must be completed before finishing its session')
        value = self.perform('end', ['session', 'end', '--session', self.state['session'],
                                   '--expected-revision', str(expected_revision)])
        return self.completed(value, phase='ended')

    @serialized
    def inspect(self, session=None, work=None):
        # These public reads preserve source files; CLI projection refresh may write cache.
        sid, key = session or self.state['session'], work or self.state['work']
        value = dict(binding=self.binding, phase=self.state['phase'], pending=self.state['pending'],
                     session=None, work=None, recovery=None, side_effects_replayed=False)
        if sid and key:
            value['session'], value['work'] = self.session_identity(sid, key)
            value['recovery'] = self.host.ok('recovery', 'inspect', '--session', sid)
        else:
            value['sessions'] = self.host.ok('session', 'list')
        if self.state['pending'] and self.state['pending']['operation'] == 'run':
            value['executions'] = {key: ExecutionReports(self).observe(run)
                                   for key, run in self.state.get('runs', {}).items()}
        output = protected_file(self.host.receipts, '.inspection.json', json.dumps(value, ensure_ascii=False).encode())
        self.state['inspection'] = dict(path=str(output), sha256=digest(output), pending_id=(self.state['pending'] or {}).get('id'))
        self.save()
        return dict(value, inspection=self.state['inspection'])

    @serialized
    def reconcile(self, inspection_sha256, reason, session=None, work=None):
        """Explicit recovery decision after inspecting receipts; never replays a command."""
        pending, observed = self.state['pending'], self.state.get('inspection')
        if not pending or not observed or observed['pending_id'] != pending['id'] or observed['sha256'] != inspection_sha256:
            raise ValueError('Inspect this exact pending operation before reconciliation')
        if not reason.strip() or digest(observed['path']) != inspection_sha256:
            raise ValueError('An intact inspection and explicit reconciliation reason are required')
        sid, key = session or self.state['session'], work or self.state['work']
        if not sid or not key:
            raise ValueError('Recover a specific observed session; unbound begin outcomes need explicit adoption')
        value, item = self.session_identity(sid, key)
        phase = 'ended' if value['session']['status'] != 'active' else ('work_completed' if item['work']['status'] == 'completed' else 'active')
        # This records an operator decision, not inferred success of the uncertain command.
        self.state['history'].append(dict(pending, resolution='operator_reconciled', reason=reason, inspection=observed))
        if sid != self.state['session']:
            self.state['context'] = None
        self.state.update(pending=None, session=sid, work=key, phase=phase, last_revision=value['project_revision'])
        self.save()
        return dict(phase=phase, project_revision=value['project_revision'], side_effects_replayed=False)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    for key in ('binary', 'sha256', 'version', 'project', 'project-id', 'state'):
        p.add_argument('--'+key, required=True)
    sub = p.add_subparsers(dest='command', required=True)
    begin = sub.add_parser('begin')
    for key in ('work','agent','provider','model'):
        begin.add_argument('--'+key, required=True)
    begin.add_argument('--expected-revision', type=int)
    adopt = sub.add_parser('adopt')
    for key in ('session','work'):
        adopt.add_argument('--'+key, required=True)
    sub.add_parser('context')
    prepare = sub.add_parser('prepare')
    prepare.add_argument('--observation', help='JSON file with explicit host observations')
    prepare.add_argument('--goal', dest='goals', action='append', default=[])
    prepare.add_argument('--response-view', choices=['full', 'summary', 'action'], default='action')
    compact = sub.add_parser('observe-compaction', help='Call after a completed native compaction')
    compact.add_argument('--input', required=True, help='JSON with observation, optional policy and expected_revision')
    compact_get = sub.add_parser('compaction')
    compact_get.add_argument('--include-observation', action='store_true')
    compact_defer = sub.add_parser('defer-compaction')
    compact_defer.add_argument('--observation-event-id', required=True)
    compact_defer.add_argument('--expected-revision', type=int)
    progress = sub.add_parser('progress')
    progress.add_argument('--reason', required=True)
    progress.add_argument('--next-action', required=True)
    progress.add_argument('--expected-revision', type=int)
    progress.add_argument('--response-view', choices=['full', 'summary'], default='full')
    ack = sub.add_parser('ack')
    ack.add_argument('--consumed-hash', required=True)
    checkpoint = sub.add_parser('checkpoint')
    for key in ('consumed-hash','digest','next-action'):
        checkpoint.add_argument('--'+key, required=True)
    checkpoint.add_argument('--expected-revision', type=int)
    evidence = sub.add_parser('evidence')
    evidence.add_argument('--input', required=True)
    evidence.add_argument('--expected-revision', type=int)
    finish = sub.add_parser('finish')
    finish.add_argument('--input', required=True)
    finish.add_argument('--reason', required=True)
    finish.add_argument('--expected-revision', type=int)
    finish.add_argument('--response-view', choices=['full', 'summary'], default='full')
    run = sub.add_parser('run', help='Explicitly dispatch managed argv; never execute report text')
    run.add_argument('--input', required=True, help='JSON with key, purpose, command, source_paths, artifact_paths')
    collect = sub.add_parser('collect-run')
    collect.add_argument('--key', required=True)
    report = sub.add_parser('prepare-report')
    report.add_argument('--input', required=True, help='JSON with key, checks, reviewer, evidence_key')
    close = sub.add_parser('finish-report')
    close.add_argument('--report-id', required=True)
    close.add_argument('--reason', required=True)
    close.add_argument('--expected-revision', type=int)
    close.add_argument('--response-view', choices=['full', 'summary'], default='full')
    inspect = sub.add_parser('inspect')
    inspect.add_argument('--session'); inspect.add_argument('--work')
    reconcile = sub.add_parser('reconcile')
    reconcile.add_argument('--inspection-sha256', required=True)
    reconcile.add_argument('--reason', required=True)
    reconcile.add_argument('--session'); reconcile.add_argument('--work')
    args = vars(p.parse_args()); command = args.pop('command')
    wf = Workflow(args.pop('binary'),args.pop('sha256'),args.pop('version'),args.pop('project'),args.pop('project_id'),args.pop('state'))
    if command in ('run', 'prepare-report', 'observe-compaction'):
        args = json.loads(Path(args.pop('input')).read_text())
    elif 'input' in args:
        args['draft' if command == 'evidence' else 'completion'] = json.loads(Path(args.pop('input')).read_text())
    if command == 'prepare' and args['observation'] is not None:
        args['observation'] = json.loads(Path(args['observation']).read_text())
    if command == 'checkpoint': args['digest_text'] = args.pop('digest')
    value = getattr(wf, 'acknowledge' if command == 'ack' else command.replace('-', '_'))(**args)
    print(json.dumps(value, ensure_ascii=False, indent=2))


if __name__ == '__main__':
    try:
        main()
    except (ValueError, OSError, CommandFailed) as error:
        print(json.dumps({'ok':False,'error':str(error),'automatic_retry':False}), file=sys.stderr)
        sys.exit(1)
