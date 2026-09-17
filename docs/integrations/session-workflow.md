# Host-agnostic session workflow

This is the **L0** operator path. It works from any coding agent's terminal
tool, and from any MCP client that can start `awr-mcp` or reach the shared
HTTP service. Host-specific merge paths live in [L1/L2 notes](README.md).

AWR 0.4.0 packages include the CLI, stdio MCP, session/claim tools and the
[shared HTTP service](../reference/mcp-service.md).

## Bind the project

Initialize the target project from a reviewed source manifest, as in
[the basic example](../../examples/basic/README.md). Starting `awr-mcp` does
not create or migrate a database. Use absolute executable and project paths.

```sh
AWR_BIN=/absolute/path/to/awr
AWR_PROJECT=/absolute/path/to/initialized/project
AWR_WORK=EXAMPLE-001
AWR_AGENT=agent-primary
AWR_MODEL=your-current-model
AWR_NOTES=$(mktemp -d "${TMPDIR:-/tmp}/awr-session.XXXXXX")
awrj() { "$AWR_BIN" --project "$AWR_PROJECT" --json "$@"; }

awrj session list --active
awrj ready
```

`--provider` and `--model` are recorded labels. They do not invoke a host.
If a session already exists for the work, use `session show` and that AWR
session ID. Native chat IDs are not AWR session IDs.

## Generic MCP

Any MCP client can launch a project-bound stdio server:

```json
{
  "mcpServers": {
    "awr": {
      "command": "/absolute/path/to/awr-mcp",
      "args": ["--project", "/absolute/path/to/initialized/project"]
    }
  }
}
```

Reload the client and call `awr_project_status` before work. Check the project
identity. The server does not need a model API key. Shared HTTP uses a URL and
a bearer token; every tool then requires an explicit `project` key. See
[MCP tools](../../crates/awr-mcp/README.md).

Stdio cannot run on a laptop filesystem from a remote/cloud agent. Those hosts
need a reachable HTTP service whose registered roots exist on the server.

## Start or resume

New work, with a claim:

```sh
AWR_REV=$(awrj status | jq -er '.project_revision')
awrj session start --work "$AWR_WORK" --agent "$AWR_AGENT" \
  --provider generic --model "$AWR_MODEL" --claim --ttl-ms 3600000 \
  --expected-revision "$AWR_REV" > "$AWR_NOTES/start.json"
AWR_SESSION=$(jq -er '.session.id' "$AWR_NOTES/start.json")
```

The claim is runtime ownership. It does not rewrite source work status.

Handoff from a predecessor session:

```sh
AWR_PREDECESSOR=the-recorded-awr-session-id
awrj session show "$AWR_PREDECESSOR"
AWR_REV=$(awrj status | jq -er '.project_revision')
awrj session resume --from-session "$AWR_PREDECESSOR" \
  --agent "$AWR_AGENT" --provider generic --model "$AWR_MODEL" \
  --budget 5000 --expected-revision "$AWR_REV" > "$AWR_NOTES/resume.json"
jq -e '.context_ready' "$AWR_NOTES/resume.json"
AWR_SESSION=$(jq -er '.resumed.session.id' "$AWR_NOTES/resume.json")
```

Resume creates a new AWR session. It does not switch the host's native chat.

## Compile context

```sh
awrj context bootstrap --session "$AWR_SESSION" --budget 1000 \
  > "$AWR_NOTES/bootstrap.json"
jq -e '.context.complete' "$AWR_NOTES/bootstrap.json"

awrj context compile --work "$AWR_WORK" --session "$AWR_SESSION" \
  --budget 5000 > "$AWR_NOTES/context.json"
jq -e '.completeness.complete and (.work_context != null)' "$AWR_NOTES/context.json"
```

Read the packet, not only the boolean. On `BudgetExceeded`, inspect `required`
and widen the budget or scope. On `SourceStale`, inspect the source change and
run `awr source reindex` before reading again. MCP `awr_context_compile` is
read-only against the persistent index; CLI context reads may refresh it.

## Checkpoint and continue

```sh
AWR_CONTEXT_HASH=$(jq -er '.work_context.context_hash' "$AWR_NOTES/context.json")
AWR_REV=$(awrj status | jq -er '.project_revision')
awrj session checkpoint --session "$AWR_SESSION" \
  --context-hash "$AWR_CONTEXT_HASH" \
  --digest "Record the work actually done; do not invent a passing review." \
  --next-action "State the exact next operator or agent action." \
  --open-loop "List every unresolved loop." \
  --expected-revision "$AWR_REV" > "$AWR_NOTES/checkpoint.json"
```

Digest and hash are caller assertions. Use returned/current revisions after a
save. Incomplete saves are not recovery checkpoints.

When the same AWR session is still active after host compaction, compile again.
Use `session resume` only for a real session handoff.

## Bind a native conversation

Attach to an **active** AWR session:

```sh
awrj client bind --client generic --external-session HOST_CONVERSATION_ID \
  --work "$AWR_WORK" --session "$AWR_SESSION"
```

Continue from a **predecessor** (creates a successor session, then binds):

```sh
awrj client bind --client generic --external-session HOST_CONVERSATION_ID \
  --work "$AWR_WORK" --from-session "$AWR_PREDECESSOR"
```

Do not pass both flags. `--client generic` is the L0 identity. `awr client
install` is L2 and currently exists only for Codex; other values return
`Unsupported`. That error is not a missing binary or a failed MCP connection.

## End

```sh
AWR_REV=$(awrj status | jq -er '.project_revision')
awrj session end --session "$AWR_SESSION" --outcome incomplete \
  --expected-revision "$AWR_REV"
```

Ending releases claims. It does not complete source work.

## Fixture

[examples/codex/lifecycle.py](../../examples/codex/README.md) walks this CLI
path on a copy of `examples/basic`. It invokes AWR, not a coding agent. The
directory name is historical; treat the script as the L0 fixture.

## Verification boundary

A configured MCP server, a bind receipt or this fixture does not prove that a
named host called a tool, installed hooks or accepted business work. Record the
host version and the exact tools or commands that ran.
