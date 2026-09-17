#![cfg(feature = "pg-tests")]

use awr_team_pg::{Bootstrap, LeaseStore, migrate};
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;
use tokio_postgres::{Client, NoTls};

static DB: Mutex<()> = Mutex::new(());

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

#[tokio::test]
async fn records_actual_claim_rate_without_declaring_an_sla() {
    let _guard: MutexGuard<_> = DB.lock().unwrap_or_else(|e| e.into_inner());
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
             INSERT INTO awr_team.actors(tenant_id,id,kind,display_name,status)
                VALUES ('tenant-a','actor-a','agent','A','active');
             INSERT INTO awr_team.projects(tenant_id,id,key,mode,coordinator_epoch,status)
                VALUES ('tenant-a','project-a','alpha','team','epoch-1','active');
             INSERT INTO awr_team.work_scopes(tenant_id,project_id,id,name,status)
                VALUES ('tenant-a','project-a','main','main','active');",
        )
        .await
        .unwrap();
    let store = LeaseStore::new(app_url());
    let n = 20usize;
    let start = Instant::now();
    for i in 0..n {
        let work = format!("work-{i}");
        admin
            .execute(
                "INSERT INTO awr_team.work_items(tenant_id,project_id,id,external_key) VALUES ('tenant-a','project-a',$1,$1)",
                &[&work],
            )
            .await
            .unwrap();
        let session = store
            .start_session(
                "tenant-a",
                "project-a",
                "actor-a",
                "client-a",
                &format!("c{i}"),
                "main",
                &work,
            )
            .await
            .unwrap();
        store
            .claim(
                "tenant-a",
                "project-a",
                &session.id,
                "actor-a",
                "client-a",
                &format!("r{i}"),
                60,
            )
            .await
            .unwrap();
    }
    let elapsed = start.elapsed();
    let pg: String = admin
        .query_one("SHOW server_version", &[])
        .await
        .unwrap()
        .get(0);
    let rate = n as f64 / elapsed.as_secs_f64().max(0.001);
    // Recorded measurement only. Do not treat this as a production SLA.
    assert!(rate > 0.0);
    assert!(pg.starts_with('1'));
    eprintln!(
        "capacity_probe clients=1 ops={n} elapsed_ms={} rate_ops_s={rate:.2} pg={pg}",
        elapsed.as_millis()
    );
}
