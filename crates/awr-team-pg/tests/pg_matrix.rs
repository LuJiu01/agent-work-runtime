#[test]
fn evidence_matrix_lists_all_required_cases_and_does_not_claim_a_release() {
    let raw = include_str!("../../../docs/reference/team-v1-evidence-matrix.json");
    let value: serde_json::Value = serde_json::from_str(raw).unwrap();
    assert_eq!(value["counts"]["required"], 69);
    assert_eq!(value["counts"]["real_agent_accepted"], 1);
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
    assert!(cases.iter().all(|c| c["real_agent_clients"] == false));
}
