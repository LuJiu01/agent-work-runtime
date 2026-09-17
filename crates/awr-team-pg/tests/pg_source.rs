#![cfg(feature = "pg-tests")]

use awr_team::{SourceActivationPlan, WorkContract, WorkId};
use awr_team_pg::{
    Bootstrap, IngestRequest, PgError, SourceFile, SourceStore, migrate, validate_source_path,
};
use std::sync::{Mutex, MutexGuard};
use tokio_postgres::{Client, NoTls};

static DB: Mutex<()> = Mutex::new(());

const TENANT: &str = "tenant-a";
const PROJECT: &str = "project-a";
const AUTHOR: &str = "actor-a";
const REVIEWER: &str = "actor-b";
const PARSER_V1: &str = "awr-team-source/1";
const PARSER_V2: &str = "awr-team-source/2";

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
        .expect("postgres 17 must be running for TEAM-P3");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

async fn setup() -> (MutexGuard<'static, ()>, Client, SourceStore) {
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
    (guard, admin, SourceStore::new(app_url()))
}

fn contract_bytes(acceptance: &str) -> Vec<u8> {
    let contract = WorkContract {
        codec: WorkContract::CODEC.into(),
        work_id: WorkId::new("work-a").unwrap(),
        external_key: "W".into(),
        goals: vec!["g".into()],
        hard_rules: vec!["r".into()],
        scope_paths: vec!["src".into()],
        acceptance: vec![acceptance.into()],
        required_dependencies: vec![],
        completion_policy: "evidence".into(),
        verification_requirements: vec!["report".into()],
    };
    serde_json::to_vec(&contract).unwrap()
}

fn package(parser: &str, acceptance: &str) -> IngestRequest {
    IngestRequest {
        tenant_id: TENANT.into(),
        project_id: PROJECT.into(),
        actor_id: AUTHOR.into(),
        parser_version: parser.into(),
        files: vec![SourceFile {
            path: "contract.json".into(),
            bytes: contract_bytes(acceptance),
        }],
    }
}

fn plan(candidate: &awr_team_pg::CandidateRecord, epoch: &str) -> SourceActivationPlan {
    SourceActivationPlan {
        candidate_digest: candidate.manifest_digest.clone(),
        parser_version: candidate.parser_version.clone(),
        expected_authority_epoch: epoch.into(),
        approved_candidate_digest: candidate.manifest_digest.clone(),
    }
}

#[test]
fn path_safety_is_enforced_without_postgres() {
    assert!(validate_source_path("../etc/passwd").is_err());
    assert!(validate_source_path("/abs").is_err());
    assert!(validate_source_path("ok/file.yaml").is_ok());
}

#[tokio::test]
async fn unactivated_candidate_is_not_current_contract() {
    let (_lock, _, store) = setup().await;
    let candidate = store.ingest(package(PARSER_V1, "a")).await.unwrap();
    let err = store
        .contract_for_snapshot(TENANT, PROJECT, &candidate.snapshot_id, "work-a")
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::InactiveCandidate));
}

#[tokio::test]
async fn crash_between_projection_write_and_commit_keeps_old_source() {
    let (_lock, _, store) = setup().await;
    let first = store.ingest(package(PARSER_V1, "a")).await.unwrap();
    store
        .approve(
            TENANT,
            PROJECT,
            &first.proposal_id,
            REVIEWER,
            &first.manifest_digest,
        )
        .await
        .unwrap();
    let current = store
        .activate(
            TENANT,
            PROJECT,
            AUTHOR,
            &first.proposal_id,
            &plan(&first, "0"),
        )
        .await
        .unwrap();
    let second = store.ingest(package(PARSER_V1, "b")).await.unwrap();
    store
        .approve(
            TENANT,
            PROJECT,
            &second.proposal_id,
            REVIEWER,
            &second.manifest_digest,
        )
        .await
        .unwrap();
    store
        .abort_after_installing_projection(
            TENANT,
            PROJECT,
            AUTHOR,
            &second.proposal_id,
            &plan(&second, "1"),
        )
        .await
        .unwrap();
    let still = store.current(TENANT, PROJECT, "work-a").await.unwrap();
    assert_eq!(still.snapshot_id, current.snapshot_id);
    assert_eq!(still.contract_hash, current.contract_hash);
}

#[tokio::test]
async fn old_approval_cannot_activate_a_new_digest() {
    let (_lock, _, store) = setup().await;
    let first = store.ingest(package(PARSER_V1, "a")).await.unwrap();
    store
        .approve(
            TENANT,
            PROJECT,
            &first.proposal_id,
            REVIEWER,
            &first.manifest_digest,
        )
        .await
        .unwrap();
    let second = store.ingest(package(PARSER_V1, "b")).await.unwrap();
    let mut stolen = plan(&second, "0");
    stolen.approved_candidate_digest = first.manifest_digest;
    stolen.candidate_digest = second.manifest_digest;
    let err = store
        .activate(TENANT, PROJECT, AUTHOR, &second.proposal_id, &stolen)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        PgError::StaleApproval | PgError::CandidateNotApproved
    ));
    let err = store.current(TENANT, PROJECT, "work-a").await.unwrap_err();
    assert!(matches!(err, PgError::InactiveCandidate));
}

#[tokio::test]
async fn parser_upgrade_does_not_silently_change_active_contract() {
    let (_lock, _, store) = setup().await;
    let first = store.ingest(package(PARSER_V1, "a")).await.unwrap();
    store
        .approve(
            TENANT,
            PROJECT,
            &first.proposal_id,
            REVIEWER,
            &first.manifest_digest,
        )
        .await
        .unwrap();
    let active = store
        .activate(
            TENANT,
            PROJECT,
            AUTHOR,
            &first.proposal_id,
            &plan(&first, "0"),
        )
        .await
        .unwrap();
    let upgraded = store.ingest(package(PARSER_V2, "a")).await.unwrap();
    assert_ne!(upgraded.manifest_digest, first.manifest_digest);
    assert_ne!(upgraded.parser_version, first.parser_version);
    let mut reused = plan(&upgraded, "1");
    reused.parser_version = PARSER_V1.into();
    reused.approved_candidate_digest = first.manifest_digest;
    let err = store
        .activate(TENANT, PROJECT, AUTHOR, &upgraded.proposal_id, &reused)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        PgError::ParserMismatch | PgError::StaleApproval | PgError::CandidateNotApproved
    ));
    let still = store.current(TENANT, PROJECT, "work-a").await.unwrap();
    assert_eq!(still.snapshot_id, active.snapshot_id);
    assert_eq!(still.parser_version, PARSER_V1);
}

#[tokio::test]
async fn author_cannot_approve_own_candidate_and_unsafe_paths_never_land() {
    let (_lock, admin, store) = setup().await;
    let candidate = store.ingest(package(PARSER_V1, "a")).await.unwrap();
    let err = store
        .approve(
            TENANT,
            PROJECT,
            &candidate.proposal_id,
            AUTHOR,
            &candidate.manifest_digest,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::AuthorCannotApprove));
    let mut unsafe_pkg = package(PARSER_V1, "a");
    unsafe_pkg.files[0].path = "../secret.json".into();
    let err = store.ingest(unsafe_pkg).await.unwrap_err();
    assert!(matches!(err, PgError::UnsafeSourcePath(_)));
    let count: i64 = admin
        .query_one("SELECT count(*) FROM awr_team.source_snapshots", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 1);
}
