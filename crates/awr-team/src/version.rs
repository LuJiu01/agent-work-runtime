use crate::error::{TeamError, TeamResult};

/// Team API 64-bit counters travel as decimal strings.
pub fn encode_u64(value: u64) -> String {
    value.to_string()
}

pub fn decode_u64(value: &str) -> TeamResult<u64> {
    // Canonical form only: ASCII digits, no leading zeros, no sign, no
    // whitespace. `str::parse` alone would accept "+1" and "+01", leaving
    // multiple accepted spellings for one value (CR #34 P2-3).
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(TeamError::InvalidVersion(value.into()));
    }
    if value.len() > 1 && value.starts_with('0') {
        return Err(TeamError::InvalidVersion(value.into()));
    }
    let parsed: u64 = value
        .parse()
        .map_err(|_| TeamError::InvalidVersion(value.into()))?;
    debug_assert_eq!(encode_u64(parsed), value);
    Ok(parsed)
}
