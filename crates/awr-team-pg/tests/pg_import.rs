#![cfg(feature = "pg-tests")]

use awr_team_pg::{Bootstrap, ImportStore, PgError, migrate};
use serde_json::json;
use std::sync::{Mutex, MutexGuard};
use tokio_postgres::{Client, NoTls};

static DB: Mutex<()> = Mutex::new(());
const TENANT: &str = "tenant-a";
const PROJECT: &str = "project-a";
const ACTOR: &str = "actor-a";

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
        .expect("postgres 17 must be running for TEAM-P9");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

async fn setup() -> (MutexGuard<'static, ()>, Client, ImportStore) {
    let guard = DB.lock().unwrap_or_else(|e| e.into_inner());
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
            "INSERT INTO awr_team.tenants(id,name,status) VALUES ('tenant-a','A','active');
             INSERT INTO awr_team.actors(tenant_id,id,kind,display_name,status)
                VALUES ('tenant-a','actor-a','agent','A','active');
             INSERT INTO awr_team.credentials(tenant_id,id,actor_id,client_id,secret_hash)
                VALUES ('tenant-a','cred-1','actor-a','client-a','hash');
             INSERT INTO awr_team.projects(tenant_id,id,key,mode,coordinator_epoch,status)
                VALUES ('tenant-a','project-a','alpha','team','epoch-1','active');
             INSERT INTO awr_team.work_scopes(tenant_id,project_id,id,name,status)
                VALUES ('tenant-a','project-a','main','main','active');",
        )
        .await
        .unwrap();
    (guard, admin, ImportStore::new(app_url()))
}

fn manifest() -> serde_json::Value {
    json!({
        "works": [{"id":"work-a","external_key":"W"}],
        "evidence": [{
            "id":"ev-1",
            "work_id":"work-a",
            "claimed_trust":"trusted_executor",
            "trust_basis":"caller_asserted",
            "digest":"abc",
            "bytes_present": true
        }],
        "local_claims": [{"work_id":"work-a","state":"active"}],
        "scopes": ["main"]
    })
}

#[tokio::test]
async fn duplicate_import_is_idempotent_and_does_not_copy_local_claims() {
    let (_lock, admin, store) = setup().await;
    store.freeze(TENANT, PROJECT).await.unwrap();
    let first = store
        .load(TENANT, PROJECT, ACTOR, "imp-1", &manifest())
        .await
        .unwrap();
    let again = store
        .load(TENANT, PROJECT, ACTOR, "imp-1", &manifest())
        .await
        .unwrap();
    assert!(again.replayed);
    assert_eq!(first.id, again.id);
    let works: i64 = admin
        .query_one("SELECT count(*) FROM awr_team.work_items", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(works, 1);
    store
        .activate(TENANT, PROJECT, &first.id, false)
        .await
        .unwrap();
    assert!(
        store
            .local_claim_is_not_team_lease(TENANT, PROJECT, "work-a")
            .await
            .unwrap()
    );
    let trust: String = admin
        .query_one(
            "SELECT trust_basis FROM awr_team.evidence WHERE id='ev-1'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(trust, "caller_asserted");
}

#[test]
fn source_divergence_is_not_resolved_by_mtime() {
    let store = ImportStore::new("postgres://unused");
    let err = store
        .inspect_sources(&[("a", "fp-1"), ("b", "fp-2")])
        .unwrap_err();
    assert!(matches!(err, PgError::SourceDivergence));
    store
        .inspect_sources(&[("a", "fp-1"), ("b", "fp-1")])
        .unwrap();
}

#[tokio::test]
async fn restore_isolates_old_epoch_and_does_not_replay_outbox() {
    let (_lock, admin, store) = setup().await;
    admin
        .batch_execute(
            "INSERT INTO awr_team.work_items(tenant_id,project_id,id,external_key)
             VALUES ('tenant-a','project-a','work-a','W');
             INSERT INTO awr_team.work_runtime(tenant_id,project_id,scope_id,work_id,state,work_version,last_fence)
             VALUES ('tenant-a','project-a','main','work-a','pending',1,0);
             INSERT INTO awr_team.outbox(tenant_id,project_id,id,state,payload_json)
             VALUES ('tenant-a','project-a','ob-1','pending','{}');",
        )
        .await
        .unwrap();
    let backup = store
        .backup(TENANT, PROJECT, &["art-1".into()], &["src-1".into()])
        .await
        .unwrap();
    let err = store
        .restore(TENANT, PROJECT, &backup.id, true, true)
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::OutboxReplayForbidden));
    let err = store
        .restore(TENANT, PROJECT, &backup.id, false, false)
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::RestoreIncomplete));
    let run = store
        .restore(TENANT, PROJECT, &backup.id, true, false)
        .await
        .unwrap();
    assert_ne!(run.new_epoch, backup.coordinator_epoch);
    store
        .require_epoch(TENANT, PROJECT, &backup.coordinator_epoch)
        .await
        .unwrap_err();
    store
        .require_epoch(TENANT, PROJECT, &run.new_epoch)
        .await
        .unwrap();
    let pending: i64 = admin
        .query_one(
            "SELECT count(*) FROM awr_team.outbox WHERE state='pending'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(pending, 0);
    let revoked: i64 = admin
        .query_one(
            "SELECT count(*) FROM awr_team.credentials WHERE revoked_at IS NOT NULL",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(revoked, 1);
}

#[tokio::test]
async fn new_team_writes_block_sqlite_rollback_and_unknown_blocks_activate() {
    let (_lock, admin, store) = setup().await;
    let job = store
        .load(TENANT, PROJECT, ACTOR, "imp-2", &manifest())
        .await
        .unwrap();
    let err = store
        .activate(TENANT, PROJECT, &job.id, true)
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::RecoveryBlocked));
    store
        .activate(TENANT, PROJECT, &job.id, false)
        .await
        .unwrap();
    admin
        .batch_execute("UPDATE awr_team.projects SET project_revision=3 WHERE id='project-a'")
        .await
        .unwrap();
    let err = store
        .refuse_sqlite_rollback(TENANT, PROJECT)
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::RollbackForbidden));
}

#[tokio::test]
async fn dry_run_reports_missing_evidence_and_rejects_non_main_scope() {
    let store = ImportStore::new("postgres://unused");
    let report = store
        .dry_run(&json!({
            "scopes":["main"],
            "evidence":[{"bytes_present": false, "id":"ev-missing"}]
        }))
        .unwrap();
    assert_eq!(report["can_activate"], json!(false));
    let err = store.dry_run(&json!({"scopes":["legacy"]})).unwrap_err();
    assert!(matches!(err, PgError::ScopeUnsupported));
}
