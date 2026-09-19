use crate::execution::OutboxDelivery;
use crate::graph::path_within_scope;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

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
    #[serde(default)]
    pub error: Option<String>,
    /// Files touched but whose final state is uncertain after an I/O error
    /// (may be truncated or partially written). Never silently reported as
    /// "not executed" (CR #58 P2-4).
    #[serde(default)]
    pub partial_paths: Vec<String>,
}

pub struct ReferenceRunner {
    journal_dir: PathBuf,
    worktree_root: PathBuf,
}

enum JournalLoad {
    Missing,
    Owned(RunnerOutcome),
    Corrupt(String),
}

impl ReferenceRunner {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            journal_dir: root.join("journal"),
            worktree_root: root.join("worktree"),
        }
    }

    /// Visible to pg-tests for crash-recovery fixtures.
    pub fn base_outcome(&self, delivery: &OutboxDelivery, state: &str) -> RunnerOutcome {
        RunnerOutcome {
            execution_id: delivery.execution_id.clone(),
            effect_key: delivery.effect_key.clone(),
            state: state.into(),
            unknown: false,
            started: false,
            observed_paths: vec![],
            output_digest: None,
            environment_digest: env_digest(&self.worktree_root),
            scope_violation: false,
            exactly_once_supported: delivery.fencing_class != "uncontrolled",
            error: None,
            partial_paths: vec![],
        }
    }

    /// Resource-end fencing: side effects are bound to a monotonic fencing
    /// token kept next to the journals. A delivery whose fence is OLDER than
    /// the current token is a stale executor and must not write (CR #58 P1).
    fn check_fence(&self, delivery: &OutboxDelivery) -> Result<(), String> {
        let ledger = self.journal_dir.join("fence-ledger");
        let current: Option<i64> = fs::read_to_string(&ledger)
            .ok()
            .and_then(|raw| raw.trim().parse().ok());
        if let Some(current) = current {
            if delivery.fence < current {
                return Err(format!(
                    "stale fencing token {} (current {current})",
                    delivery.fence
                ));
            }
        }
        if current.map(|c| delivery.fence > c).unwrap_or(true) {
            fs::write(&ledger, delivery.fence.to_string())
                .map_err(|e| format!("fence ledger persist failed: {e}"))?;
        }
        Ok(())
    }

    pub fn handle_delivery(&self, delivery: &OutboxDelivery, crash: CrashPoint) -> RunnerOutcome {
        if let Err(error) = fs::create_dir_all(&self.journal_dir)
            .and_then(|_| fs::create_dir_all(&self.worktree_root))
        {
            let mut outcome = self.base_outcome(delivery, "failed");
            outcome.error = Some(format!("runner directories unavailable: {error}"));
            return outcome;
        }
        // An existing journal is the execution admission record. A corrupt or
        // unreadable one is NOT "never executed" — it takes the unknown path
        // and must not re-run effects (CR #41 P2-3).
        match self.load(&delivery.execution_id) {
            JournalLoad::Owned(existing) => {
                // A terminal journal is the idempotent result. A NON-terminal
                // one means the previous handler died mid-execution: effects
                // are uncertain, so recovery must not treat it as "never
                // executed" nor as a final answer (CR #58 P2-3).
                if matches!(existing.state.as_str(), "succeeded" | "failed") || existing.unknown {
                    return existing;
                }
                let mut outcome = existing;
                outcome.state = "unknown".into();
                outcome.unknown = true;
                outcome.error =
                    Some("previous handler died mid-execution; effects uncertain".into());
                let _ = self.persist(&outcome);
                return outcome;
            }
            JournalLoad::Corrupt(error) => {
                let mut outcome = self.base_outcome(delivery, "unknown");
                outcome.unknown = true;
                outcome.error = Some(format!("journal unreadable: {error}"));
                return outcome;
            }
            JournalLoad::Missing => {}
        }
        if crash == CrashPoint::BeforeJournal {
            return self.base_outcome(delivery, "prepared");
        }
        // Atomic admission: only the FIRST handler may create the journal.
        if let Err(error) = self.persist_new(&self.base_outcome(delivery, "accepted")) {
            match error.kind() {
                ErrorKind::AlreadyExists => match self.load(&delivery.execution_id) {
                    JournalLoad::Owned(existing) => return existing,
                    JournalLoad::Corrupt(error) => {
                        let mut outcome = self.base_outcome(delivery, "unknown");
                        outcome.unknown = true;
                        outcome.error =
                            Some(format!("journal unreadable after admission race: {error}"));
                        return outcome;
                    }
                    JournalLoad::Missing => {
                        let mut outcome = self.base_outcome(delivery, "unknown");
                        outcome.unknown = true;
                        outcome.error = Some("journal vanished after admission race".into());
                        return outcome;
                    }
                },
                _ => {
                    let mut outcome = self.base_outcome(delivery, "failed");
                    outcome.error = Some(format!("journal persist failed: {error}"));
                    return outcome;
                }
            }
        }
        if crash == CrashPoint::AfterJournalBeforeEffect {
            let mut outcome = self.base_outcome(delivery, "unknown");
            outcome.unknown = true;
            self.persist(&outcome);
            return outcome;
        }
        // Validate the COMPLETE write plan before any side effect (CR #41
        // P1-1, P2-10). A single violation means zero writes.
        let root_canon = match self.worktree_root.canonicalize() {
            Ok(path) => path,
            Err(error) => {
                let mut outcome = self.base_outcome(delivery, "failed");
                outcome.error = Some(format!("worktree unavailable: {error}"));
                let _ = self.persist(&outcome);
                return outcome;
            }
        };
        let writes = delivery
            .payload
            .get("writes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let declared: Vec<String> = delivery
            .declared_scope
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(ToOwned::to_owned)
            .collect();
        let mut plan: Vec<(String, PathBuf, String)> = Vec::new();
        for item in &writes {
            let raw = item.get("path").and_then(Value::as_str).unwrap_or_default();
            let content = item
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let Some(rel) = normalize_write_path(raw) else {
                return self.reject_plan(delivery, format!("unsafe write path: {raw}"));
            };
            if !declared.iter().any(|item| path_within_scope(item, &rel)) {
                let mut outcome = self.base_outcome(delivery, "failed");
                outcome.scope_violation = true;
                outcome.observed_paths = vec![rel.clone()];
                outcome.error = Some(format!("path outside declared scope: {rel}"));
                let _ = self.persist(&outcome);
                return outcome;
            }
            let dest = self.worktree_root.join(&rel);
            if let Err(error) = self.ensure_inside(&root_canon, &dest) {
                return self.reject_plan(delivery, error);
            }
            plan.push((rel, dest, content));
        }
        // Execute the plan; propagate real I/O errors and record only the
        // writes that actually happened (CR #41 P2-4).
        if let Err(error) = self.check_fence(delivery) {
            let mut outcome = self.base_outcome(delivery, "failed");
            outcome.error = Some(error);
            let _ = self.persist(&outcome);
            return outcome;
        }
        let mut observed = Vec::new();
        let mut partial = Vec::new();
        for (rel, dest, content) in &plan {
            // Re-verify AT WRITE TIME: the destination itself must not be a
            // symlink, and its parent must still resolve inside the worktree
            // (plan-time checks alone leave a check/use window — CR #58 P1).
            let guarded = (|| -> Result<(), String> {
                if let Ok(meta) = fs::symlink_metadata(dest) {
                    if meta.file_type().is_symlink() {
                        return Err(format!("write target is a symlink: {}", dest.display()));
                    }
                }
                self.ensure_inside(&root_canon, dest)
            })();
            let result = guarded
                .map_err(|e| std::io::Error::new(ErrorKind::PermissionDenied, e))
                .and_then(|_| {
                    dest.parent()
                        .map(|parent| fs::create_dir_all(parent))
                        .unwrap_or_else(|| Ok(()))
                })
                .and_then(|_| fs::write(dest, content));
            if let Err(error) = result {
                // A failed write does NOT mean the file is unchanged: it may
                // be truncated or partially written. Record it as touched,
                // never as untouched (CR #58 P2-4).
                partial.push(rel.clone());
                let mut outcome = self.base_outcome(delivery, "failed");
                outcome.started = true;
                outcome.observed_paths = observed.clone();
                outcome.partial_paths = partial.clone();
                outcome.output_digest = Some(output_digest(&self.worktree_root, &observed));
                outcome.error = Some(format!("write failed for {rel}: {error}"));
                let _ = self.persist(&outcome);
                return outcome;
            }
            observed.push(rel.clone());
        }
        let digest = output_digest(&self.worktree_root, &observed);
        if crash == CrashPoint::AfterEffectBeforeReport {
            let mut outcome = self.base_outcome(delivery, "unknown");
            outcome.unknown = true;
            outcome.started = true;
            outcome.observed_paths = observed;
            outcome.output_digest = Some(digest);
            self.persist(&outcome);
            return outcome;
        }
        let mut outcome = self.base_outcome(delivery, "succeeded");
        outcome.started = true;
        outcome.observed_paths = observed;
        outcome.output_digest = Some(digest);
        if let Err(error) = self.persist_result(&outcome) {
            let mut failed = outcome.clone();
            failed.state = "unknown".into();
            failed.unknown = true;
            failed.error = Some(format!("journal final persist failed: {error}"));
            return failed;
        }
        outcome
    }

    fn reject_plan(&self, delivery: &OutboxDelivery, error: String) -> RunnerOutcome {
        let mut outcome = self.base_outcome(delivery, "failed");
        outcome.scope_violation = true;
        outcome.error = Some(error);
        let _ = self.persist(&outcome);
        outcome
    }

    /// Defense in depth at write time: the destination's deepest existing
    /// ancestor must resolve INSIDE the canonical worktree, so symlinks
    /// cannot escape (CR #41 P1-1).
    fn ensure_inside(&self, root_canon: &Path, dest: &Path) -> Result<(), String> {
        let mut ancestor = dest.parent().map(Path::to_path_buf);
        while let Some(dir) = ancestor.clone() {
            if dir.exists() {
                let canon = dir
                    .canonicalize()
                    .map_err(|e| format!("cannot resolve {}: {e}", dir.display()))?;
                if !canon.starts_with(root_canon) {
                    return Err(format!(
                        "write target escapes the worktree: {}",
                        dest.display()
                    ));
                }
                return Ok(());
            }
            ancestor = dir.parent().map(Path::to_path_buf);
        }
        Err(format!(
            "write target has no existing ancestor: {}",
            dest.display()
        ))
    }

    fn journal_path(&self, execution_id: &str) -> PathBuf {
        self.journal_dir.join(format!("{execution_id}.json"))
    }

    fn load(&self, execution_id: &str) -> JournalLoad {
        match fs::read(self.journal_path(execution_id)) {
            Ok(bytes) => match serde_json::from_slice(&bytes) {
                Ok(outcome) => JournalLoad::Owned(outcome),
                Err(error) => JournalLoad::Corrupt(error.to_string()),
            },
            Err(error) if error.kind() == ErrorKind::NotFound => JournalLoad::Missing,
            Err(error) => JournalLoad::Corrupt(error.to_string()),
        }
    }

    fn persist_new(&self, outcome: &RunnerOutcome) -> std::io::Result<()> {
        let bytes = serde_json::to_vec(outcome)
            .map_err(|e| std::io::Error::new(ErrorKind::InvalidData, e))?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(self.journal_path(&outcome.execution_id))?;
        use std::io::Write;
        file.write_all(&bytes)?;
        // Closing is not durability; make the admission record durable
        // before relying on it (CR #58 P2-3).
        file.sync_all()
    }

    fn persist_result(&self, outcome: &RunnerOutcome) -> std::io::Result<()> {
        let bytes = serde_json::to_vec(outcome)
            .map_err(|e| std::io::Error::new(ErrorKind::InvalidData, e))?;
        let path = self.journal_path(&outcome.execution_id);
        fs::write(&path, &bytes)?;
        fs::File::open(&path)?.sync_all()
    }

    fn persist(&self, outcome: &RunnerOutcome) {
        let _ = self.persist_result(outcome);
    }
}

/// Normalize a write path to a safe relative form; rejects absolute paths,
/// parent components, prefixes and NUL (CR #41 P1-1).
fn normalize_write_path(path: &str) -> Option<String> {
    if path.is_empty() || path.contains('\0') {
        return None;
    }
    let mut parts = Vec::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?.to_string()),
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) | Component::ParentDir => return None,
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
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
