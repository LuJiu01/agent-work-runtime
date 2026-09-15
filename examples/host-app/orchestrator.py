"""Caller-driven bounded scheduling. AWR remains the work/acceptance authority.

The injected driver owns graph queries, Workflow sessions and the executor. Its
observe method must query a stable dispatch identity, never repeat dispatch.
This file neither calls a model nor launches shell commands.
"""
from contextlib import contextmanager
import json
import os
from pathlib import Path, PurePosixPath
import uuid

from workflow import atomic_json


def resources(paths):
    """Conservative path hints; missing/glob paths reserve the whole workspace.

    These are scheduling hints, not filesystem isolation. A real host may inject
    a resource resolver covering worktrees, databases, credentials and quotas.
    """
    if not paths or any(any(c in path for c in '*?[]') for path in paths):
        return ['*']
    result = []
    for path in paths:
        value = PurePosixPath(path.replace('\\', '/'))
        if value.is_absolute() or '..' in value.parts or str(value) == '.':
            return ['*']
        result.append(str(value).casefold().rstrip('/'))
    return sorted(set(result))


def conflicts(left, right):
    return '*' in left or '*' in right or any(
        a == b or a.startswith(b + '/') or b.startswith(a + '/')
        for a in left for b in right)


class Orchestrator:
    """One step inspects retained jobs and fills available slots; no polling loop.

    driver.binding identifies the pinned runtime, canonical project and executor.
    graph() returns the current complete AWR work_graph result.
    dispatch(job) reserves an AWR claim, delivers context and dispatches once using
      job['id']; its durable Workflow records must use that same identity.
    observe(job) queries those actual records and the executor and returns
      {'execution': 'running'|'unknown'|'stopped', 'receipt': <query receipt>}.
    A stopped executor is not a completion claim: the next AWR graph decides that.
    """
    def __init__(self, driver, state_path, parallelism=2, resolve_resources=None):
        if not 1 <= parallelism <= 64:
            raise ValueError('parallelism must be 1..64')
        self.driver, self.parallelism = driver, parallelism
        self.resolve_resources = resolve_resources or (lambda node: resources(node['paths']))
        self.path = Path(state_path).absolute()
        if self.path.is_symlink():
            raise ValueError('dispatch state must not be a symlink')
        self.path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        with self.guard():
            if not self.path.exists():
                atomic_json(self.path, dict(version=1, binding=driver.binding, jobs=[]))
            self.load()

    @contextmanager
    def guard(self):
        fd = os.open(str(self.path) + '.lock', os.O_CREAT | os.O_RDWR, 0o600)
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

    def load(self):
        self.state = json.loads(self.path.read_text())
        if self.state.get('version') != 1 or self.state['binding'] != self.driver.binding:
            raise ValueError('dispatch project/runtime/executor binding changed')

    def save(self):
        atomic_json(self.path, self.state)

    def step(self):
        with self.guard():
            self.load()
            for job in self.state['jobs']:
                if job['execution'] == 'stopped':
                    continue
                # Includes jobs persisted before dispatch whose response was lost.
                # Unknown means inspect again later, never a second dispatch.
                try:
                    observed = self.driver.observe(dict(job))
                    if observed['execution'] not in ('running', 'unknown', 'stopped'):
                        raise ValueError('invalid executor observation')
                    job['execution'] = observed['execution']
                    job['receipt'] = observed['receipt']
                except Exception:
                    job['execution'] = 'unknown'
                self.save()
            graph = self.driver.graph()
            if not graph.get('complete') or not graph.get('graph_valid'):
                raise ValueError('a complete valid AWR graph is required')
            nodes = {node['key']: node for node in graph['nodes']}
            for job in self.state['jobs']:
                # This is an observed source status, not an independent completion fact.
                job['observed_work_status'] = nodes.get(job['work'], {}).get('status')
            self.save()
            known = {job['work'] for job in self.state['jobs']}
            occupying = [j for j in self.state['jobs'] if j['execution'] != 'stopped']
            dispatched = []
            for key, node in sorted(nodes.items()):
                if len(occupying) >= self.parallelism:
                    break
                if not node['ready'] or key in known:
                    continue
                requested = self.resolve_resources(node)
                if not requested or any(conflicts(requested,j['resources']) for j in occupying):
                    continue
                job = dict(id=str(uuid.uuid4()), work=key, resources=requested,
                           execution='unknown', reviewed_graph=graph['graph_fingerprint'],
                           reviewed_revision=graph['project_revision'])
                self.state['jobs'].append(job)
                occupying.append(job)
                self.save()  # Before claims, processes, network dispatch, or any side effect.
                try:
                    self.driver.dispatch(dict(job))
                    job['execution'] = 'running'
                except Exception:
                    # Preserve the stable identity even when dispatch partially succeeded.
                    job['execution'] = 'unknown'
                self.save()
                dispatched.append(job['id'])
            return dict(dispatched=dispatched, jobs=self.state['jobs'],
                        project_revision=graph['project_revision'], side_effects_replayed=False)
