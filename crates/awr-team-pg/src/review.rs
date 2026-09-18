use crate::error::{PgError, PgResult};
use crate::tx::{bind_scope, new_id};
use awr_team::{CompletionView, EvidenceBundle, EvidenceGrade, ReviewPolicy, current_completion};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Serialize)]
pub struct EvidenceRecord {
    pub id: String,
    pub work_id: String,
    pub trust_basis: String,
    pub digest: String,
    pub contract_hash: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReviewRound {
    pub id: String,
    pub work_id: String,
    pub round_index: i32,
    pub bundle_hash: String,
    pub state: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct CompletionReceipt {
    pub id: String,
    pub work_id: String,
    pub contract_hash: String,
    pub policy: String,
}

pub struct ReviewStore {
    pool: crate::PgPool,
}

impl ReviewStore {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            pool: crate::PgPool::new(url),
        }
    }

    async fn connect(&self) -> PgResult<crate::PgClient> {
        self.pool.get().await
    }

    pub async fn record_evidence(
        &self,
        tenant_id: &str,
        project_id: &str,
        actor_id: &str,
        work_id: &str,
        contract_hash: &str,
        claimed_trust: Option<&str>,
        payload: &Value,
        artifact_bytes: Option<&[u8]>,
        input_digest: Option<&str>,
        dirty_tree: bool,
    ) -> PgResult<EvidenceRecord> {
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
        let trust_basis = assigned_trust(&actor_kind, claimed_trust);
        if dirty_tree && input_digest.is_none() && artifact_bytes.is_none() {
            return Err(PgError::EvidenceInvalid);
        }
        let mut payload = payload.clone();
        if payload.get("passed").and_then(Value::as_bool) == Some(true)
            && artifact_bytes.is_none()
            && payload.get("output_digest").is_none()
        {
            return Err(PgError::EvidenceInvalid);
        }
        let output_digest = artifact_bytes.map(|bytes| sha256_hex(bytes));
        if let Some(digest) = &output_digest {
            payload
                .as_object_mut()
                .map(|map| map.insert("output_digest".into(), json!(digest)));
        }
        let digest = sha256_hex(payload.to_string().as_bytes());
        let mut artifact_id: Option<String> = None;
        if let Some(bytes) = artifact_bytes {
            let id = new_id();
            tx.execute(
                "INSERT INTO awr_team.artifacts(
                    tenant_id, project_id, id, object_key, sha256, byte_length,
                    media_type, state, created_by)
                 VALUES ($1,$2,$3,$4,$5,$6,'application/octet-stream','finalized',$7)",
                &[
                    &tenant_id,
                    &project_id,
                    &id,
                    &format!("evidence/{id}"),
                    &sha256_hex(bytes),
                    &(bytes.len() as i64),
                    &actor_id,
                ],
            )
            .await?;
            artifact_id = Some(id);
        }
        let id = new_id();
        tx.execute(
            "INSERT INTO awr_team.evidence(
                tenant_id, project_id, id, work_id, artifact_id, contract_hash,
                input_digest, output_digest, evidence_kind, trust_basis, digest,
                payload_json, created_by)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,'report',$9,$10,$11,$12)",
            &[
                &tenant_id,
                &project_id,
                &id,
                &work_id,
                &artifact_id,
                &contract_hash,
                &input_digest.map(ToOwned::to_owned),
                &output_digest,
                &trust_basis,
                &digest,
                &payload,
                &actor_id,
            ],
        )
        .await?;
        tx.commit().await?;
        Ok(EvidenceRecord {
            id,
            work_id: work_id.into(),
            trust_basis,
            digest,
            contract_hash: contract_hash.into(),
        })
    }

    pub async fn open_review(
        &self,
        tenant_id: &str,
        project_id: &str,
        author_actor_id: &str,
        work_id: &str,
        evidence_id: &str,
    ) -> PgResult<ReviewRound> {
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        lock_project(&tx, tenant_id, project_id).await?;
        let evidence = load_evidence(&tx, tenant_id, project_id, evidence_id).await?;
        if evidence.work_id != work_id {
            return Err(PgError::EvidenceInvalid);
        }
        invalidate_open_rounds(&tx, tenant_id, project_id, work_id, &evidence.digest).await?;
        let round_index: i32 = tx
            .query_one(
                "SELECT COALESCE(max(round_index),0)+1 FROM awr_team.review_rounds
                 WHERE tenant_id=$1 AND project_id=$2 AND work_id=$3",
                &[&tenant_id, &project_id, &work_id],
            )
            .await?
            .get(0);
        let id = new_id();
        tx.execute(
            "INSERT INTO awr_team.review_rounds(
                tenant_id, project_id, id, work_id, round_index, bundle_hash,
                contract_hash, author_actor_id, state)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,'open')",
            &[
                &tenant_id,
                &project_id,
                &id,
                &work_id,
                &round_index,
                &evidence.digest,
                &evidence.contract_hash,
                &author_actor_id,
            ],
        )
        .await?;
        tx.commit().await?;
        Ok(ReviewRound {
            id,
            work_id: work_id.into(),
            round_index,
            bundle_hash: evidence.digest,
            state: "open".into(),
        })
    }

    pub async fn decide_review(
        &self,
        tenant_id: &str,
        project_id: &str,
        reviewer_actor_id: &str,
        round_id: &str,
        decision: &str,
        reason: &str,
    ) -> PgResult<ReviewRound> {
        if !matches!(decision, "approve" | "reject") {
            return Err(PgError::Protocol("invalid review decision".into()));
        }
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        lock_project(&tx, tenant_id, project_id).await?;
        let row = tx
            .query_opt(
                "SELECT work_id, author_actor_id, bundle_hash, state, round_index, contract_hash
                 FROM awr_team.review_rounds
                 WHERE tenant_id=$1 AND project_id=$2 AND id=$3
                 FOR UPDATE",
                &[&tenant_id, &project_id, &round_id],
            )
            .await?
            .ok_or_else(|| PgError::Protocol("review round not found".into()))?;
        let work_id: String = row.get(0);
        let author: String = row.get(1);
        let bundle_hash: String = row.get(2);
        let state: String = row.get(3);
        let round_index: i32 = row.get(4);
        if state != "open" {
            return Err(PgError::ReviewRequired);
        }
        if reviewer_actor_id == author {
            return Err(PgError::AuthorCannotReview);
        }
        let reviewer_kind: String = tx
            .query_opt(
                "SELECT kind FROM awr_team.actors WHERE tenant_id=$1 AND id=$2",
                &[&tenant_id, &reviewer_actor_id],
            )
            .await?
            .map(|r| r.get(0))
            .ok_or(PgError::Forbidden)?;
        if reviewer_kind == "agent" {
            return Err(PgError::AuthorCannotReview);
        }
        let next = if decision == "approve" {
            "approved"
        } else {
            "rejected"
        };
        tx.execute(
            "INSERT INTO awr_team.review_decisions(
                tenant_id, project_id, id, review_round_id, work_id, bundle_hash,
                reviewer_actor_id, decision, reason)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)",
            &[
                &tenant_id,
                &project_id,
                &new_id(),
                &round_id,
                &work_id,
                &bundle_hash,
                &reviewer_actor_id,
                &decision,
                &reason,
            ],
        )
        .await?;
        tx.execute(
            "UPDATE awr_team.review_rounds SET state=$4
             WHERE tenant_id=$1 AND project_id=$2 AND id=$3",
            &[&tenant_id, &project_id, &round_id, &next],
        )
        .await?;
        tx.commit().await?;
        Ok(ReviewRound {
            id: round_id.into(),
            work_id,
            round_index,
            bundle_hash,
            state: next.into(),
        })
    }

    pub async fn complete(
        &self,
        tenant_id: &str,
        project_id: &str,
        actor_id: &str,
        work_id: &str,
        scope_id: &str,
        evidence_id: &str,
        requested_policy: Option<&str>,
        context_complete: bool,
    ) -> PgResult<CompletionReceipt> {
        if !context_complete {
            return Err(PgError::ContextIncomplete);
        }
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        lock_project(&tx, tenant_id, project_id).await?;
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
        let contract = current_contract(&tx, tenant_id, project_id, scope_id, work_id).await?;
        let policy = contract
            .get("completion_policy")
            .and_then(Value::as_str)
            .unwrap_or("trusted_execution_and_review");
        if let Some(requested) = requested_policy {
            if requested != policy {
                return Err(PgError::PolicyDowngrade);
            }
        }
        let evidence = load_evidence(&tx, tenant_id, project_id, evidence_id).await?;
        if evidence.work_id != work_id || evidence.contract_hash != current_contract_hash(&contract)
        {
            return Err(PgError::EvidenceInvalid);
        }
        if evidence_bytes_changed(&evidence.payload, &evidence.digest) {
            return Err(PgError::EvidenceInvalid);
        }
        if evidence.artifact_id.is_some() {
            let row = tx
                .query_opt(
                    "SELECT sha256, state FROM awr_team.artifacts
                     WHERE tenant_id=$1 AND project_id=$2 AND id=$3",
                    &[&tenant_id, &project_id, &evidence.artifact_id],
                )
                .await?
                .ok_or(PgError::EvidenceInvalid)?;
            let sha: String = row.get(0);
            let state: String = row.get(1);
            if state != "finalized" || evidence.output_digest.as_deref() != Some(sha.as_str()) {
                return Err(PgError::EvidenceInvalid);
            }
        }
        let binding_valid = dependency_bindings_valid(&tx, tenant_id, project_id, work_id).await?;
        let mut review =
            current_review(&tx, tenant_id, project_id, work_id, &evidence.digest).await?;
        let grade = match evidence.trust_basis.as_str() {
            "trusted_executor" => EvidenceGrade::TrustedExecutionReceipt,
            "human_review" => EvidenceGrade::AuthorizedReview,
            _ => EvidenceGrade::AgentSelfReport,
        };
        if policy == "ordinary_confirm" {
            let kind: String = tx
                .query_one(
                    "SELECT kind FROM awr_team.actors WHERE tenant_id=$1 AND id=$2",
                    &[&tenant_id, &actor_id],
                )
                .await?
                .get(0);
            if kind != "human" {
                return Err(PgError::Forbidden);
            }
            review.required = false;
        } else if grade == EvidenceGrade::AgentSelfReport {
            return Err(PgError::EvidenceInvalid);
        }
        let bundle = EvidenceBundle {
            grade,
            contract_hash: evidence.contract_hash.clone(),
            artifact_digest: evidence.output_digest.clone(),
            accessible: true,
        };
        let view = current_completion(
            false,
            true,
            Some(&evidence.contract_hash),
            &evidence.contract_hash,
            binding_valid,
            Some(&bundle),
            &review,
        );
        if view != CompletionView::CurrentlyVerified {
            return Err(PgError::CompletionRejected);
        }
        let receipt_id = new_id();
        let approved_by = json!({"actor_id": actor_id, "approved": review.approved});
        tx.execute(
            "INSERT INTO awr_team.completion_receipts(
                tenant_id, project_id, id, work_id, scope_id, contract_hash,
                result_digest, dependency_binding_hash, evidence_bundle_hash,
                policy, approved_by_json)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)",
            &[
                &tenant_id,
                &project_id,
                &receipt_id,
                &work_id,
                &scope_id,
                &evidence.contract_hash,
                &evidence.digest,
                &format!("bind:{}", binding_valid),
                &evidence.digest,
                &policy,
                &approved_by,
            ],
        )
        .await?;
        tx.execute(
            "INSERT INTO awr_team.work_runtime(
                tenant_id, project_id, scope_id, work_id, state, work_version, last_fence,
                selected_completion_id)
             VALUES ($1,$2,$3,$4,'completed',1,0,$5)
             ON CONFLICT (tenant_id, project_id, scope_id, work_id)
             DO UPDATE SET state='completed', selected_completion_id=$5",
            &[&tenant_id, &project_id, &scope_id, &work_id, &receipt_id],
        )
        .await?;
        tx.commit().await?;
        Ok(CompletionReceipt {
            id: receipt_id,
            work_id: work_id.into(),
            contract_hash: evidence.contract_hash,
            policy: policy.into(),
        })
    }

    pub async fn source_declared_is_not_complete(
        &self,
        tenant_id: &str,
        project_id: &str,
        work_id: &str,
        scope_id: &str,
    ) -> PgResult<bool> {
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        let completed: i64 = tx
            .query_one(
                "SELECT count(*) FROM awr_team.completion_receipts
                 WHERE tenant_id=$1 AND project_id=$2 AND work_id=$3",
                &[&tenant_id, &project_id, &work_id],
            )
            .await?
            .get(0);
        let runtime: Option<String> = tx
            .query_opt(
                "SELECT state FROM awr_team.work_runtime
                 WHERE tenant_id=$1 AND project_id=$2 AND scope_id=$3 AND work_id=$4",
                &[&tenant_id, &project_id, &scope_id, &work_id],
            )
            .await?
            .map(|row| row.get(0));
        tx.commit().await?;
        Ok(completed == 0 && runtime.as_deref() != Some("completed"))
    }
}

struct LoadedEvidence {
    work_id: String,
    contract_hash: String,
    digest: String,
    trust_basis: String,
    payload: Value,
    artifact_id: Option<String>,
    output_digest: Option<String>,
}

fn assigned_trust(actor_kind: &str, claimed: Option<&str>) -> String {
    match actor_kind {
        "system" => "trusted_executor".into(),
        "human" => "human_review".into(),
        _ => {
            let _ = claimed;
            "caller_asserted".into()
        }
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn evidence_bytes_changed(payload: &Value, digest: &str) -> bool {
    sha256_hex(payload.to_string().as_bytes()) != digest
}

fn current_contract_hash(contract: &Value) -> String {
    contract
        .get("contract_hash")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
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

async fn load_evidence(
    tx: &tokio_postgres::Transaction<'_>,
    tenant_id: &str,
    project_id: &str,
    evidence_id: &str,
) -> PgResult<LoadedEvidence> {
    let row = tx
        .query_opt(
            "SELECT work_id, contract_hash, digest, trust_basis, payload_json, artifact_id, output_digest
             FROM awr_team.evidence
             WHERE tenant_id=$1 AND project_id=$2 AND id=$3",
            &[&tenant_id, &project_id, &evidence_id],
        )
        .await?
        .ok_or(PgError::EvidenceInvalid)?;
    Ok(LoadedEvidence {
        work_id: row.get(0),
        contract_hash: row.get(1),
        digest: row.get(2),
        trust_basis: row.get(3),
        payload: row.get(4),
        artifact_id: row.get(5),
        output_digest: row.get(6),
    })
}

async fn current_contract(
    tx: &tokio_postgres::Transaction<'_>,
    tenant_id: &str,
    project_id: &str,
    scope_id: &str,
    work_id: &str,
) -> PgResult<Value> {
    let row = tx
        .query_opt(
            "SELECT c.contract_hash, c.contract_json
             FROM awr_team.projects p
             JOIN awr_team.work_contracts c
               ON c.tenant_id=p.tenant_id AND c.project_id=p.id
              AND c.snapshot_id=p.active_snapshot_id
             WHERE p.tenant_id=$1 AND p.id=$2 AND c.scope_id=$3 AND c.work_id=$4",
            &[&tenant_id, &project_id, &scope_id, &work_id],
        )
        .await?
        .ok_or(PgError::EvidenceInvalid)?;
    let hash: String = row.get(0);
    let mut json: Value = row.get(1);
    json.as_object_mut()
        .map(|map| map.insert("contract_hash".into(), Value::String(hash)));
    Ok(json)
}

async fn dependency_bindings_valid(
    tx: &tokio_postgres::Transaction<'_>,
    tenant_id: &str,
    project_id: &str,
    work_id: &str,
) -> PgResult<bool> {
    let invalid: i64 = tx
        .query_one(
            "SELECT count(*) FROM awr_team.dependency_bindings
             WHERE tenant_id=$1 AND project_id=$2 AND downstream_work_id=$3 AND valid=FALSE",
            &[&tenant_id, &project_id, &work_id],
        )
        .await?
        .get(0);
    Ok(invalid == 0)
}

async fn current_review(
    tx: &tokio_postgres::Transaction<'_>,
    tenant_id: &str,
    project_id: &str,
    work_id: &str,
    bundle_hash: &str,
) -> PgResult<ReviewPolicy> {
    let row = tx
        .query_opt(
            "SELECT author_actor_id, state FROM awr_team.review_rounds
             WHERE tenant_id=$1 AND project_id=$2 AND work_id=$3 AND bundle_hash=$4
             ORDER BY round_index DESC LIMIT 1",
            &[&tenant_id, &project_id, &work_id, &bundle_hash],
        )
        .await?;
    match row {
        None => Ok(ReviewPolicy {
            required: true,
            author_may_self_approve: false,
            approved: false,
            reviewer_is_author: false,
        }),
        Some(row) => {
            let author: String = row.get(0);
            let state: String = row.get(1);
            let reviewer: Option<String> = tx
                .query_opt(
                    "SELECT reviewer_actor_id FROM awr_team.review_decisions d
                     JOIN awr_team.review_rounds r
                       ON r.tenant_id=d.tenant_id AND r.project_id=d.project_id AND r.id=d.review_round_id
                     WHERE d.tenant_id=$1 AND d.project_id=$2 AND r.work_id=$3 AND r.bundle_hash=$4
                     ORDER BY d.created_at DESC LIMIT 1",
                    &[&tenant_id, &project_id, &work_id, &bundle_hash],
                )
                .await?
                .map(|row| row.get(0));
            Ok(ReviewPolicy {
                required: true,
                author_may_self_approve: false,
                approved: state == "approved",
                reviewer_is_author: reviewer.as_deref() == Some(author.as_str()),
            })
        }
    }
}

async fn invalidate_open_rounds(
    tx: &tokio_postgres::Transaction<'_>,
    tenant_id: &str,
    project_id: &str,
    work_id: &str,
    new_bundle: &str,
) -> PgResult<()> {
    tx.execute(
        "UPDATE awr_team.review_rounds SET state='invalidated'
         WHERE tenant_id=$1 AND project_id=$2 AND work_id=$3
           AND state IN ('open','approved') AND bundle_hash <> $4",
        &[&tenant_id, &project_id, &work_id, &new_bundle],
    )
    .await?;
    Ok(())
}
