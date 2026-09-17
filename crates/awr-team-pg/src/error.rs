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
    #[error("unsafe source path: {0}")]
    UnsafeSourcePath(String),
    #[error("stale or unbound approval")]
    StaleApproval,
    #[error("authority epoch mismatch")]
    EpochMismatch,
    #[error("parser version mismatch")]
    ParserMismatch,
    #[error("candidate is not approved")]
    CandidateNotApproved,
    #[error("author cannot approve their own candidate")]
    AuthorCannotApprove,
    #[error("candidate is not the active source")]
    InactiveCandidate,
    #[error("{0}")]
    Db(#[from] tokio_postgres::Error),
    #[error("{0}")]
    Protocol(String),
}
