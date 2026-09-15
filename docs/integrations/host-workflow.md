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

## Optional concise results

Use `--response-view summary` with CLI `work prepare` or source work transitions
(including `work complete`). MCP `awr_work_prepare` and `awr_work_transition`
accept `response_view: "summary"`. The default is `full`. Hosts negotiate the CLI
capability `workflow.response_summary`; MCP clients inspect the tool schema.
`Workflow.prepare`, `progress` and `finish` accept `response_view="summary"`.

Preparation omits only `selected_chunks` and `selected_entities`, two indexes of
the returned packet. Rendered context, hash, identity, source versions,
completeness, omitted-chunk reasons, claims, readiness, waits and management
requirements are retained. Every preparation is a fresh query. To retrieve full
indexes, repeat the read in full mode and compare its revision and context hash;
that query may observe newer sources.

Successful transitions return changes, transition, target, intent, source outcome
and recovery information without repeating the full proposal patch and event
payload. The response lists its omitted fields and full-receipt lookups. CLI
clients can read the stored proposal and event with `--full`. MCP summary writes
require a stable `request_id`, including over stdio, so `awr_operation_get` can
return the original complete receipt within the same client/project scope.
Changing only the view of that same request reads its receipt rather than
executing the transition again. Domain arguments remain bound to the request.

Errors, incomplete context, partial writes and unknown results are returned in
full. Concise output grants no additional permission and cannot satisfy a missing
acceptance criterion. Source-change previews remain full: they must still be
reviewed before applying changes.
