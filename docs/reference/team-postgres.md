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

Source publish is ingest → approve → activate. Path checks, hashing and
parser binding happen before the project lock. The lock only writes already
hashed, immutable rows. An unactivated candidate cannot be read as the
current contract. A failed activation keeps the previous `active_snapshot_id`.

This is not a production high-availability topology.
