#![cfg(feature = "pg-tests")]

use awr_team_pg::{Bootstrap, LeaseStore, PgError, migrate};
use std::sync::{Mutex, MutexGuard};
use tokio_postgres::{Client, NoTls};

static DB: Mutex<()> = Mutex::new(());
const TENANT: &str = "tenant-a";
const PROJECT: &str = "project-a";
const ACTOR: &str = "actor-a";
const OTHER: &str = "actor-b";
const CLIENT: &str = "client-a";
const CLIENT_B: &str = "client-b";

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
        .expect("postgres 17 must be running for TEAM-P5");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

async fn setup() -> (MutexGuard<'static, ()>, Client, LeaseStore) {
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
             INSERT INTO awr_team.work_items(tenant_id,project_id,id,external_key) VALUES
                ('tenant-a','project-a','work-a','W'),
                ('tenant-a','project-a','work-b','X');",
        )
        .await
        .unwrap();
    (guard, admin, LeaseStore::new(app_url()))
}

#[tokio::test]
async fn only_one_active_claim_wins_and_unique_index_holds() {
    let (_lock, admin, store) = setup().await;
    let left = store
        .start_session(TENANT, PROJECT, ACTOR, CLIENT, "conv-a", "main", "work-a")
        .await
        .unwrap();
    let right = store
        .start_session(TENANT, PROJECT, OTHER, CLIENT_B, "conv-b", "main", "work-a")
        .await
        .unwrap();
    let a = store.claim(TENANT, PROJECT, &left.id, ACTOR, CLIENT, "r1", 60);
    let b = store.claim(TENANT, PROJECT, &right.id, OTHER, CLIENT_B, "r2", 60);
    let (ra, rb) = tokio::join!(a, b);
    let wins = [&ra, &rb].iter().filter(|r| r.is_ok()).count();
    let losses = [&ra, &rb].iter().filter(|r| r.is_err()).count();
    assert_eq!(wins, 1);
    assert_eq!(losses, 1);
    let active: i64 = admin
        .query_one(
            "SELECT count(*) FROM awr_team.claims WHERE state='active' AND work_id='work-a'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(active, 1);
}

#[tokio::test]
async fn different_work_items_do_not_share_a_global_cas() {
    let (_lock, _, store) = setup().await;
    let a = store
        .start_session(TENANT, PROJECT, ACTOR, CLIENT, "c1", "main", "work-a")
        .await
        .unwrap();
    let b = store
        .start_session(TENANT, PROJECT, OTHER, CLIENT_B, "c2", "main", "work-b")
        .await
        .unwrap();
    store
        .claim(TENANT, PROJECT, &a.id, ACTOR, CLIENT, "w1", 60)
        .await
        .unwrap();
    store
        .claim(TENANT, PROJECT, &b.id, OTHER, CLIENT_B, "w2", 60)
        .await
        .unwrap();
}

#[tokio::test]
async fn expired_claim_is_not_revived_and_row_is_kept() {
    let (_lock, admin, store) = setup().await;
    let session = store
        .start_session(TENANT, PROJECT, ACTOR, CLIENT, "c", "main", "work-a")
        .await
        .unwrap();
    let claim = store
        .claim(TENANT, PROJECT, &session.id, ACTOR, CLIENT, "c1", 1)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    store
        .expire_due(TENANT, PROJECT, "main", "work-a")
        .await
        .unwrap();
    let err = store
        .renew(TENANT, PROJECT, &claim.id, ACTOR, CLIENT, "late", 60)
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::LeaseExpired));
    let state: String = admin
        .query_one(
            "SELECT state FROM awr_team.claims WHERE id=$1",
            &[&claim.id],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(state, "expired");
}

#[tokio::test]
async fn renew_replay_does_not_move_expiry_and_handoff_invalidates_old_fence() {
    let (_lock, _, store) = setup().await;
    let session = store
        .start_session(TENANT, PROJECT, ACTOR, CLIENT, "c", "main", "work-a")
        .await
        .unwrap();
    let claim = store
        .claim(TENANT, PROJECT, &session.id, ACTOR, CLIENT, "c1", 60)
        .await
        .unwrap();
    let first = store
        .renew(TENANT, PROJECT, &claim.id, ACTOR, CLIENT, "hb1", 60)
        .await
        .unwrap();
    let replay = store
        .renew(TENANT, PROJECT, &claim.id, ACTOR, CLIENT, "hb1", 60)
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(first.expires_at, replay.expires_at);
    assert_eq!(first.lease_version, replay.lease_version);
    store
        .require_fence(TENANT, PROJECT, "work-a", ACTOR, claim.fence)
        .await
        .unwrap();
    let handed = store
        .handoff(TENANT, PROJECT, &claim.id, ACTOR, OTHER, CLIENT_B, "next")
        .await
        .unwrap();
    assert!(handed.fence > claim.fence);
    let err = store
        .require_fence(TENANT, PROJECT, "work-a", ACTOR, claim.fence)
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::StaleFence | PgError::LeaseExpired));
}

#[tokio::test]
async fn wait_does_not_renew_and_recovery_block_stops_new_claims() {
    let (_lock, _, store) = setup().await;
    let session = store
        .start_session(TENANT, PROJECT, ACTOR, CLIENT, "c", "main", "work-a")
        .await
        .unwrap();
    store
        .claim(TENANT, PROJECT, &session.id, ACTOR, CLIENT, "c1", 60)
        .await
        .unwrap();
    store
        .wait(TENANT, PROJECT, &session.id, ACTOR, "need review?")
        .await
        .unwrap();
    let rival = store
        .start_session(TENANT, PROJECT, OTHER, CLIENT_B, "c2", "main", "work-a")
        .await
        .unwrap();
    let wait_err = store
        .claim(
            TENANT,
            PROJECT,
            &rival.id,
            OTHER,
            CLIENT_B,
            "blocked-by-wait",
            60,
        )
        .await
        .unwrap_err();
    assert!(matches!(wait_err, PgError::WaitOpen | PgError::ClaimHeld));
    store
        .set_recovery_blocked(TENANT, PROJECT, "main", "work-b", true)
        .await
        .unwrap();
    let other = store
        .start_session(TENANT, PROJECT, OTHER, CLIENT_B, "c3", "main", "work-b")
        .await
        .unwrap();
    let err = store
        .claim(TENANT, PROJECT, &other.id, OTHER, CLIENT_B, "rb", 60)
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::RecoveryBlocked));
}

#[tokio::test]
async fn cannot_release_someone_elses_claim() {
    let (_lock, _, store) = setup().await;
    let session = store
        .start_session(TENANT, PROJECT, ACTOR, CLIENT, "c", "main", "work-a")
        .await
        .unwrap();
    let claim = store
        .claim(TENANT, PROJECT, &session.id, ACTOR, CLIENT, "c1", 60)
        .await
        .unwrap();
    let err = store
        .release(TENANT, PROJECT, &claim.id, OTHER)
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::Forbidden));
}
