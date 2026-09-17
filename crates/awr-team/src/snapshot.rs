use crate::completion::CompletionView;
use crate::ids::{ActorId, ProjectId, ScopeId, SessionId, TenantId, WorkId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequiredDependencyProof {
    pub work_id: WorkId,
    pub completion: CompletionView,
    pub receipt_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimPreconditions {
    pub tenant_id: TenantId,
    pub project_id: ProjectId,
    pub scope_id: ScopeId,
    pub work_id: WorkId,
    pub actor_id: ActorId,
    pub session_id: SessionId,
    pub expected_contract_hash: String,
    pub expected_work_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseProof {
    pub coordinator_epoch: String,
    pub claim_id: String,
    pub session_id: SessionId,
    pub actor_id: ActorId,
    pub fence: String,
    pub lease_version: String,
    pub expires_at_millis: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceActivationPlan {
    pub candidate_digest: String,
    pub parser_version: String,
    pub expected_authority_epoch: String,
    pub approved_candidate_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectReadSnapshot {
    pub coordinator_epoch: String,
    pub authority_epoch: String,
    pub authority_snapshot_id: String,
    pub work_id: WorkId,
    pub work_version: String,
    pub contract_hash: String,
    pub completion: CompletionView,
    pub source_declared_completed: bool,
}
