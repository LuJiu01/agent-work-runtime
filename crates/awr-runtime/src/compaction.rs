//! Native compaction telemetry is host-supplied. AWR advises; it never clears host history.
use awr_core::*;
use awr_store::Store;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObserveCompactionRequest {
    pub session: Id,
    pub expected_revision: Revision,
    pub observation: CompactionObservation,
    #[serde(default)]
    pub policy: CompactionPolicy,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeferCompactionRequest {
    pub session: Id,
    pub expected_revision: Revision,
    pub observation_event_id: Id,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectCompactionRequest {
    pub session: Id,
    #[serde(default)]
    pub include_observation: bool,
}

pub fn observe_compaction(
    store: &mut Store,
    root: &Path,
    request: &ObserveCompactionRequest,
) -> Result<Value> {
    let project = store.project_by_root(root)?;
    let (event, replay) = store.record_compaction(
        project.id,
        request.expected_revision,
        request.session,
        &request.observation,
        &request.policy,
    )?;
    let mut result = inspect_compaction(
        store,
        root,
        &InspectCompactionRequest {
            session: request.session,
            include_observation: false,
        },
    )?;
    result["recorded_event_id"] = json!(event.id);
    result["already_recorded"] = json!(replay);
    result["read_only"] = json!(false);
    Ok(result)
}
pub fn defer_compaction(
    store: &mut Store,
    root: &Path,
    request: &DeferCompactionRequest,
) -> Result<Value> {
    let project = store.project_by_root(root)?;
    let (event, replay) = store.defer_compaction(
        project.id,
        request.expected_revision,
        request.session,
        request.observation_event_id,
    )?;
    let mut result = inspect_compaction(
        store,
        root,
        &InspectCompactionRequest {
            session: request.session,
            include_observation: false,
        },
    )?;
    result["recorded_event_id"] = json!(event.id);
    result["already_recorded"] = json!(replay);
    result["read_only"] = json!(false);
    Ok(result)
}

pub fn inspect_compaction(
    store: &Store,
    root: &Path,
    request: &InspectCompactionRequest,
) -> Result<Value> {
    let project = store.project_by_root(root)?;
    let session = store.session(project.id, request.session)?;
    let events = store.compaction_events(project.id, session.id, 2)?;
    let latest = events.first();
    let observation = latest
        .map(|e| serde_json::from_value::<CompactionObservation>(e.payload["observation"].clone()))
        .transpose()?;
    let policy = latest
        .map(|e| serde_json::from_value::<CompactionPolicy>(e.payload["policy"].clone()))
        .transpose()?
        .unwrap_or_default();
    let deferred = latest
        .map(|e| store.compaction_deferral(project.id, session.id, e.id))
        .transpose()?
        .flatten();
    let occupancy = observation
        .as_ref()
        .and_then(CompactionObservation::occupancy_percent);
    let previous = events
        .get(1)
        .map(|e| serde_json::from_value::<CompactionObservation>(e.payload["observation"].clone()))
        .transpose()?;
    let comparable = observation
        .as_ref()
        .zip(previous.as_ref())
        .filter(|(a, b)| {
            a.model == b.model
                && a.context_window_tokens == b.context_window_tokens
                && a.trigger == b.trigger
        });
    let delta = comparable.and_then(|(a, b)| Some(a.occupancy_percent()? - b.occupancy_percent()?));
    let (state, mut guidance) = if deferred.is_some() {
        (
            "deferred",
            ActionGuidance::new(
                "This observation was deferred",
                "Recorded deferral; native compaction stays enabled",
                "Continue the current task without repeating this suggestion",
                "Next completed native compaction or changed model/window",
            ),
        )
    } else if observation
        .as_ref()
        .is_some_and(|o| o.trigger == CompactionTrigger::Manual)
    {
        (
            "manual_observation",
            ActionGuidance::new(
                "The recorded compaction was manual",
                "Manual observation retained separately from automatic compaction policy",
                "Continue with native compaction enabled",
                "Next completed automatic compaction",
            ),
        )
    } else if let Some(ratio) = occupancy {
        let basis = format!(
            "Host-reported post-compaction occupancy {ratio:.1}%; threshold {}% (heuristic)",
            policy.post_compaction_threshold_percent
        );
        if ratio >= f64::from(policy.post_compaction_threshold_percent) {
            (
                "handoff_candidate",
                ActionGuidance::new(
                    "Same model/window; completed automatic compaction is at or above threshold",
                    &basis,
                    "At a safe boundary, checkpoint and estimate fixed + required restart context; if smaller and work remains, ask before opening a new session",
                    "User decision, next compaction, or changed model/window; query unresolved execution before retrying",
                ),
            )
        } else {
            (
                "continue",
                ActionGuidance::new(
                    "Same model/window; completed automatic compaction is below threshold",
                    &basis,
                    "Continue the current task with native compaction enabled",
                    "Next completed compaction or changed model/window",
                ),
            )
        }
    } else {
        (
            "measurement_missing",
            ActionGuidance::new(
                "Post-compaction full-request usage or effective window is unknown",
                "Summary length and cumulative billed tokens cannot establish occupancy",
                "Continue existing workflow; have the host report available full-request metrics without guessing",
                "Next completed compaction with comparable host measurements",
            ),
        )
    };
    // A switch suggestion must never displace an unresolved outcome or persistent wait.
    if let Some(guard) = crate::guidance::session_guard(store, project.id, session.id)? {
        guidance = guard;
    }
    if serde_json::to_vec(&guidance)?.len() > ACTION_GUIDANCE_MAX_BYTES {
        return Err(Error::InvalidInput(
            "action guidance exceeds its fixed byte budget".into(),
        ));
    }
    let mut value = json!({"ok":true,"version":1,"read_only":true,"project_revision":project.project_revision,
        "project_id":project.id,"freshness_basis":"runtime_records_only",
        "session":session.id,"state":state,"observation_event_id":latest.map(|e|e.id),
        "post_compaction_percent":occupancy,"previous_delta_percentage_points":delta,
        "policy":policy,"guidance":guidance,"native_compaction":"unchanged","session_switch_performed":false});
    if request.include_observation {
        value["observation"] = json!(observation);
        value["previous_observation"] = json!(previous);
        value["deferral_event_id"] = json!(deferred.map(|e| e.id));
        value["measurement_basis"] = json!(
            "host_reported_not_independently_verified; absent costs/durations remain unknown"
        );
    }
    Ok(value)
}
