#![cfg(feature = "pg-tests")]
//! Regression tests for the retrospective CR of PR #36 (TEAM-P2).
//! All cases run against real PostgreSQL through the real command entry,
//! not only the standalone diagnostic function.

use awr_team_pg::{Bootstrap, CommandRequest, PgError, TeamStore, check_schema, migrate};
use serde_json::json;
use std::sync::{Mutex, MutexGuard};
use tokio_postgres::{Client, NoTls};

static DB: Mutex<()> = Mutex::new(());

const TENANT: &str = "tenant-g";
const PROJECT: &str = "project-g";
const ACTOR: &str = "actor-g";
const CLIENT: &str = "client-g";

fn admin_url() -> String {
    std::env::var("AWR_TEAM_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:awr-test@127.0.0.1:55432/awr_team_test".into())
}
fn app_url() -> String {
    admin_url().replacen("postgres:awr-test", "awr_app:app-test", 1)
}
async fn connect(url: &str) -> Client {
    let (client, connection) = tokio_postgres::connect(url, NoTls)
        .await
        .expect("postgres 17 must be running for schema gate tests");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}
async fn setup() -> (MutexGuard<'static, ()>, Client) {
    let guard = DB.lock().expect("db fixture lock");
    let admin = connect(&admin_url()).await;
    admin
        .batch_execute("DROP SCHEMA IF EXISTS awr_team CASCADE")
        .await
        .unwrap();
    migrate(&admin).await.unwrap();
    admin
        .batch_execute(
            "DO $$ BEGIN CREATE ROLE awr_app LOGIN PASSWORD 'app-test' NOSUPERUSER NOBYPASSRLS; EXCEPTION WHEN duplicate_object THEN NULL; END $$",
        )
        .await
        .unwrap();
    Bootstrap::grant_app(&admin, "awr_app").await.unwrap();
    admin
        .batch_execute(
            "INSERT INTO awr_team.tenants(id,name,status) VALUES ('tenant-g','G','active');
             INSERT INTO awr_team.actors(tenant_id,id,kind,display_name,status) VALUES ('tenant-g','actor-g','agent','G','active');
             INSERT INTO awr_team.projects(tenant_id,id,key,mode,coordinator_epoch,status) VALUES ('tenant-g','project-g','gamma','team','epoch-g','active');
             INSERT INTO awr_team.work_scopes(tenant_id,project_id,id,name,status) VALUES ('tenant-g','project-g','main','main','active');
             INSERT INTO awr_team.work_items(tenant_id,project_id,id,external_key) VALUES ('tenant-g','project-g','work-g','G');",
        )
        .await
        .unwrap();
    (guard, admin)
}
fn touch(request_id: &str) -> CommandRequest {
    CommandRequest {
        tenant_id: TENANT.into(),
        project_id: PROJECT.into(),
        actor_id: ACTOR.into(),
        client_id: CLIENT.into(),
        request_id: request_id.into(),
        op: "work.touch".into(),
        args: json!({"work_id": "work-g", "scope_id": "main"}),
    }
}

// CR #36 P2-1: the app role can read the version but can never modify it.
#[tokio::test]
async fn app_role_checks_version_but_cannot_modify_it() {
    let (_lock, _admin) = setup().await;
    let app = connect(&app_url()).await;
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

// CR #36 P2-2: a too-new version blocks the real command entry with zero side effects.
#[tokio::test]
async fn incompatible_version_blocks_command_without_side_effects() {
    let (_lock, admin) = setup().await;
    let store = TeamStore::new(app_url());
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
    let (_lock, admin) = setup().await;
    admin
        .execute(
            "DELETE FROM awr_team.schema_state WHERE component='awr_team'",
            &[],
        )
        .await
        .unwrap();
    let store = TeamStore::new(app_url());
    let err = store.execute(touch("gate-3")).await.unwrap_err();
    assert!(matches!(err, PgError::SchemaIncompatible(_)));
}

// CR #36 P2-4: the inner revision is a decimal string on the immediate
// return, in the persisted event and operation, and on idempotent replay.
#[tokio::test]
async fn receipt_revision_is_decimal_string_end_to_end() {
    let (_lock, admin) = setup().await;
    let big: i64 = 9_007_199_254_740_992; // 2^53, beyond JS safe integers
    admin
        .execute(
            "UPDATE awr_team.projects SET project_revision=$1 WHERE id='project-g'",
            &[&big],
        )
        .await
        .unwrap();
    let store = TeamStore::new(app_url());
    let outcome = store.execute(touch("gate-4")).await.unwrap();
    let expected = "9007199254740993";
    assert_eq!(outcome.committed_project_revision, expected);
    assert_eq!(outcome.result["revision"], json!(expected));
    let event_revision: String = admin
        .query_one(
            "SELECT payload_json->>'revision' FROM awr_team.events WHERE project_id='project-g'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(event_revision, expected);
    let op_revision: String = admin
        .query_one(
            "SELECT result_json->>'revision' FROM awr_team.operations WHERE project_id='project-g'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(op_revision, expected);
    let replay = store.execute(touch("gate-4")).await.unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.result["revision"], json!(expected));
    assert_eq!(replay.committed_project_revision, expected);
}

// Positive control: a receipt write that aborts leaves no partial state.
#[tokio::test]
async fn aborted_receipt_write_rolls_back_atomically() {
    let (_lock, admin) = setup().await;
    let store = TeamStore::new(app_url());
    store
        .abort_after_partial_write(touch("gate-5"))
        .await
        .unwrap();
    let events: i64 = admin
        .query_one(
            "SELECT count(*) FROM awr_team.events WHERE project_id='project-g'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(events, 0, "aborted write left a partial event");
}

// Positive control: concurrent retries of one request commit exactly once.
#[tokio::test]
async fn concurrent_retry_commits_once() {
    let (_lock, admin) = setup().await;
    let store = TeamStore::new(app_url());
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
