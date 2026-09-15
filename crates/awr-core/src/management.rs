//! Management intensity is independent of the completion/authorization policy.
use crate::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagementObservation {
    pub observed_at: i64,
    pub note: String,
    pub single_outcome: Option<bool>,
    pub bounded_scope: Option<bool>,
    pub single_executor: Option<bool>,
    pub no_deferred_wait: Option<bool>,
    pub independently_schedulable_units: Option<u32>,
    pub plan_valid: Option<bool>,
    pub outcome_known: Option<bool>,
    pub active_elapsed_ms: Option<u64>,
    pub completed_rework_cycles: Option<u32>,
}
impl ManagementObservation {
    pub fn validate(&self, at: i64) -> Result<()> {
        ensure_public_data(self)?;
        if self.observed_at < 0
            || self.observed_at > at
            || self.note.trim().is_empty()
            || self.note.len() > 4096
            || self.independently_schedulable_units == Some(0)
        {
            return Err(Error::InvalidInput("observations require a past/current observed_at, bounded explanatory note and positive unit count when known".into()));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagementReason {
    pub code: String,
    pub basis: String,
    pub reference: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagementMode {
    Undetermined,
    Lightweight,
    Continuous,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagementDecision {
    pub version: u32,
    pub mode: ManagementMode,
    pub reasons: Vec<ManagementReason>,
    pub unknown_observations: Vec<String>,
    pub reevaluation_signals: Vec<String>,
    pub required_actions: Vec<String>,
    pub optional_maintenance: Vec<String>,
    pub completion_policy: String,
    pub execution_admission: String,
}
pub fn decide_management(
    observation: Option<&ManagementObservation>,
    mut reasons: Vec<ManagementReason>,
    prior_continuous: bool,
) -> ManagementDecision {
    let mut unknown = vec![];
    let mut reevaluate = vec![];
    let empty = ManagementObservation::default();
    let o = observation.unwrap_or(&empty);
    for (name, value, trigger) in [
        ("single_outcome", o.single_outcome, "multiple_outcomes"),
        ("bounded_scope", o.bounded_scope, "scope_requires_planning"),
        (
            "single_executor",
            o.single_executor,
            "handoff_or_collaboration",
        ),
        ("no_deferred_wait", o.no_deferred_wait, "deferred_wait"),
        ("plan_valid", o.plan_valid, "plan_invalidated"),
        (
            "outcome_known",
            o.outcome_known,
            "query_outcome_before_retry",
        ),
    ] {
        match value {
            None => unknown.push(name.into()),
            Some(false) => reasons.push(ManagementReason {
                code: trigger.into(),
                basis: "host_assertion".into(),
                reference: name.into(),
            }),
            Some(true) => (),
        }
    }
    match o.independently_schedulable_units {
        None => unknown.push("independently_schedulable_units".into()),
        Some(n) if n >= 2 => reasons.push(ManagementReason {
            code: "independent_work_units".into(),
            basis: "host_assertion".into(),
            reference: "independently_schedulable_units".into(),
        }),
        _ => (),
    }
    if o.active_elapsed_ms.is_some_and(|n| n >= 30 * 60 * 1000) {
        reevaluate.push("active_elapsed_at_least_30_minutes".into());
    }
    if o.completed_rework_cycles.is_some_and(|n| n >= 3) {
        reevaluate.push("at_least_3_completed_rework_cycles".into());
    }
    let mode = if prior_continuous || !reasons.is_empty() {
        ManagementMode::Continuous
    } else if unknown.is_empty() {
        ManagementMode::Lightweight
    } else {
        ManagementMode::Undetermined
    };
    if prior_continuous {
        reasons.push(ManagementReason {
            code: "continuous_management_retained".into(),
            basis: "runtime_history".into(),
            reference: "previous_management_record".into(),
        });
    }
    let mut required_actions = vec![
        "preserve_identity_intent_scope_and_current_state",
        "consume_required_context_and_hard_rules",
        "retain_completion_basis_and_actual_outcome",
        "check_source_versions_permissions_claims_and_request_identity",
    ];
    let optional = if mode == ManagementMode::Continuous {
        required_actions.extend([
            "maintain_dependencies_and_ownership",
            "checkpoint_at_wait_handoff_interruption_and_phase_change",
            "maintain_next_action_wait_conditions_and_open_loops",
            "query_unknown_results_before_any_retry",
        ]);
        vec![
            "per_tool_checkpoints",
            "duplicate_goal_rule_or_report_bodies",
        ]
    } else {
        vec![
            "separate_goal_document_when_explicit_inheritance_suffices",
            "per_tool_checkpoints",
            "task_graph_without_dependencies",
            "duplicate_summaries_or_evidence_bodies",
        ]
    };
    if !unknown.is_empty() && mode == ManagementMode::Undetermined {
        required_actions.push("assess_unknown_management_facts");
    }
    if !reevaluate.is_empty() {
        required_actions.push("reevaluate_scope_plan_and_recovery_without_automatic_upgrade");
    }
    ManagementDecision {
        version: 1,
        mode,
        reasons,
        unknown_observations: unknown,
        reevaluation_signals: reevaluate,
        required_actions: required_actions.into_iter().map(String::from).collect(),
        optional_maintenance: optional.into_iter().map(String::from).collect(),
        completion_policy: "unchanged_source_policy".into(),
        execution_admission: "not_granted_by_management_classification".into(),
    }
}
