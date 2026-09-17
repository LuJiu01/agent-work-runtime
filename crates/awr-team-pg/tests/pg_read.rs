#![cfg(feature = "pg-tests")]

use awr_team::{SourceActivationPlan, WorkContract, WorkId};
use awr_team_pg::{
    CommandRequest, EventCursor, IngestRequest, PgError, ReadStore, SourceFile, SourceStore,
    TeamStore, dispatch_query, migrate,
};
use serde_json::json;
use std::sync::{Mutex, MutexGuard};
use tokio_postgres::{Client, IsolationLevel, NoTls};

static DB: Mutex<()> = Mutex::new(());

const TENANT: &str = "tenant-a";
const PROJECT: &str = "project-a";
const AUTHOR: &str = "actor-a";
const REVIEWER: &str = "actor-b";
const PARSER: &str = "awr-team-source/1";

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
        .expect("postgres 17 must be running for TEAM-P4");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

async fn setup() -> (MutexGuard<'static, ()>, Client, String) {
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
    awr_team_pg::Bootstrap::grant_app(&admin, "awr_app")
        .await
        .unwrap();
    admin
        .batch_execute(
            "INSERT INTO awr_team.tenants(id,name,status) VALUES ('tenant-a','A','active');
             INSERT INTO awr_team.actors(tenant_id,id,kind,display_name,status) VALUES
                ('tenant-a','actor-a','agent','A','active'),
                ('tenant-a','actor-b','human','B','active');
             INSERT INTO awr_team.projects(tenant_id,id,key,mode,coordinator_epoch,status)
                VALUES ('tenant-a','project-a','alpha','team','epoch-1','active');
             INSERT INTO awr_team.work_scopes(tenant_id,project_id,id,name,status)
                VALUES ('tenant-a','project-a','main','main','active');
             INSERT INTO awr_team.work_items(tenant_id,project_id,id,external_key)
                VALUES ('tenant-a','project-a','work-a','W');",
        )
        .await
        .unwrap();
    (guard, admin, app_url())
}

fn contract_bytes(rule: &str) -> Vec<u8> {
    let contract = WorkContract {
        codec: WorkContract::CODEC.into(),
        work_id: WorkId::new("work-a").unwrap(),
        external_key: "W".into(),
        goals: vec!["g".into()],
        hard_rules: vec![rule.into()],
        scope_paths: vec!["src".into()],
        acceptance: vec!["done".into()],
        required_dependencies: vec![],
        completion_policy: "evidence".into(),
        verification_requirements: vec!["report".into()],
    };
    serde_json::to_vec(&contract).unwrap()
}

async fn activate_rule(url: &str, rule: &str) -> String {
    let sources = SourceStore::new(url);
    let candidate = sources
        .ingest(IngestRequest {
            tenant_id: TENANT.into(),
            project_id: PROJECT.into(),
            actor_id: AUTHOR.into(),
            parser_version: PARSER.into(),
            files: vec![SourceFile {
                path: "contract.json".into(),
                bytes: contract_bytes(rule),
            }],
        })
        .await
        .unwrap();
    sources
        .approve(
            TENANT,
            PROJECT,
            &candidate.proposal_id,
            REVIEWER,
            &candidate.manifest_digest,
        )
        .await
        .unwrap();
    let epoch = sources
        .current(TENANT, PROJECT, "work-a")
        .await
        .map(|c| c.authority_epoch)
        .unwrap_or_else(|_| "0".into());
    let current = sources
        .activate(
            TENANT,
            PROJECT,
            AUTHOR,
            &candidate.proposal_id,
            &SourceActivationPlan {
                candidate_digest: candidate.manifest_digest.clone(),
                parser_version: candidate.parser_version.clone(),
                expected_authority_epoch: epoch,
                approved_candidate_digest: candidate.manifest_digest,
            },
        )
        .await
        .unwrap();
    current.authority_snapshot_id_or_id()
}

trait SnapshotId {
    fn authority_snapshot_id_or_id(&self) -> String;
}

impl SnapshotId for awr_team_pg::CurrentSource {
    fn authority_snapshot_id_or_id(&self) -> String {
        self.snapshot_id.clone()
    }
}

fn touch(request_id: &str) -> CommandRequest {
    CommandRequest {
        tenant_id: TENANT.into(),
        project_id: PROJECT.into(),
        actor_id: AUTHOR.into(),
        client_id: "client-a".into(),
        request_id: request_id.into(),
        op: "work.touch".into(),
        args: json!({"work_id": "work-a", "scope_id": "main"}),
    }
}

#[tokio::test]
async fn unsupported_queries_are_explicit() {
    let err = dispatch_query("work.complete").unwrap_err();
    assert!(matches!(err, PgError::Unsupported(_)));
}

#[tokio::test]
async fn event_pages_include_event_index_and_survive_reconnect() {
    let (_lock, _, url) = setup().await;
    let store = TeamStore::new(url.clone());
    let revision = store
        .emit_revision_events(
            TENANT,
            PROJECT,
            AUTHOR,
            vec![
                ("note.a".into(), json!({"n": 1})),
                ("note.b".into(), json!({"n": 2})),
                ("note.c".into(), json!({"n": 3})),
            ],
        )
        .await
        .unwrap();
    let reads = ReadStore::new(url);
    let mut after = None;
    let mut seen = Vec::new();
    loop {
        let page = reads
            .list_events(TENANT, PROJECT, after.as_deref(), 1)
            .await
            .unwrap();
        assert!(page.events.len() <= 1);
        for event in &page.events {
            let cursor = EventCursor::decode(&event.cursor).unwrap();
            assert_eq!(cursor.project_revision, revision);
            assert_eq!(cursor.event_index, event.event_index);
            seen.push((
                event.project_revision,
                event.event_index,
                event.event_type.clone(),
            ));
        }
        if page.exhausted {
            break;
        }
        after = Some(page.next_cursor);
    }
    assert_eq!(
        seen,
        vec![
            (revision, 0, "note.a".into()),
            (revision, 1, "note.b".into()),
            (revision, 2, "note.c".into())
        ]
    );
}

#[tokio::test]
async fn concurrent_commits_are_ordered_by_project_revision_not_a_sequence() {
    let (_lock, _, url) = setup().await;
    let store = TeamStore::new(url.clone());
    let a = store.execute(touch("c1"));
    let b = store.execute(touch("c2"));
    let (ra, rb) = tokio::join!(a, b);
    ra.unwrap();
    rb.unwrap();
    let page = ReadStore::new(url)
        .list_events(TENANT, PROJECT, None, 10)
        .await
        .unwrap();
    let revs: Vec<i64> = page.events.iter().map(|e| e.project_revision).collect();
    let mut sorted = revs.clone();
    sorted.sort();
    assert_eq!(revs, sorted);
    assert_eq!(sorted, vec![1, 2]);
    assert!(page.events.iter().all(|e| e.event_index == 0));
}

#[tokio::test]
async fn prepare_keeps_required_rules_when_budget_is_too_small() {
    let (_lock, _, url) = setup().await;
    let snapshot = activate_rule(&url, "never-omit-this-rule").await;
    let prepared = ReadStore::new(url)
        .prepare(TENANT, PROJECT, "work-a", Some(3))
        .await
        .unwrap();
    assert_eq!(prepared.authority_snapshot_id, snapshot);
    assert!(prepared.hard_rules.contains(&"never-omit-this-rule".into()));
    assert_eq!(prepared.completeness, "incomplete");
    assert!(
        prepared
            .completeness_reasons
            .contains(&"required_content_exceeds_budget".into())
    );
}

#[tokio::test]
async fn repeatable_read_prepare_does_not_mix_source_versions() {
    let (_lock, _, url) = setup().await;
    let first = activate_rule(&url, "old-rule").await;
    let mut reader = connect(&url).await;
    let tx = reader
        .build_transaction()
        .isolation_level(IsolationLevel::RepeatableRead)
        .start()
        .await
        .unwrap();
    tx.execute("SELECT set_config('awr.tenant_id', $1, true)", &[&TENANT])
        .await
        .unwrap();
    tx.execute("SELECT set_config('awr.project_id', $1, true)", &[&PROJECT])
        .await
        .unwrap();
    let before: String = tx
        .query_one(
            "SELECT p.active_snapshot_id FROM awr_team.projects p WHERE p.id=$1",
            &[&PROJECT],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(before, first);
    let second = activate_rule(&url, "new-rule").await;
    assert_ne!(first, second);
    let during: String = tx
        .query_one(
            "SELECT p.active_snapshot_id FROM awr_team.projects p WHERE p.id=$1",
            &[&PROJECT],
        )
        .await
        .unwrap()
        .get(0);
    let rule: serde_json::Value = tx
        .query_one(
            "SELECT c.contract_json FROM awr_team.work_contracts c
             WHERE c.snapshot_id=$1 AND c.work_id='work-a'",
            &[&during],
        )
        .await
        .unwrap()
        .get(0);
    tx.commit().await.unwrap();
    assert_eq!(during, first);
    assert_eq!(rule["hard_rules"][0], "old-rule");
    let after = ReadStore::new(url)
        .prepare(TENANT, PROJECT, "work-a", None)
        .await
        .unwrap();
    assert_eq!(after.authority_snapshot_id, second);
    assert_eq!(after.hard_rules, vec!["new-rule".to_string()]);
}

#[tokio::test]
async fn stale_epoch_cursor_and_missing_session_are_explicit() {
    let (_lock, _, url) = setup().await;
    let reads = ReadStore::new(url);
    let err = reads
        .inspect_session(TENANT, PROJECT, "missing")
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::SessionNotFound));
    let err = reads
        .list_events(
            TENANT,
            PROJECT,
            Some("awr-team-cursor-v1:other-epoch:0:-1"),
            10,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::EpochChanged));
}
