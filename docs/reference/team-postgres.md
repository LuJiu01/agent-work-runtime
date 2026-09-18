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

## Connection pooling and TLS

Domain stores acquire connections from a `deadpool-postgres` pool instead of
opening one TCP connection per operation (ADR-0004). Pool size defaults to 8
per store instance and can be overridden with `AWR_TEAM_PG_POOL_MAX_SIZE`.
Acquire/create/recycle timeouts are fixed at 10s/5s/5s. Scope binding uses
transaction-local `set_config`, so fast connection recycling is safe.

Owner migration commands (`migrate`, `check_schema`) keep a dedicated single
connection and do not use the pool.

TLS is an opt-in `tls` cargo feature (rustls + webpki-roots). With the feature
enabled, `sslmode=require` in `AWR_TEAM_DATABASE_URL` selects a verified TLS
connection; `disable`/`prefer` or an omitted sslmode stays plaintext. Without
the feature, a TLS-requiring URL fails with an explicit error instead of
silently downgrading.

This is not a production high-availability topology.

## Consistent reads

`work.prepare`, `work.graph`, `session.inspect` and `events.list` run in
`REPEATABLE READ`. Event cursors are `awr-team-cursor-v1:{epoch}:{revision}:{index}`.
A changed coordinator epoch returns `EPOCH_CHANGED` instead of skipping history.
Required hard rules are never dropped to fit a context budget.

Experimental entry:

```sh
cargo run -p awr-server -- query --op capabilities
cargo run -p awr-server -- query --op work.prepare --body '{"tenant_id":"...","project_id":"...","work_id":"work-a"}'
```

Unknown query names return `Unsupported` without changing personal CLI/MCP.

## Sessions and leases

Claims are unique per work item while `state='active'`. Lease expiry uses
`clock_timestamp()` after the project lock, not transaction `now()`. Renew
replays keep the original `expires_at`. Wait records do not extend the lease.
Handoff increments the work fence so the previous actor cannot write.

## Dependencies and conflicts

Required dependency graphs are rejected if they cycle or reference missing
work. Resource reservations treat directory prefixes as overlapping path
segments (`src/foo` vs `src/foo/bar`), not raw string prefixes (`src/a` vs
`src/abc`). Splitting a work item does not complete the parent. Unknown
scopes are rejected instead of falling back to `main`.


## Execution protocol

`execution.prepare` writes the execution row, effect key and outbox record in one
transaction. Outbox delivery is claimed with `SKIP LOCKED` after the project lock
and sent outside that transaction. The reference runner persists `execution_id`
before side effects; a duplicate delivery returns the journaled outcome without a
new effect key. `unknown` sets `recovery_blocked` and keeps resource reservations.
`cancel_requested` is not `cancelled`. Callers cannot mint `trusted_executor`
receipts. Uncontrolled third parties do not receive an exactly-once claim.


## Evidence and completion

Completion receipts are written only through the domain `complete` entry.
`caller_asserted` reports cannot satisfy `trusted_execution_and_review`.
Authors cannot approve their own review round; a new bundle hash invalidates
the previous round. `work_runtime.state='completed'` requires
`selected_completion_id`. Ordinary confirmation is allowed only when the
current contract already selects that policy.


## Import and restore

Import is freeze → export → dry-run → load → activate. The same
`import_key` and manifest hash replay the original job and do not create
duplicate work. Divergent local sources are rejected instead of last-write
wins. Historical self-reports stay `caller_asserted`. Restore mints a new
coordinator epoch, revokes restored credentials, fails pending outbox rows
instead of replaying them, and refuses a SQLite file rollback after the
team project has accepted new revisions.
