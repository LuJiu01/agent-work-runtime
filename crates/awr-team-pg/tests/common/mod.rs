//! Shared hardened fixture for pg-tests suites (CR #52 / CR #37 isolation
//! contract):
//! - targets are validated AFTER parsing with `tokio_postgres::Config`;
//! - each test process creates its own uniquely named database and never
//!   adopts a pre-existing one;
//! - registration is atomic (tokio OnceCell::get_or_init);
//! - only resources created by this process are cleaned.
//! The runtime `AWR_TEAM_DATABASE_URL` is never read here.
#![cfg(feature = "pg-tests")]
#![allow(dead_code)]

use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio_postgres::config::{Config, Host};
use tokio_postgres::{Client, NoTls};

static DB: Mutex<()> = Mutex::new(());
static GATE_DB: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();

/// The only targets this fixture may talk to.
pub fn check_loopback(config: &Config) -> Result<(), String> {
    let hosts = config.get_hosts();
    if hosts.len() != 1 {
        return Err(format!(
            "multi-host configurations are not supported ({})",
            hosts.len()
        ));
    }
    match &hosts[0] {
        Host::Tcp(name) => {
            let loopback = name == "localhost"
                || name
                    .parse::<std::net::IpAddr>()
                    .map(|ip| ip.is_loopback())
                    .unwrap_or(false);
            if !loopback {
                return Err(format!("non-loopback host {name}"));
            }
        }
        // Unix-domain sockets are local by construction. The variant only
        // exists on unix targets.
        #[cfg(unix)]
        Host::Unix(_) => {}
    }
    // hostaddr overrides the host name for dialing; it must be loopback too.
    for addr in config.get_hostaddrs() {
        if !addr.is_loopback() {
            return Err(format!("non-loopback hostaddr {addr}"));
        }
    }
    Ok(())
}

/// The raw test connection string exactly as configured (or the loopback
/// default). Pass THIS to subprocesses instead of re-serializing a parsed
/// Config — the child re-parses it and keeps IPv6/hostaddr/Unix semantics
/// (CR #56 round 3).
pub fn test_database_url_raw() -> String {
    std::env::var("AWR_TEAM_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:awr-test@127.0.0.1:55432/postgres".into())
}

pub fn test_config() -> Config {
    let raw = std::env::var("AWR_TEAM_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:awr-test@127.0.0.1:55432/postgres".into());
    let config: Config = raw.parse().expect("invalid AWR_TEAM_TEST_DATABASE_URL");
    check_loopback(&config).expect("pg test target must be loopback");
    config
}

pub fn with_db(config: &Config, db: &str) -> Config {
    let mut c = config.clone();
    c.dbname(db);
    c
}

pub fn with_app_role(config: &Config, db: &str) -> Config {
    let mut c = with_db(config, db);
    c.user("awr_app");
    c.password("app-test");
    c
}

pub async fn connect_config(config: &Config) -> Client {
    let (client, connection) = config
        .connect(NoTls)
        .await
        .expect("loopback postgres 17 must be running for pg-tests");
    tokio::spawn(async move {
        let _ = connection.await;
    });
    client
}

pub fn nonce(attempt: u32) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{:x}_{}_{}", std::process::id(), nanos, attempt)
}

/// Create THIS process's database. A name collision is retried with a fresh
/// name; an existing database is never adopted.
async fn create_gate_db() -> String {
    let maintenance = connect_config(&with_db(&test_config(), "postgres")).await;
    for attempt in 0..8 {
        let name = format!("awr_team_gate_{}", nonce(attempt));
        match maintenance
            .batch_execute(&format!("CREATE DATABASE \"{name}\""))
            .await
        {
            Ok(()) => return name,
            Err(error) => {
                let duplicate = error
                    .as_db_error()
                    .map(|db| *db.code() == tokio_postgres::error::SqlState::DUPLICATE_DATABASE)
                    .unwrap_or(false);
                if !duplicate {
                    panic!("failed to create the gate database: {error}");
                }
            }
        }
    }
    panic!("could not allocate a fresh gate database name");
}

/// Atomic one-time registration shared by every caller in this process.
pub async fn gate_db_name() -> String {
    GATE_DB.get_or_init(create_gate_db).await.clone()
}

/// Lock the fixture, (re)create the awr_team schema inside THIS process's
/// own database, apply migrations and grants. The returned admin client is
/// connected to that database. Callers seed their own rows afterwards.
pub async fn fresh_team_schema() -> (MutexGuard<'static, ()>, Client, String) {
    let guard = DB.lock().expect("db fixture lock");
    let name = gate_db_name().await;
    let admin = connect_config(&with_db(&test_config(), &name)).await;
    // Safe: `name` was created by this process in create_gate_db(); a
    // pre-existing database is never adopted.
    admin
        .batch_execute("DROP SCHEMA IF EXISTS awr_team CASCADE")
        .await
        .unwrap();
    awr_team_pg::migrate(&admin).await.unwrap();
    admin
        .batch_execute(
            "DO $$ BEGIN CREATE ROLE awr_app LOGIN PASSWORD 'app-test' NOSUPERUSER NOBYPASSRLS; EXCEPTION WHEN duplicate_object THEN NULL; END $$",
        )
        .await
        .unwrap();
    awr_team_pg::Bootstrap::grant_app(&admin, "awr_app")
        .await
        .unwrap();
    (guard, admin, name)
}

pub async fn app_client(db: &str) -> Client {
    connect_config(&with_app_role(&test_config(), db)).await
}
