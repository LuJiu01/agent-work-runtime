//! Regression tests for the retrospective CR of PR #34 (TEAM-P1).
//! Covers the three confirmed P2 findings through the real deserialization
//! entry points, plus characterization of the completion input contract.
use awr_team::*;
use serde_json::{Value, json};

fn valid_contract_json() -> Value {
    json!({
        "codec": WorkContract::CODEC,
        "work_id": "work-orders",
        "external_key": "ORDERS-1",
        "goals": ["ship"],
        "hard_rules": ["no-prod-write"],
        "scope_paths": ["src/orders.rs"],
        "acceptance": ["query returns today's orders"],
        "required_dependencies": [],
        "completion_policy": "trusted_execution_and_review",
        "verification_requirements": ["cargo test"]
    })
}

// P2-1: typed ID deserialization must enforce the same rules as `new()`.
macro_rules! id_deser_case {
    ($test_name:ident, $name:ident) => {
        #[test]
        fn $test_name() {
            assert!(serde_json::from_value::<$name>(json!("")).is_err());
            assert!(serde_json::from_value::<$name>(json!("x".repeat(129))).is_err());
            assert!(serde_json::from_value::<$name>(json!("a\nb")).is_err());
            assert!(serde_json::from_value::<$name>(json!("a\u{0}b")).is_err());
            let ok = serde_json::from_value::<$name>(json!("valid-1")).unwrap();
            assert_eq!(ok.as_str(), "valid-1");
        }
    };
}
id_deser_case!(tenant_id_deserialization_is_validated, TenantId);
id_deser_case!(project_id_deserialization_is_validated, ProjectId);
id_deser_case!(scope_id_deserialization_is_validated, ScopeId);
id_deser_case!(work_id_deserialization_is_validated, WorkId);
id_deser_case!(actor_id_deserialization_is_validated, ActorId);
id_deser_case!(session_id_deserialization_is_validated, SessionId);
id_deser_case!(request_id_deserialization_is_validated, RequestId);

// P2-2: the real WorkContract entry rejects unknown fields.
#[test]
fn unknown_contract_fields_are_rejected_at_the_real_entry() {
    let mut with_unknown = valid_contract_json();
    // Test-only input: a constraint V1 does not know. It must be rejected,
    // not dropped before hashing.
    with_unknown["required_security_policy"] = json!({"minimum_independent_approvals": 2});
    assert!(serde_json::from_value::<WorkContract>(with_unknown).is_err());
}

#[test]
fn valid_contract_deserializes_and_hashes_stably() {
    let contract = serde_json::from_value::<WorkContract>(valid_contract_json()).unwrap();
    let again = serde_json::from_value::<WorkContract>(valid_contract_json()).unwrap();
    assert_eq!(contract.hash().unwrap(), again.hash().unwrap());
}

// P2-3: decode_u64 accepts only the canonical decimal form.
#[test]
fn decode_u64_canonical_form_only() {
    assert_eq!(decode_u64("0").unwrap(), 0);
    assert_eq!(decode_u64("1").unwrap(), 1);
    assert_eq!(decode_u64(&u64::MAX.to_string()).unwrap(), u64::MAX);
    for bad in [
        "", "01", "00", "+1", "+01", "+00", " 1", "1 ", "1.0", "１２",
    ] {
        assert!(decode_u64(bad).is_err(), "accepted non-canonical {bad:?}");
    }
    assert!(decode_u64("18446744073709551616").is_err()); // u64::MAX + 1
}

// Boundary note: characterization of the completion input contract.
// `artifact_digest` is caller-bound; the pure function does not re-check it.
#[test]
fn completion_ignores_artifact_digest_by_documented_contract() {
    let review = ReviewPolicy {
        required: true,
        author_may_self_approve: false,
        approved: true,
        reviewer_is_author: false,
    };
    let bundle = |digest: Option<&str>| EvidenceBundle {
        grade: EvidenceGrade::TrustedExecutionReceipt,
        contract_hash: "contract-a".into(),
        artifact_digest: digest.map(str::to_owned),
        accessible: true,
    };
    for digest in [None, Some("artifact-before"), Some("artifact-after")] {
        let view = current_completion(
            true,
            true,
            Some("contract-a"),
            "contract-a",
            true,
            Some(&bundle(digest)),
            &review,
        );
        assert_eq!(view, CompletionView::CurrentlyVerified);
    }
}
