use awr_core::*;

fn small() -> ManagementObservation {
    ManagementObservation {
        observed_at: 1,
        note: "One bounded result, one owner, no deferred work".into(),
        single_outcome: Some(true),
        bounded_scope: Some(true),
        single_executor: Some(true),
        no_deferred_wait: Some(true),
        independently_schedulable_units: Some(1),
        plan_valid: Some(true),
        outcome_known: Some(true),
        ..Default::default()
    }
}
#[test]
fn elapsed_time_and_completed_rework_prompt_review_without_promoting_a_bounded_task() {
    for (elapsed, cycles) in [
        (1, 0),
        (30 * 60 * 1000, 0),
        (1, 3),
        (72 * 60 * 60 * 1000, 80),
    ] {
        let mut o = small();
        o.active_elapsed_ms = Some(elapsed);
        o.completed_rework_cycles = Some(cycles);
        let d = decide_management(Some(&o), vec![], false);
        assert_eq!(d.mode, ManagementMode::Lightweight);
        assert_eq!(
            d.reevaluation_signals.is_empty(),
            elapsed < 30 * 60 * 1000 && cycles < 3
        );
        assert_eq!(d.completion_policy, "unchanged_source_policy");
    }
}
#[test]
fn omitted_observations_never_mean_false_and_upgrade_never_relaxes_completion() {
    let unknown = decide_management(None, vec![], false);
    assert_eq!(unknown.mode, ManagementMode::Undetermined);
    assert_eq!(unknown.unknown_observations.len(), 7);
    for field in 0..7 {
        let mut o = small();
        match field {
            0 => o.single_outcome = Some(false),
            1 => o.bounded_scope = Some(false),
            2 => o.single_executor = Some(false),
            3 => o.no_deferred_wait = Some(false),
            4 => o.independently_schedulable_units = Some(2),
            5 => o.plan_valid = Some(false),
            _ => o.outcome_known = Some(false),
        }
        let d = decide_management(Some(&o), vec![], false);
        assert_eq!(d.mode, ManagementMode::Continuous);
        assert!(d.reasons.iter().all(|r| r.basis == "host_assertion"));
        assert_eq!(d.completion_policy, unknown.completion_policy);
        assert_eq!(d.execution_admission, unknown.execution_admission);
    }
    assert_eq!(
        decide_management(Some(&small()), vec![], true).mode,
        ManagementMode::Continuous
    );
}
