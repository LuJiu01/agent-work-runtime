use awr_team::*;
use serde_json::json;

fn auth() -> AuthContext {
    AuthContext {
        tenant_id: "tenant-a".into(),
        project_id: "project-a".into(),
        actor_id: "actor-a".into(),
        client_id: "client-a".into(),
    }
}

fn remote() -> RemoteProfile {
    RemoteProfile {
        name: "prod".into(),
        endpoint: "https://awr.example/team/v1".into(),
        project_key: "alpha".into(),
        credential_env: "AWR_TEAM_TOKEN".into(),
        protocol_version: 1,
    }
}

#[test]
fn old_client_without_protocol_is_rejected() {
    let err = parse_envelope(&json!({"op":"work.claim","request_id":"r1"})).unwrap_err();
    assert_eq!(err, TeamError::ProtocolUnsupported);
    assert_eq!(err.code(), "PROTOCOL_UNSUPPORTED");
}

#[test]
fn body_cannot_override_authorized_project() {
    let err = authorize(
        &auth(),
        &json!({"project_id":"other","protocol_version":1,"request_id":"r","op":"work.claim"}),
    )
    .unwrap_err();
    assert_eq!(err, TeamError::AuthProjectMismatch);
}

#[test]
fn missing_project_selection_is_rejected() {
    let mut missing = auth();
    missing.project_id.clear();
    let err = authorize(&missing, &json!({})).unwrap_err();
    assert_eq!(err, TeamError::ProjectRequired);
}

#[test]
fn offline_or_missing_remote_cannot_claim() {
    let env = parse_envelope(&json!({
        "protocol_version": 1,
        "request_id": "r1",
        "op": "work.claim",
        "args": {"work_id":"work-a","scope_id":"main","session_id":"s1","expected_work_version":"1","expected_contract_hash":"h"}
    }))
    .unwrap();
    let err = execute("cli", env.clone(), &auth(), None, true).unwrap_err();
    assert_eq!(err, TeamError::OfflineWriteForbidden);
    let err = execute("cli", env, &auth(), Some(&remote()), false).unwrap_err();
    assert_eq!(err, TeamError::OfflineWriteForbidden);
}

#[test]
fn three_surfaces_share_error_and_success_semantics() {
    let env = parse_envelope(&json!({
        "protocol_version": 1,
        "request_id": "r1",
        "op": "work.claim",
        "args": {"work_id":"work-a","scope_id":"main","session_id":"s1","expected_work_version":"1","expected_contract_hash":"h"}
    }))
    .unwrap();
    same_error_on_all_surfaces(env.clone(), &auth(), None, true).unwrap_err();
    same_error_on_all_surfaces(env, &auth(), Some(&remote()), true).unwrap();
}

#[test]
fn profile_does_not_store_secrets_or_postgres_dsn() {
    let mut bad = remote();
    bad.credential_env = "postgres://user:pass@localhost/db".into();
    assert_eq!(bad.validate().unwrap_err(), TeamError::SecretRefInvalid);
    let ok = remote();
    let redacted = ok.redacted().to_string();
    assert!(!redacted.contains("password"));
    assert!(redacted.contains("AWR_TEAM_TOKEN"));
}

#[test]
fn u64_versions_round_trip_without_float_rounding() {
    let value = u64::MAX;
    let encoded = encode_u64(value);
    assert_eq!(encoded, "18446744073709551615");
    assert_eq!(decode_u64(&encoded).unwrap(), value);
    assert!(decode_u64("18446744073709551616").is_err());
}

#[test]
fn unknown_claim_fields_are_rejected() {
    let err = parse_envelope(&json!({
        "protocol_version": 1,
        "request_id": "r1",
        "op": "work.claim",
        "args": {"work_id":"work-a","extra_bypass": true}
    }))
    .unwrap_err();
    assert!(matches!(err, TeamError::UnknownRequiredField(_)));
}

#[test]
fn personal_crates_do_not_depend_on_team_postgres() {
    let cli = include_str!("../../awr-cli/Cargo.toml");
    let mcp = include_str!("../../awr-mcp/Cargo.toml");
    assert!(!cli.contains("awr-team-pg"));
    assert!(!mcp.contains("awr-team-pg"));
}
