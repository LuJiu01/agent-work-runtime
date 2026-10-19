# Unreleased

## Concise prepare omission indexes

`work prepare` / `awr_work_prepare` now summarize optional omission indexes by
default within `summary` and `action`. No additional CLI/MCP option is required.
Successful, complete responses defer a nonempty `omitted_chunks` array only
when every entry has the known whole-chunk budget-selection reason. The
existing `response_view` metadata lists the deferred path, reports its total
in `detail_counts`, and explains the reason and when to inspect `full_result`
in a bounded `detail_note`. A single entry without entity annotations stays
inline rather than gaining summary metadata. Zero omissions remain unchanged;
other reasons, incomplete contexts and errors keep their details.

This changes concise JSON field availability within the existing view metadata
version. Consumers that need every omitted entry must request `full`; do not
interpret a missing array as zero omissions. Full remains the default when no
view is specified. Full output, `context compile`, required context/hash and
existing error, budget and write-receipt semantics retain their meaning.

Full-details reads are fresh queries. Preserve the original request scope,
including explicit project routing over shared HTTP, and compare revision/hash.
See the [concise-view contract](../integrations/host-workflow.md#optional-concise-results)
and [design discussion](https://github.com/originoneai/awr/issues/351).
