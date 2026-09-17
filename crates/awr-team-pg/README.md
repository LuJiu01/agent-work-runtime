# awr-team-pg

PostgreSQL coordination store for Team V1. The personal SQLite runtime does
not depend on this crate.

This crate uses `tokio-postgres` rather than `sqlx`. sqlx 0.8 pulls
`sqlx-sqlite` and conflicts with the personal runtime's `rusqlite` /
`libsqlite3-sys` link.

```sh
docker compose -f docker/team-postgres.yml up -d
export AWR_TEAM_DATABASE_URL='postgres://postgres:awr-test@127.0.0.1:55432/awr_team_test?sslmode=disable'
cargo run -p awr-server -- migrate
cargo test -p awr-team-pg --features pg-tests
```

Default `cargo test --workspace` compiles this crate but does not run the
PostgreSQL tests, so CI without Postgres stays green. `awr-server migrate`
is a no-op when `awr_team.schema_state` already matches; it refuses to start
when the version is missing or unexpected.
