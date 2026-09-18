#![cfg(feature = "pg-tests")]
//! Regression tests for the retrospective CR of PR #36 (TEAM-P2).
//!
//! Isolation contract (CR #52 P2-2): this suite never uses the runtime
//! `AWR_TEAM_DATABASE_URL` and never drops the shared schema. It runs in a
//! dedicated database `awr_team_gate_test` on a loopback server, created by
//! the suite itself. The URL guard refuses non-loopback targets BEFORE any
//! DROP runs.

use awr_team_pg::{Bootstrap, CommandRequest, PgError, TeamStore, check_schema, migrate};
use serde_json::json;
use std::sync::{Mutex, MutexGuard};
use tokio_postgres::{Client, NoTls};

static DB: Mutex<()> = Mutex::new(());

const GATE_DB: &str = "awr_team_gate_test";
const TENANT: &str = "tenant-g";
const PROJECT: &str = "project-g";
const ACTOR: &str = "agent-g";

fn maintenance_url() -> String {
    let url = std::env::var("AWR_TEAM_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:awr-test@127.0.0.1:55432/postgres".into());
    assert!(
        url.contains("127.0.0.1") || url.contains("localhost"),
        "pg_schema_gate refuses non-loopback targets; set AWR_TEAM_TEST_DATABASE_URL to a disposable loopback server"
    );
    url
}
fn url_for(db: &str, user: &str, password: &str) -> String {
    let base = maintenance_url();
    let scheme_split = base.split("://").collect::<Vec<_>>();
    let after_creds = scheme_split[1].split('@').collect::<Vec<_>>();
    let host = after_creds[after_creds.len() - 1]
        .split('/')
        .next()
        .unwrap_or("127.0.0.1:55432");
    format!("postgres://{user}:{password}@{host}/{db}")
}
async fn connect(url: &str) -> Client {
    let (client, connection) = tokio_postgres::connect(url, NoTls)
        .await
        .expect("loopback postgres 17 must be running for schema gate tests");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}
fn admin_url() -> String {
    url_for(GATE_DB, "postgres", "awr-test")
}
fn app_url() -> String {
    url_for(GATE_DB, "awr_app", "app-test")
}

async fn ensure_dedicated_db() {
    let maintenance = connect(&url_for("postgres", "postgres", "awr-test")).await;
    let exists: bool = maintenance
        .query_one(
            "SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname=$1)",
            &[&GATE_DB],
        )
        .await
        .unwrap()
        .get(0);
    if !exists {
        maintenance
            .batch_execute(&format!("CREATE DATABASE \"{GATE_DB}\""))
            .await
            .unwrap();
    }
}

async fn setup() -> (MutexGuard<'static, ()>, Client) {
    let guard = DB.lock().expect("db fixture lock");
    ensure_dedicated_db().await;
    let admin = connect(&admin_url()).await;
    // Safe: GATE_DB is created by this suite and holds nothing else.
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
             INSERT INTO awr_team.actors(tenant_id,id,kind,display_name,status) VALUES ('tenant-g','agent-g','agent','G','active');
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
        client_id: "client-g".into(),
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

// CR #52 P2-1: a database bootstrapped by the OLD version (schema_state
// fully revoked) keeps working after the non-destructive grant upgrade.
#[tokio::test]
async fn upgrade_regrants_existing_database_without_data_loss() {
    let (_lock, admin) = setup().await;
    // Simulate the pre-fix grant set: the app role lost every privilege on
    // schema_state. Existing business data must survive the upgrade.
    admin
        .batch_execute("REVOKE ALL ON awr_team.schema_state FROM awr_app")
        .await
        .unwrap();
    let store = TeamStore::new(app_url());
    let err = store.execute(touch("gate-pre-upgrade")).await.unwrap_err();
    assert!(
        matches!(err, PgError::Db(_)),
        "old grants unexpectedly still read schema_state: {err}"
    );
    // The owner-side, repeatable upgrade step (`awr-server migrate
    // --app-role awr_app` calls the same entry). No schema rebuild.
    Bootstrap::grant_app(&admin, "awr_app").await.unwrap();
    let app = connect(&app_url()).await;
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
// return, in the persisted event/operation (type checked, not only text),
// and on idempotent replay.
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
    let (_lock, admin) = setup().await;
    admin
        .batch_execute(
            "CREATE OR REPLACE FUNCTION awr_team.fail_receipt_insert() RETURNS trigger
             LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected receipt failure'; END; $$;
             CREATE TRIGGER fail_receipt BEFORE INSERT ON awr_team.operations
             FOR EACH ROW EXECUTE FUNCTION awr_team.fail_receipt_insert();",
        )
        .await
        .unwrap();
    let store = TeamStore::new(app_url());
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
