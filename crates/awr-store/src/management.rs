use crate::{Store, db_error};
use awr_core::*;
use rusqlite::{OptionalExtension, params};
use serde_json::Value;

impl Store {
    pub fn latest_work_event(
        &self,
        project: Id,
        work: Id,
        branch: Option<Id>,
        kind: &str,
    ) -> Result<Option<Event>> {
        let id:Option<String>=self.conn.query_row("SELECT id FROM events WHERE project_id=?1 AND work_item_id=?2 AND branch_id IS ?3 AND event_type=?4 ORDER BY project_revision DESC,created_at DESC,id DESC LIMIT 1",params![project.to_string(),work.to_string(),branch.map(|b|b.to_string()),kind],|r|r.get(0)).optional().map_err(db_error)?;
        id.map(|id| {
            self.event(
                project,
                id.parse()
                    .map_err(|_| Error::Storage("invalid work event identity".into()))?,
            )
        })
        .transpose()
    }
    pub fn management_by_key(&self, project: Id, work: Id, key: &str) -> Result<Option<Event>> {
        let id:Option<String>=self.conn.query_row("SELECT id FROM events WHERE project_id=?1 AND work_item_id=?2 AND event_type='management.assessed' AND json_extract(payload_json,'$.request_key')=?3 ORDER BY project_revision DESC LIMIT 1",params![project.to_string(),work.to_string(),key],|r|r.get(0)).optional().map_err(db_error)?;
        id.map(|id| {
            self.event(
                project,
                id.parse()
                    .map_err(|_| Error::Storage("invalid management event identity".into()))?,
            )
        })
        .transpose()
    }
    /// A typed domain receipt, bound to an active session and the same revision as its assessment.
    pub fn record_management(
        &mut self,
        project: Id,
        expected: Revision,
        session: Id,
        work: Id,
        payload: Value,
    ) -> Result<Event> {
        ensure_public_value(&payload)?;
        if serde_json::to_vec(&payload)?.len() > 64 * 1024 {
            return Err(Error::InvalidInput(
                "management receipt exceeds 64 KiB".into(),
            ));
        }
        self.runtime_transaction_with_event(
            project,
            expected,
            EventDraft {
                work_item_id: Some(work),
                session_id: Some(session),
                branch_id: None,
                event_type: "management.assessed".into(),
                importance: "normal".into(),
                summary: "Assessed management intensity; completion policy unchanged".into(),
                payload,
            },
            |tx, _, event| crate::events::bind_event(tx, project, event, true),
        )
        .map(|(_, event)| event)
    }
}
