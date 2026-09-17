#![cfg(feature = "pg-tests")]

use awr_team_pg::{Bootstrap, PgError, ReviewStore, migrate};
use serde_json::json;
use std::sync::{Mutex, MutexGuard};
use tokio_postgres::{Client, NoTls};

static DB: Mutex<()> = Mutex::new(());
const TENANT: &str = "tenant-a";
const PROJECT: &str = "project-a";
const AUTHOR: &str = "actor-a";
const REVIEWER: &str = "reviewer-a";
const RUNNER: &str = "runner-a";

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
        .expect("postgres 17 must be running for TEAM-P8");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

async fn setup(policy: &str) -> (MutexGuard<'static, ()>, Client, ReviewStore) {
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
    let seed = r#"
INSERT INTO awr_team.tenants(id,name,status) VALUES ('tenant-a','A','active');
INSERT INTO awr_team.actors(tenant_id,id,kind,display_name,status) VALUES
   ('tenant-a','actor-a','agent','A','active'),
   ('tenant-a','reviewer-a','human','R','active'),
   ('tenant-a','runner-a','system','Runner','active');
INSERT INTO awr_team.projects(tenant_id,id,key,mode,coordinator_epoch,status)
   VALUES ('tenant-a','project-a','alpha','team','epoch-1','active');
INSERT INTO awr_team.work_scopes(tenant_id,project_id,id,name,status)
   VALUES ('tenant-a','project-a','main','main','active');
INSERT INTO awr_team.work_items(tenant_id,project_id,id,external_key)
   VALUES ('tenant-a','project-a','work-a','W'), ('tenant-a','project-a','work-b','X');
INSERT INTO awr_team.source_snapshots(
   tenant_id, project_id, id, manifest_digest, source_ref_json, parser_version, created_by)
   VALUES ('tenant-a','project-a','snap-1','digest','{}','p1','actor-a');
UPDATE awr_team.projects SET active_snapshot_id='snap-1'
   WHERE tenant_id='tenant-a' AND id='project-a';
INSERT INTO awr_team.work_contracts(
   tenant_id, project_id, snapshot_id, scope_id, work_id, contract_hash,
   definition_state, title, contract_json)
   VALUES
   ('tenant-a','project-a','snap-1','main','work-a','hash-a','enabled','W',
    '{"completion_policy":"POLICY","acceptance":["done"]}'),
   ('tenant-a','project-a','snap-1','main','work-b','hash-b','enabled','X',
    '{"completion_policy":"POLICY"}');
"#
    .replace("POLICY", policy);
    admin.batch_execute(&seed).await.unwrap();
    (guard, admin, ReviewStore::new(app_url()))
}

#[tokio::test]
async fn source_declared_completed_does_not_write_a_team_receipt() {
    let (_lock, _, store) = setup("trusted_execution_and_review").await;
    assert!(
        store
            .source_declared_is_not_complete(TENANT, PROJECT, "work-a", "main")
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn agent_cannot_upgrade_self_report_or_self_review() {
    let (_lock, _, store) = setup("trusted_execution_and_review").await;
    let evidence = store
        .record_evidence(
            TENANT,
            PROJECT,
            AUTHOR,
            "work-a",
            "hash-a",
            Some("trusted_executor"),
            &json!({"passed": true, "output_digest": "deadbeef"}),
            Some(b"report-bytes"),
            Some("in-1"),
            false,
        )
        .await
        .unwrap();
    assert_eq!(evidence.trust_basis, "caller_asserted");
    let err = store
        .complete(
            TENANT,
            PROJECT,
            AUTHOR,
            "work-a",
            "main",
            &evidence.id,
            None,
            true,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::EvidenceInvalid));
    let round = store
        .open_review(TENANT, PROJECT, AUTHOR, "work-a", &evidence.id)
        .await
        .unwrap();
    let err = store
        .decide_review(TENANT, PROJECT, AUTHOR, &round.id, "approve", "self")
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::AuthorCannotReview));
}

#[tokio::test]
async fn trusted_receipt_with_independent_review_completes_and_old_contract_does_not() {
    let (_lock, admin, store) = setup("trusted_execution_and_review").await;
    let old = store
        .record_evidence(
            TENANT,
            PROJECT,
            RUNNER,
            "work-a",
            "hash-old",
            None,
            &json!({"log": "old"}),
            Some(b"old-bytes"),
            Some("in-old"),
            false,
        )
        .await
        .unwrap();
    let err = store
        .complete(
            TENANT, PROJECT, REVIEWER, "work-a", "main", &old.id, None, true,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::EvidenceInvalid));
    let evidence = store
        .record_evidence(
            TENANT,
            PROJECT,
            RUNNER,
            "work-a",
            "hash-a",
            None,
            &json!({"log": "ok"}),
            Some(b"good-bytes"),
            Some("in-1"),
            false,
        )
        .await
        .unwrap();
    let round = store
        .open_review(TENANT, PROJECT, AUTHOR, "work-a", &evidence.id)
        .await
        .unwrap();
    store
        .decide_review(TENANT, PROJECT, REVIEWER, &round.id, "approve", "ok")
        .await
        .unwrap();
    store
        .complete(
            TENANT,
            PROJECT,
            REVIEWER,
            "work-a",
            "main",
            &evidence.id,
            None,
            true,
        )
        .await
        .unwrap();
    let count: i64 = admin
        .query_one("SELECT count(*) FROM awr_team.completion_receipts", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 1);
}

#[tokio::test]
async fn changed_bytes_and_material_invalidate_review_and_forbid_policy_downgrade() {
    let (_lock, _, store) = setup("trusted_execution_and_review").await;
    let first = store
        .record_evidence(
            TENANT,
            PROJECT,
            RUNNER,
            "work-a",
            "hash-a",
            None,
            &json!({"log": "a"}),
            Some(b"bytes-a"),
            Some("in-1"),
            false,
        )
        .await
        .unwrap();
    let round = store
        .open_review(TENANT, PROJECT, AUTHOR, "work-a", &first.id)
        .await
        .unwrap();
    store
        .decide_review(TENANT, PROJECT, REVIEWER, &round.id, "approve", "ok")
        .await
        .unwrap();
    let second = store
        .record_evidence(
            TENANT,
            PROJECT,
            RUNNER,
            "work-a",
            "hash-a",
            None,
            &json!({"log": "b"}),
            Some(b"bytes-b"),
            Some("in-1"),
            false,
        )
        .await
        .unwrap();
    store
        .open_review(TENANT, PROJECT, AUTHOR, "work-a", &second.id)
        .await
        .unwrap();
    let err = store
        .complete(
            TENANT, PROJECT, REVIEWER, "work-a", "main", &first.id, None, true,
        )
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        PgError::CompletionRejected | PgError::ReviewRequired | PgError::EvidenceInvalid
    ));
    let err = store
        .complete(
            TENANT,
            PROJECT,
            AUTHOR,
            "work-a",
            "main",
            &second.id,
            Some("ordinary_confirm"),
            true,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::PolicyDowngrade));
}

#[tokio::test]
async fn ordinary_confirm_works_for_humans_and_dirty_tree_needs_bytes() {
    let (_lock, _, store) = setup("ordinary_confirm").await;
    let err = store
        .record_evidence(
            TENANT,
            PROJECT,
            AUTHOR,
            "work-a",
            "hash-a",
            None,
            &json!({"passed": true}),
            None,
            None,
            true,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::EvidenceInvalid));
    let evidence = store
        .record_evidence(
            TENANT,
            PROJECT,
            REVIEWER,
            "work-a",
            "hash-a",
            None,
            &json!({"confirmed": true}),
            Some(b"checklist"),
            Some("tree-digest"),
            true,
        )
        .await
        .unwrap();
    store
        .complete(
            TENANT,
            PROJECT,
            REVIEWER,
            "work-a",
            "main",
            &evidence.id,
            Some("ordinary_confirm"),
            true,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn completed_state_cannot_be_forged_without_a_receipt() {
    let (_lock, _, store) = setup("trusted_execution_and_review").await;
    let _ = store;
    let app = connect(&app_url()).await;
    app.batch_execute("SELECT set_config('awr.tenant_id','tenant-a',false); SELECT set_config('awr.project_id','project-a',false);")
        .await
        .unwrap();
    let err = app
        .execute(
            "INSERT INTO awr_team.work_runtime(
                tenant_id, project_id, scope_id, work_id, state, work_version, last_fence)
             VALUES ('tenant-a','project-a','main','work-a','completed',1,0)",
            &[],
        )
        .await;
    assert!(err.is_err());
}

#[tokio::test]
async fn incomplete_context_cannot_complete() {
    let (_lock, _, store) = setup("ordinary_confirm").await;
    let evidence = store
        .record_evidence(
            TENANT,
            PROJECT,
            REVIEWER,
            "work-a",
            "hash-a",
            None,
            &json!({"confirmed": true}),
            Some(b"ok"),
            Some("in"),
            false,
        )
        .await
        .unwrap();
    let err = store
        .complete(
            TENANT,
            PROJECT,
            REVIEWER,
            "work-a",
            "main",
            &evidence.id,
            None,
            false,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::ContextIncomplete));
}
