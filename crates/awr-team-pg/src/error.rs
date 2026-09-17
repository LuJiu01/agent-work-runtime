use thiserror::Error;

pub type PgResult<T> = Result<T, PgError>;

#[derive(Debug, Error)]
pub enum PgError {
    #[error("schema incompatible: {0}")]
    SchemaIncompatible(String),
    #[error("idempotency conflict")]
    IdempotencyConflict,
    #[error("project not available")]
    ProjectNotAvailable,
    #[error("{0}")]
    Db(#[from] tokio_postgres::Error),
    #[error("{0}")]
    Protocol(String),
}
