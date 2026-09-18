use crate::error::{PgError, PgResult};
use crate::graph::paths_conflict;
use crate::tx::{bind_scope, new_id};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Serialize)]
pub struct ExecutionRecord {
    pub id: String,
    pub work_id: String,
    pub session_id: String,
    pub claim_id: String,
    pub fence: i64,
    pub contract_hash: String,
    pub effect_key: String,
    pub state: String,
    pub cancel_requested: bool,
    pub fencing_class: String,
    pub replayed: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct OutboxDelivery {
    pub outbox_id: String,
    pub execution_id: String,
    pub effect_key: String,
    pub fence: i64,
    pub fencing_class: String,
    pub declared_scope: Value,
    pub payload: Value,
    pub delivery_attempts: i32,
}

pub struct ExecutionStore {
    pool: crate::PgPool,
}

impl ExecutionStore {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            pool: crate::PgPool::new(url),
        }
    }

    async fn connect(&self) -> PgResult<crate::PgClient> {
        self.pool.get().await
    }

    pub async fn prepare(
        &self,
        tenant_id: &str,
        project_id: &str,
        actor_id: &str,
        client_id: &str,
        request_id: &str,
        claim_id: &str,
        executor_actor_id: &str,
        contract_hash: &str,
        input_digest: &str,
        fencing_class: &str,
        declared_scope: &[String],
        writes: &Value,
    ) -> PgResult<ExecutionRecord> {
        validate_fencing_class(fencing_class)?;
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        lock_project(&tx, tenant_id, project_id).await?;
        let request_hash = format!(
            "execution.prepare:{claim_id}:{executor_actor_id}:{contract_hash}:{fencing_class}"
        );
        if let Some((existing_hash, result)) =
            load_operation(&tx, tenant_id, project_id, actor_id, client_id, request_id).await?
        {
            if existing_hash != request_hash {
                return Err(PgError::IdempotencyConflict);
            }
            return replay_execution(&result);
        }
        let claim = tx
            .query_opt(
                "SELECT session_id, work_id, scope_id, actor_id, fence, state
                 FROM awr_team.claims
                 WHERE tenant_id=$1 AND project_id=$2 AND id=$3
                   AND state='active' AND expires_at > clock_timestamp()
                 FOR UPDATE",
                &[&tenant_id, &project_id, &claim_id],
            )
            .await?
            .ok_or(PgError::LeaseExpired)?;
        let session_id: String = claim.get(0);
        let work_id: String = claim.get(1);
        let scope_id: String = claim.get(2);
        let holder: String = claim.get(3);
        let fence: i64 = claim.get(4);
        if holder != actor_id {
            return Err(PgError::Forbidden);
        }
        let blocked: bool = tx
            .query_opt(
                "SELECT recovery_blocked FROM awr_team.work_runtime
                 WHERE tenant_id=$1 AND project_id=$2 AND scope_id=$3 AND work_id=$4",
                &[&tenant_id, &project_id, &scope_id, &work_id],
            )
            .await?
            .map(|row| row.get(0))
            .unwrap_or(false);
        if blocked {
            return Err(PgError::RecoveryBlocked);
        }
        let unknown: i64 = tx
            .query_one(
                "SELECT count(*) FROM awr_team.executions
                 WHERE tenant_id=$1 AND project_id=$2 AND work_id=$3 AND state='unknown'",
                &[&tenant_id, &project_id, &work_id],
            )
            .await?
            .get(0);
        if unknown > 0 {
            return Err(PgError::RecoveryBlocked);
        }
        let execution_id = new_id();
        let effect_key = execution_id.clone();
        let scope_json = json!(declared_scope);
        tx.execute(
            "INSERT INTO awr_team.executions(
                tenant_id, project_id, id, work_id, session_id, claim_id, fence,
                contract_hash, input_digest, executor_actor_id, state, effect_key,
                fencing_class, declared_scope_json, scope_id)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,'prepared',$11,$12,$13,$14)",
            &[
                &tenant_id,
                &project_id,
                &execution_id,
                &work_id,
                &session_id,
                &claim_id,
                &fence,
                &contract_hash,
                &input_digest,
                &executor_actor_id,
                &effect_key,
                &fencing_class,
                &scope_json,
                &scope_id,
            ],
        )
        .await?;
        let payload = json!({
            "execution_id": execution_id,
            "effect_key": effect_key,
            "work_id": work_id,
            "session_id": session_id,
            "claim_id": claim_id,
            "fence": fence,
            "contract_hash": contract_hash,
            "input_digest": input_digest,
            "executor_actor_id": executor_actor_id,
            "fencing_class": fencing_class,
            "declared_scope": declared_scope,
            "writes": writes,
        });
        let outbox_id = new_id();
        tx.execute(
            "INSERT INTO awr_team.outbox(
                tenant_id, project_id, id, state, payload_json, action_kind, aggregate_id)
             VALUES ($1,$2,$3,'pending',$4,'execution.dispatch',$5)",
            &[&tenant_id, &project_id, &outbox_id, &payload, &execution_id],
        )
        .await?;
        let result = json!({
            "id": execution_id,
            "work_id": work_id,
            "session_id": session_id,
            "claim_id": claim_id,
            "fence": fence,
            "contract_hash": contract_hash,
            "effect_key": effect_key,
            "state": "prepared",
            "cancel_requested": false,
            "fencing_class": fencing_class,
        });
        store_operation(
            &tx,
            tenant_id,
            project_id,
            actor_id,
            client_id,
            request_id,
            "execution.prepare",
            &request_hash,
            &result,
        )
        .await?;
        tx.commit().await?;
        replay_execution(&result).map(|mut record| {
            record.replayed = false;
            record
        })
    }

    pub async fn claim_dispatch(
        &self,
        tenant_id: &str,
        project_id: &str,
    ) -> PgResult<Option<OutboxDelivery>> {
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        lock_project(&tx, tenant_id, project_id).await?;
        let row = tx
            .query_opt(
                "UPDATE awr_team.outbox SET
                    state='sending',
                    delivery_attempts = delivery_attempts + 1,
                    delivery_token = $3
                 WHERE id = (
                    SELECT id FROM awr_team.outbox
                    WHERE tenant_id=$1 AND project_id=$2
                      AND action_kind='execution.dispatch'
                      AND state IN ('pending','sending')
                      AND available_at <= clock_timestamp()
                    ORDER BY available_at, id
                    FOR UPDATE SKIP LOCKED
                    LIMIT 1
                 )
                 RETURNING id, aggregate_id, payload_json, delivery_attempts",
                &[&tenant_id, &project_id, &new_id()],
            )
            .await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(None);
        };
        let outbox_id: String = row.get(0);
        let execution_id: String = row.get(1);
        let payload: Value = row.get(2);
        let attempts: i32 = row.get(3);
        tx.execute(
            "UPDATE awr_team.executions SET state='queued'
             WHERE tenant_id=$1 AND project_id=$2 AND id=$3 AND state='prepared'",
            &[&tenant_id, &project_id, &execution_id],
        )
        .await?;
        tx.commit().await?;
        Ok(Some(OutboxDelivery {
            outbox_id,
            execution_id: execution_id.clone(),
            effect_key: payload
                .get("effect_key")
                .and_then(Value::as_str)
                .unwrap_or(&execution_id)
                .to_owned(),
            fence: payload.get("fence").and_then(Value::as_i64).unwrap_or(0),
            fencing_class: payload
                .get("fencing_class")
                .and_then(Value::as_str)
                .unwrap_or("uncontrolled")
                .to_owned(),
            declared_scope: payload
                .get("declared_scope")
                .cloned()
                .unwrap_or_else(|| json!([])),
            payload,
            delivery_attempts: attempts,
        }))
    }

    pub async fn ack_dispatch(
        &self,
        tenant_id: &str,
        project_id: &str,
        outbox_id: &str,
    ) -> PgResult<()> {
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        tx.execute(
            "UPDATE awr_team.outbox SET state='delivered'
             WHERE tenant_id=$1 AND project_id=$2 AND id=$3",
            &[&tenant_id, &project_id, &outbox_id],
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn accept(
        &self,
        tenant_id: &str,
        project_id: &str,
        execution_id: &str,
        fence: i64,
    ) -> PgResult<ExecutionRecord> {
        self.transition(
            tenant_id,
            project_id,
            execution_id,
            fence,
            &["prepared", "queued", "accepted"],
            "accepted",
        )
        .await
    }

    pub async fn start(
        &self,
        tenant_id: &str,
        project_id: &str,
        execution_id: &str,
        fence: i64,
    ) -> PgResult<ExecutionRecord> {
        self.transition(
            tenant_id,
            project_id,
            execution_id,
            fence,
            &["accepted", "running"],
            "running",
        )
        .await
    }

    pub async fn cancel(
        &self,
        tenant_id: &str,
        project_id: &str,
        actor_id: &str,
        client_id: &str,
        request_id: &str,
        execution_id: &str,
    ) -> PgResult<ExecutionRecord> {
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        lock_project(&tx, tenant_id, project_id).await?;
        let request_hash = format!("execution.cancel:{execution_id}");
        if let Some((existing_hash, result)) =
            load_operation(&tx, tenant_id, project_id, actor_id, client_id, request_id).await?
        {
            if existing_hash != request_hash {
                return Err(PgError::IdempotencyConflict);
            }
            return replay_execution(&result);
        }
        let row = tx
            .query_opt(
                "SELECT state, fence, work_id, session_id, claim_id, contract_hash,
                        effect_key, fencing_class, cancel_requested
                 FROM awr_team.executions
                 WHERE tenant_id=$1 AND project_id=$2 AND id=$3
                 FOR UPDATE",
                &[&tenant_id, &project_id, &execution_id],
            )
            .await?
            .ok_or(PgError::ExecutionNotFound)?;
        let state: String = row.get(0);
        let fence: i64 = row.get(1);
        let mut cancel_requested: bool = row.get(8);
        let next_state = if matches!(state.as_str(), "prepared" | "queued") {
            "cancelled"
        } else {
            cancel_requested = true;
            state.as_str()
        };
        tx.execute(
            "UPDATE awr_team.executions
             SET cancel_requested=$4, state=$5
             WHERE tenant_id=$1 AND project_id=$2 AND id=$3",
            &[
                &tenant_id,
                &project_id,
                &execution_id,
                &cancel_requested,
                &next_state,
            ],
        )
        .await?;
        let result = json!({
            "id": execution_id,
            "work_id": row.get::<_, String>(2),
            "session_id": row.get::<_, Option<String>>(3).unwrap_or_default(),
            "claim_id": row.get::<_, Option<String>>(4).unwrap_or_default(),
            "fence": fence,
            "contract_hash": row.get::<_, String>(5),
            "effect_key": row.get::<_, Option<String>>(6).unwrap_or_default(),
            "state": next_state,
            "cancel_requested": cancel_requested,
            "fencing_class": row.get::<_, String>(7),
        });
        store_operation(
            &tx,
            tenant_id,
            project_id,
            actor_id,
            client_id,
            request_id,
            "execution.cancel",
            &request_hash,
            &result,
        )
        .await?;
        tx.commit().await?;
        replay_execution(&result).map(|mut record| {
            record.replayed = false;
            record
        })
    }

    pub async fn report(
        &self,
        tenant_id: &str,
        project_id: &str,
        actor_id: &str,
        receipt_kind: &str,
        execution_id: &str,
        outcome: &str,
        payload: Value,
        observed_paths: &[String],
    ) -> PgResult<ExecutionRecord> {
        if !matches!(
            receipt_kind,
            "caller_asserted" | "trusted_executor" | "reconcile"
        ) {
            return Err(PgError::Protocol("invalid receipt kind".into()));
        }
        if !matches!(outcome, "succeeded" | "failed" | "unknown" | "cancelled") {
            return Err(PgError::Protocol("invalid outcome".into()));
        }
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        lock_project(&tx, tenant_id, project_id).await?;
        let actor_kind: String = tx
            .query_opt(
                "SELECT kind FROM awr_team.actors WHERE tenant_id=$1 AND id=$2",
                &[&tenant_id, &actor_id],
            )
            .await?
            .map(|row| row.get(0))
            .ok_or(PgError::Forbidden)?;
        if receipt_kind == "trusted_executor" && actor_kind != "system" {
            return Err(PgError::Forbidden);
        }
        let row = tx
            .query_opt(
                "SELECT state, fence, work_id, session_id, claim_id, contract_hash,
                        effect_key, fencing_class, cancel_requested, declared_scope_json,
                        scope_id
                 FROM awr_team.executions
                 WHERE tenant_id=$1 AND project_id=$2 AND id=$3
                 FOR UPDATE",
                &[&tenant_id, &project_id, &execution_id],
            )
            .await?
            .ok_or(PgError::ExecutionNotFound)?;
        let current: String = row.get(0);
        let _fence: i64 = row.get(1);
        let work_id: String = row.get(2);
        let scope_id: String = row.get(10);
        let declared: Value = row.get(9);
        let cancel_requested: bool = row.get(8);
        let fencing_class: String = row.get(7);
        let _exactly_once = exactly_once_supported(&fencing_class);
        if matches!(current.as_str(), "succeeded" | "failed" | "cancelled")
            && outcome == "cancelled"
        {
            insert_receipt(
                &tx,
                tenant_id,
                project_id,
                execution_id,
                actor_id,
                receipt_kind,
                &payload,
            )
            .await?;
            tx.execute(
                "UPDATE awr_team.executions SET cancel_requested=TRUE
                 WHERE tenant_id=$1 AND project_id=$2 AND id=$3",
                &[&tenant_id, &project_id, &execution_id],
            )
            .await?;
            tx.commit().await?;
            return self.get(tenant_id, project_id, execution_id).await;
        }
        let mut next = outcome.to_owned();
        if outcome == "cancelled" && !matches!(current.as_str(), "prepared" | "queued") {
            next = current.clone();
        }
        if outcome == "succeeded" && scope_exceeded(&declared, observed_paths) {
            insert_receipt(
                &tx,
                tenant_id,
                project_id,
                execution_id,
                actor_id,
                receipt_kind,
                &json!({"scope_violation": true, "observed_paths": observed_paths, "payload": payload}),
            )
            .await?;
            tx.execute(
                "UPDATE awr_team.executions
                 SET state='failed', observed_paths_json=$4
                 WHERE tenant_id=$1 AND project_id=$2 AND id=$3",
                &[
                    &tenant_id,
                    &project_id,
                    &execution_id,
                    &json!(observed_paths),
                ],
            )
            .await?;
            tx.commit().await?;
            return Err(PgError::ScopeExceeded);
        }
        insert_receipt(
            &tx,
            tenant_id,
            project_id,
            execution_id,
            actor_id,
            receipt_kind,
            &payload,
        )
        .await?;
        let unknown_reason = payload
            .get("unknown_reason")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let environment_digest = payload
            .get("environment_digest")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let result_digest = payload
            .get("output_digest")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let cancel_flag = cancel_requested || outcome == "cancelled";
        tx.execute(
            "UPDATE awr_team.executions
             SET state=$4,
                 cancel_requested=$5,
                 observed_paths_json=$6,
                 environment_digest=$7,
                 unknown_reason=$8,
                 result_digest=$9
             WHERE tenant_id=$1 AND project_id=$2 AND id=$3",
            &[
                &tenant_id,
                &project_id,
                &execution_id,
                &next,
                &cancel_flag,
                &json!(observed_paths),
                &environment_digest,
                &unknown_reason,
                &result_digest,
            ],
        )
        .await?;
        if next == "unknown" {
            tx.execute(
                "UPDATE awr_team.work_runtime SET recovery_blocked=TRUE
                 WHERE tenant_id=$1 AND project_id=$2 AND scope_id=$3 AND work_id=$4",
                &[&tenant_id, &project_id, &scope_id, &work_id],
            )
            .await?;
            tx.execute(
                "UPDATE awr_team.resource_reservations SET state='unknown'
                 WHERE tenant_id=$1 AND project_id=$2 AND work_id=$3 AND state='reserved'",
                &[&tenant_id, &project_id, &work_id],
            )
            .await?;
        }
        tx.commit().await?;
        let _ = _exactly_once;
        self.get(tenant_id, project_id, execution_id).await
    }

    pub async fn reconcile(
        &self,
        tenant_id: &str,
        project_id: &str,
        actor_id: &str,
        execution_id: &str,
        terminal_state: &str,
        payload: Value,
        clear_block: bool,
    ) -> PgResult<ExecutionRecord> {
        if !matches!(
            terminal_state,
            "succeeded" | "failed" | "cancelled" | "unknown"
        ) {
            return Err(PgError::Protocol("invalid reconcile state".into()));
        }
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        lock_project(&tx, tenant_id, project_id).await?;
        let actor_kind: String = tx
            .query_opt(
                "SELECT kind FROM awr_team.actors WHERE tenant_id=$1 AND id=$2",
                &[&tenant_id, &actor_id],
            )
            .await?
            .map(|row| row.get(0))
            .ok_or(PgError::Forbidden)?;
        if actor_kind != "system" {
            return Err(PgError::Forbidden);
        }
        let row = tx
            .query_opt(
                "SELECT work_id, scope_id, state FROM awr_team.executions
                 WHERE tenant_id=$1 AND project_id=$2 AND id=$3
                 FOR UPDATE",
                &[&tenant_id, &project_id, &execution_id],
            )
            .await?
            .ok_or(PgError::ExecutionNotFound)?;
        let work_id: String = row.get(0);
        let scope_id: String = row.get(1);
        insert_receipt(
            &tx,
            tenant_id,
            project_id,
            execution_id,
            actor_id,
            "reconcile",
            &payload,
        )
        .await?;
        tx.execute(
            "UPDATE awr_team.executions SET state=$4, unknown_reason=NULL
             WHERE tenant_id=$1 AND project_id=$2 AND id=$3",
            &[&tenant_id, &project_id, &execution_id, &terminal_state],
        )
        .await?;
        if clear_block && terminal_state != "unknown" {
            tx.execute(
                "UPDATE awr_team.work_runtime SET recovery_blocked=FALSE
                 WHERE tenant_id=$1 AND project_id=$2 AND scope_id=$3 AND work_id=$4",
                &[&tenant_id, &project_id, &scope_id, &work_id],
            )
            .await?;
            tx.execute(
                "UPDATE awr_team.resource_reservations SET state='released'
                 WHERE tenant_id=$1 AND project_id=$2 AND work_id=$3 AND state='unknown'",
                &[&tenant_id, &project_id, &work_id],
            )
            .await?;
        }
        tx.commit().await?;
        self.get(tenant_id, project_id, execution_id).await
    }

    pub async fn get(
        &self,
        tenant_id: &str,
        project_id: &str,
        execution_id: &str,
    ) -> PgResult<ExecutionRecord> {
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        let row = tx
            .query_opt(
                "SELECT id, work_id, session_id, claim_id, fence, contract_hash, effect_key,
                        state, cancel_requested, fencing_class
                 FROM awr_team.executions
                 WHERE tenant_id=$1 AND project_id=$2 AND id=$3",
                &[&tenant_id, &project_id, &execution_id],
            )
            .await?
            .ok_or(PgError::ExecutionNotFound)?;
        tx.commit().await?;
        Ok(ExecutionRecord {
            id: row.get(0),
            work_id: row.get(1),
            session_id: row.get::<_, Option<String>>(2).unwrap_or_default(),
            claim_id: row.get::<_, Option<String>>(3).unwrap_or_default(),
            fence: row.get(4),
            contract_hash: row.get(5),
            effect_key: row.get::<_, Option<String>>(6).unwrap_or_default(),
            state: row.get(7),
            cancel_requested: row.get(8),
            fencing_class: row.get(9),
            replayed: false,
        })
    }

    async fn transition(
        &self,
        tenant_id: &str,
        project_id: &str,
        execution_id: &str,
        fence: i64,
        allowed: &[&str],
        next: &str,
    ) -> PgResult<ExecutionRecord> {
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        lock_project(&tx, tenant_id, project_id).await?;
        let row = tx
            .query_opt(
                "SELECT state, fence FROM awr_team.executions
                 WHERE tenant_id=$1 AND project_id=$2 AND id=$3
                 FOR UPDATE",
                &[&tenant_id, &project_id, &execution_id],
            )
            .await?
            .ok_or(PgError::ExecutionNotFound)?;
        let state: String = row.get(0);
        let current_fence: i64 = row.get(1);
        if current_fence != fence {
            return Err(PgError::StaleFence);
        }
        if !allowed.contains(&state.as_str()) {
            if state == next {
                tx.commit().await?;
                return self.get(tenant_id, project_id, execution_id).await;
            }
            return Err(PgError::Protocol(format!(
                "cannot move execution from {state} to {next}"
            )));
        }
        tx.execute(
            "UPDATE awr_team.executions SET state=$4
             WHERE tenant_id=$1 AND project_id=$2 AND id=$3",
            &[&tenant_id, &project_id, &execution_id, &next],
        )
        .await?;
        tx.commit().await?;
        self.get(tenant_id, project_id, execution_id).await
    }
}

pub fn exactly_once_supported(fencing_class: &str) -> bool {
    matches!(fencing_class, "hard_fence" | "queryable_idempotent")
}

fn validate_fencing_class(class: &str) -> PgResult<()> {
    match class {
        "hard_fence" | "queryable_idempotent" | "uncontrolled" => Ok(()),
        _ => Err(PgError::Protocol("invalid fencing class".into())),
    }
}

fn scope_exceeded(declared: &Value, observed: &[String]) -> bool {
    let declared: Vec<String> = declared
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(ToOwned::to_owned)
        .collect();
    if declared.is_empty() {
        return !observed.is_empty();
    }
    observed.iter().any(|path| {
        !declared.iter().any(|item| {
            paths_conflict("file", path, "file", item)
                || paths_conflict("file", path, "prefix", item)
        })
    })
}

async fn insert_receipt(
    tx: &tokio_postgres::Transaction<'_>,
    tenant_id: &str,
    project_id: &str,
    execution_id: &str,
    actor_id: &str,
    kind: &str,
    payload: &Value,
) -> PgResult<()> {
    let digest = format!("{:x}", Sha256::digest(payload.to_string().as_bytes()));
    tx.execute(
        "INSERT INTO awr_team.execution_receipts(
            tenant_id, project_id, id, execution_id, reporter_actor_id, receipt_kind,
            digest, payload_json)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
        &[
            &tenant_id,
            &project_id,
            &new_id(),
            &execution_id,
            &actor_id,
            &kind,
            &digest,
            payload,
        ],
    )
    .await?;
    Ok(())
}

async fn lock_project(
    tx: &tokio_postgres::Transaction<'_>,
    tenant_id: &str,
    project_id: &str,
) -> PgResult<()> {
    tx.query_opt(
        "SELECT id FROM awr_team.projects WHERE tenant_id=$1 AND id=$2 FOR UPDATE",
        &[&tenant_id, &project_id],
    )
    .await?
    .ok_or(PgError::ProjectNotAvailable)?;
    Ok(())
}

async fn load_operation(
    tx: &tokio_postgres::Transaction<'_>,
    tenant_id: &str,
    project_id: &str,
    actor_id: &str,
    client_id: &str,
    request_id: &str,
) -> PgResult<Option<(String, Value)>> {
    let row = tx
        .query_opt(
            "SELECT request_hash, result_json FROM awr_team.operations
             WHERE tenant_id=$1 AND project_id=$2 AND actor_id=$3 AND client_id=$4 AND request_id=$5",
            &[&tenant_id, &project_id, &actor_id, &client_id, &request_id],
        )
        .await?;
    Ok(row.map(|row| (row.get(0), row.get(1))))
}

async fn store_operation(
    tx: &tokio_postgres::Transaction<'_>,
    tenant_id: &str,
    project_id: &str,
    actor_id: &str,
    client_id: &str,
    request_id: &str,
    op: &str,
    request_hash: &str,
    result: &Value,
) -> PgResult<()> {
    tx.execute(
        "INSERT INTO awr_team.operations(
            tenant_id, project_id, id, actor_id, client_id, request_id, op,
            request_hash, state, result_json)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,'committed',$9)",
        &[
            &tenant_id,
            &project_id,
            &new_id(),
            &actor_id,
            &client_id,
            &request_id,
            &op,
            &request_hash,
            result,
        ],
    )
    .await?;
    Ok(())
}

fn replay_execution(result: &Value) -> PgResult<ExecutionRecord> {
    Ok(ExecutionRecord {
        id: result["id"].as_str().unwrap_or_default().into(),
        work_id: result["work_id"].as_str().unwrap_or_default().into(),
        session_id: result["session_id"].as_str().unwrap_or_default().into(),
        claim_id: result["claim_id"].as_str().unwrap_or_default().into(),
        fence: result["fence"].as_i64().unwrap_or(0),
        contract_hash: result["contract_hash"].as_str().unwrap_or_default().into(),
        effect_key: result["effect_key"].as_str().unwrap_or_default().into(),
        state: result["state"].as_str().unwrap_or_default().into(),
        cancel_requested: result["cancel_requested"].as_bool().unwrap_or(false),
        fencing_class: result["fencing_class"].as_str().unwrap_or_default().into(),
        replayed: true,
    })
}
