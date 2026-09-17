# Team PostgreSQL store

Team V1 coordination state lives in PostgreSQL. Personal CLI/MCP still use
SQLite and do not link this store.

## Local verification

```sh
docker compose -f docker/team-postgres.yml up -d
export AWR_TEAM_DATABASE_URL='postgres://postgres:awr-test@127.0.0.1:55432/awr_team_test?sslmode=disable'
cargo run -p awr-server -- migrate
cargo test -p awr-team-pg --features pg-tests
```

`awr-server check` exits non-zero when `awr_team.schema_state` is missing or
the version does not match. `awr-server migrate` applies owner migrations on
a clean database and returns successfully when the expected version is
already present. The application role is not table owner and does not
receive `BYPASSRLS`. Event history is insert-only for that role.

This is not a production high-availability topology.
