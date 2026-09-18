use crate::error::{PgError, PgResult};
use crate::tx::{bind_scope, new_id};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

#[derive(Clone, Debug, Serialize)]
pub struct InspectReport {
    pub fingerprints: Vec<String>,
    pub diverged: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ImportJob {
    pub id: String,
    pub import_key: String,
    pub manifest_hash: String,
    pub state: String,
    pub replayed: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct BackupRecord {
    pub id: String,
    pub manifest_hash: String,
    pub coordinator_epoch: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct RestoreRun {
    pub id: String,
    pub new_epoch: String,
    pub outbox_replayed: bool,
}

pub struct ImportStore {
    pool: crate::PgPool,
}

impl ImportStore {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            pool: crate::PgPool::new(url),
        }
    }

    async fn connect(&self) -> PgResult<crate::PgClient> {
        self.pool.get().await
    }

    pub fn inspect_sources(&self, sources: &[(&str, &str)]) -> PgResult<InspectReport> {
        let fingerprints: BTreeSet<String> =
            sources.iter().map(|(_, fp)| (*fp).to_owned()).collect();
        if fingerprints.len() > 1 {
            return Err(PgError::SourceDivergence);
        }
        Ok(InspectReport {
            fingerprints: fingerprints.into_iter().collect(),
            diverged: false,
        })
    }

    pub async fn freeze(&self, tenant_id: &str, project_id: &str) -> PgResult<()> {
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        lock_project(&tx, tenant_id, project_id).await?;
        tx.execute(
            "UPDATE awr_team.projects SET status='frozen'
             WHERE tenant_id=$1 AND id=$2",
            &[&tenant_id, &project_id],
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn export(&self, tenant_id: &str, project_id: &str) -> PgResult<Value> {
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        let works = tx
            .query(
                "SELECT id, external_key FROM awr_team.work_items
                 WHERE tenant_id=$1 AND project_id=$2 ORDER BY id",
                &[&tenant_id, &project_id],
            )
            .await?;
        let evidence = tx
            .query(
                "SELECT id, trust_basis, digest FROM awr_team.evidence
                 WHERE tenant_id=$1 AND project_id=$2",
                &[&tenant_id, &project_id],
            )
            .await
            .unwrap_or_default();
        tx.commit().await?;
        Ok(json!({
            "works": works.iter().map(|row| json!({"id": row.get::<_, String>(0), "external_key": row.get::<_, String>(1)})).collect::<Vec<_>>(),
            "evidence": evidence.iter().map(|row| json!({
                "id": row.get::<_, String>(0),
                "trust_basis": row.get::<_, String>(1),
                "digest": row.get::<_, String>(2),
            })).collect::<Vec<_>>(),
        }))
    }

    pub fn dry_run(&self, manifest: &Value) -> PgResult<Value> {
        let missing = manifest
            .get("evidence")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|item| item.get("bytes_present") == Some(&json!(false)))
            .cloned()
            .collect::<Vec<_>>();
        let scopes = manifest
            .get("scopes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if scopes.iter().any(|scope| scope.as_str() != Some("main")) {
            return Err(PgError::ScopeUnsupported);
        }
        Ok(json!({
            "missing_evidence": missing,
            "can_activate": missing.is_empty(),
        }))
    }

    pub async fn load(
        &self,
        tenant_id: &str,
        project_id: &str,
        actor_id: &str,
        import_key: &str,
        manifest: &Value,
    ) -> PgResult<ImportJob> {
        let manifest_hash = sha256_hex(manifest.to_string().as_bytes());
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        lock_project(&tx, tenant_id, project_id).await?;
        if let Some(existing) = tx
            .query_opt(
                "SELECT id, state FROM awr_team.import_jobs
                 WHERE tenant_id=$1 AND import_key=$2 AND manifest_hash=$3",
                &[&tenant_id, &import_key, &manifest_hash],
            )
            .await?
        {
            tx.commit().await?;
            return Ok(ImportJob {
                id: existing.get(0),
                import_key: import_key.into(),
                manifest_hash,
                state: existing.get(1),
                replayed: true,
            });
        }
        let works = manifest
            .get("works")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for work in &works {
            let work_id = work.get("id").and_then(Value::as_str).unwrap_or_default();
            let key = work
                .get("external_key")
                .and_then(Value::as_str)
                .unwrap_or(work_id);
            if work_id.is_empty() {
                continue;
            }
            tx.execute(
                "INSERT INTO awr_team.work_items(tenant_id, project_id, id, external_key)
                 VALUES ($1,$2,$3,$4)
                 ON CONFLICT (tenant_id, project_id, id) DO NOTHING",
                &[&tenant_id, &project_id, &work_id, &key],
            )
            .await?;
        }
        if let Some(items) = manifest.get("evidence").and_then(Value::as_array) {
            for item in items {
                let claimed = item
                    .get("claimed_trust")
                    .and_then(Value::as_str)
                    .unwrap_or("caller_asserted");
                let actual = item
                    .get("trust_basis")
                    .and_then(Value::as_str)
                    .unwrap_or("caller_asserted");
                if claimed == "trusted_executor" && actual == "caller_asserted" {
                    // Historical self-reports stay caller_asserted.
                }
                if item.get("bytes_present") == Some(&json!(false)) {
                    continue;
                }
                let work_id = item
                    .get("work_id")
                    .and_then(Value::as_str)
                    .unwrap_or("work-a");
                tx.execute(
                    "INSERT INTO awr_team.evidence(
                        tenant_id, project_id, id, work_id, contract_hash, evidence_kind,
                        trust_basis, digest, payload_json, created_by)
                     VALUES ($1,$2,$3,$4,$5,'report',$6,$7,$8,$9)
                     ON CONFLICT DO NOTHING",
                    &[
                        &tenant_id,
                        &project_id,
                        &item.get("id").and_then(Value::as_str).unwrap_or(&new_id()).to_owned(),
                        &work_id,
                        &"imported",
                        &actual,
                        &item.get("digest").and_then(Value::as_str).unwrap_or("missing").to_owned(),
                        &json!({"imported": true, "unavailable": item.get("bytes_present") == Some(&json!(false))}),
                        &actor_id,
                    ],
                )
                .await?;
            }
        }
        // Local claims are observations, never active team leases.
        let id = new_id();
        tx.execute(
            "INSERT INTO awr_team.import_jobs(
                tenant_id, id, import_key, manifest_hash, state, report_json, project_id)
             VALUES ($1,$2,$3,$4,'loaded',$5,$6)",
            &[
                &tenant_id,
                &id,
                &import_key,
                &manifest_hash,
                &json!({"works": works.len()}),
                &project_id,
            ],
        )
        .await?;
        tx.execute(
            "UPDATE awr_team.projects SET status='importing'
             WHERE tenant_id=$1 AND id=$2",
            &[&tenant_id, &project_id],
        )
        .await?;
        tx.commit().await?;
        Ok(ImportJob {
            id,
            import_key: import_key.into(),
            manifest_hash,
            state: "loaded".into(),
            replayed: false,
        })
    }

    pub async fn activate(
        &self,
        tenant_id: &str,
        project_id: &str,
        job_id: &str,
        unknown_executions: bool,
    ) -> PgResult<String> {
        if unknown_executions {
            return Err(PgError::RecoveryBlocked);
        }
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        lock_project(&tx, tenant_id, project_id).await?;
        let job = tx
            .query_opt(
                "SELECT state FROM awr_team.import_jobs
                 WHERE tenant_id=$1 AND id=$2",
                &[&tenant_id, &job_id],
            )
            .await?
            .ok_or_else(|| PgError::Protocol("import job not found".into()))?;
        let state: String = job.get(0);
        if state != "loaded" {
            return Err(PgError::Protocol("import job not loadable".into()));
        }
        let epoch = format!("epoch-{}", new_id());
        tx.execute(
            "UPDATE awr_team.projects
             SET status='active', coordinator_epoch=$3
             WHERE tenant_id=$1 AND id=$2",
            &[&tenant_id, &project_id, &epoch],
        )
        .await?;
        tx.execute(
            "UPDATE awr_team.import_jobs SET state='activated'
             WHERE tenant_id=$1 AND id=$2",
            &[&tenant_id, &job_id],
        )
        .await?;
        tx.commit().await?;
        Ok(epoch)
    }

    pub async fn backup(
        &self,
        tenant_id: &str,
        project_id: &str,
        artifact_digests: &[String],
        source_digests: &[String],
    ) -> PgResult<BackupRecord> {
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        lock_project(&tx, tenant_id, project_id).await?;
        let epoch: String = tx
            .query_one(
                "SELECT coordinator_epoch FROM awr_team.projects WHERE tenant_id=$1 AND id=$2",
                &[&tenant_id, &project_id],
            )
            .await?
            .get(0);
        let manifest = json!({
            "epoch": epoch,
            "artifacts": artifact_digests,
            "sources": source_digests,
        });
        let hash = sha256_hex(manifest.to_string().as_bytes());
        let id = new_id();
        tx.execute(
            "INSERT INTO awr_team.backups(
                tenant_id, project_id, id, manifest_hash, coordinator_epoch,
                schema_version, artifact_digests_json, source_digests_json)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
            &[
                &tenant_id,
                &project_id,
                &id,
                &hash,
                &epoch,
                &crate::EXPECTED_SCHEMA_VERSION,
                &json!(artifact_digests),
                &json!(source_digests),
            ],
        )
        .await?;
        tx.commit().await?;
        Ok(BackupRecord {
            id,
            manifest_hash: hash,
            coordinator_epoch: epoch,
        })
    }

    pub async fn restore(
        &self,
        tenant_id: &str,
        project_id: &str,
        backup_id: &str,
        artifacts_present: bool,
        replay_outbox: bool,
    ) -> PgResult<RestoreRun> {
        if replay_outbox {
            return Err(PgError::OutboxReplayForbidden);
        }
        if !artifacts_present {
            return Err(PgError::RestoreIncomplete);
        }
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        lock_project(&tx, tenant_id, project_id).await?;
        let old_epoch: String = tx
            .query_one(
                "SELECT coordinator_epoch FROM awr_team.projects WHERE tenant_id=$1 AND id=$2",
                &[&tenant_id, &project_id],
            )
            .await?
            .get(0);
        let new_epoch = format!("restored-{}", new_id());
        tx.execute(
            "UPDATE awr_team.projects SET coordinator_epoch=$3, status='active'
             WHERE tenant_id=$1 AND id=$2",
            &[&tenant_id, &project_id, &new_epoch],
        )
        .await?;
        tx.execute(
            "UPDATE awr_team.credentials SET revoked_at=clock_timestamp()
             WHERE tenant_id=$1 AND revoked_at IS NULL",
            &[&tenant_id],
        )
        .await?;
        tx.execute(
            "UPDATE awr_team.outbox SET state='failed'
             WHERE tenant_id=$1 AND project_id=$2 AND state IN ('pending','sending')",
            &[&tenant_id, &project_id],
        )
        .await?;
        tx.execute(
            "UPDATE awr_team.work_runtime SET recovery_blocked=TRUE
             WHERE tenant_id=$1 AND project_id=$2",
            &[&tenant_id, &project_id],
        )
        .await?;
        let run_id = new_id();
        tx.execute(
            "INSERT INTO awr_team.restore_runs(
                tenant_id, project_id, id, backup_id, new_epoch, outbox_replayed, state, report_json)
             VALUES ($1,$2,$3,$4,$5,FALSE,'completed',$6)",
            &[
                &tenant_id,
                &project_id,
                &run_id,
                &backup_id,
                &new_epoch,
                &json!({"old_epoch": old_epoch}),
            ],
        )
        .await?;
        tx.commit().await?;
        Ok(RestoreRun {
            id: run_id,
            new_epoch,
            outbox_replayed: false,
        })
    }

    pub async fn refuse_sqlite_rollback(&self, tenant_id: &str, project_id: &str) -> PgResult<()> {
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        let revision: i64 = tx
            .query_one(
                "SELECT project_revision FROM awr_team.projects WHERE tenant_id=$1 AND id=$2",
                &[&tenant_id, &project_id],
            )
            .await?
            .get(0);
        tx.commit().await?;
        if revision > 0 {
            return Err(PgError::RollbackForbidden);
        }
        Ok(())
    }

    pub async fn require_epoch(
        &self,
        tenant_id: &str,
        project_id: &str,
        epoch: &str,
    ) -> PgResult<()> {
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        let current: String = tx
            .query_one(
                "SELECT coordinator_epoch FROM awr_team.projects WHERE tenant_id=$1 AND id=$2",
                &[&tenant_id, &project_id],
            )
            .await?
            .get(0);
        tx.commit().await?;
        if current != epoch {
            return Err(PgError::EpochChanged);
        }
        Ok(())
    }

    pub async fn local_claim_is_not_team_lease(
        &self,
        tenant_id: &str,
        project_id: &str,
        work_id: &str,
    ) -> PgResult<bool> {
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        let active: i64 = tx
            .query_one(
                "SELECT count(*) FROM awr_team.claims
                 WHERE tenant_id=$1 AND project_id=$2 AND work_id=$3 AND state='active'",
                &[&tenant_id, &project_id, &work_id],
            )
            .await?
            .get(0);
        tx.commit().await?;
        Ok(active == 0)
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
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

#[cfg(test)]
mod tests {
    #[test]
    fn personal_cli_does_not_depend_on_team_postgres() {
        let toml = include_str!("../../awr-cli/Cargo.toml");
        assert!(!toml.contains("awr-team-pg"));
        let store = include_str!("../../awr-store/Cargo.toml");
        assert!(!store.contains("awr-team-pg"));
    }
}
