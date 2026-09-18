use crate::error::{PgError, PgResult};
use crate::tx::{bind_scope, new_id};
use serde::Serialize;
use serde_json::{Value, json};
use tokio_postgres::error::SqlState;

#[derive(Clone, Debug, Serialize)]
pub struct SessionRecord {
    pub id: String,
    pub actor_id: String,
    pub client_id: String,
    pub conversation_id: String,
    pub work_id: String,
    pub state: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ClaimRecord {
    pub id: String,
    pub session_id: String,
    pub actor_id: String,
    pub work_id: String,
    pub fence: i64,
    pub lease_version: i64,
    pub expires_at: String,
    pub state: String,
    pub replayed: bool,
}

pub struct LeaseStore {
    pool: crate::PgPool,
}

impl LeaseStore {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            pool: crate::PgPool::new(url),
        }
    }

    async fn connect(&self) -> PgResult<crate::PgClient> {
        self.pool.get().await
    }

    pub async fn start_session(
        &self,
        tenant_id: &str,
        project_id: &str,
        actor_id: &str,
        client_id: &str,
        conversation_id: &str,
        scope_id: &str,
        work_id: &str,
    ) -> PgResult<SessionRecord> {
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        lock_project(&tx, tenant_id, project_id).await?;
        let id = new_id();
        tx.execute(
            "INSERT INTO awr_team.sessions(
                tenant_id, project_id, id, scope_id, work_id, actor_id, client_id,
                conversation_id, state)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,'active')",
            &[
                &tenant_id,
                &project_id,
                &id,
                &scope_id,
                &work_id,
                &actor_id,
                &client_id,
                &conversation_id,
            ],
        )
        .await?;
        tx.commit().await?;
        Ok(SessionRecord {
            id,
            actor_id: actor_id.into(),
            client_id: client_id.into(),
            conversation_id: conversation_id.into(),
            work_id: work_id.into(),
            state: "active".into(),
        })
    }

    pub async fn claim(
        &self,
        tenant_id: &str,
        project_id: &str,
        session_id: &str,
        actor_id: &str,
        client_id: &str,
        request_id: &str,
        ttl_seconds: i32,
    ) -> PgResult<ClaimRecord> {
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        lock_project(&tx, tenant_id, project_id).await?;
        if let Some(existing) =
            load_operation(&tx, tenant_id, project_id, actor_id, client_id, request_id).await?
        {
            return replay_claim(&existing.1);
        }
        let session = tx
            .query_opt(
                "SELECT work_id, scope_id, actor_id, client_id, conversation_id, state
                 FROM awr_team.sessions
                 WHERE tenant_id=$1 AND project_id=$2 AND id=$3",
                &[&tenant_id, &project_id, &session_id],
            )
            .await?
            .ok_or(PgError::SessionNotFound)?;
        let work_id: String = session.get(0);
        let scope_id: String = session.get(1);
        let session_actor: String = session.get(2);
        let session_client: String = session.get(3);
        let state: String = session.get(5);
        if session_actor != actor_id || session_client != client_id || state != "active" {
            return Err(PgError::Forbidden);
        }
        expire_due(&tx, tenant_id, project_id, &scope_id, &work_id).await?;
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
        let open_wait: i64 = tx
            .query_one(
                "SELECT count(*) FROM awr_team.wait_items
                 WHERE tenant_id=$1 AND project_id=$2 AND work_id=$3 AND state='open'",
                &[&tenant_id, &project_id, &work_id],
            )
            .await?
            .get(0);
        if open_wait > 0 {
            return Err(PgError::WaitOpen);
        }
        let held = tx
            .query_opt(
                "SELECT actor_id FROM awr_team.claims
                 WHERE tenant_id=$1 AND project_id=$2 AND scope_id=$3 AND work_id=$4
                   AND state='active'",
                &[&tenant_id, &project_id, &scope_id, &work_id],
            )
            .await?;
        if let Some(row) = held {
            let holder: String = row.get(0);
            if holder != actor_id {
                return Err(PgError::ClaimHeld);
            }
        }
        tx.execute(
            "INSERT INTO awr_team.work_runtime(
                tenant_id, project_id, scope_id, work_id, state, work_version, last_fence)
             VALUES ($1,$2,$3,$4,'claimed',1,1)
             ON CONFLICT (tenant_id, project_id, scope_id, work_id)
             DO UPDATE SET last_fence = awr_team.work_runtime.last_fence + 1, state='claimed'",
            &[&tenant_id, &project_id, &scope_id, &work_id],
        )
        .await?;
        let fence: i64 = tx
            .query_one(
                "SELECT last_fence FROM awr_team.work_runtime
                 WHERE tenant_id=$1 AND project_id=$2 AND scope_id=$3 AND work_id=$4",
                &[&tenant_id, &project_id, &scope_id, &work_id],
            )
            .await?
            .get(0);
        let claim_id = new_id();
        let insert = tx.execute(
            "INSERT INTO awr_team.claims(
                tenant_id, project_id, id, scope_id, work_id, session_id, actor_id,
                fence, lease_version, expires_at, state)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,1, clock_timestamp() + make_interval(secs => $9), 'active')",
            &[
                &tenant_id,
                &project_id,
                &claim_id,
                &scope_id,
                &work_id,
                &session_id,
                &actor_id,
                &fence,
                &(ttl_seconds as f64),
            ],
        )
        .await;
        if let Err(error) = insert {
            if error.code() == Some(&SqlState::UNIQUE_VIOLATION) {
                return Err(PgError::ClaimHeld);
            }
            return Err(error.into());
        }
        let expires_at: String = tx
            .query_one(
                "SELECT expires_at::text FROM awr_team.claims
                 WHERE tenant_id=$1 AND project_id=$2 AND id=$3",
                &[&tenant_id, &project_id, &claim_id],
            )
            .await?
            .get(0);
        let result = json!({
            "id": claim_id,
            "session_id": session_id,
            "actor_id": actor_id,
            "work_id": work_id,
            "fence": fence,
            "lease_version": 1,
            "expires_at": expires_at,
            "state": "active",
        });
        store_operation(
            &tx,
            tenant_id,
            project_id,
            actor_id,
            client_id,
            request_id,
            "claim.acquire",
            &result,
        )
        .await?;
        tx.commit().await?;
        Ok(ClaimRecord {
            id: claim_id,
            session_id: session_id.into(),
            actor_id: actor_id.into(),
            work_id,
            fence,
            lease_version: 1,
            expires_at,
            state: "active".into(),
            replayed: false,
        })
    }

    pub async fn renew(
        &self,
        tenant_id: &str,
        project_id: &str,
        claim_id: &str,
        actor_id: &str,
        client_id: &str,
        request_id: &str,
        ttl_seconds: i32,
    ) -> PgResult<ClaimRecord> {
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        lock_project(&tx, tenant_id, project_id).await?;
        if let Some(existing) =
            load_operation(&tx, tenant_id, project_id, actor_id, client_id, request_id).await?
        {
            return replay_claim(&existing.1);
        }
        let row = tx
            .query_opt(
                "SELECT session_id, actor_id, work_id, fence, lease_version, expires_at::text, state, scope_id
                 FROM awr_team.claims
                 WHERE tenant_id=$1 AND project_id=$2 AND id=$3
                 FOR UPDATE",
                &[&tenant_id, &project_id, &claim_id],
            )
            .await?
            .ok_or_else(|| PgError::Protocol("claim not found".into()))?;
        let session_id: String = row.get(0);
        let holder: String = row.get(1);
        let work_id: String = row.get(2);
        let fence: i64 = row.get(3);
        let lease_version: i64 = row.get(4);
        let state: String = row.get(6);
        let scope_id: String = row.get(7);
        if holder != actor_id {
            return Err(PgError::Forbidden);
        }
        expire_due(&tx, tenant_id, project_id, &scope_id, &work_id).await?;
        let still: String = tx
            .query_one(
                "SELECT state FROM awr_team.claims WHERE tenant_id=$1 AND project_id=$2 AND id=$3",
                &[&tenant_id, &project_id, &claim_id],
            )
            .await?
            .get(0);
        if still != "active" || state != "active" {
            return Err(PgError::LeaseExpired);
        }
        tx.execute(
            "UPDATE awr_team.claims
             SET lease_version = lease_version + 1,
                 expires_at = clock_timestamp() + make_interval(secs => $4)
             WHERE tenant_id=$1 AND project_id=$2 AND id=$3 AND state='active'",
            &[&tenant_id, &project_id, &claim_id, &(ttl_seconds as f64)],
        )
        .await?;
        let updated = tx
            .query_one(
                "SELECT lease_version, expires_at::text FROM awr_team.claims
                 WHERE tenant_id=$1 AND project_id=$2 AND id=$3",
                &[&tenant_id, &project_id, &claim_id],
            )
            .await?;
        let next_version: i64 = updated.get(0);
        let expires_at: String = updated.get(1);
        let result = json!({
            "id": claim_id,
            "session_id": session_id,
            "actor_id": actor_id,
            "work_id": work_id,
            "fence": fence,
            "lease_version": next_version,
            "expires_at": expires_at,
            "state": "active",
        });
        store_operation(
            &tx,
            tenant_id,
            project_id,
            actor_id,
            client_id,
            request_id,
            "claim.renew",
            &result,
        )
        .await?;
        tx.commit().await?;
        let _ = lease_version;
        Ok(ClaimRecord {
            id: claim_id.into(),
            session_id,
            actor_id: actor_id.into(),
            work_id,
            fence,
            lease_version: next_version,
            expires_at,
            state: "active".into(),
            replayed: false,
        })
    }

    pub async fn release(
        &self,
        tenant_id: &str,
        project_id: &str,
        claim_id: &str,
        actor_id: &str,
    ) -> PgResult<()> {
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        lock_project(&tx, tenant_id, project_id).await?;
        let row = tx
            .query_opt(
                "SELECT actor_id, state FROM awr_team.claims
                 WHERE tenant_id=$1 AND project_id=$2 AND id=$3 FOR UPDATE",
                &[&tenant_id, &project_id, &claim_id],
            )
            .await?
            .ok_or_else(|| PgError::Protocol("claim not found".into()))?;
        let holder: String = row.get(0);
        let state: String = row.get(1);
        if holder != actor_id {
            return Err(PgError::Forbidden);
        }
        if state != "active" {
            return Err(PgError::LeaseExpired);
        }
        tx.execute(
            "UPDATE awr_team.claims SET state='released'
             WHERE tenant_id=$1 AND project_id=$2 AND id=$3",
            &[&tenant_id, &project_id, &claim_id],
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn expire_due(
        &self,
        tenant_id: &str,
        project_id: &str,
        scope_id: &str,
        work_id: &str,
    ) -> PgResult<u64> {
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        lock_project(&tx, tenant_id, project_id).await?;
        let count = expire_due(&tx, tenant_id, project_id, scope_id, work_id).await?;
        tx.commit().await?;
        Ok(count)
    }

    pub async fn handoff(
        &self,
        tenant_id: &str,
        project_id: &str,
        claim_id: &str,
        actor_id: &str,
        successor_actor_id: &str,
        successor_client_id: &str,
        conversation_id: &str,
    ) -> PgResult<ClaimRecord> {
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        lock_project(&tx, tenant_id, project_id).await?;
        let row = tx
            .query_opt(
                "SELECT session_id, actor_id, work_id, scope_id, state
                 FROM awr_team.claims
                 WHERE tenant_id=$1 AND project_id=$2 AND id=$3 FOR UPDATE",
                &[&tenant_id, &project_id, &claim_id],
            )
            .await?
            .ok_or_else(|| PgError::Protocol("claim not found".into()))?;
        let old_session: String = row.get(0);
        let holder: String = row.get(1);
        let work_id: String = row.get(2);
        let scope_id: String = row.get(3);
        let state: String = row.get(4);
        if holder != actor_id {
            return Err(PgError::Forbidden);
        }
        expire_due(&tx, tenant_id, project_id, &scope_id, &work_id).await?;
        if state != "active" {
            return Err(PgError::LeaseExpired);
        }
        tx.execute(
            "UPDATE awr_team.claims SET state='handed_off'
             WHERE tenant_id=$1 AND project_id=$2 AND id=$3",
            &[&tenant_id, &project_id, &claim_id],
        )
        .await?;
        tx.execute(
            "UPDATE awr_team.sessions SET state='ended'
             WHERE tenant_id=$1 AND project_id=$2 AND id=$3",
            &[&tenant_id, &project_id, &old_session],
        )
        .await?;
        tx.execute(
            "UPDATE awr_team.work_runtime
             SET last_fence = last_fence + 1
             WHERE tenant_id=$1 AND project_id=$2 AND scope_id=$3 AND work_id=$4",
            &[&tenant_id, &project_id, &scope_id, &work_id],
        )
        .await?;
        let fence: i64 = tx
            .query_one(
                "SELECT last_fence FROM awr_team.work_runtime
                 WHERE tenant_id=$1 AND project_id=$2 AND scope_id=$3 AND work_id=$4",
                &[&tenant_id, &project_id, &scope_id, &work_id],
            )
            .await?
            .get(0);
        let session_id = new_id();
        tx.execute(
            "INSERT INTO awr_team.sessions(
                tenant_id, project_id, id, scope_id, work_id, actor_id, client_id,
                conversation_id, predecessor_id, state)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,'active')",
            &[
                &tenant_id,
                &project_id,
                &session_id,
                &scope_id,
                &work_id,
                &successor_actor_id,
                &successor_client_id,
                &conversation_id,
                &old_session,
            ],
        )
        .await?;
        let new_claim = new_id();
        tx.execute(
            "INSERT INTO awr_team.claims(
                tenant_id, project_id, id, scope_id, work_id, session_id, actor_id,
                fence, lease_version, expires_at, state)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,1, clock_timestamp() + interval '60 seconds', 'active')",
            &[
                &tenant_id,
                &project_id,
                &new_claim,
                &scope_id,
                &work_id,
                &session_id,
                &successor_actor_id,
                &fence,
            ],
        )
        .await?;
        let expires_at: String = tx
            .query_one(
                "SELECT expires_at::text FROM awr_team.claims
                 WHERE tenant_id=$1 AND project_id=$2 AND id=$3",
                &[&tenant_id, &project_id, &new_claim],
            )
            .await?
            .get(0);
        tx.commit().await?;
        Ok(ClaimRecord {
            id: new_claim,
            session_id,
            actor_id: successor_actor_id.into(),
            work_id,
            fence,
            lease_version: 1,
            expires_at,
            state: "active".into(),
            replayed: false,
        })
    }

    pub async fn wait(
        &self,
        tenant_id: &str,
        project_id: &str,
        session_id: &str,
        actor_id: &str,
        question: &str,
    ) -> PgResult<String> {
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        lock_project(&tx, tenant_id, project_id).await?;
        let claim = tx
            .query_opt(
                "SELECT id, expires_at::text FROM awr_team.claims
                 WHERE tenant_id=$1 AND project_id=$2 AND session_id=$3
                   AND actor_id=$4 AND state='active'",
                &[&tenant_id, &project_id, &session_id, &actor_id],
            )
            .await?
            .ok_or(PgError::LeaseExpired)?;
        let expires_before: String = claim.get(1);
        let work_id: String = tx
            .query_one(
                "SELECT work_id FROM awr_team.sessions
                 WHERE tenant_id=$1 AND project_id=$2 AND id=$3",
                &[&tenant_id, &project_id, &session_id],
            )
            .await?
            .get(0);
        let wait_id = new_id();
        tx.execute(
            "INSERT INTO awr_team.wait_items(
                tenant_id, project_id, id, session_id, work_id, question, state)
             VALUES ($1,$2,$3,$4,$5,$6,'open')",
            &[
                &tenant_id,
                &project_id,
                &wait_id,
                &session_id,
                &work_id,
                &question,
            ],
        )
        .await?;
        let expires_after: String = tx
            .query_one(
                "SELECT expires_at::text FROM awr_team.claims
                 WHERE tenant_id=$1 AND project_id=$2 AND session_id=$3 AND state='active'",
                &[&tenant_id, &project_id, &session_id],
            )
            .await?
            .get(0);
        if expires_after != expires_before {
            return Err(PgError::Protocol("wait must not renew the lease".into()));
        }
        tx.commit().await?;
        Ok(wait_id)
    }

    pub async fn reply(
        &self,
        tenant_id: &str,
        project_id: &str,
        wait_id: &str,
        reply: &str,
    ) -> PgResult<()> {
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        lock_project(&tx, tenant_id, project_id).await?;
        let updated = tx
            .execute(
                "UPDATE awr_team.wait_items SET state='replied', reply=$4
                 WHERE tenant_id=$1 AND project_id=$2 AND id=$3 AND state='open'",
                &[&tenant_id, &project_id, &wait_id, &reply],
            )
            .await?;
        if updated != 1 {
            return Err(PgError::Protocol("wait not open".into()));
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn set_recovery_blocked(
        &self,
        tenant_id: &str,
        project_id: &str,
        scope_id: &str,
        work_id: &str,
        blocked: bool,
    ) -> PgResult<()> {
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        lock_project(&tx, tenant_id, project_id).await?;
        tx.execute(
            "INSERT INTO awr_team.work_runtime(
                tenant_id, project_id, scope_id, work_id, state, work_version, last_fence, recovery_blocked)
             VALUES ($1,$2,$3,$4,'blocked',1,0,$5)
             ON CONFLICT (tenant_id, project_id, scope_id, work_id)
             DO UPDATE SET recovery_blocked=$5",
            &[&tenant_id, &project_id, &scope_id, &work_id, &blocked],
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn require_fence(
        &self,
        tenant_id: &str,
        project_id: &str,
        work_id: &str,
        actor_id: &str,
        fence: i64,
    ) -> PgResult<()> {
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        let row = tx
            .query_opt(
                "SELECT fence, actor_id, state FROM awr_team.claims
                 WHERE tenant_id=$1 AND project_id=$2 AND work_id=$3 AND state='active'",
                &[&tenant_id, &project_id, &work_id],
            )
            .await?
            .ok_or(PgError::LeaseExpired)?;
        let current: i64 = row.get(0);
        let holder: String = row.get(1);
        if holder != actor_id || current != fence {
            return Err(PgError::StaleFence);
        }
        tx.commit().await?;
        Ok(())
    }
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

async fn expire_due(
    tx: &tokio_postgres::Transaction<'_>,
    tenant_id: &str,
    project_id: &str,
    scope_id: &str,
    work_id: &str,
) -> PgResult<u64> {
    let count = tx
        .execute(
            "UPDATE awr_team.claims SET state='expired'
             WHERE tenant_id=$1 AND project_id=$2 AND scope_id=$3 AND work_id=$4
               AND state='active' AND expires_at <= clock_timestamp()",
            &[&tenant_id, &project_id, &scope_id, &work_id],
        )
        .await?;
    Ok(count)
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
    result: &Value,
) -> PgResult<()> {
    let op_id = new_id();
    let request_hash = format!("{op}:{request_id}");
    tx.execute(
        "INSERT INTO awr_team.operations(
            tenant_id, project_id, id, actor_id, client_id, request_id, op,
            request_hash, state, result_json)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,'committed',$9)",
        &[
            &tenant_id,
            &project_id,
            &op_id,
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

fn replay_claim(result: &Value) -> PgResult<ClaimRecord> {
    Ok(ClaimRecord {
        id: result["id"].as_str().unwrap_or_default().into(),
        session_id: result["session_id"].as_str().unwrap_or_default().into(),
        actor_id: result["actor_id"].as_str().unwrap_or_default().into(),
        work_id: result["work_id"].as_str().unwrap_or_default().into(),
        fence: result["fence"].as_i64().unwrap_or(0),
        lease_version: result["lease_version"].as_i64().unwrap_or(0),
        expires_at: result["expires_at"].as_str().unwrap_or_default().into(),
        state: result["state"].as_str().unwrap_or("active").into(),
        replayed: true,
    })
}
