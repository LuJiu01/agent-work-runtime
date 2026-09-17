#![cfg(feature = "pg-tests")]

use awr_team_pg::{Bootstrap, ImportStore, LeaseStore, PgError, check_schema, migrate};
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
        .expect("postgres 17 must be running for TEAM-P11");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

async fn setup() -> (MutexGuard<'static, ()>, Client) {
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
                ('tenant-a','actor-b','agent','B','active');
             INSERT INTO awr_team.projects(tenant_id,id,key,mode,coordinator_epoch,status)
                VALUES ('tenant-a','project-a','alpha','team','epoch-1','active');
             INSERT INTO awr_team.work_scopes(tenant_id,project_id,id,name,status)
                VALUES ('tenant-a','project-a','main','main','active');
             INSERT INTO awr_team.work_items(tenant_id,project_id,id,external_key)
                VALUES ('tenant-a','project-a','work-a','W');",
        )
        .await
        .unwrap();
    (guard, admin)
}

#[tokio::test]
async fn two_clients_cannot_hold_the_same_claim() {
    let (_lock, admin) = setup().await;
    let left = LeaseStore::new(app_url());
    let right = LeaseStore::new(app_url());
    let s1 = left
        .start_session(TENANT, PROJECT, "actor-a", "c1", "conv-a", "main", "work-a")
        .await
        .unwrap();
    let s2 = right
        .start_session(TENANT, PROJECT, "actor-b", "c2", "conv-b", "main", "work-a")
        .await
        .unwrap();
    let a = left.claim(TENANT, PROJECT, &s1.id, "actor-a", "c1", "r1", 60);
    let b = right.claim(TENANT, PROJECT, &s2.id, "actor-b", "c2", "r2", 60);
    let (ra, rb) = tokio::join!(a, b);
    assert_eq!([&ra, &rb].iter().filter(|r| r.is_ok()).count(), 1);
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
async fn reconnect_after_drop_still_requires_matching_schema() {
    let (_lock, _) = setup().await;
    let client = connect(&admin_url()).await;
    check_schema(&client).await.unwrap();
    drop(client);
    let again = connect(&admin_url()).await;
    check_schema(&again).await.unwrap();
}

#[tokio::test]
async fn missing_backup_objects_fail_closed() {
    let (_lock, _) = setup().await;
    let store = ImportStore::new(app_url());
    let backup = store
        .backup(TENANT, PROJECT, &["missing-art".into()], &["src".into()])
        .await
        .unwrap();
    let err = store
        .restore(TENANT, PROJECT, &backup.id, false, false)
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::RestoreIncomplete));
}
