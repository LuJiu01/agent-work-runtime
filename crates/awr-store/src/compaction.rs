use crate::{Store, db_error};
use awr_core::*;
use rusqlite::{OptionalExtension, params};
use serde_json::json;

impl Store {
    /// Bounded observations; full history uses the existing event cursor.
    pub fn compaction_events(&self, project: Id, session: Id, limit: usize) -> Result<Vec<Event>> {
        self.session(project, session)?;
        if !(1..=100).contains(&limit) {
            return Err(Error::InvalidInput(
                "compaction limit must be 1..100".into(),
            ));
        }
        let ids: Vec<String> = self.conn.prepare("SELECT id FROM events WHERE project_id=?1 AND session_id=?2 AND event_type='client.compaction_observed' ORDER BY project_revision DESC LIMIT ?3")
            .map_err(db_error)?.query_map(params![project.to_string(), session.to_string(), limit as i64], |r| r.get(0)).map_err(db_error)?
            .collect::<rusqlite::Result<_>>().map_err(db_error)?;
        ids.into_iter()
            .map(|id| {
                self.event(
                    project,
                    id.parse()
                        .map_err(|_| Error::Storage("invalid compaction event identity".into()))?,
                )
            })
            .collect()
    }
    fn compaction_event_by_key(
        &self,
        project: Id,
        session: Id,
        kind: &str,
        key: &str,
    ) -> Result<Option<Event>> {
        let id: Option<String> = self.conn.query_row("SELECT id FROM events WHERE project_id=?1 AND session_id=?2 AND event_type=?3 AND (json_extract(payload_json,'$.observation.compaction_id')=?4 OR json_extract(payload_json,'$.observation_event_id')=?4) ORDER BY project_revision DESC LIMIT 1",
            params![project.to_string(), session.to_string(), kind, key], |r| r.get(0)).optional().map_err(db_error)?;
        id.map(|id| {
            self.event(
                project,
                id.parse()
                    .map_err(|_| Error::Storage("invalid compaction event identity".into()))?,
            )
        })
        .transpose()
    }
    pub fn compaction_deferral(
        &self,
        project: Id,
        session: Id,
        observation: Id,
    ) -> Result<Option<Event>> {
        self.session(project, session)?;
        self.compaction_event_by_key(
            project,
            session,
            "client.compaction_deferred",
            &observation.to_string(),
        )
    }
    pub fn record_compaction(
        &mut self,
        project: Id,
        expected: Revision,
        session: Id,
        observation: &CompactionObservation,
        policy: &CompactionPolicy,
    ) -> Result<(Event, bool)> {
        observation.validate(now_millis()?)?;
        policy.validate()?;
        let bound = self.session(project, session)?;
        if bound.work_item_id.is_none() {
            return Err(Error::InvalidInput(
                "compaction requires a work-bound session".into(),
            ));
        }
        if let Some(event) = self.compaction_event_by_key(
            project,
            session,
            "client.compaction_observed",
            &observation.compaction_id,
        )? {
            if event.payload["observation"] != json!(observation)
                || event.payload["policy"] != json!(policy)
            {
                return Err(Error::SourceConflict(
                    "compaction_id already binds different observations or policy".into(),
                ));
            }
            return Ok((event, true));
        }
        self.runtime_transaction_with_event(project, expected, EventDraft {
            work_item_id: bound.work_item_id, session_id: Some(session), branch_id: bound.branch_id,
            event_type: "client.compaction_observed".into(), importance: "low".into(),
            summary: "Recorded completed native compaction; no session switch performed".into(),
            payload: json!({"observation":observation,"policy":policy}),
        }, |tx, _, event| {
            crate::events::bind_event(tx, project, event, true)?;
            let previous: Option<String> = tx.query_row("SELECT payload_json FROM events WHERE project_id=?1 AND session_id=?2 AND event_type='client.compaction_observed' ORDER BY project_revision DESC LIMIT 1",
                params![project.to_string(), session.to_string()], |r| r.get(0)).optional().map_err(db_error)?;
            if let Some(previous) = previous {
                let v: serde_json::Value = serde_json::from_str(&previous)?;
                let prior: CompactionObservation = serde_json::from_value(v["observation"].clone())?;
                if observation.sequence <= prior.sequence || observation.observed_at < prior.observed_at {
                    return Err(Error::SourceConflict("out-of-order compaction; latest observation was retained".into()));
                }
            }
            Ok(())
        }).map(|(_, event)| (event, false))
    }
    /// Deferral is scoped to one observation, not a permanent permission or policy change.
    pub fn defer_compaction(
        &mut self,
        project: Id,
        expected: Revision,
        session: Id,
        observation: Id,
    ) -> Result<(Event, bool)> {
        let bound = self.session(project, session)?;
        if let Some(event) = self.compaction_deferral(project, session, observation)? {
            return Ok((event, true));
        }
        self.runtime_transaction_with_event(project, expected, EventDraft {
            work_item_id: bound.work_item_id, session_id: Some(session), branch_id: bound.branch_id,
            event_type: "client.compaction_deferred".into(), importance: "low".into(),
            summary: "Deferred session-switch suggestion until the next compaction observation".into(),
            payload: json!({"observation_event_id":observation}),
        }, |tx, _, event| {
            crate::events::bind_event(tx, project, event, true)?;
            let latest: Option<String> = tx.query_row("SELECT id FROM events WHERE project_id=?1 AND session_id=?2 AND event_type='client.compaction_observed' ORDER BY project_revision DESC LIMIT 1",
                params![project.to_string(), session.to_string()], |r| r.get(0)).optional().map_err(db_error)?;
            if latest.as_deref() != Some(observation.to_string().as_str()) {
                return Err(Error::SourceConflict("defer requires the latest observation in this session".into()));
            }
            Ok(())
        }).map(|(_, event)| (event, false))
    }
}
