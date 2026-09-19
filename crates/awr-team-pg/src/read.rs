use crate::error::{PgError, PgResult};
use crate::tx::{bind_scope, new_id};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_postgres::IsolationLevel;

pub const CURSOR_PROTOCOL: &str = "awr-team-cursor-v1";
pub const CAPABILITIES_PROTOCOL: &str = "awr-team";
pub const CAPABILITIES_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventCursor {
    pub protocol: String,
    pub coordinator_epoch: String,
    pub project_revision: i64,
    pub event_index: i32,
}

impl EventCursor {
    pub fn encode(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.protocol, self.coordinator_epoch, self.project_revision, self.event_index
        )
    }

    pub fn decode(raw: &str) -> PgResult<Self> {
        let parts: Vec<&str> = raw.split(':').collect();
        if parts.len() != 4 || parts[0] != CURSOR_PROTOCOL {
            return Err(PgError::CursorExpired);
        }
        let project_revision = parts[2]
            .parse::<i64>()
            .map_err(|_| PgError::CursorExpired)?;
        let event_index = parts[3]
            .parse::<i32>()
            .map_err(|_| PgError::CursorExpired)?;
        Ok(Self {
            protocol: parts[0].into(),
            coordinator_epoch: parts[1].into(),
            project_revision,
            event_index,
        })
    }

    pub fn origin(coordinator_epoch: impl Into<String>) -> Self {
        Self {
            protocol: CURSOR_PROTOCOL.into(),
            coordinator_epoch: coordinator_epoch.into(),
            project_revision: 0,
            event_index: -1,
        }
    }
}

pub fn capabilities() -> Value {
    json!({
        "protocol": CAPABILITIES_PROTOCOL,
        "protocol_version": CAPABILITIES_VERSION,
        "queries": [
            "capabilities",
            "work.prepare",
            "work.graph",
            "session.inspect",
            "events.list"
        ],
        "commands": [
            "work.touch",
            "work.propose_split",
            "source.ingest",
            "source.approve",
            "source.activate",
            "session.start",
            "claim.acquire",
            "claim.renew",
            "claim.release",
            "claim.handoff"
        ],
        "isolation": "repeatable_read",
        "cursor_protocol": CURSOR_PROTOCOL
    })
}

pub fn dispatch_query(op: &str) -> PgResult<Value> {
    match op {
        "capabilities" => Ok(capabilities()),
        "work.prepare" | "work.graph" | "session.inspect" | "events.list" => Err(
            PgError::Protocol("query requires a project-bound TeamStore".into()),
        ),
        other => Err(PgError::Unsupported(other.into())),
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct PreparedWork {
    pub coordinator_epoch: String,
    pub authority_epoch: String,
    pub authority_snapshot_id: String,
    /// V1 serves only the main scope; the identity is explicit so contract,
    /// runtime version and response cannot silently mix scopes (CR #38 P2-1).
    pub scope_id: String,
    pub work_id: String,
    pub work_version: String,
    pub contract_hash: String,
    pub goals: Vec<String>,
    pub scope_paths: Vec<String>,
    pub acceptance: Vec<String>,
    pub required_dependencies: Vec<String>,
    pub completion_policy: String,
    pub verification_requirements: Vec<String>,
    pub hard_rules: Vec<String>,
    pub completeness: String,
    pub completeness_reasons: Vec<String>,
    pub context_hash: String,
    pub snapshot_cursor: String,
    /// All required contract content, flattened for prompt assembly.
    /// Completeness is computed over THIS set, not hard_rules alone
    /// (CR #38 P2-2).
    pub required_context: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WorkGraph {
    pub snapshot_id: String,
    pub edges: Vec<Value>,
}

#[derive(Clone, Debug, Serialize)]
pub struct EventRecord {
    /// Decimal string at the response boundary; a JSON number would lose
    /// precision past 2^53 for JavaScript consumers (CR #38 P2-3).
    pub project_revision: String,
    pub event_index: i32,
    pub event_type: String,
    pub id: String,
    pub payload: Value,
    pub cursor: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct EventPage {
    pub events: Vec<EventRecord>,
    pub next_cursor: String,
    pub exhausted: bool,
}

pub struct ReadStore {
    pool: crate::PgPool,
}

impl ReadStore {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            pool: crate::PgPool::new(url),
        }
    }

    /// Build from a validated `tokio_postgres::Config` (see PgPool::from_config).
    pub fn from_config(config: tokio_postgres::Config) -> Self {
        Self {
            pool: crate::PgPool::from_config(config),
        }
    }

    async fn connect(&self) -> PgResult<crate::PgClient> {
        self.pool.get().await
    }

    pub async fn prepare(
        &self,
        tenant_id: &str,
        project_id: &str,
        work_id: &str,
        max_context_bytes: Option<usize>,
    ) -> PgResult<PreparedWork> {
        self.prepare_inner(tenant_id, project_id, work_id, max_context_bytes, None)
            .await
    }

    /// Test-only probe (pg-tests): pauses at a deterministic point between
    /// the contract read and the runtime/event reads, letting a concurrent
    /// writer commit in between (CR #38 P3).
    #[cfg(feature = "pg-tests")]
    #[doc(hidden)]
    pub async fn prepare_with_sync_point(
        &self,
        tenant_id: &str,
        project_id: &str,
        work_id: &str,
        max_context_bytes: Option<usize>,
        sync: &tokio::sync::Barrier,
    ) -> PgResult<PreparedWork> {
        self.prepare_inner(
            tenant_id,
            project_id,
            work_id,
            max_context_bytes,
            Some(sync),
        )
        .await
    }

    async fn prepare_inner(
        &self,
        tenant_id: &str,
        project_id: &str,
        work_id: &str,
        max_context_bytes: Option<usize>,
        sync: Option<&tokio::sync::Barrier>,
    ) -> PgResult<PreparedWork> {
        // V1 serves only the main scope; bind every read to it explicitly.
        const SCOPE: &str = "main";
        let mut client = self.connect().await?;
        let tx = client
            .build_transaction()
            .isolation_level(IsolationLevel::RepeatableRead)
            .start()
            .await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        let project = tx
            .query_opt(
                "SELECT coordinator_epoch, authority_epoch, active_snapshot_id, project_revision
                 FROM awr_team.projects
                 WHERE tenant_id=$1 AND id=$2",
                &[&tenant_id, &project_id],
            )
            .await?
            .ok_or(PgError::ProjectNotAvailable)?;
        let coordinator_epoch: String = project.get(0);
        let authority_epoch: i64 = project.get(1);
        let snapshot_id: Option<String> = project.get(2);
        let project_revision: i64 = project.get(3);
        let snapshot_id = snapshot_id.ok_or(PgError::InactiveCandidate)?;
        let contract = tx
            .query_opt(
                "SELECT contract_hash, contract_json FROM awr_team.work_contracts
                 WHERE tenant_id=$1 AND project_id=$2 AND snapshot_id=$3 AND work_id=$4
                   AND scope_id='main'",
                &[&tenant_id, &project_id, &snapshot_id, &work_id],
            )
            .await?
            .ok_or(PgError::InactiveCandidate)?;
        let contract_hash: String = contract.get(0);
        let contract_json: Value = contract.get(1);
        // Never emit a stored hash that does not recompute from its content:
        // records written by older builds (e.g. splits that stored the raw
        // child id as the hash) must surface as an integrity error instead
        // of passing as a valid identity (CR #57 P2-2).
        let parsed_contract: awr_team::WorkContract = serde_json::from_value(contract_json.clone())
            .map_err(|e| PgError::Protocol(format!("stored contract is not parseable: {e}")))?;
        let recomputed = parsed_contract
            .hash()
            .map_err(|e| PgError::Protocol(e.to_string()))?;
        if recomputed != contract_hash {
            return Err(PgError::Protocol(format!(
                "stored contract hash does not match its content (legacy or corrupt record; republish or repair): {}",
                parsed_contract.work_id.as_str()
            )));
        }
        let text_list = |key: &str| {
            contract_json
                .get(key)
                .and_then(Value::as_array)
                .map(|rows| {
                    rows.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        let hard_rules = text_list("hard_rules");
        let goals = text_list("goals");
        let scope_paths = text_list("scope_paths");
        let acceptance = text_list("acceptance");
        let required_dependencies = text_list("required_dependencies");
        let verification_requirements = text_list("verification_requirements");
        let completion_policy = contract_json
            .get("completion_policy")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // Deterministic probe point (tests only): a concurrent writer may
        // commit between the contract read and the runtime/event reads.
        if let Some(barrier) = sync {
            barrier.wait().await;
            barrier.wait().await;
        }
        let work_version = tx
            .query_opt(
                "SELECT work_version FROM awr_team.work_runtime
                 WHERE tenant_id=$1 AND project_id=$2 AND work_id=$3 AND scope_id='main'",
                &[&tenant_id, &project_id, &work_id],
            )
            .await?
            .map(|row| {
                let version: i64 = row.get(0);
                version.to_string()
            })
            .unwrap_or_else(|| "0".into());
        let boundary = tx
            .query_one(
                "SELECT COALESCE(MAX(project_revision), 0)::bigint,
                        COALESCE(MAX(event_index) FILTER (
                            WHERE project_revision = (
                                SELECT COALESCE(MAX(project_revision), 0)
                                FROM awr_team.events
                                WHERE tenant_id=$1 AND project_id=$2
                            )
                        ), -1)::int
                 FROM awr_team.events
                 WHERE tenant_id=$1 AND project_id=$2",
                &[&tenant_id, &project_id],
            )
            .await?;
        let boundary_revision: i64 = boundary.get(0);
        let boundary_index: i32 = boundary.get(1);
        tx.commit().await?;
        drop(client);

        let mut required_context = Vec::new();
        required_context.extend(goals.iter().map(|s| format!("goal: {s}")));
        required_context.extend(scope_paths.iter().map(|s| format!("scope: {s}")));
        required_context.extend(acceptance.iter().map(|s| format!("acceptance: {s}")));
        required_context.extend(
            required_dependencies
                .iter()
                .map(|s| format!("dependency: {s}")),
        );
        if !completion_policy.is_empty() {
            required_context.push(format!("completion_policy: {completion_policy}"));
        }
        required_context.extend(
            verification_requirements
                .iter()
                .map(|s| format!("verification: {s}")),
        );
        required_context.extend(hard_rules.iter().map(|s| format!("rule: {s}")));
        let mut reasons = Vec::new();
        if hard_rules.is_empty() {
            reasons.push("missing_hard_rules".into());
        }
        if goals.is_empty() {
            reasons.push("missing_goals".into());
        }
        if acceptance.is_empty() {
            reasons.push("missing_acceptance".into());
        }
        if completion_policy.is_empty() {
            reasons.push("missing_completion_policy".into());
        }
        let required_bytes = required_context.iter().map(|r| r.len()).sum::<usize>();
        if let Some(max) = max_context_bytes {
            if required_bytes > max {
                reasons.push("required_content_exceeds_budget".into());
            }
        }
        let completeness = if reasons.is_empty() {
            "complete"
        } else {
            "incomplete"
        };
        let snapshot = json!({
            "coordinator_epoch": coordinator_epoch,
            "authority_epoch": authority_epoch.to_string(),
            "authority_snapshot_id": snapshot_id,
            "scope_id": SCOPE,
            "work_id": work_id,
            "work_version": work_version,
            "contract_hash": contract_hash,
            "required_context": required_context,
            "hard_rules": hard_rules,
            "event_boundary": {"project_revision": boundary_revision.to_string(), "event_index": boundary_index},
        });
        let context_hash =
            awr_team::request_hash(&snapshot).map_err(|e| PgError::Protocol(e.to_string()))?;
        let cursor = EventCursor {
            protocol: CURSOR_PROTOCOL.into(),
            coordinator_epoch: coordinator_epoch.clone(),
            project_revision: if project_revision == 0 {
                0
            } else {
                boundary_revision
            },
            event_index: boundary_index,
        };
        Ok(PreparedWork {
            coordinator_epoch,
            authority_epoch: authority_epoch.to_string(),
            authority_snapshot_id: snapshot_id,
            scope_id: SCOPE.into(),
            work_id: work_id.into(),
            work_version,
            contract_hash,
            goals,
            scope_paths,
            acceptance,
            required_dependencies,
            completion_policy,
            verification_requirements,
            required_context,
            hard_rules,
            completeness: completeness.into(),
            completeness_reasons: reasons,
            context_hash,
            snapshot_cursor: cursor.encode(),
        })
    }

    pub async fn graph(&self, tenant_id: &str, project_id: &str) -> PgResult<WorkGraph> {
        let mut client = self.connect().await?;
        let tx = client
            .build_transaction()
            .isolation_level(IsolationLevel::RepeatableRead)
            .start()
            .await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        let snapshot_id: Option<String> = tx
            .query_opt(
                "SELECT active_snapshot_id FROM awr_team.projects
                 WHERE tenant_id=$1 AND id=$2",
                &[&tenant_id, &project_id],
            )
            .await?
            .ok_or(PgError::ProjectNotAvailable)?
            .get(0);
        let snapshot_id = snapshot_id.ok_or(PgError::InactiveCandidate)?;
        let rows = tx
            .query(
                "SELECT from_work_id, to_work_id, relation, required
                 FROM awr_team.dependency_edges
                 WHERE tenant_id=$1 AND project_id=$2 AND snapshot_id=$3
                 ORDER BY from_work_id, to_work_id, relation",
                &[&tenant_id, &project_id, &snapshot_id],
            )
            .await?;
        tx.commit().await?;
        let edges = rows
            .into_iter()
            .map(|row| {
                json!({
                    "from": row.get::<_, String>(0),
                    "to": row.get::<_, String>(1),
                    "relation": row.get::<_, String>(2),
                    "required": row.get::<_, bool>(3),
                })
            })
            .collect();
        Ok(WorkGraph { snapshot_id, edges })
    }

    pub async fn inspect_session(
        &self,
        tenant_id: &str,
        project_id: &str,
        session_id: &str,
    ) -> PgResult<Value> {
        let mut client = self.connect().await?;
        let tx = client
            .build_transaction()
            .isolation_level(IsolationLevel::RepeatableRead)
            .start()
            .await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        let found = tx
            .query_opt(
                "SELECT id, state FROM awr_team.sessions
                 WHERE tenant_id=$1 AND project_id=$2 AND id=$3",
                &[&tenant_id, &project_id, &session_id],
            )
            .await?;
        tx.commit().await?;
        match found {
            Some(row) => Ok(json!({
                "id": row.get::<_, String>(0),
                "state": row.get::<_, String>(1),
            })),
            None => Err(PgError::SessionNotFound),
        }
    }

    pub async fn list_events(
        &self,
        tenant_id: &str,
        project_id: &str,
        after: Option<&str>,
        limit: i64,
    ) -> PgResult<EventPage> {
        if limit <= 0 {
            return Err(PgError::Protocol("page size must be positive".into()));
        }
        let mut client = self.connect().await?;
        let tx = client
            .build_transaction()
            .isolation_level(IsolationLevel::RepeatableRead)
            .start()
            .await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        let epoch: String = tx
            .query_opt(
                "SELECT coordinator_epoch FROM awr_team.projects
                 WHERE tenant_id=$1 AND id=$2",
                &[&tenant_id, &project_id],
            )
            .await?
            .ok_or(PgError::ProjectNotAvailable)?
            .get(0);
        let cursor = match after {
            Some(raw) => EventCursor::decode(raw)?,
            None => EventCursor::origin(&epoch),
        };
        if cursor.coordinator_epoch != epoch {
            return Err(PgError::EpochChanged);
        }
        let rows = tx
            .query(
                "SELECT id, project_revision, event_index, event_type, payload_json
                 FROM awr_team.events
                 WHERE tenant_id=$1 AND project_id=$2
                   AND (project_revision, event_index) > ($3, $4)
                 ORDER BY project_revision, event_index
                 LIMIT $5",
                &[
                    &tenant_id,
                    &project_id,
                    &cursor.project_revision,
                    &cursor.event_index,
                    &limit,
                ],
            )
            .await?;
        tx.commit().await?;
        let events: Vec<EventRecord> = rows
            .into_iter()
            .map(|row| {
                let revision: i64 = row.get(1);
                let index: i32 = row.get(2);
                let encoded = EventCursor {
                    protocol: CURSOR_PROTOCOL.into(),
                    coordinator_epoch: epoch.clone(),
                    project_revision: revision,
                    event_index: index,
                }
                .encode();
                EventRecord {
                    id: row.get(0),
                    project_revision: revision.to_string(),
                    event_index: index,
                    event_type: row.get(3),
                    payload: row.get(4),
                    cursor: encoded,
                }
            })
            .collect();
        let exhausted = (events.len() as i64) < limit;
        let next_cursor = events
            .last()
            .map(|event| event.cursor.clone())
            .unwrap_or_else(|| cursor.encode());
        Ok(EventPage {
            events,
            next_cursor,
            exhausted,
        })
    }
}

impl crate::tx::TeamStore {
    pub async fn emit_revision_events(
        &self,
        tenant_id: &str,
        project_id: &str,
        actor_id: &str,
        events: Vec<(String, Value)>,
    ) -> PgResult<i64> {
        if events.is_empty() {
            return Err(PgError::Protocol("at least one event required".into()));
        }
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        let locked = tx
            .query_opt(
                "SELECT project_revision FROM awr_team.projects
                 WHERE tenant_id=$1 AND id=$2 FOR UPDATE",
                &[&tenant_id, &project_id],
            )
            .await?
            .ok_or(PgError::ProjectNotAvailable)?;
        let revision: i64 = locked.get(0);
        let next = revision + 1;
        tx.execute(
            "UPDATE awr_team.projects SET project_revision=$1
             WHERE tenant_id=$2 AND id=$3 AND project_revision=$4",
            &[&next, &tenant_id, &project_id, &revision],
        )
        .await?;
        for (index, (event_type, payload)) in events.iter().enumerate() {
            let event_id = new_id();
            let event_index = index as i32;
            tx.execute(
                "INSERT INTO awr_team.events(
                    tenant_id, project_id, id, project_revision, event_index,
                    event_type, actor_id, work_id, payload_json)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,NULL,$8)",
                &[
                    &tenant_id,
                    &project_id,
                    &event_id,
                    &next,
                    &event_index,
                    event_type,
                    &actor_id,
                    payload,
                ],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_roundtrip_and_rejects_foreign_protocol() {
        let cursor = EventCursor {
            protocol: CURSOR_PROTOCOL.into(),
            coordinator_epoch: "epoch-1".into(),
            project_revision: 4,
            event_index: 2,
        };
        let encoded = cursor.encode();
        assert_eq!(EventCursor::decode(&encoded).unwrap(), cursor);
        assert!(EventCursor::decode("seq:1").is_err());
    }

    #[test]
    fn capabilities_list_supported_queries() {
        let value = capabilities();
        assert_eq!(value["protocol"], CAPABILITIES_PROTOCOL);
        assert_eq!(value["protocol_version"], CAPABILITIES_VERSION);
        assert!(dispatch_query("nope").is_err());
        assert!(dispatch_query("capabilities").is_ok());
    }
}
