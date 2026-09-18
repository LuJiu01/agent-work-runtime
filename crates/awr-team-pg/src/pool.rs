//! Connection pooling for the Team PostgreSQL store (ADR-0004).
//!
//! Domain stores acquire pooled connections instead of opening one TCP
//! connection per operation. Owner migration paths still use a dedicated
//! single connection (`crate::connect`).
//!
//! Scope binding uses transaction-local `set_config(..., is_local = true)`,
//! so a recycled connection carries no residual session state and
//! `RecyclingMethod::Fast` is safe.
use std::time::Duration;

use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod, Timeouts};
use tokio::sync::OnceCell;
use tokio_postgres::config::SslMode;

use crate::error::{PgError, PgResult};

/// Default upper bound of pooled connections per store instance.
const DEFAULT_MAX_SIZE: usize = 8;
/// Environment override for the pool size (§19.1 configuration structure).
const MAX_SIZE_ENV: &str = "AWR_TEAM_PG_POOL_MAX_SIZE";

/// Pooled connection handle; dereferences to `tokio_postgres::Client`.
pub type PgClient = deadpool_postgres::Object;

/// Lazily initialized connection pool bound to one database URL.
///
/// Construction is infallible so `Store::new(url)` keeps its signature;
/// URL or TLS configuration errors surface on the first `get()`.
pub struct PgPool {
    url: String,
    inner: OnceCell<Pool>,
}

impl PgPool {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            inner: OnceCell::new(),
        }
    }

    pub async fn get(&self) -> PgResult<PgClient> {
        let pool = self
            .inner
            .get_or_try_init(|| async { build_pool(&self.url) })
            .await?;
        Ok(pool.get().await?)
    }
}

fn parse_config(url: &str) -> PgResult<tokio_postgres::Config> {
    url.parse()
        .map_err(|error| PgError::Protocol(format!("invalid database url: {error}")))
}

fn tls_required(config: &tokio_postgres::Config) -> bool {
    // tokio-postgres models libpq verify-ca/verify-full as Require; rustls
    // always performs full certificate and hostname verification anyway.
    matches!(config.get_ssl_mode(), SslMode::Require)
}

#[cfg(feature = "tls")]
fn rustls_connector() -> tokio_postgres_rustls::MakeRustlsConnect {
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tokio_postgres_rustls::MakeRustlsConnect::new(config)
}

#[cfg(not(feature = "tls"))]
fn tls_disabled_error() -> PgError {
    PgError::Protocol(
        "database url requires TLS (sslmode=require/verify-ca/verify-full) but awr-team-pg was built without the `tls` feature"
            .into(),
    )
}

fn max_size() -> usize {
    std::env::var(MAX_SIZE_ENV)
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
        .filter(|size| *size > 0)
        .unwrap_or(DEFAULT_MAX_SIZE)
}

fn timeouts() -> Timeouts {
    Timeouts {
        wait: Some(Duration::from_secs(10)),
        create: Some(Duration::from_secs(5)),
        recycle: Some(Duration::from_secs(5)),
    }
}

fn manager_config() -> ManagerConfig {
    ManagerConfig {
        recycling_method: RecyclingMethod::Fast,
    }
}

fn build_pool(url: &str) -> PgResult<Pool> {
    let config = parse_config(url)?;
    let manager = if tls_required(&config) {
        #[cfg(feature = "tls")]
        {
            Manager::from_config(config, rustls_connector(), manager_config())
        }
        #[cfg(not(feature = "tls"))]
        {
            return Err(tls_disabled_error());
        }
    } else {
        Manager::from_config(config, tokio_postgres::NoTls, manager_config())
    };
    Pool::builder(manager)
        .runtime(deadpool_postgres::Runtime::Tokio1)
        .max_size(max_size())
        .timeouts(timeouts())
        .build()
        .map_err(|error| PgError::Protocol(format!("pool build failed: {error}")))
}

/// Connect a single dedicated client (owner migration / bootstrap path).
/// Honors `sslmode` the same way as pooled connections.
pub async fn connect(url: &str) -> PgResult<tokio_postgres::Client> {
    let config = parse_config(url)?;
    if tls_required(&config) {
        #[cfg(feature = "tls")]
        {
            let (client, connection) = config.connect(rustls_connector()).await?;
            tokio::spawn(async move {
                let _ = connection.await;
            });
            return Ok(client);
        }
        #[cfg(not(feature = "tls"))]
        {
            return Err(tls_disabled_error());
        }
    }
    let (client, connection) = config.connect(tokio_postgres::NoTls).await?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    Ok(client)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn invalid_url_surfaces_on_first_acquire() {
        let pool = PgPool::new("not a url");
        let error = pool.get().await.expect_err("invalid url must fail");
        assert!(matches!(error, PgError::Protocol(_)), "{error}");
    }

    #[cfg(not(feature = "tls"))]
    #[tokio::test]
    async fn sslmode_require_without_tls_feature_fails_loudly() {
        let pool = PgPool::new("postgres://u:p@127.0.0.1:1/db?sslmode=require");
        let error = pool.get().await.expect_err("tls must be required");
        match error {
            PgError::Protocol(message) => assert!(message.contains("tls"), "{message}"),
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn max_size_defaults_and_env_override() {
        unsafe { std::env::remove_var(MAX_SIZE_ENV) };
        assert_eq!(max_size(), DEFAULT_MAX_SIZE);
        unsafe { std::env::set_var(MAX_SIZE_ENV, "3") };
        assert_eq!(max_size(), 3);
        unsafe { std::env::set_var(MAX_SIZE_ENV, "0") };
        assert_eq!(max_size(), DEFAULT_MAX_SIZE);
        unsafe { std::env::remove_var(MAX_SIZE_ENV) };
    }
}
