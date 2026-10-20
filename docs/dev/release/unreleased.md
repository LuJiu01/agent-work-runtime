# Unreleased

Changes shipped in 0.6.0 are listed in [0.6.0](0.6.0.md).

## Added

- Explicit requirement items and YAML task `spec_refs`, with bounded, read-only
  `awr spec check` and MCP `awr_spec_check` views of one evaluator. Checks identify
  changed or superseded item bindings, missing task associations and contradictions
  with decision `affected_keys`; applicability never infers requirement coverage.
  References reuse the existing task-contract fingerprint and evidence rules.
  Run `awr source reindex` after upgrading to refresh the changed projections.
  Markdown task bindings and cross-document replacement traversal are deferred.
  See [specification references and coverage](../reference/specification-coverage.md).
