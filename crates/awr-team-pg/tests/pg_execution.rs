#![cfg(feature = "pg-tests")]

use awr_team_pg::{
    Bootstrap, CrashPoint, ExecutionStore, GraphStore, LeaseStore, PgError, ReferenceRunner,
    exactly_once_supported, migrate,
};
use serde_json::json;
use std::sync::{Mutex, MutexGuard};
use tokio_postgres::{Client, NoTls};

static DB: Mutex<()> = Mutex::new(());
const TENANT: &str = "tenant-a";
const PROJECT: &str = "project-a";
const ACTOR: &str = "actor-a";
const RUNNER: &str = "runner-a";
const CLIENT: &str = "client-a";

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
        .expect("postgres 17 must be running for TEAM-P7");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

async fn setup() -> (
    MutexGuard<'static, ()>,
    Client,
    ExecutionStore,
    LeaseStore,
    GraphStore,
) {
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
             INSERT INTO awr_team.actors(tenant_id,id,kind,display_name,status) VALUES
                ('tenant-a','actor-a','agent','A','active'),
                ('tenant-a','runner-a','system','Runner','active');
             INSERT INTO awr_team.projects(tenant_id,id,key,mode,coordinator_epoch,status)
                VALUES ('tenant-a','project-a','alpha','team','epoch-1','active');
             INSERT INTO awr_team.work_scopes(tenant_id,project_id,id,name,status)
                VALUES ('tenant-a','project-a','main','main','active');
             INSERT INTO awr_team.work_items(tenant_id,project_id,id,external_key)
                VALUES ('tenant-a','project-a','work-a','W');",
        )
        .await
        .unwrap();
    (
        guard,
        admin,
        ExecutionStore::new(app_url()),
        LeaseStore::new(app_url()),
        GraphStore::new(app_url()),
    )
}

async fn claimed(leases: &LeaseStore) -> (String, String) {
    let session = leases
        .start_session(TENANT, PROJECT, ACTOR, CLIENT, "conv", "main", "work-a")
        .await
        .unwrap();
    let claim = leases
        .claim(TENANT, PROJECT, &session.id, ACTOR, CLIENT, "claim-1", 60)
        .await
        .unwrap();
    (session.id, claim.id)
}

fn writes_in_scope() -> serde_json::Value {
    json!([{"path": "src/foo/a.rs", "content": "fn a() {}"}])
}

#[tokio::test]
async fn request_id_replay_returns_the_committed_execution() {
    let (_lock, _, store, leases, _) = setup().await;
    let (_session, claim) = claimed(&leases).await;
    let first = store
        .prepare(
            TENANT,
            PROJECT,
            ACTOR,
            CLIENT,
            "prep-1",
            &claim,
            RUNNER,
            "hash-a",
            "in-1",
            "hard_fence",
            &["src/foo".into()],
            &writes_in_scope(),
        )
        .await
        .unwrap();
    let replay = store
        .prepare(
            TENANT,
            PROJECT,
            ACTOR,
            CLIENT,
            "prep-1",
            &claim,
            RUNNER,
            "hash-a",
            "in-1",
            "hard_fence",
            &["src/foo".into()],
            &writes_in_scope(),
        )
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(first.id, replay.id);
    assert_eq!(first.effect_key, replay.effect_key);
}

#[tokio::test]
async fn unknown_outcome_blocks_redispatch_and_keeps_reservations() {
    let (_lock, admin, store, leases, graph) = setup().await;
    let (_session, claim) = claimed(&leases).await;
    graph
        .reserve(TENANT, PROJECT, "work-a", "prefix", "src/foo")
        .await
        .unwrap();
    let prepared = store
        .prepare(
            TENANT,
            PROJECT,
            ACTOR,
            CLIENT,
            "prep-u",
            &claim,
            RUNNER,
            "hash-a",
            "in-1",
            "hard_fence",
            &["src/foo".into()],
            &writes_in_scope(),
        )
        .await
        .unwrap();
    let delivery = store
        .claim_dispatch(TENANT, PROJECT)
        .await
        .unwrap()
        .unwrap();
    let root = std::env::temp_dir().join(format!("awr-p7-{}", prepared.id));
    let runner = ReferenceRunner::new(&root);
    let outcome = runner.handle_delivery(&delivery, CrashPoint::AfterJournalBeforeEffect);
    assert!(outcome.unknown);
    store
        .report(
            TENANT,
            PROJECT,
            RUNNER,
            "trusted_executor",
            &prepared.id,
            "unknown",
            json!({"unknown_reason": "crash-after-journal"}),
            &[],
        )
        .await
        .unwrap();
    admin
        .batch_execute(
            "UPDATE awr_team.claims SET expires_at = clock_timestamp() - interval '1 second'
             WHERE id IN (SELECT id FROM awr_team.claims WHERE work_id='work-a')",
        )
        .await
        .ok();
    let session2 = leases
        .start_session(TENANT, PROJECT, ACTOR, CLIENT, "conv-2", "main", "work-a")
        .await
        .unwrap();
    let err = leases
        .claim(TENANT, PROJECT, &session2.id, ACTOR, CLIENT, "claim-2", 60)
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::RecoveryBlocked));
    let err = graph
        .reserve(TENANT, PROJECT, "work-a", "file", "src/foo/b.rs")
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::ResourceConflict));
    let held: i64 = admin
        .query_one(
            "SELECT count(*) FROM awr_team.resource_reservations WHERE state='unknown'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(held, 1);
}

#[tokio::test]
async fn duplicate_outbox_delivery_reuses_effect_key() {
    let (_lock, admin, store, leases, _) = setup().await;
    let (_session, claim) = claimed(&leases).await;
    let prepared = store
        .prepare(
            TENANT,
            PROJECT,
            ACTOR,
            CLIENT,
            "prep-d",
            &claim,
            RUNNER,
            "hash-a",
            "in-1",
            "hard_fence",
            &["src/foo".into()],
            &writes_in_scope(),
        )
        .await
        .unwrap();
    let first = store
        .claim_dispatch(TENANT, PROJECT)
        .await
        .unwrap()
        .unwrap();
    let root = std::env::temp_dir().join(format!("awr-p7-dup-{}", prepared.id));
    let runner = ReferenceRunner::new(&root);
    let once = runner.handle_delivery(&first, CrashPoint::None);
    store
        .accept(TENANT, PROJECT, &prepared.id, first.fence)
        .await
        .unwrap();
    store
        .start(TENANT, PROJECT, &prepared.id, first.fence)
        .await
        .unwrap();
    store
        .report(
            TENANT,
            PROJECT,
            RUNNER,
            "trusted_executor",
            &prepared.id,
            "succeeded",
            json!({
                "environment_digest": once.environment_digest,
                "output_digest": once.output_digest
            }),
            &once.observed_paths,
        )
        .await
        .unwrap();
    let second = store
        .claim_dispatch(TENANT, PROJECT)
        .await
        .unwrap()
        .unwrap();
    let again = runner.handle_delivery(&second, CrashPoint::None);
    assert_eq!(once.effect_key, again.effect_key);
    assert_eq!(once.state, again.state);
    let count: i64 = admin
        .query_one("SELECT count(*) FROM awr_team.executions", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 1);
    assert!(second.delivery_attempts >= 2);
}

#[tokio::test]
async fn crash_before_journal_is_not_started_and_late_cancel_is_not_cancelled() {
    let (_lock, _, store, leases, _) = setup().await;
    let (_session, claim) = claimed(&leases).await;
    let prepared = store
        .prepare(
            TENANT,
            PROJECT,
            ACTOR,
            CLIENT,
            "prep-c",
            &claim,
            RUNNER,
            "hash-a",
            "in-1",
            "hard_fence",
            &["src/foo".into()],
            &writes_in_scope(),
        )
        .await
        .unwrap();
    let delivery = store
        .claim_dispatch(TENANT, PROJECT)
        .await
        .unwrap()
        .unwrap();
    let root = std::env::temp_dir().join(format!("awr-p7-c-{}", prepared.id));
    let runner = ReferenceRunner::new(&root);
    let before = runner.handle_delivery(&delivery, CrashPoint::BeforeJournal);
    assert!(!before.started);
    assert!(!before.unknown);
    let accepted = runner.handle_delivery(&delivery, CrashPoint::None);
    store
        .accept(TENANT, PROJECT, &prepared.id, delivery.fence)
        .await
        .unwrap();
    store
        .start(TENANT, PROJECT, &prepared.id, delivery.fence)
        .await
        .unwrap();
    let cancel = store
        .cancel(TENANT, PROJECT, ACTOR, CLIENT, "cancel-1", &prepared.id)
        .await
        .unwrap();
    assert!(cancel.cancel_requested);
    assert_ne!(cancel.state, "cancelled");
    let recorded = store
        .report(
            TENANT,
            PROJECT,
            RUNNER,
            "trusted_executor",
            &prepared.id,
            "succeeded",
            json!({
                "environment_digest": accepted.environment_digest,
                "output_digest": accepted.output_digest
            }),
            &accepted.observed_paths,
        )
        .await
        .unwrap();
    assert_eq!(recorded.state, "succeeded");
    assert!(recorded.cancel_requested);
}

#[tokio::test]
async fn observed_paths_outside_scope_are_rejected_and_uncontrolled_has_no_exactly_once() {
    let (_lock, _, store, leases, _) = setup().await;
    let (_session, claim) = claimed(&leases).await;
    let prepared = store
        .prepare(
            TENANT,
            PROJECT,
            ACTOR,
            CLIENT,
            "prep-s",
            &claim,
            RUNNER,
            "hash-a",
            "in-1",
            "uncontrolled",
            &["src/foo".into()],
            &json!([
                {"path": "src/foo/a.rs", "content": "a"},
                {"path": "README.md", "content": "leak"}
            ]),
        )
        .await
        .unwrap();
    assert!(!exactly_once_supported(&prepared.fencing_class));
    let delivery = store
        .claim_dispatch(TENANT, PROJECT)
        .await
        .unwrap()
        .unwrap();
    let root = std::env::temp_dir().join(format!("awr-p7-s-{}", prepared.id));
    let runner = ReferenceRunner::new(&root);
    let outcome = runner.handle_delivery(&delivery, CrashPoint::None);
    assert!(outcome.scope_violation);
    assert!(!outcome.exactly_once_supported);
    store
        .accept(TENANT, PROJECT, &prepared.id, delivery.fence)
        .await
        .unwrap();
    let err = store
        .report(
            TENANT,
            PROJECT,
            RUNNER,
            "trusted_executor",
            &prepared.id,
            "succeeded",
            json!({"output_digest": outcome.output_digest}),
            &outcome.observed_paths,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::ScopeExceeded));
}

#[tokio::test]
async fn stale_fence_is_rejected_and_agent_cannot_mint_trusted_receipts() {
    let (_lock, _, store, leases, _) = setup().await;
    let (_session, claim) = claimed(&leases).await;
    let prepared = store
        .prepare(
            TENANT,
            PROJECT,
            ACTOR,
            CLIENT,
            "prep-f",
            &claim,
            RUNNER,
            "hash-a",
            "in-1",
            "hard_fence",
            &["src/foo".into()],
            &writes_in_scope(),
        )
        .await
        .unwrap();
    let err = store
        .accept(TENANT, PROJECT, &prepared.id, prepared.fence + 9)
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::StaleFence));
    let err = store
        .report(
            TENANT,
            PROJECT,
            ACTOR,
            "trusted_executor",
            &prepared.id,
            "succeeded",
            json!({}),
            &["src/foo/a.rs".into()],
        )
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::Forbidden));
}

#[tokio::test]
async fn receipts_are_append_only_for_the_app_role() {
    let (_lock, _, store, leases, _) = setup().await;
    let (_session, claim) = claimed(&leases).await;
    let prepared = store
        .prepare(
            TENANT,
            PROJECT,
            ACTOR,
            CLIENT,
            "prep-r",
            &claim,
            RUNNER,
            "hash-a",
            "in-1",
            "hard_fence",
            &["src/foo".into()],
            &writes_in_scope(),
        )
        .await
        .unwrap();
    store
        .report(
            TENANT,
            PROJECT,
            RUNNER,
            "trusted_executor",
            &prepared.id,
            "unknown",
            json!({"unknown_reason": "lost-receipt"}),
            &[],
        )
        .await
        .unwrap();
    let app = connect(&app_url()).await;
    app.batch_execute("SELECT set_config('awr.tenant_id','tenant-a',false); SELECT set_config('awr.project_id','project-a',false);")
        .await
        .unwrap();
    let deleted = app
        .execute("DELETE FROM awr_team.execution_receipts", &[])
        .await;
    assert!(
        deleted.is_err(),
        "app role must not delete receipts: {deleted:?}"
    );
}
