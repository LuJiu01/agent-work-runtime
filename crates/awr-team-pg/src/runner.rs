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
    /// How old a non-terminal journal must be before recovery may convert
    /// it to unknown. Tests override; production default is 60s.
    recovery_stale_ms: u64,
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
            recovery_stale_ms: 60_000,
        }
    }

    /// Test hook: zero means "immediately stale" for recovery fixtures.
    pub fn with_recovery_stale_ms(mut self, ms: u64) -> Self {
        self.recovery_stale_ms = ms;
        self
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

    fn fence_ledger_path(&self, work_id: &str) -> PathBuf {
        self.journal_dir.join(format!("fence-ledger-{work_id}"))
    }

    /// Resource-end fencing (CR #58 r3):
    /// - the ledger identity matches the token issuer: one ledger PER WORK,
    ///   because fences from different works are unrelated counters
    /// - read-check-update runs under an advisory lock held for the WHOLE
    ///   effect phase, so "passes check, another finishes, resumes" cannot
    ///   reorder side effects
    /// - a missing ledger is "not yet fenced"; a CORRUPT one refuses side
    ///   effects instead of pretending the token state is unknown-safe
    fn acquire_fence(&self, delivery: &OutboxDelivery) -> Result<FenceGuard, String> {
        let ledger = self.fence_ledger_path(&delivery.work_id);
        let lock = self
            .journal_dir
            .join(format!("fence-ledger-{}.lock", delivery.work_id));
        let mut attempts = 0;
        let lock_file = loop {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock)
            {
                Ok(file) => break file,
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    attempts += 1;
                    if attempts > 40 {
                        return Err("fence ledger busy".to_string());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(error) => return Err(format!("fence ledger lock failed: {error}")),
            }
        };
        let outcome = (|| {
            let current: Option<i64> = match fs::read_to_string(&ledger) {
                Ok(raw) => Some(
                    raw.trim()
                        .parse()
                        .map_err(|_| "fence ledger is corrupt".to_string())?,
                ),
                Err(error) if error.kind() == ErrorKind::NotFound => None,
                Err(error) => return Err(format!("fence ledger unreadable: {error}")),
            };
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
        })();
        match outcome {
            Ok(()) => Ok(FenceGuard {
                _lock: lock_file,
                lock_path: lock,
            }),
            Err(error) => {
                let _ = fs::remove_file(&lock);
                Err(error)
            }
        }
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
                // one proves NOTHING about the previous handler being dead —
                // recovery requires explicit ownership (CR #58 r3 P2-4).
                if matches!(existing.state.as_str(), "succeeded" | "failed") || existing.unknown {
                    return existing;
                }
                return self.recover_or_wait(existing);
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
        // writes that actually happened (CR #41 P2-4). The fence guard is
        // held for the whole effect phase (CR #58 r3 P1).
        let _fence_guard = match self.acquire_fence(delivery) {
            Ok(guard) => guard,
            Err(error) => {
                let mut outcome = self.base_outcome(delivery, "failed");
                outcome.error = Some(error);
                let _ = self.persist(&outcome);
                return outcome;
            }
        };
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
                .and_then(|_| guarded_write(dest, content));
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

    /// Recovery ownership protocol: a live duplicate returns the in-flight
    /// record unchanged; only a caller that acquires the recovery lock AND
    /// finds the record stale may convert it to unknown. Terminal records
    /// discovered on re-read win over any stale local copy (CR #58 r3 P2-4).
    fn recover_or_wait(&self, existing: RunnerOutcome) -> RunnerOutcome {
        let recovery_lock = self
            .journal_dir
            .join(format!("{}.recovery.lock", existing.execution_id));
        let lock = match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&recovery_lock)
        {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                // Another handler holds recovery ownership; report the
                // in-flight record unchanged.
                return existing;
            }
            Err(_) => return existing,
        };
        let result = (|| {
            let current = match self.load(&existing.execution_id) {
                JournalLoad::Owned(current) => current,
                _ => return existing,
            };
            if matches!(current.state.as_str(), "succeeded" | "failed") || current.unknown {
                return current;
            }
            let stale_ms = self.recovery_stale_ms;
            let age_ms = fs::metadata(self.journal_path(&current.execution_id))
                .and_then(|m| m.modified())
                .ok()
                .and_then(|m| m.elapsed().ok())
                .map(|e| e.as_millis() as u64)
                .unwrap_or(0);
            if age_ms < stale_ms {
                // Still fresh: the owner may be alive; report unchanged.
                return current;
            }
            let mut recovered = current;
            recovered.state = "unknown".into();
            recovered.unknown = true;
            recovered.error = Some("previous handler died mid-execution; effects uncertain".into());
            let _ = self.persist_result(&recovered);
            recovered
        })();
        drop(lock);
        let _ = fs::remove_file(&recovery_lock);
        result
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
        // Atomic replace: a torn write must never look like a valid journal.
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, &bytes)?;
        fs::File::open(&tmp)?.sync_all()?;
        fs::rename(&tmp, &path)?;
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

/// Guard keeping the per-work fence lock for the whole effect phase.
struct FenceGuard {
    _lock: fs::File,
    lock_path: PathBuf,
}

impl Drop for FenceGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.lock_path);
    }
}

/// On unix the final component is opened with O_NOFOLLOW so a symlink
/// swapped in after the last check cannot be followed (CR #58 r3 P1).
/// Residual note: full resolution control would need openat2-style
/// semantics; ancestor re-checks plus O_NOFOLLOW are what std+libc offer.
#[cfg(unix)]
fn guarded_write(dest: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dest)?;
    file.write_all(content.as_bytes())?;
    file.sync_all()
}

#[cfg(not(unix))]
fn guarded_write(dest: &Path, content: &str) -> std::io::Result<()> {
    // Weaker platform: the write-time symlink_metadata and ancestor checks
    // in the caller still apply, but the final open cannot refuse links.
    fs::write(dest, content)
}
