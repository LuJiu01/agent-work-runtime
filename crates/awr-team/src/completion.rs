use serde::{Deserialize, Serialize};

/// Caller-declared reports never become trusted execution receipts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceGrade {
    AgentSelfReport,
    TrustedExecutionReceipt,
    AuthorizedReview,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceBundle {
    pub grade: EvidenceGrade,
    pub contract_hash: String,
    pub artifact_digest: Option<String>,
    pub accessible: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewPolicy {
    pub required: bool,
    pub author_may_self_approve: bool,
    pub approved: bool,
    pub reviewer_is_author: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionView {
    /// YAML/source status only. Must not release required downstream work.
    SourceDeclared,
    /// A receipt existed; it is not valid for the current contract.
    Historical,
    /// Current contract, dependencies, artifacts and review all match.
    CurrentlyVerified,
    NeedsRevalidation,
}

pub fn current_completion(
    source_declared_completed: bool,
    historical_receipt: bool,
    receipt_contract_hash: Option<&str>,
    current_contract_hash: &str,
    dependencies_valid: bool,
    bundle: Option<&EvidenceBundle>,
    review: &ReviewPolicy,
) -> CompletionView {
    if historical_receipt {
        if receipt_contract_hash != Some(current_contract_hash) {
            return CompletionView::NeedsRevalidation;
        }
        if !dependencies_valid {
            return CompletionView::NeedsRevalidation;
        }
        let Some(bundle) = bundle else {
            return CompletionView::NeedsRevalidation;
        };
        if bundle.contract_hash != current_contract_hash || !bundle.accessible {
            return CompletionView::NeedsRevalidation;
        }
        if bundle.grade == EvidenceGrade::AgentSelfReport {
            return if source_declared_completed {
                CompletionView::SourceDeclared
            } else {
                CompletionView::Historical
            };
        }
        if review.required
            && (!review.approved || (!review.author_may_self_approve && review.reviewer_is_author))
        {
            return CompletionView::NeedsRevalidation;
        }
        return CompletionView::CurrentlyVerified;
    }
    if source_declared_completed {
        CompletionView::SourceDeclared
    } else {
        CompletionView::Historical
    }
}
