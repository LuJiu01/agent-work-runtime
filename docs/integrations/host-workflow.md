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
