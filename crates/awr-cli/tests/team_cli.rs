use serde_json::Value;
use std::process::Command;

fn awr() -> Command {
    Command::new(env!("CARGO_BIN_EXE_awr"))
}

#[test]
fn team_claim_without_remote_does_not_succeed_locally() {
    let output = awr()
        .args([
            "--json",
            "team",
            "command",
            "--body",
            r#"{"protocol_version":1,"request_id":"r1","op":"work.claim","args":{"work_id":"work-a","scope_id":"main","session_id":"s1","expected_work_version":"1","expected_contract_hash":"h"}}"#,
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["code"], "Unsupported");
    assert!(error["message"].as_str().unwrap().contains("team remote"));
}

#[test]
fn remote_add_rejects_database_urls() {
    let dir = tempfile_dir();
    let output = awr()
        .args([
            "--json",
            "--project",
            &dir,
            "remote",
            "add",
            "prod",
            "--endpoint",
            "postgres://postgres:awr-test@127.0.0.1/awr",
            "--project-key",
            "alpha",
            "--credential-env",
            "AWR_TEAM_TOKEN",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["code"], "RuleViolation");
}

fn tempfile_dir() -> String {
    let path = std::env::temp_dir().join(format!("awr-team-cli-{}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    path.to_string_lossy().into_owned()
}
