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
    #[error("protocol unsupported")]
    ProtocolUnsupported,
    #[error("project required")]
    ProjectRequired,
    #[error("offline team writes are forbidden")]
    OfflineWriteForbidden,
    #[error("body cannot override authorized project")]
    AuthProjectMismatch,
    #[error("credential reference invalid")]
    SecretRefInvalid,
}

impl TeamError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::ProtocolUnsupported => "PROTOCOL_UNSUPPORTED",
            Self::ProjectRequired => "PROJECT_NOT_AVAILABLE",
            Self::OfflineWriteForbidden => "SERVICE_UNAVAILABLE",
            Self::AuthProjectMismatch => "FORBIDDEN",
            Self::SecretRefInvalid => "UNAUTHENTICATED",
            Self::UnknownRequiredField(_) => "PROTOCOL_UNSUPPORTED",
            Self::MissingRequiredField(_) => "PROTOCOL_UNSUPPORTED",
            Self::InvalidVersion(_) => "PROTOCOL_UNSUPPORTED",
            _ => "INVALID_CONTRACT",
        }
    }
}
