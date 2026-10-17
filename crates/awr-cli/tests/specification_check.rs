use awr_core::Id;
use serde_json::{Value, json};
use sha2::Digest;
use std::{fs, path::PathBuf, process::Command};

struct Project(PathBuf);
impl Drop for Project {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
impl Project {
    fn new(items: Value, mut works: Value) -> Self {
        let p = Self(std::env::temp_dir().join(format!("awr-spec-check-{}", Id::new())));
        fs::create_dir_all(p.0.join("decisions")).unwrap();
        fs::write(p.0.join("mapping.toml"), "[project]\nname='Explicit specification fixture'\ncontext_profile='minimal'\n[[sources]]\ndomain='ledger'\nrole='primary'\npath='work.yaml'\nadapter='yaml-ledger-v1'\n[[sources]]\ndomain='decisions'\nrole='supporting'\npath='decisions'\nadapter='markdown-directory-v1'\n").unwrap();
        p.spec("accepted", "0.80", items);
        let fingerprint = format!(
            "sha256:{:x}",
            sha2::Sha256::digest(fs::read(p.0.join("decisions/dashboard.md")).unwrap())
        );
        for work in works.as_array_mut().unwrap() {
            if let Some(refs) = work.get_mut("spec_refs").and_then(Value::as_array_mut) {
                for r in refs {
                    if r["specification"] == "dashboard" && r["approved_version"] == "0.80" {
                        r["source_fingerprint"] = json!(fingerprint);
                    }
                }
            }
        }
        fs::write(
            p.0.join("work.yaml"),
            serde_json::to_vec_pretty(&json!({"work_items":works})).unwrap(),
        )
        .unwrap();
        p.ok(&["init", "--manifest", "mapping.toml", "--accept"]);
        p
    }
    fn spec(&self, status: &str, version: &str, items: Value) {
        let metadata = json!({"id":"dashboard","status":status,"specification":{"version":version,"items":items}});
        fs::write(
            self.0.join("decisions/dashboard.md"),
            format!("---\n{}\n---\n# Dashboard\n\n## Decision\nUse the explicitly approved requirements.\n", serde_json::to_string_pretty(&metadata).unwrap()),
        )
        .unwrap();
    }
    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_awr"))
            .args(["--project", self.0.to_str().unwrap(), "--json"])
            .args(args)
            .output()
            .unwrap()
    }
    fn ok(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
    fn logical_state(&self) -> (i64, i64, i64, i64) {
        let db = rusqlite::Connection::open(self.0.join(".awr/state.db")).unwrap();
        db.query_row("SELECT (SELECT project_revision FROM projects LIMIT 1), (SELECT count(*) FROM events), (SELECT count(*) FROM sessions), (SELECT count(*) FROM claims)", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap()
    }
}

fn reference(item: &str, version: &str) -> Value {
    json!({"specification":"dashboard","item":item,"approved_version":version,"source_fingerprint":format!("sha256:{}","a".repeat(64)),"criterion":"Save the reviewed configuration"})
}

#[test]
fn superseded_reference_is_reported_without_rewriting_sources_or_runtime() {
    let p = Project::new(
        json!([
            {"id":"C64","disposition":"superseded","superseded_by":"C90"},
            {"id":"C90","disposition":"required"}
        ]),
        json!([{"id":"W","title":"Save configuration","status":"planned","acceptance":["Save the reviewed configuration"],"spec_refs":[reference("C64","0.74")]}]),
    );
    let source = fs::read(p.0.join("work.yaml")).unwrap();
    let spec = fs::read(p.0.join("decisions/dashboard.md")).unwrap();
    let state = p.logical_state();
    let result = p.ok(&["spec", "check"]);
    assert_eq!(result["status"], "needs_review");
    assert_eq!(result["references"][0]["work"], "W");
    assert_eq!(result["references"][0]["status"], "needs_review");
    assert_eq!(result["references"][0]["replacement"], "C90");
    assert!(
        result["references"][0]["reasons"]
            .as_array()
            .unwrap()
            .contains(&json!("approved_version_changed"))
    );
    assert_eq!(result["read_only"], true);
    assert_eq!(fs::read(p.0.join("work.yaml")).unwrap(), source);
    assert_eq!(fs::read(p.0.join("decisions/dashboard.md")).unwrap(), spec);
    assert_eq!(p.logical_state(), state);
}

#[test]
fn future_coverage_distinguishes_missing_work_cancelled_and_out_of_scope() {
    let items = (1..=10)
        .map(|i| json!({"id":format!("F{i}"),"disposition":"future"}))
        .chain([
            json!({"id":"CANCELLED","disposition":"cancelled"}),
            json!({"id":"OUT","disposition":"out_of_scope"}),
        ])
        .collect::<Vec<_>>();
    let works=(1..=9).map(|i|json!({"id":format!("W{i}"),"status":"planned","acceptance":["Save the reviewed configuration"],"spec_refs":[reference(&format!("F{i}"),"0.80")]})).collect::<Vec<_>>();
    let p = Project::new(json!(items), json!(works));
    let result = p.ok(&["spec", "check", "--spec", "dashboard"]);
    let requirements = result["requirements"].as_array().unwrap();
    let missing = requirements
        .iter()
        .filter(|r| r["status"] == "uncovered")
        .collect::<Vec<_>>();
    assert_eq!(missing.len(), 1);
    assert_eq!(missing[0]["item"], "F10");
    for key in ["CANCELLED", "OUT"] {
        assert_eq!(
            requirements.iter().find(|r| r["item"] == key).unwrap()["status"],
            "not_required"
        );
    }
    assert_eq!(result["counts"]["covered"], 9);
    assert_eq!(result["counts"]["uncovered"], 1);
}

#[test]
fn unregistered_specification_and_unresolved_criterion_stay_unknown_despite_fresh_sources() {
    let mut unknown = reference("C90", "0.80");
    unknown["specification"] = json!("unregistered");
    let mut bad_criterion = reference("C90", "0.80");
    bad_criterion["criterion"] = json!("Not an acceptance criterion");
    let p = Project::new(
        json!([{"id":"C90","disposition":"required"}]),
        json!([
            {"id":"W","status":"planned","acceptance":["Save the reviewed configuration"],"spec_refs":[unknown,bad_criterion,reference("MISSING","0.80")]}
        ]),
    );
    let result = p.ok(&["spec", "check"]);
    assert_eq!(result["status"], "unknown");
    assert_eq!(result["references"].as_array().unwrap().len(), 3);
    assert!(
        result["references"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["status"] == "unknown")
    );
    let unregistered = p.ok(&["spec", "check", "--spec", "unregistered"]);
    assert_eq!(unregistered["status"], "unknown");
    assert!(
        unregistered["diagnostics"]
            .as_array()
            .unwrap()
            .contains(&json!("specification_not_registered"))
    );
}

#[test]
fn changing_bytes_without_bumping_version_requires_review_and_stale_reads_write_nothing() {
    let p = Project::new(
        json!([{"id":"C90","disposition":"required"}]),
        json!([
            {"id":"W","status":"planned","acceptance":["Save the reviewed configuration"],"spec_refs":[reference("C90","0.80")]}
        ]),
    );
    assert_eq!(p.ok(&["spec", "check"])["status"], "matched");
    let path = p.0.join("decisions/dashboard.md");
    let mut bytes = fs::read_to_string(&path).unwrap();
    bytes.push_str("\nAn explicitly revised boundary.\n");
    fs::write(&path, bytes).unwrap();
    let before = p.logical_state();
    let stale = p.run(&["spec", "check"]);
    assert!(!stale.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&stale.stderr).unwrap()["code"],
        "SourceStale"
    );
    assert_eq!(p.logical_state(), before);
    p.ok(&["source", "reindex"]);
    let current = p.logical_state();
    let r = p.ok(&["spec", "check"]);
    assert_eq!(r["references"][0]["status"], "needs_review");
    assert_eq!(
        r["references"][0]["reasons"],
        json!(["approved_source_changed"])
    );
    assert_eq!(p.logical_state(), current);
}

#[test]
fn proposed_decisions_and_replacement_cycles_never_prove_coverage() {
    let p = Project::new(
        json!([{"id":"C90","disposition":"required"}]),
        json!([
            {"id":"W","status":"planned","acceptance":["Save the reviewed configuration"],"spec_refs":[reference("C90","0.80")]}
        ]),
    );
    p.spec(
        "proposed",
        "0.80",
        json!([{"id":"C90","disposition":"required"}]),
    );
    p.ok(&["source", "reindex"]);
    let r = p.ok(&["spec", "check"]);
    assert_eq!(r["status"], "unknown");
    assert_eq!(r["counts"]["covered"], 0);
    p.spec(
        "accepted",
        "0.80",
        json!([
            {"id":"C90","disposition":"superseded","superseded_by":"C64"},
            {"id":"C64","disposition":"superseded","superseded_by":"C90"}
        ]),
    );
    p.ok(&["source", "reindex"]);
    let r = p.ok(&["spec", "check"]);
    assert_eq!(r["status"], "unknown");
    assert!(
        r["references"][0]["reasons"]
            .as_array()
            .unwrap()
            .contains(&json!("replacement_cycle"))
    );
    assert!(
        r["requirements"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["status"] == "unknown")
    );
}

#[test]
fn cancelled_work_does_not_cover_a_future_item_and_limit_never_silently_truncates() {
    let p = Project::new(
        json!([{"id":"C90","disposition":"future"}]),
        json!([
            {"id":"W","status":"cancelled","acceptance":["Save the reviewed configuration"],"spec_refs":[reference("C90","0.80")]}
        ]),
    );
    let r = p.ok(&["spec", "check"]);
    assert_eq!(r["counts"]["covered"], 0);
    assert_eq!(r["requirements"][0]["status"], "needs_review");
    let limited = p.run(&["spec", "check", "--limit", "1"]);
    assert!(!limited.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&limited.stderr).unwrap()["code"],
        "BudgetExceeded"
    );
}

#[test]
fn legacy_ledger_and_unregistered_files_do_not_become_specification_authority() {
    let p = Project::new(
        json!([{"id":"C90","disposition":"future"}]),
        json!([{"id":"W","status":"planned"}]),
    );
    fs::remove_file(p.0.join("decisions/dashboard.md")).unwrap();
    p.ok(&["source", "reindex"]);
    fs::write(
        p.0.join("unregistered.md"),
        "# Approved specification\nC90 is future.\n",
    )
    .unwrap();
    let before = p.logical_state();
    let r = p.ok(&["spec", "check"]);
    assert_eq!(r["status"], "unknown");
    assert_eq!(r["diagnostics"], json!(["no_explicit_specifications"]));
    assert!(r["requirements"].as_array().unwrap().is_empty());
    assert_eq!(p.logical_state(), before);
}

#[test]
fn malformed_contracts_are_rejected_without_promoting_previous_projection() {
    let p = Project::new(
        json!([{"id":"C90","disposition":"future"}]),
        json!([{"id":"W","status":"planned"}]),
    );
    for items in [
        json!([{"id":"C90","disposition":"future"},{"id":"C90","disposition":"cancelled"}]),
        json!([{"id":"C90","disposition":"private-invalid-value"}]),
        json!([{"id":"C90","disposition":"required","private-unknown-field":"private-invalid-value"}]),
    ] {
        p.spec("accepted", "0.80", items);
        let before = p.logical_state();
        let r = p.run(&["spec", "check"]);
        assert!(!r.status.success());
        assert_eq!(
            serde_json::from_slice::<Value>(&r.stderr).unwrap()["code"],
            "SourceStale"
        );
        assert!(!String::from_utf8_lossy(&r.stderr).contains("private-invalid-value"));
        assert_eq!(p.logical_state(), before);
    }
}

#[test]
fn adapter_upgrade_requires_explicit_reindex_and_restores_declared_fields() {
    let p = Project::new(
        json!([{"id":"C90","disposition":"future"}]),
        json!([{"id":"W","status":"planned","acceptance":["Save the reviewed configuration"],"spec_refs":[reference("C90","0.80")]}]),
    );
    let ledger = fs::read(p.0.join("work.yaml")).unwrap();
    let spec = fs::read(p.0.join("decisions/dashboard.md")).unwrap();
    // Simulate the previous adapters: identical source bytes, fields not projected.
    {
        let db = rusqlite::Connection::open(p.0.join(".awr/state.db")).unwrap();
        db.execute_batch("UPDATE sources SET config_json=json_set(config_json,'$.adapter_version',CASE adapter WHEN 'yaml-ledger-v1' THEN 4 ELSE 3 END); UPDATE work_items SET payload_json=json_remove(payload_json,'$.spec_refs'); UPDATE decisions SET payload_json=json_remove(payload_json,'$.specification');").unwrap();
    }
    let before = p.logical_state();
    let stale = p.run(&["spec", "check"]);
    assert!(!stale.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&stale.stderr).unwrap()["code"],
        "SourceStale"
    );
    assert_eq!(p.logical_state(), before);
    p.ok(&["source", "reindex"]);
    let result = p.ok(&["spec", "check"]);
    assert_eq!(result["status"], "matched");
    assert_eq!(result["counts"]["covered"], 1);
    assert_eq!(fs::read(p.0.join("work.yaml")).unwrap(), ledger);
    assert_eq!(fs::read(p.0.join("decisions/dashboard.md")).unwrap(), spec);
}

#[test]
fn reviewed_reference_edits_use_existing_batch_and_active_claim_guards() {
    let p = Project::new(
        json!([{"id":"C90","disposition":"future"}]),
        json!([{"id":"W","status":"planned","acceptance":["Save the reviewed configuration"],"spec_refs":[reference("C90","0.80")]}]),
    );
    let source = p.ok(&["source", "list"])["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["domain"] == "ledger")
        .unwrap()
        .clone();
    let request = json!({"version":1,"request_key":"remove-reviewed-reference","actor":{"host":"fixture","subject":"editor","origin":"ai_accepted"},"reason":"Remove the explicit association after review","change":{"kind":"ledger","source_id":source["id"],"source_fingerprint":source["fingerprint"],"operations":[{"operation":"fields","target":"W","fields":{"spec_refs":[]}}]}});
    fs::write(
        p.0.join("batch.json"),
        serde_json::to_vec(&request).unwrap(),
    )
    .unwrap();
    let preview = p.ok(&["batch", "change", "--input", "batch.json"]);
    p.ok(&[
        "batch",
        "change",
        "--input",
        "batch.json",
        "--accept",
        "--expected-preview",
        preview["preview"]["fingerprint"].as_str().unwrap(),
        "--expected-revision",
        &preview["project_revision"].to_string(),
    ]);
    assert_eq!(p.ok(&["spec", "check"])["counts"]["uncovered"], 1);

    let rev = p.ok(&["session", "list"])["project_revision"].to_string();
    p.ok(&[
        "session",
        "start",
        "--work",
        "W",
        "--claim",
        "--agent",
        "fixture",
        "--provider",
        "synthetic",
        "--model",
        "fixture",
        "--expected-revision",
        &rev,
    ]);
    let source = p.ok(&["source", "list"])["sources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["domain"] == "ledger")
        .unwrap()
        .clone();
    let mut request = request;
    request["request_key"] = json!("restore-reviewed-reference");
    request["change"]["source_fingerprint"] = source["fingerprint"].clone();
    let mut restored = reference("C90", "0.80");
    restored["source_fingerprint"] = json!(format!(
        "sha256:{:x}",
        sha2::Sha256::digest(fs::read(p.0.join("decisions/dashboard.md")).unwrap())
    ));
    request["change"]["operations"][0]["fields"]["spec_refs"] = json!([restored]);
    fs::write(
        p.0.join("batch.json"),
        serde_json::to_vec(&request).unwrap(),
    )
    .unwrap();
    let ledger = fs::read(p.0.join("work.yaml")).unwrap();
    let blocked = p.run(&["batch", "change", "--input", "batch.json"]);
    assert!(!blocked.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&blocked.stderr).unwrap()["code"],
        "ClaimConflict"
    );
    assert_eq!(fs::read(p.0.join("work.yaml")).unwrap(), ledger);
}
