#![cfg(feature = "pg-tests")]

use awr_team_pg::{
    Bootstrap, DependencyEdge, GraphStore, PgError, migrate, paths_conflict, require_main_scope,
    validate_required_graph,
};
use serde_json::json;
use std::sync::{Mutex, MutexGuard};
use tokio_postgres::{Client, NoTls};

static DB: Mutex<()> = Mutex::new(());
const TENANT: &str = "tenant-a";
const PROJECT: &str = "project-a";

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
        .expect("postgres 17 must be running for TEAM-P6");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

async fn setup() -> (MutexGuard<'static, ()>, Client, GraphStore) {
    let guard = DB.lock().expect("lock");
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
                ('tenant-a','actor-a','agent','A','active');
             INSERT INTO awr_team.projects(tenant_id,id,key,mode,coordinator_epoch,status)
                VALUES ('tenant-a','project-a','alpha','team','epoch-1','active');
             INSERT INTO awr_team.work_scopes(tenant_id,project_id,id,name,status)
                VALUES ('tenant-a','project-a','main','main','active');
             INSERT INTO awr_team.work_items(tenant_id,project_id,id,external_key) VALUES
                ('tenant-a','project-a','work-a','W'),
                ('tenant-a','project-a','work-b','X');
             INSERT INTO awr_team.source_snapshots(
                tenant_id, project_id, id, manifest_digest, source_ref_json, parser_version, created_by)
                VALUES ('tenant-a','project-a','snap-1','digest','{}','p1','actor-a');
             UPDATE awr_team.projects SET active_snapshot_id='snap-1'
                WHERE tenant_id='tenant-a' AND id='project-a';
             INSERT INTO awr_team.work_contracts(
                tenant_id, project_id, snapshot_id, scope_id, work_id, contract_hash,
                definition_state, title, contract_json)
                VALUES
                ('tenant-a','project-a','snap-1','main','work-a','hash-a','enabled','W','{\"acceptance\":[\"parent\"]}'),
                ('tenant-a','project-a','snap-1','main','work-b','hash-b','enabled','X','{\"acceptance\":[\"child\"]}');",
        )
        .await
        .unwrap();
    (guard, admin, GraphStore::new(app_url()))
}

#[test]
fn prefix_rules_are_segment_based() {
    assert!(paths_conflict("prefix", "src/foo", "file", "src/foo/a.rs"));
    assert!(!paths_conflict("prefix", "src/a", "file", "src/abc"));
    assert!(
        validate_required_graph(
            &["a".into()],
            &[DependencyEdge {
                from: "a".into(),
                to: "missing".into(),
                relation: "requires".into(),
                required: true
            }]
        )
        .is_err()
    );
    assert!(require_main_scope("legacy").is_err());
}

#[tokio::test]
async fn invalid_graph_is_rejected_without_partial_edges() {
    let (_lock, admin, store) = setup().await;
    let err = store
        .replace_edges(
            TENANT,
            PROJECT,
            "snap-1",
            "main",
            &["work-a".into(), "work-b".into()],
            &[
                DependencyEdge {
                    from: "work-a".into(),
                    to: "work-b".into(),
                    relation: "requires".into(),
                    required: true,
                },
                DependencyEdge {
                    from: "work-b".into(),
                    to: "work-a".into(),
                    relation: "requires".into(),
                    required: true,
                },
            ],
        )
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::DependencyCycle));
    let count: i64 = admin
        .query_one("SELECT count(*) FROM awr_team.dependency_edges", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 0);
}

#[tokio::test]
async fn prefix_conflict_is_not_string_equality() {
    let (_lock, _, store) = setup().await;
    store
        .reserve(TENANT, PROJECT, "work-a", "prefix", "src/foo")
        .await
        .unwrap();
    let err = store
        .reserve(TENANT, PROJECT, "work-b", "file", "src/foo/bar.rs")
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::ResourceConflict));
    store
        .reserve(TENANT, PROJECT, "work-b", "file", "src/foobar.rs")
        .await
        .unwrap();
}

#[tokio::test]
async fn split_children_do_not_complete_parent_and_bindings_invalidate() {
    let (_lock, _, store) = setup().await;
    let split = store
        .propose_split(
            TENANT,
            PROJECT,
            "work-a",
            &["work-a-1".into(), "work-a-2".into()],
            &json!({"acceptance": "inherited"}),
        )
        .await
        .unwrap();
    assert_eq!(split.child_work_ids.len(), 2);
    let err = store
        .complete_parent_from_children(TENANT, PROJECT, "work-a")
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::ParentEvidenceRequired));
    store
        .bind_dependency(TENANT, PROJECT, "work-b", "work-a", "bind-1")
        .await
        .unwrap();
    store
        .invalidate_downstream(TENANT, PROJECT, "work-a")
        .await
        .unwrap();
    let valid = store
        .current_binding_valid(TENANT, PROJECT, "work-b", "work-a")
        .await
        .unwrap();
    assert!(!valid);
}

#[tokio::test]
async fn claimed_work_blocks_contract_change_and_unknown_scope_is_explicit() {
    let (_lock, admin, store) = setup().await;
    admin
        .batch_execute(
            "INSERT INTO awr_team.sessions(
                tenant_id, project_id, id, scope_id, work_id, actor_id, client_id, conversation_id, state)
             VALUES ('tenant-a','project-a','s1','main','work-a','actor-a','c1','conv','active');
             INSERT INTO awr_team.claims(
                tenant_id, project_id, id, scope_id, work_id, session_id, actor_id, fence, expires_at, state)
             VALUES ('tenant-a','project-a','cl1','main','work-a','s1','actor-a',1, now() + interval '1 hour','active');",
        )
        .await
        .unwrap();
    let err = store
        .activation_blocked_by_claims(TENANT, PROJECT, "work-a", "hash-new")
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::ClaimBlocksActivation));
    store
        .activation_blocked_by_claims(TENANT, PROJECT, "work-a", "hash-a")
        .await
        .unwrap();
    let err = store
        .replace_edges(
            TENANT,
            PROJECT,
            "snap-1",
            "feature",
            &["work-a".into()],
            &[],
        )
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::ScopeUnsupported));
    let err = store
        .graph_within_budget(
            &vec![
                DependencyEdge {
                    from: "a".into(),
                    to: "b".into(),
                    relation: "requires".into(),
                    required: true,
                };
                3
            ],
            2,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::GraphBudgetExceeded));
}
