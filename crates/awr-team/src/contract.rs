use crate::canonical::{canonical_json, contract_hash};
use crate::error::{TeamError, TeamResult};
use crate::ids::WorkId;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkDefinitionState {
    Draft,
    Enabled,
    Archived,
}

/// Semantic work definition used for Team contract hashing.
/// Progress notes and next_action are intentionally excluded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkContract {
    pub codec: String,
    pub work_id: WorkId,
    pub external_key: String,
    pub goals: Vec<String>,
    pub hard_rules: Vec<String>,
    pub scope_paths: Vec<String>,
    pub acceptance: Vec<String>,
    pub required_dependencies: Vec<String>,
    pub completion_policy: String,
    pub verification_requirements: Vec<String>,
}

impl WorkContract {
    pub const CODEC: &'static str = "awr-team-contract-v1";

    pub fn validate(&self) -> TeamResult<()> {
        if self.codec != Self::CODEC {
            return Err(TeamError::InvalidContract(
                "unsupported contract codec".into(),
            ));
        }
        if self.external_key.trim().is_empty() {
            return Err(TeamError::InvalidContract("external_key required".into()));
        }
        if self.acceptance.is_empty() {
            return Err(TeamError::InvalidContract("acceptance required".into()));
        }
        if self.completion_policy.trim().is_empty() {
            return Err(TeamError::InvalidContract(
                "completion_policy required".into(),
            ));
        }
        Ok(())
    }

    pub fn hash(&self) -> TeamResult<String> {
        self.validate()?;
        contract_hash(&self.canonical_fields()?)
    }

    fn canonical_fields(&self) -> TeamResult<Value> {
        let mut goals = self.goals.clone();
        let mut hard_rules = self.hard_rules.clone();
        let mut scope_paths = self.scope_paths.clone();
        let mut acceptance = self.acceptance.clone();
        let mut required_dependencies = self.required_dependencies.clone();
        let mut verification_requirements = self.verification_requirements.clone();
        goals.sort();
        hard_rules.sort();
        scope_paths.sort();
        acceptance.sort();
        required_dependencies.sort();
        verification_requirements.sort();
        let value = json!({
            "codec": self.codec,
            "work_id": self.work_id.as_str(),
            "external_key": self.external_key,
            "goals": goals,
            "hard_rules": hard_rules,
            "scope_paths": scope_paths,
            "acceptance": acceptance,
            "required_dependencies": required_dependencies,
            "completion_policy": self.completion_policy,
            "verification_requirements": verification_requirements,
        });
        let _ = canonical_json(&value)?;
        Ok(value)
    }
}
