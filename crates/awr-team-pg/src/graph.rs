use crate::error::{PgError, PgResult};
use crate::tx::{bind_scope, new_id};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyEdge {
    pub from: String,
    pub to: String,
    pub relation: String,
    pub required: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct SplitProposal {
    pub id: String,
    pub parent_work_id: String,
    pub child_work_ids: Vec<String>,
}

pub fn paths_conflict(kind_a: &str, key_a: &str, kind_b: &str, key_b: &str) -> bool {
    if kind_a == "named" || kind_b == "named" {
        return kind_a == kind_b && key_a == key_b;
    }
    if kind_a == "file" && kind_b == "file" {
        return canonicalize(key_a) == canonicalize(key_b);
    }
    segment_prefix_overlap(&canonicalize(key_a), &canonicalize(key_b))
}

fn canonicalize(path: &str) -> String {
    path.trim_matches('/').replace('\\', "/")
}

fn segment_prefix_overlap(a: &str, b: &str) -> bool {
    let aa = a.split('/').filter(|s| !s.is_empty()).collect::<Vec<_>>();
    let bb = b.split('/').filter(|s| !s.is_empty()).collect::<Vec<_>>();
    if aa.is_empty() || bb.is_empty() {
        return false;
    }
    let n = aa.len().min(bb.len());
    aa[..n] == bb[..n]
}

pub fn validate_required_graph(nodes: &[String], edges: &[DependencyEdge]) -> PgResult<()> {
    let known: HashSet<&str> = nodes.iter().map(|n| n.as_str()).collect();
    let mut incoming: HashMap<&str, usize> = nodes.iter().map(|n| (n.as_str(), 0usize)).collect();
    let mut outgoing: HashMap<&str, Vec<&str>> = HashMap::new();
    for edge in edges.iter().filter(|e| e.required) {
        if !known.contains(edge.from.as_str()) || !known.contains(edge.to.as_str()) {
            return Err(PgError::MissingDependency);
        }
        if edge.from == edge.to {
            return Err(PgError::DependencyCycle);
        }
        *incoming.entry(edge.to.as_str()).or_default() += 1;
        outgoing
            .entry(edge.from.as_str())
            .or_default()
            .push(edge.to.as_str());
    }
    let mut queue: VecDeque<&str> = incoming
        .iter()
        .filter(|(_, c)| **c == 0)
        .map(|(n, _)| *n)
        .collect();
    let mut seen = 0usize;
    while let Some(node) = queue.pop_front() {
        seen += 1;
        if let Some(next) = outgoing.get(node) {
            for child in next {
                let count = incoming.get_mut(child).expect("node");
                *count -= 1;
                if *count == 0 {
                    queue.push_back(child);
                }
            }
        }
    }
    let required_nodes = incoming.len();
    if seen != required_nodes {
        return Err(PgError::DependencyCycle);
    }
    Ok(())
}

pub fn require_main_scope(scope_id: &str) -> PgResult<()> {
    if scope_id != "main" {
        return Err(PgError::ScopeUnsupported);
    }
    Ok(())
}

pub struct GraphStore {
    pool: crate::PgPool,
}

impl GraphStore {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            pool: crate::PgPool::new(url),
        }
    }

    async fn connect(&self) -> PgResult<crate::PgClient> {
        self.pool.get().await
    }

    pub async fn replace_edges(
        &self,
        tenant_id: &str,
        project_id: &str,
        snapshot_id: &str,
        scope_id: &str,
        nodes: &[String],
        edges: &[DependencyEdge],
    ) -> PgResult<()> {
        require_main_scope(scope_id)?;
        validate_required_graph(nodes, edges)?;
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        tx.execute(
            "DELETE FROM awr_team.dependency_edges
             WHERE tenant_id=$1 AND project_id=$2 AND snapshot_id=$3 AND scope_id=$4",
            &[&tenant_id, &project_id, &snapshot_id, &scope_id],
        )
        .await?;
        for edge in edges {
            tx.execute(
                "INSERT INTO awr_team.dependency_edges(
                    tenant_id, project_id, snapshot_id, scope_id, from_work_id, to_work_id,
                    relation, required)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
                &[
                    &tenant_id,
                    &project_id,
                    &snapshot_id,
                    &scope_id,
                    &edge.from,
                    &edge.to,
                    &edge.relation,
                    &edge.required,
                ],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn reserve(
        &self,
        tenant_id: &str,
        project_id: &str,
        work_id: &str,
        kind: &str,
        key: &str,
    ) -> PgResult<String> {
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        let rows = tx
            .query(
                "SELECT resource_kind, canonical_key FROM awr_team.resource_reservations
                 WHERE tenant_id=$1 AND project_id=$2 AND state IN ('reserved', 'unknown')",
                &[&tenant_id, &project_id],
            )
            .await?;
        for row in rows {
            let existing_kind: String = row.get(0);
            let existing_key: String = row.get(1);
            if paths_conflict(kind, key, &existing_kind, &existing_key) {
                return Err(PgError::ResourceConflict);
            }
        }
        let id = new_id();
        tx.execute(
            "INSERT INTO awr_team.resource_reservations(
                tenant_id, project_id, id, work_id, resource_kind, canonical_key, state)
             VALUES ($1,$2,$3,$4,$5,$6,'reserved')",
            &[&tenant_id, &project_id, &id, &work_id, &kind, &key],
        )
        .await?;
        tx.commit().await?;
        Ok(id)
    }

    pub async fn propose_split(
        &self,
        tenant_id: &str,
        project_id: &str,
        parent_work_id: &str,
        children: &[String],
        mapping: &Value,
    ) -> PgResult<SplitProposal> {
        if children.is_empty() {
            return Err(PgError::Protocol("split requires children".into()));
        }
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        let parent = tx
            .query_opt(
                "SELECT c.contract_json FROM awr_team.work_contracts c
                 JOIN awr_team.projects p
                   ON p.tenant_id=c.tenant_id AND p.id=c.project_id
                  AND p.active_snapshot_id=c.snapshot_id
                 WHERE c.tenant_id=$1 AND c.project_id=$2 AND c.work_id=$3",
                &[&tenant_id, &project_id, &parent_work_id],
            )
            .await?
            .ok_or_else(|| PgError::Protocol("parent contract missing".into()))?;
        let parent_json: Value = parent.get(0);
        for child in children {
            tx.execute(
                "INSERT INTO awr_team.work_items(tenant_id, project_id, id, external_key)
                 VALUES ($1,$2,$3,$3)
                 ON CONFLICT (tenant_id, project_id, id) DO NOTHING",
                &[&tenant_id, &project_id, &child],
            )
            .await?;
            let mut child_json = parent_json.clone();
            if let Some(obj) = child_json.as_object_mut() {
                obj.insert("work_id".into(), json!(child));
                obj.insert("external_key".into(), json!(child));
            }
            let snapshot: String = tx
                .query_one(
                    "SELECT active_snapshot_id FROM awr_team.projects
                     WHERE tenant_id=$1 AND id=$2",
                    &[&tenant_id, &project_id],
                )
                .await?
                .get(0);
            tx.execute(
                "INSERT INTO awr_team.work_contracts(
                    tenant_id, project_id, snapshot_id, scope_id, work_id,
                    contract_hash, definition_state, title, contract_json)
                 VALUES ($1,$2,$3,'main',$4,$4,'enabled',$4,$5)
                 ON CONFLICT (tenant_id, project_id, snapshot_id, scope_id, work_id) DO NOTHING",
                &[&tenant_id, &project_id, &snapshot, &child, &child_json],
            )
            .await?;
            tx.execute(
                "INSERT INTO awr_team.dependency_edges(
                    tenant_id, project_id, snapshot_id, scope_id, from_work_id, to_work_id,
                    relation, required)
                 VALUES ($1,$2,$3,'main',$4,$5,'split-child',true)
                 ON CONFLICT DO NOTHING",
                &[&tenant_id, &project_id, &snapshot, &parent_work_id, &child],
            )
            .await?;
        }
        let id = new_id();
        let child_json = json!(children);
        tx.execute(
            "INSERT INTO awr_team.split_proposals(
                tenant_id, project_id, id, parent_work_id, child_work_ids, mapping_json, state)
             VALUES ($1,$2,$3,$4,$5,$6,'accepted')",
            &[
                &tenant_id,
                &project_id,
                &id,
                &parent_work_id,
                &child_json,
                mapping,
            ],
        )
        .await?;
        tx.commit().await?;
        Ok(SplitProposal {
            id,
            parent_work_id: parent_work_id.into(),
            child_work_ids: children.to_vec(),
        })
    }

    pub async fn complete_parent_from_children(
        &self,
        tenant_id: &str,
        project_id: &str,
        parent_work_id: &str,
    ) -> PgResult<()> {
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        let _proposal = tx
            .query_opt(
                "SELECT id FROM awr_team.split_proposals
                 WHERE tenant_id=$1 AND project_id=$2 AND parent_work_id=$3 AND state='accepted'",
                &[&tenant_id, &project_id, &parent_work_id],
            )
            .await?
            .ok_or_else(|| PgError::Protocol("split proposal missing".into()))?;
        tx.commit().await?;
        Err(PgError::ParentEvidenceRequired)
    }

    pub async fn bind_dependency(
        &self,
        tenant_id: &str,
        project_id: &str,
        downstream: &str,
        upstream: &str,
        binding_hash: &str,
    ) -> PgResult<()> {
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        tx.execute(
            "INSERT INTO awr_team.dependency_bindings(
                tenant_id, project_id, downstream_work_id, upstream_work_id, binding_hash, valid)
             VALUES ($1,$2,$3,$4,$5,true)
             ON CONFLICT (tenant_id, project_id, downstream_work_id, upstream_work_id)
             DO UPDATE SET binding_hash=$5, valid=true",
            &[
                &tenant_id,
                &project_id,
                &downstream,
                &upstream,
                &binding_hash,
            ],
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn invalidate_downstream(
        &self,
        tenant_id: &str,
        project_id: &str,
        upstream: &str,
    ) -> PgResult<u64> {
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        let count = tx
            .execute(
                "UPDATE awr_team.dependency_bindings SET valid=false
                 WHERE tenant_id=$1 AND project_id=$2 AND upstream_work_id=$3 AND valid=true",
                &[&tenant_id, &project_id, &upstream],
            )
            .await?;
        tx.commit().await?;
        Ok(count)
    }

    pub async fn current_binding_valid(
        &self,
        tenant_id: &str,
        project_id: &str,
        downstream: &str,
        upstream: &str,
    ) -> PgResult<bool> {
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        let row = tx
            .query_opt(
                "SELECT valid FROM awr_team.dependency_bindings
                 WHERE tenant_id=$1 AND project_id=$2 AND downstream_work_id=$3 AND upstream_work_id=$4",
                &[&tenant_id, &project_id, &downstream, &upstream],
            )
            .await?;
        tx.commit().await?;
        match row {
            Some(row) => Ok(row.get(0)),
            None => Err(PgError::BindingInvalid),
        }
    }

    pub async fn activation_blocked_by_claims(
        &self,
        tenant_id: &str,
        project_id: &str,
        work_id: &str,
        new_contract_hash: &str,
    ) -> PgResult<()> {
        let mut client = self.connect().await?;
        let tx = client.transaction().await?;
        bind_scope(&tx, tenant_id, project_id).await?;
        let claimed: i64 = tx
            .query_one(
                "SELECT count(*) FROM awr_team.claims
                 WHERE tenant_id=$1 AND project_id=$2 AND work_id=$3 AND state='active'",
                &[&tenant_id, &project_id, &work_id],
            )
            .await?
            .get(0);
        if claimed == 0 {
            tx.commit().await?;
            return Ok(());
        }
        let current = tx
            .query_opt(
                "SELECT c.contract_hash FROM awr_team.work_contracts c
                 JOIN awr_team.projects p
                   ON p.tenant_id=c.tenant_id AND p.id=c.project_id
                  AND p.active_snapshot_id=c.snapshot_id
                 WHERE c.tenant_id=$1 AND c.project_id=$2 AND c.work_id=$3",
                &[&tenant_id, &project_id, &work_id],
            )
            .await?;
        tx.commit().await?;
        if let Some(row) = current {
            let hash: String = row.get(0);
            if hash != new_contract_hash {
                return Err(PgError::ClaimBlocksActivation);
            }
        }
        Ok(())
    }

    pub async fn graph_within_budget(
        &self,
        edges: &[DependencyEdge],
        max_edges: usize,
    ) -> PgResult<()> {
        if edges.len() > max_edges {
            return Err(PgError::GraphBudgetExceeded);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_overlaps_segment_wise_not_string_prefix() {
        assert!(paths_conflict(
            "prefix",
            "src/foo",
            "file",
            "src/foo/bar.rs"
        ));
        assert!(!paths_conflict("prefix", "src/a", "file", "src/abc"));
        assert!(paths_conflict("file", "src/a.rs", "file", "src/a.rs"));
        assert!(!paths_conflict("named", "lock-a", "named", "lock-b"));
    }

    #[test]
    fn required_cycle_and_missing_nodes_are_rejected() {
        let nodes = vec!["a".into(), "b".into()];
        let cycle = vec![
            DependencyEdge {
                from: "a".into(),
                to: "b".into(),
                relation: "requires".into(),
                required: true,
            },
            DependencyEdge {
                from: "b".into(),
                to: "a".into(),
                relation: "requires".into(),
                required: true,
            },
        ];
        assert!(matches!(
            validate_required_graph(&nodes, &cycle),
            Err(PgError::DependencyCycle)
        ));
        let missing = vec![DependencyEdge {
            from: "a".into(),
            to: "z".into(),
            relation: "requires".into(),
            required: true,
        }];
        assert!(matches!(
            validate_required_graph(&nodes, &missing),
            Err(PgError::MissingDependency)
        ));
        assert!(require_main_scope("feature").is_err());
        assert!(require_main_scope("main").is_ok());
    }
}
