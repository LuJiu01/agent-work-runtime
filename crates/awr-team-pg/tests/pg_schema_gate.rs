#![cfg(feature = "pg-tests")]
//! Regression tests for the retrospective CR of PR #36 (TEAM-P2).
//! Isolation infra lives in tests/common (parsed-host guard, per-process
//! unique database, atomic registration); this file keeps the guard,
//! concurrency and ownership regressions plus the schema-gate cases.

use awr_team_pg::{Bootstrap, CommandRequest, PgError, TeamStore, check_schema};
use serde_json::json;
use std::sync::MutexGuard;
use tokio_postgres::Client;
use tokio_postgres::config::Config;

mod common;
use common::{
    app_client, check_loopback, connect_config, fresh_team_schema, gate_db_name, nonce,
    test_config, with_app_role, with_db,
};

const TENANT: &str = "tenant-g";
const PROJECT: &str = "project-g";
const ACTOR: &str = "agent-g";

async fn setup() -> (MutexGuard<'static, ()>, Client, String) {
    let (guard, admin, name) = fresh_team_schema().await;
    admin
        .batch_execute(
            "INSERT INTO awr_team.tenants(id,name,status) VALUES ('tenant-g','G','active');
             INSERT INTO awr_team.actors(tenant_id,id,kind,display_name,status) VALUES ('tenant-g','agent-g','agent','G','active');
             INSERT INTO awr_team.projects(tenant_id,id,key,mode,coordinator_epoch,status) VALUES ('tenant-g','project-g','gamma','team','epoch-g','active');
             INSERT INTO awr_team.work_scopes(tenant_id,project_id,id,name,status) VALUES ('tenant-g','project-g','main','main','active');
             INSERT INTO awr_team.work_items(tenant_id,project_id,id,external_key) VALUES ('tenant-g','project-g','work-g','G');",
        )
        .await
        .unwrap();
    (guard, admin, name)
}

/// The store path keeps the validated Config untouched: no URL
/// re-serialization, so IPv6 brackets, hostaddr overrides and Unix sockets
/// keep their meaning (CR #52 round 4).
fn app_store(db: &str) -> TeamStore {
    TeamStore::from_config(with_app_role(&test_config(), db))
}

fn touch(request_id: &str) -> CommandRequest {
    CommandRequest {
        tenant_id: TENANT.into(),
        project_id: PROJECT.into(),
        actor_id: ACTOR.into(),
        client_id: "client-g".into(),
        request_id: request_id.into(),
        op: "work.touch".into(),
        args: json!({"work_id": "work-g", "scope_id": "main"}),
    }
}

// CR #52 round 3/4: the guard judges the parsed host, not decoy substrings.
#[test]
fn loopback_guard_uses_parsed_host_not_decoy_fields() {
    let decoys = [
        "postgres://localhost:awr-test@192.0.2.10:55432/db",
        "postgres://postgres:127.0.0.1@192.0.2.10:55432/db",
        "postgres://postgres:awr-test@192.0.2.10:55432/localhost_backup",
        "postgres://postgres:awr-test@192.0.2.10:55432/db?application_name=localhost",
        "postgres://postgres:awr-test@localhost:55432/db?hostaddr=192.0.2.10",
        "postgres://postgres:awr-test@127.0.0.1,192.0.2.10/db",
    ];
    for decoy in decoys {
        let config: Config = decoy.parse().unwrap();
        assert!(check_loopback(&config).is_err(), "decoy passed: {decoy}");
    }
    let good = [
        "postgres://postgres:awr-test@127.0.0.1:55432/postgres",
        "postgres://postgres:awr-test@localhost/postgres",
        "postgres://postgres:awr-test@[::1]:55432/postgres",
    ];
    for ok in good {
        let config: Config = ok.parse().unwrap();
        assert!(check_loopback(&config).is_ok(), "loopback rejected: {ok}");
    }
}

// CR #52 round 4: concurrent first callers share one registered identity.
#[tokio::test]
async fn concurrent_first_callers_share_one_registered_database() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let created = Arc::new(AtomicUsize::new(0));
    let counter = created.clone();
    let cell = tokio::sync::OnceCell::new();
    let make_init = |created: Arc<AtomicUsize>| {
        move || {
            let created = created.clone();
            async move {
                created.fetch_add(1, Ordering::SeqCst);
                // Give a racing caller time to enter before registration completes.
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                "db-unique".to_string()
            }
        }
    };
    let (a, b) = tokio::join!(
        cell.get_or_init(make_init(created.clone())),
        cell.get_or_init(make_init(created))
    );
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "more than one database created"
    );
    assert_eq!(a, b, "callers observe different registrations");
    assert_eq!(a, "db-unique");
}

// CR #52 round 3: a pre-existing database with sentinel data is never adopted.
#[tokio::test]
async fn preexisting_same_prefix_databases_are_never_adopted() {
    let maintenance = connect_config(&with_db(&test_config(), "postgres")).await;
    let sentinel_db = format!("awr_team_gate_sentinel_{}", nonce(99));
    maintenance
        .batch_execute(&format!("CREATE DATABASE \"{sentinel_db}\""))
        .await
        .unwrap();
    let sentinel = connect_config(&with_db(&test_config(), &sentinel_db)).await;
    sentinel
        .batch_execute("CREATE TABLE keepme(id int primary key); INSERT INTO keepme VALUES (1)")
        .await
        .unwrap();
    // This suite's own database exists only after gate_db_name() ran; it is a
    // different, freshly created identity. Do NOT touch its schema here —
    // other tests hold the fixture lock for that.
    let ours = gate_db_name().await;
    assert_ne!(ours, sentinel_db);
    let kept: i64 = sentinel
        .query_one("SELECT count(*) FROM keepme", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(kept, 1, "sentinel database was modified");
    drop(sentinel);
    // The sentinel connection needs a moment to close before DROP DATABASE.
    for _ in 0..10 {
        let result = maintenance
            .batch_execute(&format!("DROP DATABASE \"{sentinel_db}\""))
            .await;
        if result.is_ok() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    panic!("could not drop the sentinel database created by this test");
}

// CR #36 P2-1: the app role can read the version but can never modify it.
#[tokio::test]
async fn app_role_checks_version_but_cannot_modify_it() {
    let (_lock, _admin, db) = setup().await;
    let app = app_client(&db).await;
    check_schema(&app)
        .await
        .expect("app role must read a compatible schema version");
    let update = app
        .execute(
            "UPDATE awr_team.schema_state SET version=version+1 WHERE component='awr_team'",
            &[],
        )
        .await;
    assert!(update.is_err(), "app role updated schema_state");
    let delete = app
        .execute(
            "DELETE FROM awr_team.schema_state WHERE component='awr_team'",
            &[],
        )
        .await;
    assert!(delete.is_err(), "app role deleted schema_state");
    let insert = app
        .execute(
            "INSERT INTO awr_team.schema_state(component, version) VALUES ('awr_team_fake', 1)",
            &[],
        )
        .await;
    assert!(insert.is_err(), "app role inserted into schema_state");
}

// CR #52 P2-1: a database bootstrapped by the OLD version (schema_state
// fully revoked) keeps working after the non-destructive grant upgrade.
#[tokio::test]
async fn upgrade_regrants_existing_database_without_data_loss() {
    let (_lock, admin, db) = setup().await;
    admin
        .batch_execute("REVOKE ALL ON awr_team.schema_state FROM awr_app")
        .await
        .unwrap();
    let store = app_store(&db);
    let err = store.execute(touch("gate-pre-upgrade")).await.unwrap_err();
    assert!(
        matches!(err, PgError::Db(_)),
        "old grants unexpectedly still read schema_state: {err}"
    );
    // The owner-side, repeatable upgrade step (`awr-server migrate
    // --app-role awr_app` calls the same entry). No schema rebuild.
    Bootstrap::grant_app(&admin, "awr_app").await.unwrap();
    let app = app_client(&db).await;
    check_schema(&app)
        .await
        .expect("upgraded app role reads version");
    let outcome = store.execute(touch("gate-post-upgrade")).await.unwrap();
    assert_eq!(outcome.committed_project_revision, "1");
    let denied = app
        .execute(
            "UPDATE awr_team.schema_state SET version=version+1 WHERE component='awr_team'",
            &[],
        )
        .await;
    assert!(
        denied.is_err(),
        "upgrade must not grant schema_state writes"
    );
}

// CR #36 P2-2: a too-new version blocks the real command entry with zero side effects.
#[tokio::test]
async fn incompatible_version_blocks_command_without_side_effects() {
    let (_lock, admin, db) = setup().await;
    let store = app_store(&db);
    let first = store.execute(touch("gate-1")).await.unwrap();
    assert_eq!(first.committed_project_revision, "1");
    admin
        .execute(
            "UPDATE awr_team.schema_state SET version=version+1 WHERE component='awr_team'",
            &[],
        )
        .await
        .unwrap();
    let err = store.execute(touch("gate-2")).await.unwrap_err();
    assert!(matches!(err, PgError::SchemaIncompatible(_)));
    let revision: i64 = admin
        .query_one(
            "SELECT project_revision FROM awr_team.projects WHERE id='project-g'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(revision, 1, "blocked command still bumped the revision");
    let events: i64 = admin
        .query_one(
            "SELECT count(*) FROM awr_team.events WHERE project_id='project-g'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(events, 1, "blocked command wrote an event");
    let ops: i64 = admin
        .query_one(
            "SELECT count(*) FROM awr_team.operations WHERE project_id='project-g'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(ops, 1, "blocked command wrote an operation receipt");
}

// CR #36 P2-2: a missing version record also blocks the command entry.
#[tokio::test]
async fn missing_version_record_blocks_command() {
    let (_lock, admin, db) = setup().await;
    admin
        .execute(
            "DELETE FROM awr_team.schema_state WHERE component='awr_team'",
            &[],
        )
        .await
        .unwrap();
    let store = app_store(&db);
    let err = store.execute(touch("gate-3")).await.unwrap_err();
    assert!(matches!(err, PgError::SchemaIncompatible(_)));
}

// CR #36 P2-4: the inner revision is a decimal string on the immediate
// return, in the persisted event/operation (type checked, not only text),
// and on idempotent replay.
#[tokio::test]
async fn receipt_revision_is_decimal_string_end_to_end() {
    let (_lock, admin, db) = setup().await;
    let big: i64 = 9_007_199_254_740_992; // 2^53, beyond JS safe integers
    admin
        .execute(
            "UPDATE awr_team.projects SET project_revision=$1 WHERE id='project-g'",
            &[&big],
        )
        .await
        .unwrap();
    let store = app_store(&db);
    let outcome = store.execute(touch("gate-4")).await.unwrap();
    let expected = "9007199254740993";
    assert_eq!(outcome.committed_project_revision, expected);
    assert_eq!(outcome.result["revision"], json!(expected));
    let event_type: String = admin
        .query_one(
            "SELECT jsonb_typeof(payload_json->'revision') FROM awr_team.events WHERE project_id='project-g'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        event_type, "string",
        "persisted event revision is not a JSON string"
    );
    let op_type: String = admin
        .query_one(
            "SELECT jsonb_typeof(result_json->'revision') FROM awr_team.operations WHERE project_id='project-g'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        op_type, "string",
        "persisted operation revision is not a JSON string"
    );
    let replay = store.execute(touch("gate-4")).await.unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.result["revision"], json!(expected));
    assert_eq!(replay.committed_project_revision, expected);
}

// Strengthened (CR #52 note): the REAL command path rolls back business
// state, revision, events and the receipt when the operations insert fails.
#[tokio::test]
async fn failed_receipt_write_rolls_back_the_real_command() {
    let (_lock, admin, db) = setup().await;
    admin
        .batch_execute(
            "CREATE OR REPLACE FUNCTION awr_team.fail_receipt_insert() RETURNS trigger
             LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected receipt failure'; END; $$;
             CREATE TRIGGER fail_receipt BEFORE INSERT ON awr_team.operations
             FOR EACH ROW EXECUTE FUNCTION awr_team.fail_receipt_insert();",
        )
        .await
        .unwrap();
    let store = app_store(&db);
    let err = store.execute(touch("gate-5")).await.unwrap_err();
    assert!(matches!(err, PgError::Db(_)));
    admin
        .batch_execute(
            "DROP TRIGGER fail_receipt ON awr_team.operations;
             DROP FUNCTION awr_team.fail_receipt_insert();",
        )
        .await
        .unwrap();
    let revision: i64 = admin
        .query_one(
            "SELECT project_revision FROM awr_team.projects WHERE id='project-g'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(
        revision, 0,
        "failed receipt write still bumped the revision"
    );
    let runtime: i64 = admin
        .query_one(
            "SELECT count(*) FROM awr_team.work_runtime WHERE project_id='project-g'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(runtime, 0, "failed receipt write left business state");
    let events: i64 = admin
        .query_one(
            "SELECT count(*) FROM awr_team.events WHERE project_id='project-g'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(events, 0, "failed receipt write left an event");
    let ops: i64 = admin
        .query_one(
            "SELECT count(*) FROM awr_team.operations WHERE project_id='project-g'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(ops, 0, "failed receipt write persisted a receipt");
}

// Positive control: concurrent retries of one request commit exactly once.
#[tokio::test]
async fn concurrent_retry_commits_once() {
    let (_lock, admin, db) = setup().await;
    let store = app_store(&db);
    let (a, b) = tokio::join!(
        store.execute(touch("gate-6")),
        store.execute(touch("gate-6"))
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert!(a.replayed ^ b.replayed, "exactly one call commits");
    assert_eq!(a.committed_project_revision, b.committed_project_revision);
    let events: i64 = admin
        .query_one(
            "SELECT count(*) FROM awr_team.events WHERE project_id='project-g'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(events, 1);
}
