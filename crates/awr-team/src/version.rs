use crate::error::{TeamError, TeamResult};

/// Team API 64-bit counters travel as decimal strings.
pub fn encode_u64(value: u64) -> String {
    value.to_string()
}

pub fn decode_u64(value: &str) -> TeamResult<u64> {
    if value.is_empty() || (value.len() > 1 && value.starts_with('0')) {
        return Err(TeamError::InvalidVersion(value.into()));
    }
    value
        .parse()
        .map_err(|_| TeamError::InvalidVersion(value.into()))
}
