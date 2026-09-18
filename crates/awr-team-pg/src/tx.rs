use crate::error::{PgError, PgResult};
use serde_json::{Value, json};

pub struct TeamStore {
    pool: crate::PgPool,
}

#[derive(Clone, Debug)]
pub struct CommandRequest {
    pub tenant_id: String,
    pub project_id: String,
    pub actor_id: String,
    pub client_id: String,
    pub request_id: String,
    pub op: String,
    pub args: Value,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct CommandOutcome {
    pub replayed: bool,
    pub committed_project_revision: String,
    pub result: Value,
}

impl TeamStore {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            pool: crate::PgPool::new(url),
        }
    }

    pub(crate) async fn connect(&self) -> PgResult<crate::PgClient> {
        self.pool.get().await
    }

    pub async fn execute(&self, request: CommandRequest) -> PgResult<CommandOutcome> {
        let mut client = self.connect().await?;
        let request_hash = hash_request(&request)?;
        let tx = client.transaction().await?;
        bind_scope(&tx, &request.tenant_id, &request.project_id).await?;
        let locked = tx
            .query_opt(
                "SELECT project_revision FROM awr_team.projects
                 WHERE tenant_id=$1 AND id=$2 FOR UPDATE",
                &[&request.tenant_id, &request.project_id],
            )
            .await?;
        let Some(row) = locked else {
            return Err(PgError::ProjectNotAvailable);
        };
        let revision: i64 = row.get(0);
        if let Some(existing) = load_operation(&tx, &request).await? {
            if existing.0 != request_hash {
                return Err(PgError::IdempotencyConflict);
            }
            return Ok(CommandOutcome {
                replayed: true,
                committed_project_revision: existing.1.to_string(),
                result: existing.2,
            });
        }
        let next = revision + 1;
        let result = apply_op(&tx, &request, next).await?;
        tx.execute(
            "UPDATE awr_team.projects SET project_revision=$1
             WHERE tenant_id=$2 AND id=$3 AND project_revision=$4",
            &[&next, &request.tenant_id, &request.project_id, &revision],
        )
        .await?;
        let event_id = new_id();
        let op_id = new_id();
        let event_type = format!("command.{}", request.op);
        let work_id: Option<String> = request
            .args
            .get("work_id")
            .and_then(|v| v.as_str().map(str::to_owned));
        tx.execute(
            "INSERT INTO awr_team.events(
                tenant_id, project_id, id, project_revision, event_index,
                event_type, actor_id, work_id, payload_json)
             VALUES ($1,$2,$3,$4,0,$5,$6,$7,$8)",
            &[
                &request.tenant_id,
                &request.project_id,
                &event_id,
                &next,
                &event_type,
                &request.actor_id,
                &work_id,
                &result,
            ],
        )
        .await?;
        tx.execute(
            "INSERT INTO awr_team.operations(
                tenant_id, project_id, id, actor_id, client_id, request_id, op,
                request_hash, state, committed_project_revision, result_json)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,'committed',$9,$10)",
            &[
                &request.tenant_id,
                &request.project_id,
                &op_id,
                &request.actor_id,
                &request.client_id,
                &request.request_id,
                &request.op,
                &request_hash,
                &next,
                &result,
            ],
        )
        .await?;
        tx.commit().await?;
        Ok(CommandOutcome {
            replayed: false,
            committed_project_revision: next.to_string(),
            result,
        })
    }

    pub async fn abort_after_partial_write(&self, request: CommandRequest) -> PgResult<()> {
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, &request.tenant_id, &request.project_id).await?;
        let event_id = new_id();
        let payload = json!({});
        tx.execute(
            "INSERT INTO awr_team.events(
                tenant_id, project_id, id, project_revision, event_index,
                event_type, actor_id, work_id, payload_json)
             VALUES ($1,$2,$3,1,0,'test.partial',$4,NULL,$5)",
            &[
                &request.tenant_id,
                &request.project_id,
                &event_id,
                &request.actor_id,
                &payload,
            ],
        )
        .await?;
        tx.rollback().await?;
        Ok(())
    }
}

pub(crate) async fn bind_scope(
    tx: &tokio_postgres::Transaction<'_>,
    tenant: &str,
    project: &str,
) -> PgResult<()> {
    tx.execute("SELECT set_config('awr.tenant_id', $1, true)", &[&tenant])
        .await?;
    tx.execute("SELECT set_config('awr.project_id', $1, true)", &[&project])
        .await?;
    Ok(())
}

async fn load_operation(
    tx: &tokio_postgres::Transaction<'_>,
    request: &CommandRequest,
) -> PgResult<Option<(String, i64, Value)>> {
    let row = tx
        .query_opt(
            "SELECT request_hash, committed_project_revision, result_json
             FROM awr_team.operations
             WHERE tenant_id=$1 AND project_id=$2 AND actor_id=$3 AND client_id=$4 AND request_id=$5",
            &[
                &request.tenant_id,
                &request.project_id,
                &request.actor_id,
                &request.client_id,
                &request.request_id,
            ],
        )
        .await?;
    Ok(row.map(|row| (row.get(0), row.get(1), row.get(2))))
}

async fn apply_op(
    tx: &tokio_postgres::Transaction<'_>,
    request: &CommandRequest,
    revision: i64,
) -> PgResult<Value> {
    match request.op.as_str() {
        "work.touch" => {
            let work_id = request
                .args
                .get("work_id")
                .and_then(Value::as_str)
                .ok_or_else(|| PgError::Protocol("work_id required".into()))?
                .to_owned();
            let scope_id = request
                .args
                .get("scope_id")
                .and_then(Value::as_str)
                .ok_or_else(|| PgError::Protocol("scope_id required".into()))?
                .to_owned();
            tx.execute(
                "INSERT INTO awr_team.work_runtime(
                    tenant_id, project_id, scope_id, work_id, state, work_version, last_fence)
                 VALUES ($1,$2,$3,$4,'pending',1,0)
                 ON CONFLICT (tenant_id, project_id, scope_id, work_id)
                 DO UPDATE SET work_version = awr_team.work_runtime.work_version + 1",
                &[&request.tenant_id, &request.project_id, &scope_id, &work_id],
            )
            .await?;
            Ok(json!({"op":"work.touch","work_id":work_id,"revision":revision}))
        }
        other => Err(PgError::Protocol(format!("unsupported op {other}"))),
    }
}

fn hash_request(request: &CommandRequest) -> PgResult<String> {
    let value = json!({
        "op": request.op,
        "args": request.args,
        "tenant_id": request.tenant_id,
        "project_id": request.project_id,
        "actor_id": request.actor_id,
        "client_id": request.client_id,
        "request_id": request.request_id,
    });
    awr_team::request_hash(&value).map_err(|e| PgError::Protocol(e.to_string()))
}

pub(crate) fn new_id() -> String {
    ulid::Ulid::new().to_string()
}
