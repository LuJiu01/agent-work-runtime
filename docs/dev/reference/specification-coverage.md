# Explicit specification references and coverage

`awr spec check` and MCP `awr_spec_check` inspect declared requirements and task
references. They report changed versions, superseded items, missing task coverage
and unresolved references. They do not read arbitrary PRD paths, scan prose for
requirements, create tasks, change acceptance, or establish implementation or
completion. This capability is on development `main`, outside the 0.5.1 release.

## One authoritative specification

Register the **actual authoritative document**, using the existing
`decisions` domain and `markdown-directory-v1` adapter. Do not copy its requirements
into a second editable specification registry. Explicit metadata annotates the
document's existing identifiers; its body remains the requirement text. The
existing decision status/adoption lifecycle governs its approval.

```toml
[[sources]]
domain = "decisions"
role = "supporting"
path = "specifications"
adapter = "markdown-directory-v1"
```

An illustrative document in that registered directory:

```markdown
---
id: configuration-spec
status: accepted
specification:
  version: "2"
  items:
    - id: OLD
      disposition: superseded
      superseded_by: NEW
    - id: NEW
      disposition: required
    - id: EXPORT
      disposition: future
    - id: REMOVED
      disposition: cancelled
    - id: EXTERNAL
      disposition: out_of_scope
---
# Configuration requirements

## Decision
NEW replaces OLD. EXPORT is deferred work; REMOVED is cancelled and EXTERNAL
does not belong to the product. Their detailed requirements live in this document.
```

`required` and `future` items need an explicit task association. `cancelled` and
`out_of_scope` items do not. A `superseded` item requires a resolvable replacement
within the same document; missing targets and cycles remain `unknown`. Document
supersession continues to use the existing `superseded_by: DocumentVersion`
metadata, returned with the specification's source reference. No replacement is
inferred from dates, names or similarity.

## Bind an explicit task or acceptance criterion

In the existing YAML ledger, add `spec_refs` to the actual work item. Copy the
registered document's exact fingerprint from `awr source list` after indexing:

```yaml
work_items:
  - id: BUILD-CONFIG
    title: Deliver reviewed configuration saving
    status: planned
    acceptance: [Save the reviewed configuration]
    spec_refs:
      - specification: configuration-spec
        item: NEW
        approved_version: "2"
        source_fingerprint: "sha256:<64 hexadecimal characters from source list>"
        criterion: Save the reviewed configuration
```

`specification` is a stable registered decision key, not a filesystem path.
`approved_version` and `source_fingerprint` are both required. The fingerprint
binds the whole document, including approval metadata: changing bytes without
bumping its human version still requires review. Adoption is an explicit source
assertion, not independent proof that a person reviewed it. Do not automatically
refresh a task's fingerprint to make a warning disappear.

`criterion`, when present, must exactly match one current acceptance string.
Omitting it declares a work-wide association, not per-criterion coverage.
Duplicate or malformed references fail source validation. Existing ledgers and
decisions without these optional fields keep their existing behavior.

Use existing reviewed source editing/batch operations to update the binding.
`spec_refs` is supported by YAML work creation and field changes; changing it is
part of the planning contract and cannot bypass an active execution claim.
After upgrading, explicitly reindex: the ledger and decision adapter versions
change so previously ignored metadata is projected. No database schema migration
or additional task authority is introduced.

## Inspect without changing state

```sh
awr source reindex
awr spec check --spec configuration-spec --json
```

MCP: discover `awr_work` then call its `awr_spec_check` child with
`{"spec":"configuration-spec","limit":1000}`. Flat clients can call the child
directly. Shared clients also pass the authorized `project` key. Existing project
read permissions apply; the checker grants no additional access.

The check itself uses a coherent private snapshot, verifies current indexed
sources before and after, and persists neither source edits nor runtime events.
Changed files/mappings or an upgrade requiring reindex return `SourceStale`;
explicitly refresh and inspect again. Source `fresh` only describes the indexed
bytes. It does not establish that task requirements agree with the specification.

Results contain source references, exact work bindings, declared versions,
replacement identifiers, requirement coverage and reason codes:

| Result | Meaning |
| --- | --- |
| Reference `current` | Its explicit version, fingerprint, item and optional criterion resolve; the work association is admitted and active. |
| Reference `needs_review` | An approved version/content changed, an item/document was superseded or withdrawn, or the mapped work is cancelled/archived. |
| `unknown` | Missing/ambiguous/unapproved specification, unresolved item/criterion/replacement, draft/unknown work scope, or missing explicit specification declarations. |
| Requirement `covered` | At least one current declared task association exists. This is coverage, not a test result. |
| Requirement `uncovered` | An explicit required/future item has no declared task reference. |
| Requirement `not_required` | Explicitly cancelled/out of scope, or superseded with a resolved replacement. |

Overall `matched` means only that the selected explicit declarations have no
reported gaps. Unknowns take precedence over `needs_review`. References to
unregistered keys remain unknown even when every configured source is fresh;
unregistered files are never opened or silently imported.

`limit` bounds the complete combined specification/requirement/reference row
count (default 1000, maximum 10000). Oversized selections return `BudgetExceeded`
without silently truncating findings; select a specification or raise the limit
within the ceiling. Reports also have a 4 MiB serialized ceiling. There is no
project completion percentage, automatic reconciliation or completion-policy
change.
