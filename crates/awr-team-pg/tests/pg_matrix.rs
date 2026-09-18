#[test]
fn evidence_matrix_lists_all_required_cases_and_does_not_claim_a_release() {
    let raw = include_str!("../../../docs/reference/team-v1-evidence-matrix.json");
    let value: serde_json::Value = serde_json::from_str(raw).unwrap();
    assert_eq!(value["counts"]["required"], 69);
    assert_eq!(value["release_candidate"], false);
    assert_eq!(value["tag_pushed"], false);
    assert_eq!(value["live_agent_run"]["oracle"]["pass"], true);
    assert_eq!(
        value["live_agent_run"]["clients"][0]["product"],
        "Kimi Code CLI"
    );
    assert_eq!(
        value["live_agent_run"]["clients"][1]["product"],
        "ZCode CLI"
    );
    let cases = value["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 69);
    // counts must agree with the per-case records instead of freezing a
    // historical number (TEAM-P13 raised real_agent_accepted from 1 to 42).
    let accepted = cases
        .iter()
        .filter(|c| c["status"] == "real_agent_accepted")
        .count();
    assert_eq!(value["counts"]["real_agent_accepted"], accepted as u64);
    for case in cases {
        let replayed = case["status"] == "real_agent_accepted";
        assert_eq!(case["real_agent_clients"], replayed, "case {}", case["id"]);
        if replayed {
            assert!(
                case["agent_evidence"].is_string(),
                "accepted case {} lacks agent evidence",
                case["id"]
            );
        }
    }
}
