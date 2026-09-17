use crate::execution::OutboxDelivery;
use crate::graph::paths_conflict;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrashPoint {
    None,
    BeforeJournal,
    AfterJournalBeforeEffect,
    AfterEffectBeforeReport,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunnerOutcome {
    pub execution_id: String,
    pub effect_key: String,
    pub state: String,
    pub unknown: bool,
    pub started: bool,
    pub observed_paths: Vec<String>,
    pub output_digest: Option<String>,
    pub environment_digest: String,
    pub scope_violation: bool,
    pub exactly_once_supported: bool,
}

pub struct ReferenceRunner {
    journal_dir: PathBuf,
    worktree_root: PathBuf,
}

impl ReferenceRunner {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            journal_dir: root.join("journal"),
            worktree_root: root.join("worktree"),
        }
    }

    pub fn handle_delivery(&self, delivery: &OutboxDelivery, crash: CrashPoint) -> RunnerOutcome {
        fs::create_dir_all(&self.journal_dir).ok();
        fs::create_dir_all(&self.worktree_root).ok();
        let exactly_once_supported = delivery.fencing_class != "uncontrolled";
        if let Some(existing) = self.load(&delivery.execution_id) {
            return existing;
        }
        if crash == CrashPoint::BeforeJournal {
            return RunnerOutcome {
                execution_id: delivery.execution_id.clone(),
                effect_key: delivery.effect_key.clone(),
                state: "prepared".into(),
                unknown: false,
                started: false,
                observed_paths: vec![],
                output_digest: None,
                environment_digest: env_digest(&self.worktree_root),
                scope_violation: false,
                exactly_once_supported,
            };
        }
        let mut outcome = RunnerOutcome {
            execution_id: delivery.execution_id.clone(),
            effect_key: delivery.effect_key.clone(),
            state: "accepted".into(),
            unknown: false,
            started: false,
            observed_paths: vec![],
            output_digest: None,
            environment_digest: env_digest(&self.worktree_root),
            scope_violation: false,
            exactly_once_supported,
        };
        self.persist(&outcome);
        if crash == CrashPoint::AfterJournalBeforeEffect {
            outcome.state = "unknown".into();
            outcome.unknown = true;
            outcome.started = false;
            self.persist(&outcome);
            return outcome;
        }
        let writes = delivery
            .payload
            .get("writes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut observed = Vec::new();
        for item in writes {
            let path = item.get("path").and_then(Value::as_str).unwrap_or_default();
            let content = item
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if path.is_empty() {
                continue;
            }
            let dest = self.worktree_root.join(path);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).ok();
            }
            fs::write(&dest, content).ok();
            observed.push(path.replace('\\', "/"));
        }
        let declared: Vec<String> = delivery
            .declared_scope
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(ToOwned::to_owned)
            .collect();
        let scope_violation = observed.iter().any(|path| {
            !declared.iter().any(|item| {
                paths_conflict("file", path, "file", item)
                    || paths_conflict("file", path, "prefix", item)
            })
        });
        let digest = output_digest(&self.worktree_root, &observed);
        if crash == CrashPoint::AfterEffectBeforeReport {
            outcome.state = "unknown".into();
            outcome.unknown = true;
            outcome.started = true;
            outcome.observed_paths = observed;
            outcome.output_digest = Some(digest);
            outcome.scope_violation = scope_violation;
            self.persist(&outcome);
            return outcome;
        }
        outcome.started = true;
        outcome.observed_paths = observed;
        outcome.output_digest = Some(digest);
        outcome.scope_violation = scope_violation;
        outcome.state = if scope_violation {
            "failed".into()
        } else {
            "succeeded".into()
        };
        self.persist(&outcome);
        outcome
    }

    fn journal_path(&self, execution_id: &str) -> PathBuf {
        self.journal_dir.join(format!("{execution_id}.json"))
    }

    fn load(&self, execution_id: &str) -> Option<RunnerOutcome> {
        let bytes = fs::read(self.journal_path(execution_id)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    fn persist(&self, outcome: &RunnerOutcome) {
        let _ = fs::write(
            self.journal_path(&outcome.execution_id),
            serde_json::to_vec(outcome).unwrap_or_else(|_| json!({}).to_string().into_bytes()),
        );
    }
}

fn env_digest(worktree: &Path) -> String {
    format!(
        "{:x}",
        Sha256::digest(worktree.to_string_lossy().as_bytes())
    )
}

fn output_digest(worktree: &Path, paths: &[String]) -> String {
    let mut hasher = Sha256::new();
    let mut ordered = paths.to_vec();
    ordered.sort();
    for path in ordered {
        hasher.update(path.as_bytes());
        if let Ok(bytes) = fs::read(worktree.join(&path)) {
            hasher.update(&bytes);
        }
    }
    format!("{:x}", hasher.finalize())
}
