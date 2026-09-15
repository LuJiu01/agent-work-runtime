# Host workflows with less bookkeeping

The [host example](../../examples/host-app/README.md) keeps application-owned
workflow state separate from AWR's authoritative project sources. A host can
carry session identities, revisions and execution receipts so the agent spends
less effort assembling repeated arguments.

This integration is being extended in three areas: shared work preparation and
conditional management records, optional concise operation results, and report
assembly from actual execution receipts. Context caching and delta delivery are
outside this change.

## Compatibility contract

- Existing work, source files, evidence and completion rules retain their meaning.
- Capability discovery selects supported operations before any write. A failed or
  uncertain write is inspected through its original receipt; it does not trigger
  a second write through a fallback path.
- Work preparation delivers the full required context. The caller explicitly
  acknowledges the exact delivered hash before saving a checkpoint or finishing.
  An acknowledgement is a caller assertion, not proof of model comprehension.
- Management observations come from the host. Unknown observations remain
  unknown, and continuous work never automatically downgrades to lightweight work.
- A concise result must retain decisions, blockers, errors and recovery facts.
  Full receipts remain available; returning fewer bytes cannot imply that fewer
  completion conditions apply.
- Execution receipts describe commands that actually ran. A successful exit code
  alone does not establish acceptance: reviewed checks still have to cover the
  current work contract, and AWR revalidates evidence at completion.

## Measuring the change

Separate first-time project onboarding, normal task execution and independent
verification probes in workflow reports. Keep the total alongside these subtotals
so apparent savings cannot come from removing initialization or negative checks
from the accounting. Measure lightweight and continuous workflows against the
same acceptance conditions. Tool-return bytes do not establish model-token or
billing savings.

## Prepare through the host

`Workflow.prepare(observation=None, goals=())` negotiates `workflow.prepare` and
`work.management` from the pinned program's capability catalog. It calls
`work prepare` when available and returns full context, readiness, continuity and
management facts together. Older programs use `context compile`; the result
explicitly reports that management observations were not recorded.

Provide an observation only after the host has checked those facts. Unknown
fields remain absent or null. With no new observation, a matching assessment is
left alone unless AWR requests a new record. Explicit new observations are always
recorded, including a newly discovered wait or additional executor. Each call
fetches fresh context and resets its acknowledgement, even when no new management
record is necessary.

```python
prepared = workflow.prepare(observation=observed_facts, goals=["G"])
context = prepared["context"]
# Deliver context to the agent and consume the returned rendered_context first.
workflow.acknowledge(context["work_context"]["context_hash"])
workflow.progress("Reviewed the required context", "Implement the change")
```

Lifecycle methods can use the workflow's last observed revision when
`expected_revision` is omitted. This removes repeated argument assembly, not
concurrency checks: another writer can still cause `RevisionConflict`. Inspect
and explicitly reconcile that result before proceeding. The original `context`,
`evidence` and `finish` entry points remain available.
