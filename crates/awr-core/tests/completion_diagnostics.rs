use awr_core::*;
use serde_json::{Value, json};

const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const CRITERION: &str = "The fixture result is verified";

fn report() -> Value {
    json!({"version":1,"work_item":"W","source_sha":SHA,"command":"verify fixture",
        "scope":["W","docs"],"verified_at":1000,
        "checks":[{"name":"fixture","passed":true,"details":"Observed the fixture result","criteria":[CRITERION]}]})
}

fn evidence() -> Evidence {
    Evidence {
        id: Id::new(),
        project_id: Id::new(),
        work_item_id: None,
        external_key: "E".into(),
        evidence_type: "verification_report".into(),
        level: EvidenceLevel::LocallyVerified,
        summary: "Fixture verification".into(),
        locator: "report.json".into(),
        sha256: Some("a".repeat(64)),
        source_sha: Some(SHA.into()),
        command: Some("verify fixture".into()),
        scope: vec!["W".into(), "docs".into()],
        source_ref: None,
        branch_id: None,
        revision: 1,
        verified_at: Some(1000),
    }
}

fn parse(value: &Value) -> CompletionReport {
    CompletionReport::parse(&serde_json::to_vec(value).unwrap(), "report.json").unwrap()
}

#[test]
fn rejected_report_fields_have_safe_actionable_locations() {
    for (pointer, replacement) in [
        ("/version", json!("synthetic-private-value")),
        ("/work_item", json!(false)),
        ("/source_sha", json!([])),
        ("/command", json!(null)),
        (
            "/scope",
            json!({"synthetic-private-key":"synthetic-private-value"}),
        ),
        ("/verified_at", json!(1.5)),
        ("/verified_at", json!("synthetic-private-value")),
        ("/verified_at", json!(null)),
        ("/checks", json!(false)),
        ("/checks/0", json!("synthetic-private-value")),
        ("/checks/0/name", json!(3)),
        ("/checks/0/passed", json!("true")),
        ("/checks/0/details", json!({"synthetic-private-key":true})),
        ("/checks/0/criteria", json!(null)),
    ] {
        let mut input = report();
        *input.pointer_mut(pointer).unwrap() = replacement;
        let detail = CompletionReport::parse(&serde_json::to_vec(&input).unwrap(), "report.json")
            .unwrap_err();
        assert_eq!(detail.location.pointer.as_deref(), Some(pointer));
        assert_eq!(detail.rule, "completion.report.v1");
        assert!(!detail.repair.is_empty());
        if pointer == "/verified_at" {
            assert!(detail.repair.contains("Unix epoch milliseconds"));
        }
        let output = serde_json::to_string(&detail).unwrap();
        assert!(!output.contains("synthetic-private"));
        // A structural probe has no independently established source coordinates.
        assert!(detail.location.line.is_none());
        assert!(detail.location.column.is_none());
    }
    for (parent, name) in [
        ("", "version"),
        ("", "work_item"),
        ("", "verified_at"),
        ("", "checks"),
        ("/checks/0", "passed"),
        ("/checks/0", "details"),
    ] {
        let mut input = report();
        input
            .pointer_mut(parent)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove(name);
        let detail = CompletionReport::parse(&serde_json::to_vec(&input).unwrap(), "report.json")
            .unwrap_err();
        assert_eq!(detail.location.pointer, Some(format!("{parent}/{name}")));
        assert!(detail.message.contains("required"));
        if name == "verified_at" {
            assert!(detail.repair.contains("Unix epoch milliseconds"));
        }
    }
}

#[test]
fn parse_preserves_serde_metadata_alias_defaults_and_duplicate_field_rules() {
    let mut input = report();
    input["producer"] = json!({"synthetic-private-key":"synthetic-private-value"});
    let check = input["checks"][0].as_object_mut().unwrap();
    let details = check.remove("details").unwrap();
    check.insert("result".into(), details);
    check.remove("criteria");
    assert!(parse(&input).checks[0].criteria.is_empty());
    input["checks"][0]["result"] = json!(false);
    let detail =
        CompletionReport::parse(&serde_json::to_vec(&input).unwrap(), "report.json").unwrap_err();
    assert_eq!(detail.location.pointer.as_deref(), Some("/checks/0/result"));

    // Derive supports positional struct sequences; successful inputs must not be rescanned.
    let sequence = json!([
        1,
        "W",
        SHA,
        "verify fixture",
        ["W"],
        1000,
        [["fixture", true, "observed", [CRITERION]]]
    ]);
    let bytes = serde_json::to_vec(&sequence).unwrap();
    assert!(serde_json::from_slice::<CompletionReport>(&bytes).is_ok());
    assert!(CompletionReport::parse(&bytes, "report.json").is_ok());

    for bytes in [
        serde_json::to_string(&report())
            .unwrap()
            .replacen('{', "{\"version\":1,", 1),
        serde_json::to_string(&report())
            .unwrap()
            .replace("\"details\":", "\"result\":\"extra\",\"details\":"),
        "{\"producer\":\"中文\",\"version\": }".into(),
    ] {
        assert!(serde_json::from_str::<CompletionReport>(&bytes).is_err());
        let detail = CompletionReport::parse(bytes.as_bytes(), "report.json").unwrap_err();
        assert!(detail.location.pointer.is_none());
        assert_eq!(detail.location.line, Some(1));
        assert!(detail.location.column.is_some());
        assert!(!serde_json::to_string(&detail).unwrap().contains("extra"));
    }
}

#[test]
fn duplicate_fields_keep_original_coordinates_even_when_the_repeated_value_has_a_bad_type() {
    let input = serde_json::to_string(&report()).unwrap();
    for bytes in [
        input.replace("\"version\":1", "\"version\":1,\"version\":false"),
        input.replace(
            "\"details\":\"Observed the fixture result\"",
            "\"result\":\"synthetic-private-value\",\"details\":false",
        ),
    ] {
        let original = serde_json::from_str::<CompletionReport>(&bytes).unwrap_err();
        let detail = CompletionReport::parse(bytes.as_bytes(), "report.json").unwrap_err();
        assert!(detail.location.pointer.is_none(), "{detail:?}");
        assert_eq!(detail.location.line, Some(original.line()));
        assert_eq!(detail.location.column, Some(original.column()));
        assert!(detail.repair.contains("duplicate fields"));
        assert!(
            !serde_json::to_string(&detail)
                .unwrap()
                .contains("synthetic-private")
        );
    }
}

#[test]
fn uninspectable_metadata_keeps_report_acceptance_and_drops_unverified_locations() {
    let mut input = report();
    let check = input["checks"][0].as_object_mut().unwrap();
    check.remove("details");
    check.insert("result".into(), json!(""));
    for opaque_metadata in [false, true] {
        let mut bytes = serde_json::to_string(&input).unwrap();
        if opaque_metadata {
            bytes.pop();
            bytes.push_str(",\"producer\":1e400}");
        }
        // Unknown producer metadata remains ignored by the original typed parser.
        let parsed = CompletionReport::parse(bytes.as_bytes(), "report.json").unwrap();
        let error = parsed
            .validate(&evidence(), "W", &[CRITERION.into()], 2000)
            .unwrap_err();
        let report = CompletionReport::locate_error(bytes.as_bytes(), error).report();
        assert_eq!(report.code, "EvidenceMissing");
        let detail = report.details.unwrap();
        assert_eq!(detail["location"]["locator"], "report.json");
        assert_eq!(
            detail["location"]["pointer"],
            if opaque_metadata {
                json!(null)
            } else {
                json!("/checks/0/result")
            }
        );
        assert_eq!(detail["rule"], "completion.report.check.details");
        assert!(!detail["repair"].as_str().unwrap().is_empty());
        let corrected = bytes.replace(
            "\"result\":\"\"",
            "\"result\":\"Observed the fixture result\"",
        );
        CompletionReport::parse(corrected.as_bytes(), "report.json")
            .unwrap()
            .validate(&evidence(), "W", &[CRITERION.into()], 2000)
            .unwrap();
    }
}

#[test]
fn semantic_failures_identify_the_binding_or_check_without_values() {
    for (pointer, value, expected_path) in [
        ("/version", json!(2), "/version"),
        ("/work_item", json!("synthetic-private-value"), "/work_item"),
        (
            "/source_sha",
            json!("synthetic-private-value"),
            "/source_sha",
        ),
        ("/source_sha", json!("b".repeat(40)), "/source_sha"),
        ("/command", json!(""), "/command"),
        ("/command", json!("synthetic-private-value"), "/command"),
        ("/scope", json!(["synthetic-private-value"]), "/scope"),
        ("/verified_at", json!(-1), "/verified_at"),
        ("/verified_at", json!(2001), "/verified_at"),
        ("/verified_at", json!(999), "/verified_at"),
        ("/checks", json!([]), "/checks"),
        ("/checks/0/passed", json!(false), "/checks/0/passed"),
        ("/checks/0/name", json!("  "), "/checks/0/name"),
        ("/checks/0/details", json!(""), "/checks/0/details"),
        (
            "/checks/0/criteria",
            json!(["synthetic-private-value"]),
            "/checks/0/criteria/0",
        ),
    ] {
        let mut input = report();
        *input.pointer_mut(pointer).unwrap() = value;
        let error = parse(&input)
            .validate(&evidence(), "W", &[CRITERION.into()], 2000)
            .unwrap_err()
            .report();
        assert_eq!(error.code, "EvidenceMissing");
        let output = serde_json::to_value(&error).unwrap();
        assert_eq!(output["details"]["location"]["pointer"], expected_path);
        if pointer == "/verified_at" {
            assert_eq!(output["details"]["rule"], "completion.report.verified_at");
            let repair = output["details"]["repair"].as_str().unwrap();
            if input["verified_at"] == -1 || input["verified_at"] == 2001 {
                assert!(repair.contains("Unix milliseconds"), "{output}");
            }
            if input["verified_at"] == 2001 {
                assert!(error.message.contains("later than the current time"));
            }
        }
        assert!(!output.to_string().contains("synthetic-private"));
        assert!(render_diagnostic_details(error.details.as_ref()).contains(expected_path));
    }
    let mut duplicate = report();
    let check = duplicate["checks"][0].clone();
    duplicate["checks"].as_array_mut().unwrap().push(check);
    let error = parse(&duplicate)
        .validate(&evidence(), "W", &[CRITERION.into()], 2000)
        .unwrap_err()
        .report();
    assert_eq!(
        error.details.unwrap()["location"]["pointer"],
        "/checks/1/name"
    );
}

#[test]
fn validation_keeps_first_failure_and_existing_binding_comparisons() {
    let mut input = report();
    input["source_sha"] = json!(SHA.to_uppercase());
    input["scope"] = json!(["docs", "W", "W"]);
    let mut parsed = parse(&input);
    parsed
        .validate(&evidence(), "W", &[CRITERION.into()], 2000)
        .unwrap();
    parsed.source_sha = "b".repeat(40);
    parsed.checks[0].passed = false;
    let error = parsed
        .validate(&evidence(), "W", &[CRITERION.into()], 2000)
        .unwrap_err()
        .report();
    assert_eq!(error.details.unwrap()["location"]["pointer"], "/source_sha");
}

#[test]
fn preflight_distinguishes_empty_scope_from_missing_coverage() {
    let mut parsed = parse(&report());
    let criteria = [CRITERION.into(), "A second verified result".into()];
    let detail = parsed
        .validate_coverage(&evidence(), &criteria)
        .unwrap_err()
        .report();
    assert!(detail.message.contains("acceptance[1]"));
    assert_eq!(
        detail.details.unwrap()["rule"],
        "completion.report.coverage"
    );
    parsed.scope.clear();
    let detail = parsed
        .validate_coverage(&evidence(), &criteria)
        .unwrap_err()
        .report();
    assert_eq!(detail.details.unwrap()["location"]["pointer"], "/scope");
}
