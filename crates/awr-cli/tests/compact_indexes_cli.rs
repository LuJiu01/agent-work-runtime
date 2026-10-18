use awr_core::Id;
use serde_json::Value;
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
};
#[path = "../../../tests/common/compact_indexes.rs"]
mod contract;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let f = Self(std::env::temp_dir().join(format!("awr-compact-indexes-{}", Id::new())));
        fs::create_dir_all(&f.0).unwrap();
        let mut ledger = String::from(
            "goals:\n- id: G\n  title: Deliver a guide\n  status: active\n  summary: Explain the public contract\nwork_items:\n- id: W\n  title: Write guide\n  status: ready\n  goal: G\n  acceptance: [Reviewed guide]\n  next_action: Draft the guide\n",
        );
        for n in 0..40 {
            ledger.push_str(&format!(
                "- id: HISTORY-{n}\n  title: Review support item {n}\n  status: completed\n"
            ));
        }
        fs::write(f.0.join("work.yaml"), ledger).unwrap();
        fs::write(f.0.join("rules.md"), "# Authority {#authority severity=hard scope=project value=*}\n\nPreserve every required source fact exactly.\n").unwrap();
        fs::write(f.0.join("sources.toml"), "[project]\nname='Index projection fixture'\nexternal_key='index-projection'\ncontext_profile='minimal'\n[[sources]]\ndomain='ledger'\nrole='primary'\npath='work.yaml'\nadapter='yaml-ledger-v1'\n[[sources]]\ndomain='rules'\nrole='primary'\npath='rules.md'\nadapter='markdown-rules-v1'\n").unwrap();
        assert!(
            f.run(&["init", "--manifest", "sources.toml", "--accept"])
                .status
                .success()
        );
        f
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_awr"))
            .arg("--project")
            .arg(&self.0)
            .arg("--json")
            .args(args)
            .output()
            .unwrap()
    }
    fn read(&self, args: &[&str]) -> Value {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn default_concise_cli_covers_zero_one_many_and_full_drilldown() {
    let f = Fixture::new();
    let full = f.read(&[
        "work",
        "prepare",
        "W",
        "--budget",
        "100000",
        "--branch",
        "main",
        "--source-sha",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    ]);
    let pack = &full["context"]["work_context"];
    let budgets = [
        100000,
        pack["token_estimate"].as_u64().unwrap() - 1,
        pack["required_tokens"].as_u64().unwrap() + 50,
    ];
    let help = f.run(&["work", "prepare", "--help"]);
    assert!(!String::from_utf8_lossy(&help.stdout).contains("compact-indexes"));
    for (case, budget) in budgets.into_iter().enumerate() {
        let budget = budget.to_string();
        for explain in [false, true] {
            let mut args = vec![
                "work",
                "prepare",
                "W",
                "--budget",
                &budget,
                "--branch",
                "main",
                "--source-sha",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ];
            if explain {
                args.push("--explain");
            }
            let full = f.read(&args);
            let mut explicit = args.clone();
            explicit.extend(["--response-view", "full"]);
            assert_eq!(f.read(&explicit), full);
            let count = full["context"]["work_context"]["omitted_chunks"]
                .as_array()
                .unwrap()
                .len();
            match case {
                0 => assert_eq!(count, 0),
                1 => assert_eq!(count, 1),
                _ => assert!(count > 10),
            }
            for view in ["summary", "action"] {
                let mut concise_args = args.clone();
                concise_args.extend(["--response-view", view]);
                let concise = f.read(&concise_args);
                contract::assert_prepared_view(&full, &concise, count);
                let q = &concise["response_view"]["full_result"];
                assert_eq!(q["response_view"], "full");
                assert_eq!(q["branch"], "main");
                assert_eq!(q["source_sha"], "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
                assert_eq!(q["budget"].to_string(), budget);
                if view == "action" {
                    assert!(serde_json::to_vec(&concise["guidance"]).unwrap().len() <= 1024);
                }
                // Explain is a presentation option and is not needed to retrieve indexes.
                let detail = f.read(&[
                    "work",
                    "prepare",
                    q["work"].as_str().unwrap(),
                    "--budget",
                    &budget,
                    "--branch",
                    q["branch"].as_str().unwrap(),
                    "--source-sha",
                    q["source_sha"].as_str().unwrap(),
                ]);
                assert_eq!(
                    detail["context"]["work_context"]["omitted_chunks"],
                    full["context"]["work_context"]["omitted_chunks"]
                );
                assert_eq!(detail["project_revision"], concise["project_revision"]);
                assert_eq!(
                    detail["context"]["work_context"]["context_hash"],
                    concise["context"]["work_context"]["context_hash"]
                );
            }
        }
    }
    let rev = f.read(&["session", "list"])["project_revision"].to_string();
    let started = f.read(&[
        "session",
        "start",
        "--work",
        "W",
        "--agent",
        "fixture",
        "--provider",
        "fixture",
        "--model",
        "test",
        "--expected-revision",
        &rev,
    ]);
    let sid = started["session"]["id"].as_str().unwrap();
    let args = [
        "work",
        "prepare",
        "W",
        "--session",
        sid,
        "--response-view",
        "action",
        "--budget",
        "100000",
    ];
    let concise = f.read(&args);
    assert_eq!(
        concise["context"]["work_context"]["omitted_chunks"],
        serde_json::json!([])
    );
    assert!(concise["response_view"].get("detail_note").is_none());
    assert_eq!(concise["response_view"]["full_result"]["session"], sid);
    let old_hash = concise["context"]["work_context"]["context_hash"].clone();
    fs::write(
        f.0.join("work.yaml"),
        fs::read_to_string(f.0.join("work.yaml"))
            .unwrap()
            .replace("Draft the guide", "Review the guide"),
    )
    .unwrap();
    f.read(&["source", "reindex"]);
    let changed = f.read(&[
        "work",
        "prepare",
        "W",
        "--session",
        sid,
        "--budget",
        "100000",
    ]);
    assert_ne!(changed["project_revision"], concise["project_revision"]);
    assert_ne!(changed["context"]["work_context"]["context_hash"], old_hash);
    let rev = f.read(&["session", "list"])["project_revision"].to_string();
    f.read(&[
        "session",
        "end",
        "--session",
        sid,
        "--expected-revision",
        &rev,
    ]);
}

#[test]
fn cli_concise_preserves_failures_raw_indexes_and_human_context() {
    let f = Fixture::new();
    let full = f.read(&["work", "prepare", "W"]);
    let raw = f.read(&["context", "compile", "--work", "W"]);
    for field in ["selected_chunks", "selected_entities", "omitted_chunks"] {
        assert!(raw["work_context"][field].is_array());
    }
    assert!(raw.get("response_view").is_none());
    let failed = f.run(&["work", "prepare", "W", "--budget", "1"]);
    assert!(!failed.status.success());
    for view in ["full", "summary", "action"] {
        let out = f.run(&[
            "work",
            "prepare",
            "W",
            "--budget",
            "1",
            "--response-view",
            view,
        ]);
        assert_eq!(out.status.code(), failed.status.code());
        assert_eq!(out.stdout, failed.stdout);
        assert_eq!(out.stderr, failed.stderr);
        let human = Command::new(env!("CARGO_BIN_EXE_awr"))
            .arg("--project")
            .arg(&f.0)
            .args(["work", "prepare", "W", "--response-view", view])
            .output()
            .unwrap();
        assert!(human.status.success());
        let text = String::from_utf8(human.stdout).unwrap();
        assert!(
            text.contains(
                full["context"]["work_context"]["rendered_context"]
                    .as_str()
                    .unwrap()
            )
        );
        assert!(!text.contains("Use full_result only when investigating"));
    }
    fs::write(
        f.0.join("work.yaml"),
        fs::read_to_string(f.0.join("work.yaml"))
            .unwrap()
            .replace("acceptance: [Reviewed guide]", "acceptance: []"),
    )
    .unwrap();
    f.read(&["source", "reindex"]);
    for view in ["full", "summary", "action"] {
        let out = f.run(&["work", "prepare", "W", "--response-view", view]);
        assert!(!out.status.success());
        let value: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(value["ok"], false);
        assert_eq!(value["context"]["completeness"]["complete"], false);
        assert!(value["response_view"].get("detail_counts").is_none());
        assert!(value["response_view"].get("detail_note").is_none());
        assert!(
            value["context"]["work_context"]
                .get("omitted_chunks")
                .is_some()
        );
        assert!(String::from_utf8_lossy(&out.stderr).contains("context"));
    }
}
