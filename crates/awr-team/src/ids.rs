use crate::error::{TeamError, TeamResult};
use serde::{Deserialize, Serialize};

macro_rules! typed_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> TeamResult<Self> {
                let value = value.into();
                if value.is_empty() || value.len() > 128 || value.chars().any(|c| c.is_control()) {
                    return Err(TeamError::InvalidId(stringify!($name).into()));
                }
                Ok(Self(value))
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

typed_id!(TenantId);
typed_id!(ProjectId);
typed_id!(ScopeId);
typed_id!(WorkId);
typed_id!(ActorId);
typed_id!(SessionId);
typed_id!(RequestId);
