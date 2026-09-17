use thiserror::Error;

pub type TeamResult<T> = Result<T, TeamError>;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TeamError {
    #[error("invalid identifier: {0}")]
    InvalidId(String),
    #[error("invalid version string: {0}")]
    InvalidVersion(String),
    #[error("unknown required field: {0}")]
    UnknownRequiredField(String),
    #[error("missing required field: {0}")]
    MissingRequiredField(String),
    #[error("non-canonical number: {0}")]
    NonCanonicalNumber(String),
    #[error("contract field rejected: {0}")]
    InvalidContract(String),
}
