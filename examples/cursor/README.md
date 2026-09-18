# Cursor integration examples

[L1 Cursor note](../../docs/integrations/cursor.md) records merge paths.
Session lifecycle stays on the [L0 workflow](../../docs/integrations/session-workflow.md).

`mcp.json.example` is a stdio template with Cursor's documented fields (`type`,
`command`, `args`). Merge it into `.cursor/mcp.json` or `~/.cursor/mcp.json`.
Do not commit a project file that points at a machine-local AWR root.
