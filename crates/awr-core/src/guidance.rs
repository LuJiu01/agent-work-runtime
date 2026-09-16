use serde::{Deserialize, Serialize};

/// One actionable instruction; details remain in queryable receipts.
pub const ACTION_GUIDANCE_MAX_BYTES: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionGuidance {
    pub when: String,
    pub basis: String,
    pub next_action: String,
    pub recheck: String,
}
impl ActionGuidance {
    pub fn new(when: &str, basis: &str, next_action: &str, recheck: &str) -> Self {
        Self {
            when: when.into(),
            basis: basis.into(),
            next_action: next_action.into(),
            recheck: recheck.into(),
        }
    }
}
