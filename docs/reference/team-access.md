# Team CLI, MCP and HTTP access

Team writes use a versioned envelope. Actor, tenant and project come from
authorization. The request body cannot change project ownership.

Personal `awr` and `awr-mcp` do not link PostgreSQL. `awr remote` stores an
endpoint, project key and credential *environment name* only. A missing or
offline remote cannot fall back to a successful local claim or completion.

Old clients that omit `protocol_version` receive `PROTOCOL_UNSUPPORTED`.
64-bit versions travel as decimal strings.

The three surfaces (`http`, `mcp`, `cli`) share `awr_team::execute`. Domain
mutations still run on the Team server after this admission check.
