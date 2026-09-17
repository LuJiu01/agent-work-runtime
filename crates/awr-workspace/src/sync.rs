//! The exchange plane's semantics: an index, a single-object commit, and
//! conflicts that are reported instead of merged.
//!
//! Three rules decide everything here:
//!
//! - Tracked files stay authoritative on the host that owns them. This layer
//!   moves bytes; it never rewrites a source and never deletes one.
//! - The manifest is the index, and publishing is one compare-and-swap on it,
//!   so a killed publish is never half-visible to the peer. Content objects are
//!   content-addressed and immutable, so uploading them first costs bytes and
//!   nothing else.
//! - A path changed on both ends is a conflict, never a merge. The operator
//!   decides; the tool does not guess.
use crate::backend::{Backend, LocalStore, MemoryStore, Precondition, parallel_map};
use crate::config::WorkspaceConfig;
use crate::credentials::{self, Credentials};
use crate::fsutil::{self, EntryKind};
use crate::layout;
use crate::{MAX_INDEX_FILES, MAX_OBJECT_BYTES};
use awr_core::{Error, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

/// The manifest is written in the same shape by every client, so a host running
/// one version and a host running another read one index rather than two.
const MANIFEST_SCHEMA: u32 = layout::MANIFEST_SCHEMA;
const STATE_FILE_SCHEMA: u32 = layout::SCHEMA;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct FileState {
    base_sha: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    synced_at: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct HandoffState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    etag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pushed_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pulled_at: Option<i64>,
}

/// What this host last agreed with the workspace about.
///
/// Local, and never shared: it sits in `.awr/` next to AWR's own runtime, and
/// losing it costs one comparison round rather than any data.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct State {
    schema: u32,
    files: BTreeMap<String, FileState>,
    handoffs: BTreeMap<String, HandoffState>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            schema: STATE_FILE_SCHEMA,
            files: BTreeMap::new(),
            handoffs: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManifestEntry {
    sha256: String,
    #[serde(default)]
    size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pushed_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pushed_at: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
struct Manifest {
    schema: u32,
    project_key: String,
    #[serde(default)]
    revision: Option<u64>,
    #[serde(default)]
    files: BTreeMap<String, ManifestEntry>,
}

/// The workspace's view of every tracked path, plus how it was obtained.
#[derive(Debug, Clone)]
struct Index {
    files: BTreeMap<String, ManifestEntry>,
    /// The etag a commit has to match; absent when the index came from pointers.
    etag: Option<String>,
    source: &'static str,
    revision: Option<u64>,
}

struct PublishPlan {
    index: Index,
    pending: Vec<(String, Vec<u8>, String)>,
    unchanged: Vec<(String, String)>,
    conflicts: Vec<Conflict>,
    dropped: BTreeSet<String>,
    now: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusRow {
    pub path: String,
    /// `in_sync`, `local_ahead`, `remote_ahead`, `index_missing_local`,
    /// `missing_local`, `conflicted` or `conflicted_first_sync`.
    pub state: String,
    pub local: String,
    pub base: String,
    pub remote: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusReport {
    pub project_key: String,
    pub host: String,
    pub files: Vec<StatusRow>,
    pub index_source: &'static str,
    pub conflicted: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PushedFile {
    pub path: String,
    pub sha256: String,
    pub size: u64,
    /// False when these exact bytes were already in the store.
    pub content_uploaded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Conflict {
    pub path: String,
    pub expected_remote: Option<String>,
    pub actual_remote: Option<String>,
    pub local: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PublishReport {
    pub pushed: Vec<PushedFile>,
    pub unchanged: Vec<String>,
    pub conflicts: Vec<Conflict>,
    pub index_source: &'static str,
    /// `cas` when the store enforces the compare and swap; `guarded` when the
    /// commit is instead confirmed by reading its own write back.
    pub commit_mode: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub republished_after_index_drop: Vec<String>,
    /// Paths whose commit never landed because another host committed the index
    /// every time this one rebuilt it. Nothing was overwritten and nothing is
    /// lost - these paths are still this host's to publish, and publishing again
    /// is the whole remedy. It is not a conflict: nobody else touched them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub contended: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pointer_mirror_failures: Vec<String>,
    /// Present only on a preview: the plan was computed, nothing was written.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DropReport {
    pub dropped: Vec<String>,
    pub skipped_absent: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub contended: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pointer_delete_failures: Vec<String>,
    pub index_source: &'static str,
    pub commit_mode: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct PulledFile {
    pub path: String,
    pub sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pushed_by: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PullReport {
    pub pulled: Vec<PulledFile>,
    pub unchanged: Vec<String>,
    pub conflicts: Vec<Conflict>,
    pub changed: bool,
    pub index_source: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct HandoffPulled {
    pub key: String,
    pub path: String,
    pub sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SyncReport {
    pub host: String,
    pub changed: bool,
    pub index_source: &'static str,
    pub pulled: Vec<PulledFile>,
    /// Paths that changed on both sides. The other files in this report were
    /// taken, and these were left exactly as they were.
    pub conflicts: Vec<Conflict>,
    pub handoffs: Vec<HandoffPulled>,
    pub inbound_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct HandoffPushed {
    pub key: String,
    pub sha256: String,
    /// True when these exact bytes were already published under this name.
    pub idempotent_replay: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexDrift {
    pub path: String,
    pub manifest: String,
    pub pointer: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerifyIndexReport {
    pub manifest_files: usize,
    pub pointer_files: usize,
    pub drift: Vec<IndexDrift>,
    pub drift_count: usize,
}

/// Open the store a config names, with the credentials this machine holds.
///
/// Credentials come from the project's own file first and from the environment
/// second, so a machine can be set up once and a job runner can still supply
/// them without writing a file.
pub fn open_backend(config: &WorkspaceConfig) -> Result<Box<dyn Backend>> {
    match config.store.backend.as_str() {
        "local" => {
            let path = config.store.path.clone().ok_or_else(|| {
                Error::InvalidInput("a local workspace needs store.path".to_string())
            })?;
            Ok(Box::new(LocalStore::new(path)?))
        }
        "s3" => Ok(Box::new(crate::s3::S3Store::new(
            config.store.clone(),
            load_credentials(config)?,
        )?)),
        other => Err(Error::InvalidInput(format!(
            "workspace store.backend must be s3 or local, not {other:?}"
        ))),
    }
}

fn load_credentials(config: &WorkspaceConfig) -> Result<Credentials> {
    if let Some(stored) = credentials::load(&config.credentials)?
        && stored.missing_fields().is_empty()
    {
        return Ok(stored);
    }
    let from_environment = Credentials {
        access_key: std::env::var("AWR_WORKSPACE_ACCESS_KEY").unwrap_or_default(),
        secret_key: std::env::var("AWR_WORKSPACE_SECRET_KEY").unwrap_or_default(),
        session_token: std::env::var("AWR_WORKSPACE_SESSION_TOKEN").ok(),
    };
    if from_environment.missing_fields().is_empty() {
        return Ok(from_environment);
    }
    Err(Error::InvalidInput(
        "no workspace credentials: run `awr workspace credential set --stdin`, or set \
         AWR_WORKSPACE_ACCESS_KEY and AWR_WORKSPACE_SECRET_KEY"
            .into(),
    ))
}

pub struct Workspace {
    backend: Box<dyn Backend>,
    project_key: String,
    root: PathBuf,
    host: String,
    state_path: PathBuf,
    track: Vec<String>,
    concurrency: usize,
    state: State,
    /// When set and true, pull/sync stop before further local writes.
    /// SessionStart uses this so a timed-out hook cannot keep mutating sources
    /// after the agent has already been told the exchange was left for later.
    cancel: Option<Arc<AtomicBool>>,
}

impl Workspace {
    pub fn open(config: &WorkspaceConfig, backend: Box<dyn Backend>) -> Result<Self> {
        layout::validate_project_key(&config.project_key)?;
        let state = match std::fs::read(&config.state) {
            Ok(body) => serde_json::from_slice::<State>(&body).map_err(|error| {
                Error::InvalidInput(format!(
                    "workspace state {} is not readable: {error}",
                    config.state.display()
                ))
            })?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(error) => return Err(Error::Io(error)),
        };
        Ok(Self {
            backend,
            project_key: config.project_key.clone(),
            root: config.root.clone(),
            host: config.host.clone(),
            state_path: config.state.clone(),
            track: config.track.clone(),
            concurrency: config.store.concurrency,
            state,
            cancel: None,
        })
    }

    /// A workspace backed by memory, for tests of the semantics alone.
    pub fn in_memory(config: &WorkspaceConfig) -> Result<Self> {
        Self::open(config, Box::new(MemoryStore::new()))
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn project_key(&self) -> &str {
        &self.project_key
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Attach a cancellation flag checked before local writes.
    pub fn with_cancel(mut self, cancel: Arc<AtomicBool>) -> Self {
        self.cancel = Some(cancel);
        self
    }

    fn ensure_active(&self) -> Result<()> {
        if self
            .cancel
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire))
        {
            return Err(Error::SourceUnavailable(
                "workspace exchange cancelled: the session-start deadline elapsed before the store answered"
                    .into(),
            ));
        }
        Ok(())
    }

    /// How many requests this run has spent, so distance from the store is an
    /// observable rather than a guess.
    pub fn requests(&self) -> u64 {
        self.backend.requests()
    }

    pub fn backend_label(&self) -> String {
        self.backend.describe()
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.state_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_vec_pretty(&self.state)?;
        crate::backend::write_atomically(&self.state_path, &body)
    }

    /// Every path the config tracks, as this machine has it right now.
    fn tracked(&self) -> Result<Vec<String>> {
        let mut out = BTreeSet::new();
        for entry in &self.track {
            layout::check_relpath(entry)?;
            let full = fsutil::resolve_nofollow(&self.root, entry)?;
            match fsutil::entry_kind(&full) {
                Ok(EntryKind::Dir) => walk(&self.root, &full, &mut out)?,
                Ok(EntryKind::File) => {
                    out.insert(entry.clone());
                }
                Ok(EntryKind::Symlink) => {
                    return Err(Error::RuleViolation(format!(
                        "tracked path {entry:?} is a symbolic link; tracked trees never follow links"
                    )));
                }
                Ok(EntryKind::Other) => {}
                Err(error)
                    if matches!(
                        &error,
                        Error::Io(io) if io.kind() == std::io::ErrorKind::NotFound
                    ) => {}
                Err(error) => return Err(error),
            }
            if out.len() > MAX_INDEX_FILES {
                return Err(Error::InvalidInput(format!(
                    "workspace track expands to more than {MAX_INDEX_FILES} files; narrow project.track"
                )));
            }
        }
        Ok(out.into_iter().collect())
    }

    fn local_bytes(&self, relpath: &str) -> Result<Option<Vec<u8>>> {
        let path = fsutil::resolve_nofollow(&self.root, relpath)?;
        fsutil::read_nofollow_optional(&path)
    }

    fn write_local(&self, relpath: &str, body: &[u8]) -> Result<()> {
        self.ensure_active()?;
        if body.len() as u64 > MAX_OBJECT_BYTES {
            return Err(Error::InvalidInput(format!(
                "workspace object {relpath} is {} bytes; maximum is {MAX_OBJECT_BYTES}",
                body.len()
            )));
        }
        let path = fsutil::ensure_writable_nofollow(&self.root, relpath)?;
        crate::backend::write_atomically(&path, body)
    }

    // -- the index
    fn remote_index(&self, pointers_only: bool) -> Result<Index> {
        if !pointers_only {
            let key = layout::manifest(&self.project_key);
            if let Some((body, etag)) = self.backend.get_meta(&key)? {
                let manifest: Manifest = serde_json::from_slice(&body).map_err(|_| {
                    Error::SourceUnavailable(format!("workspace manifest {key} is not valid JSON"))
                })?;
                if manifest.schema != MANIFEST_SCHEMA || manifest.project_key != self.project_key {
                    return Err(Error::SourceUnavailable(format!(
                        "workspace manifest {key} does not describe project {}",
                        self.project_key
                    )));
                }
                if manifest.files.len() > MAX_INDEX_FILES {
                    return Err(Error::SourceUnavailable(format!(
                        "workspace manifest {key} names {} files; maximum is {MAX_INDEX_FILES}",
                        manifest.files.len()
                    )));
                }
                for (relpath, entry) in &manifest.files {
                    layout::check_relpath(relpath)?;
                    validate_entry(relpath, entry)?;
                }
                return Ok(Index {
                    files: manifest.files,
                    etag: Some(etag),
                    source: "manifest",
                    revision: manifest.revision,
                });
            }
        }
        self.pointer_index()
    }

    /// Rebuild the index the way a client that predates the manifest would:
    /// one list, then one read per pointer, concurrently.
    fn pointer_index(&self) -> Result<Index> {
        let prefix = layout::files_prefix(&self.project_key);
        let keys: Vec<String> = self
            .backend
            .list(&prefix)?
            .into_iter()
            .map(|item| item.key)
            .filter(|key| layout::relpath_of_pointer(&self.project_key, key).is_some())
            .collect();
        if keys.len() > MAX_INDEX_FILES {
            return Err(Error::SourceUnavailable(format!(
                "workspace pointer set under {prefix} has {} entries; maximum is {MAX_INDEX_FILES}",
                keys.len()
            )));
        }
        let fetched = parallel_map(&keys, self.concurrency, |key| self.backend.get_meta(key))?;
        let mut files = BTreeMap::new();
        for (key, body) in keys.iter().zip(fetched) {
            let Some(relpath) = layout::relpath_of_pointer(&self.project_key, key) else {
                return Err(Error::SourceUnavailable(format!(
                    "workspace pointer key {key} is not a project path under {}",
                    self.project_key
                )));
            };
            layout::check_relpath(&relpath)?;
            let Some((body, _etag)) = body else {
                return Err(Error::SourceUnavailable(format!(
                    "workspace pointer {key} is listed but has no body"
                )));
            };
            let entry: ManifestEntry = serde_json::from_slice(&body).map_err(|_| {
                Error::SourceUnavailable(format!("workspace pointer {key} is not valid JSON"))
            })?;
            validate_entry(&relpath, &entry)?;
            files.insert(relpath, entry);
        }
        Ok(Index {
            files,
            etag: None,
            source: "pointers",
            revision: None,
        })
    }

    fn state_of(local: Option<&str>, remote: Option<&str>, base: Option<&str>) -> &'static str {
        match (local, remote) {
            (None, _) => "missing_local",
            (Some(local), Some(remote)) if local == remote => "in_sync",
            // The index carries nothing for a path this host already published.
            // A store without compare-and-swap can drop an entry when a
            // concurrent commit writes a body built from a stale read. It is
            // *not* "the peer is ahead": reporting it that way would send the
            // operator to pull, when the remedy is to publish again.
            (_, None) if base.is_some() => "index_missing_local",
            _ if remote == base => "local_ahead",
            (Some(local), _) if Some(local) == base => "remote_ahead",
            _ if base.is_none() => "conflicted_first_sync",
            _ => "conflicted",
        }
    }

    // -- operations
    pub fn status(&self, pointers_only: bool) -> Result<StatusReport> {
        let index = self.remote_index(pointers_only)?;
        let paths: BTreeSet<String> = self
            .tracked()?
            .into_iter()
            .chain(index.files.keys().cloned())
            .collect();
        let mut files = Vec::new();
        for relpath in paths {
            let local = self.local_bytes(&relpath)?;
            let local_sha = local.as_deref().map(crate::digest);
            let remote_sha = index.files.get(&relpath).map(|entry| entry.sha256.as_str());
            let base = self.base_of(&relpath);
            files.push(StatusRow {
                state: Self::state_of(local_sha.as_deref(), remote_sha, base.as_deref())
                    .to_string(),
                local: short(local_sha.as_deref()),
                base: short(base.as_deref()),
                remote: short(remote_sha),
                path: relpath,
            });
        }
        Ok(StatusReport {
            conflicted: files
                .iter()
                .filter(|row| row.state.starts_with("conflicted"))
                .map(|row| row.path.clone())
                .collect(),
            project_key: self.project_key.clone(),
            host: self.host.clone(),
            index_source: index.source,
            files,
        })
    }

    fn base_of(&self, relpath: &str) -> Option<String> {
        self.state
            .files
            .get(relpath)
            .map(|state| state.base_sha.clone())
            .filter(|base| !base.is_empty())
    }

    pub fn publish(&mut self) -> Result<PublishReport> {
        self.publish_entries(false)
    }

    /// The publish plan with no writes: no content PUT, no index commit, no
    /// local state change. Content keys are HEADed so `content_uploaded` is
    /// whether those bytes are missing from the store, not a guess.
    pub fn publish_preview(&self) -> Result<PublishReport> {
        let plan = self.plan_publish(false)?;
        let uploaded = parallel_map(
            &plan.pending,
            self.concurrency,
            |(relpath, _data, digest)| {
                Ok(self
                    .backend
                    .head(&layout::content(&self.project_key, relpath, digest))?
                    .is_none())
            },
        )?;
        Ok(PublishReport {
            pushed: plan
                .pending
                .iter()
                .zip(uploaded)
                .map(|((relpath, data, digest), content_uploaded)| PushedFile {
                    path: relpath.clone(),
                    sha256: digest.clone(),
                    size: data.len() as u64,
                    content_uploaded,
                })
                .collect(),
            unchanged: plan
                .unchanged
                .iter()
                .map(|(path, _sha)| path.clone())
                .collect(),
            conflicts: plan.conflicts,
            index_source: plan.index.source,
            commit_mode: self.commit_mode(),
            republished_after_index_drop: plan
                .pending
                .iter()
                .map(|(path, _, _)| path.clone())
                .filter(|path| plan.dropped.contains(path))
                .collect(),
            contended: Vec::new(),
            pointer_mirror_failures: Vec::new(),
            dry_run: true,
        })
    }

    /// Re-register the entries this host published and the index lost.
    ///
    /// A store without compare-and-swap can lose one entry to a commit built
    /// from a stale read. Those bytes are already in the store and content
    /// addressed, so the repair is one small commit - and it has to stay a
    /// repair: only the paths whose local bytes are exactly what this host
    /// published before are eligible, never the half-finished edits a session
    /// starts with.
    pub fn repair_index(&mut self) -> Result<PublishReport> {
        self.publish_entries(true)
    }

    fn plan_publish(&self, repair_only: bool) -> Result<PublishPlan> {
        let index = self.remote_index(false)?;
        let mut pending = Vec::new();
        let mut unchanged = Vec::new();
        let mut conflicts = Vec::new();
        let mut dropped = BTreeSet::new();
        let now = crate::timestamp();

        for relpath in self.tracked()? {
            let Some(local) = self.local_bytes(&relpath)? else {
                continue;
            };
            let local_sha = crate::digest(&local);
            let remote_sha = index.files.get(&relpath).map(|entry| entry.sha256.clone());
            let base = self.base_of(&relpath);
            if remote_sha.as_deref() == Some(local_sha.as_str()) {
                unchanged.push((relpath, local_sha));
                continue;
            }
            if remote_sha.is_some() && remote_sha != base {
                conflicts.push(Conflict {
                    path: relpath,
                    expected_remote: base,
                    actual_remote: remote_sha,
                    local: Some(local_sha),
                });
                continue;
            }
            if remote_sha.is_none() && base.as_deref() == Some(local_sha.as_str()) {
                // These exact bytes were published and the index lost the
                // entry. The content object is content-addressed and still
                // there, so re-publishing the entry is the whole repair.
                dropped.insert(relpath.clone());
            } else if repair_only {
                continue;
            }
            pending.push((relpath, local, local_sha));
        }
        Ok(PublishPlan {
            index,
            pending,
            unchanged,
            conflicts,
            dropped,
            now,
        })
    }

    fn publish_entries(&mut self, repair_only: bool) -> Result<PublishReport> {
        let mut plan = self.plan_publish(repair_only)?;
        for (relpath, sha) in &plan.unchanged {
            self.state.files.insert(
                relpath.clone(),
                FileState {
                    base_sha: sha.clone(),
                    synced_at: Some(plan.now),
                },
            );
        }

        let uploaded = parallel_map(
            &plan.pending,
            self.concurrency,
            |(relpath, data, digest)| {
                self.ensure_active()?;
                let key = layout::content(&self.project_key, relpath, digest);
                let outcome = self.backend.put(&key, data, Precondition::Absent)?;
                if outcome.created {
                    return Ok(true);
                }
                // Absent said the object exists. Confirm those bytes are exactly
                // the digest we are about to name in the index; a wrong object
                // under a content-addressed key must not let publish succeed.
                match self.backend.get(&key)? {
                    Some(existing)
                        if existing.len() == data.len() && crate::digest(&existing) == *digest =>
                    {
                        Ok(false)
                    }
                    Some(existing) => Err(Error::SourceUnavailable(format!(
                        "content object {key} already holds {} bytes that are not {digest}",
                        existing.len()
                    ))),
                    None => Err(Error::SourceUnavailable(format!(
                        "content object {key} was reported present but could not be read"
                    ))),
                }
            },
        )?;

        let mut entries: BTreeMap<String, ManifestEntry> = BTreeMap::new();
        let mut uploaded_here: BTreeSet<String> = BTreeSet::new();
        for ((relpath, data, digest), created) in plan.pending.iter().zip(uploaded) {
            if created {
                uploaded_here.insert(relpath.clone());
            }
            entries.insert(
                relpath.clone(),
                ManifestEntry {
                    sha256: digest.clone(),
                    size: data.len() as u64,
                    pushed_by: Some(self.host.clone()),
                    pushed_at: Some(plan.now),
                },
            );
        }

        // Nothing to add and a manifest that already exists means there is
        // nothing to commit; a missing manifest is always written, so the
        // pointer-only layout upgrades itself on the next publish - but a
        // repair never creates an index it was not asked to fix.
        let mut published: BTreeMap<String, ManifestEntry> = BTreeMap::new();
        let mut conflicts = plan.conflicts;
        let mut contended = Vec::new();
        if !entries.is_empty() || (plan.index.etag.is_none() && !repair_only) {
            let mut remaining = entries;
            for _attempt in 1..=3 {
                if self.commit_manifest(&plan.index, &remaining)? {
                    published = std::mem::take(&mut remaining);
                    break;
                }
                // A peer committed first. Re-read the truth and keep only what
                // is still this host's to publish.
                plan.index = self.remote_index(false)?;
                let mut survivors = BTreeMap::new();
                for (relpath, entry) in remaining {
                    let remote_sha = plan
                        .index
                        .files
                        .get(&relpath)
                        .map(|item| item.sha256.clone());
                    let base = self.base_of(&relpath);
                    if remote_sha.is_none() || remote_sha == base {
                        survivors.insert(relpath, entry);
                    } else {
                        conflicts.push(Conflict {
                            path: relpath,
                            expected_remote: base,
                            actual_remote: remote_sha,
                            local: Some(entry.sha256),
                        });
                    }
                }
                remaining = survivors;
                if remaining.is_empty() {
                    break;
                }
            }
            for relpath in remaining.into_keys() {
                contended.push(relpath);
            }
        }

        let mut mirror_failures = Vec::new();
        if !published.is_empty() {
            // Pointers mirror the manifest for single-file readers. They are
            // derived data: written after the commit, never a precondition, and
            // a failure here does not fail the publish.
            let items: Vec<(String, ManifestEntry)> = published
                .iter()
                .map(|(path, entry)| (path.clone(), entry.clone()))
                .collect();
            let mirrored = parallel_map(&items, self.concurrency, |(relpath, entry)| {
                self.mirror_pointer(relpath, entry)
            })?;
            mirror_failures = mirrored.into_iter().flatten().collect();
            for (relpath, entry) in &published {
                self.state.files.insert(
                    relpath.clone(),
                    FileState {
                        base_sha: entry.sha256.clone(),
                        synced_at: Some(plan.now),
                    },
                );
            }
        }
        self.save()?;

        Ok(PublishReport {
            pushed: published
                .iter()
                .map(|(path, entry)| PushedFile {
                    path: path.clone(),
                    sha256: entry.sha256.clone(),
                    size: entry.size,
                    content_uploaded: uploaded_here.contains(path),
                })
                .collect(),
            unchanged: plan
                .unchanged
                .iter()
                .map(|(path, _sha)| path.clone())
                .collect(),
            conflicts,
            index_source: plan.index.source,
            commit_mode: self.commit_mode(),
            republished_after_index_drop: published
                .keys()
                .filter(|path| plan.dropped.contains(*path))
                .cloned()
                .collect(),
            contended,
            pointer_mirror_failures: mirror_failures,
            dry_run: false,
        })
    }

    fn commit_mode(&self) -> &'static str {
        if self.backend.supports_if_match() {
            "cas"
        } else {
            "guarded"
        }
    }

    /// Stop sharing paths: remove them from the index, leave local files and
    /// content objects alone.
    ///
    /// Untracking is two steps. A path still covered by `project.track` is
    /// refused, because the next session start would register it again. Edit
    /// the track list first, then drop. The filesystem is not consulted: the
    /// file may already be gone locally.
    pub fn drop_paths(&mut self, paths: &[String]) -> Result<DropReport> {
        let mut requested = BTreeSet::new();
        for raw in paths {
            let relpath = raw.trim();
            layout::check_relpath(relpath)?;
            requested.insert(relpath.to_string());
        }
        if requested.is_empty() {
            return Err(Error::InvalidInput(
                "workspace drop needs at least one --path".into(),
            ));
        }
        let still: Vec<&String> = requested
            .iter()
            .filter(|relpath| self.covered_by_track(relpath))
            .collect();
        if !still.is_empty() {
            return Err(Error::InvalidInput(format!(
                "{} path(s) are still in project.track; remove them from track first, then drop: {}",
                still.len(),
                still
                    .iter()
                    .map(|path| path.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }

        let mut index = self.remote_index(false)?;
        let mut skipped_absent = Vec::new();
        let mut remaining = BTreeSet::new();
        for relpath in requested {
            if index.files.contains_key(&relpath) {
                remaining.insert(relpath);
            } else {
                skipped_absent.push(relpath);
            }
        }

        let mut dropped = BTreeSet::new();
        let mut contended = Vec::new();
        if !remaining.is_empty() {
            if index.etag.is_some() {
                for _attempt in 1..=3 {
                    let mut files = index.files.clone();
                    for path in &remaining {
                        files.remove(path);
                    }
                    if self.commit_files(&index, &files)? {
                        dropped = std::mem::take(&mut remaining);
                        break;
                    }
                    index = self.remote_index(false)?;
                    let mut survivors = BTreeSet::new();
                    for path in remaining {
                        if index.files.contains_key(&path) {
                            survivors.insert(path);
                        } else {
                            skipped_absent.push(path);
                        }
                    }
                    remaining = survivors;
                    if remaining.is_empty() {
                        break;
                    }
                }
                contended = remaining.into_iter().collect();
            } else {
                dropped = remaining;
            }
        }

        let items: Vec<String> = dropped.iter().cloned().collect();
        let deleted = parallel_map(&items, self.concurrency, |relpath| {
            match self
                .backend
                .delete(&layout::pointer(&self.project_key, relpath))
            {
                Ok(_) => Ok(None),
                Err(error) => Ok(Some(format!("{relpath}: {error}"))),
            }
        })?;
        let pointer_delete_failures: Vec<String> = deleted.into_iter().flatten().collect();

        for path in dropped.iter().chain(&skipped_absent) {
            self.state.files.remove(path);
        }
        self.save()?;
        skipped_absent.sort();

        Ok(DropReport {
            dropped: dropped.into_iter().collect(),
            skipped_absent,
            contended,
            pointer_delete_failures,
            index_source: index.source,
            commit_mode: self.commit_mode(),
        })
    }

    fn covered_by_track(&self, relpath: &str) -> bool {
        self.track.iter().any(|entry| {
            relpath == entry.as_str()
                || relpath
                    .strip_prefix(entry.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    }

    fn manifest_body(
        &self,
        index: &Index,
        files: &BTreeMap<String, ManifestEntry>,
    ) -> Result<Vec<u8>> {
        #[derive(Serialize)]
        struct Body<'a> {
            schema: u32,
            project_key: &'a str,
            revision: u64,
            updated_at: i64,
            updated_by: &'a str,
            files: &'a BTreeMap<String, ManifestEntry>,
        }
        Ok(serde_json::to_vec_pretty(&Body {
            schema: MANIFEST_SCHEMA,
            project_key: &self.project_key,
            revision: index.revision.unwrap_or(0) + 1,
            updated_at: crate::timestamp(),
            updated_by: &self.host,
            files,
        })?)
    }

    fn commit_files(&self, index: &Index, files: &BTreeMap<String, ManifestEntry>) -> Result<bool> {
        let key = layout::manifest(&self.project_key);
        let body = self.manifest_body(index, files)?;
        match index.etag.as_deref() {
            None => Ok(self.backend.put(&key, &body, Precondition::Absent)?.created),
            Some(etag) if self.backend.supports_if_match() => Ok(self
                .backend
                .put(&key, &body, Precondition::Match(etag.to_string()))?
                .created),
            Some(etag) => {
                // A store without compare-and-swap still gets an atomic commit:
                // the manifest is one object, so no peer ever sees half a
                // publish. What is missing is mutual exclusion. The pre-write
                // HEAD narrows a detectable stale base to one request, and the
                // post-write HEAD confirms our bytes landed. It cannot stop a
                // peer that writes between HEAD and PUT: our PUT overwrites that
                // peer and the readback sees our ETag. Guarded mode is for
                // personal sequential hosts on stores without If-Match, not for
                // concurrent multi-writer safety. Prefer a CAS store when two
                // machines may publish at once.
                match self.backend.head(&key)? {
                    Some(current) if current.etag == *etag => {}
                    _ => return Ok(false),
                }
                let outcome = self.backend.put(&key, &body, Precondition::None)?;
                if !outcome.created {
                    return Ok(false);
                }
                let Some(current) = self.backend.head(&key)? else {
                    return Ok(false);
                };
                Ok(Some(current.etag) == outcome.etag)
            }
        }
    }

    /// Commit the index in one write, or report that a peer got there first.
    fn commit_manifest(
        &self,
        index: &Index,
        entries: &BTreeMap<String, ManifestEntry>,
    ) -> Result<bool> {
        let mut files = index.files.clone();
        files.extend(
            entries
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
        self.commit_files(index, &files)
    }

    fn mirror_pointer(&self, relpath: &str, entry: &ManifestEntry) -> Result<Option<String>> {
        #[derive(Serialize)]
        struct Pointer<'a> {
            schema: u32,
            sha256: &'a str,
            size: u64,
            #[serde(skip_serializing_if = "Option::is_none")]
            pushed_by: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            pushed_at: Option<i64>,
        }
        let body = serde_json::to_vec_pretty(&Pointer {
            schema: layout::SCHEMA,
            sha256: &entry.sha256,
            size: entry.size,
            pushed_by: entry.pushed_by.as_deref(),
            pushed_at: entry.pushed_at,
        })?;
        let key = layout::pointer(&self.project_key, relpath);
        match self.backend.put(&key, &body, Precondition::None) {
            Ok(_) => Ok(None),
            Err(error) => Ok(Some(format!("{relpath}: {error}"))),
        }
    }

    pub fn pull(&mut self) -> Result<PullReport> {
        let index = self.remote_index(false)?;
        let now = crate::timestamp();
        let mut fetches: Vec<(String, String, Option<String>)> = Vec::new();
        let mut conflicts = Vec::new();
        let mut skipped = Vec::new();

        let paths: BTreeSet<String> = self
            .tracked()?
            .into_iter()
            .chain(index.files.keys().cloned())
            .collect();
        for relpath in paths {
            let entry = index.files.get(&relpath);
            let Some(remote_sha) = entry.map(|entry| entry.sha256.clone()) else {
                skipped.push(relpath);
                continue;
            };
            let local = self.local_bytes(&relpath)?;
            let local_sha = local.as_deref().map(crate::digest);
            let base = self.base_of(&relpath);
            if local_sha.as_deref() == Some(remote_sha.as_str()) {
                self.state.files.insert(
                    relpath.clone(),
                    FileState {
                        base_sha: remote_sha,
                        synced_at: Some(now),
                    },
                );
                skipped.push(relpath);
                continue;
            }
            if local_sha.is_some() && local_sha != base {
                conflicts.push(Conflict {
                    path: relpath,
                    expected_remote: base,
                    actual_remote: Some(remote_sha),
                    local: local_sha,
                });
                continue;
            }
            fetches.push((
                relpath,
                remote_sha,
                entry.and_then(|entry| entry.pushed_by.clone()),
            ));
        }

        // Content objects are immutable and content-addressed, so fetching them
        // concurrently is safe. Local writes stay on this thread.
        let blobs = parallel_map(&fetches, self.concurrency, |(relpath, sha256, _)| {
            self.backend
                .get(&layout::content(&self.project_key, relpath, sha256))
        })?;

        let mut pulled = Vec::new();
        for ((relpath, remote_sha, pushed_by), body) in fetches.iter().zip(blobs) {
            self.ensure_active()?;
            let Some(body) = body else {
                return Err(Error::SourceUnavailable(format!(
                    "the workspace has no content object for {relpath} ({remote_sha})"
                )));
            };
            if body.len() as u64 > MAX_OBJECT_BYTES {
                return Err(Error::InvalidInput(format!(
                    "workspace object {relpath} is {} bytes; maximum is {MAX_OBJECT_BYTES}",
                    body.len()
                )));
            }
            if crate::digest(&body) != *remote_sha {
                return Err(Error::SourceUnavailable(format!(
                    "the content object for {relpath} does not match its index entry"
                )));
            }
            // Re-check immediately before writing: the plan said this path was
            // absent or still at base, but the agent may have edited it after
            // the session-start hook began, and a cancelled hook must not write.
            let current = self.local_bytes(relpath)?;
            let current_sha = current.as_deref().map(crate::digest);
            let base = self.base_of(relpath);
            if current_sha.is_some() && current_sha != base {
                conflicts.push(Conflict {
                    path: relpath.clone(),
                    expected_remote: base,
                    actual_remote: Some(remote_sha.clone()),
                    local: current_sha,
                });
                continue;
            }
            self.write_local(relpath, &body)?;
            self.state.files.insert(
                relpath.clone(),
                FileState {
                    base_sha: remote_sha.clone(),
                    synced_at: Some(now),
                },
            );
            pulled.push(PulledFile {
                path: relpath.clone(),
                sha256: remote_sha.clone(),
                pushed_by: pushed_by.clone(),
            });
        }
        self.ensure_active()?;
        self.save()?;
        Ok(PullReport {
            changed: !pulled.is_empty(),
            pulled,
            unchanged: skipped,
            conflicts,
            index_source: index.source,
        })
    }

    pub fn sync(&mut self, handoff_outdir: &Path) -> Result<SyncReport> {
        let pulled = self.pull()?;
        let handoffs = self.handoff_pull(handoff_outdir)?;
        Ok(SyncReport {
            host: self.host.clone(),
            changed: pulled.changed,
            index_source: pulled.index_source,
            pulled: pulled.pulled,
            conflicts: pulled.conflicts,
            inbound_count: handoffs.len(),
            handoffs,
        })
    }

    pub fn verify_index(&self) -> Result<VerifyIndexReport> {
        let manifest = self.remote_index(false)?;
        let pointers = self.pointer_index()?;
        let paths: BTreeSet<&String> = manifest.files.keys().chain(pointers.files.keys()).collect();
        let mut drift = Vec::new();
        for relpath in paths {
            let left = manifest
                .files
                .get(relpath)
                .map(|entry| entry.sha256.as_str());
            let right = pointers
                .files
                .get(relpath)
                .map(|entry| entry.sha256.as_str());
            if left != right {
                drift.push(IndexDrift {
                    path: relpath.clone(),
                    manifest: short(left),
                    pointer: short(right),
                });
            }
        }
        Ok(VerifyIndexReport {
            manifest_files: manifest.files.len(),
            pointer_files: pointers.files.len(),
            drift_count: drift.len(),
            drift,
        })
    }

    pub fn handoff_push(&mut self, source: &Path, name: &str) -> Result<HandoffPushed> {
        layout::check_component(name)?;
        let body = fsutil::read_nofollow(source)?;
        let digest = crate::digest(&body);
        let key = layout::handoff(&self.project_key, &self.host, name);
        let outcome = self.backend.put(&key, &body, Precondition::Absent)?;
        if !outcome.created {
            let existing = self.backend.get(&key)?;
            if existing.is_none()
                || crate::digest(existing.as_deref().unwrap_or_default()) != digest
            {
                return Err(Error::WorkspaceConflict(format!(
                    "handoff {key} already holds different content; handoffs are write-once, \
                     so publish this one under another name"
                )));
            }
            return Ok(HandoffPushed {
                key,
                sha256: digest,
                idempotent_replay: true,
            });
        }
        self.state.handoffs.insert(
            key.clone(),
            HandoffState {
                sha256: Some(digest.clone()),
                etag: outcome.etag,
                pushed_at: Some(crate::timestamp()),
                pulled_at: None,
            },
        );
        self.save()?;
        Ok(HandoffPushed {
            key,
            sha256: digest,
            idempotent_replay: false,
        })
    }

    pub fn handoff_pull(&mut self, outdir: &Path) -> Result<Vec<HandoffPulled>> {
        let prefix = layout::handoffs_prefix(&self.project_key);
        let mut wanted = Vec::new();
        for item in self.backend.list(&prefix)? {
            let Some((origin, name)) = layout::handoff_parts(&self.project_key, &item.key) else {
                continue;
            };
            if origin == self.host {
                continue;
            }
            // Handoffs are write-once, so an unchanged etag means unchanged
            // bytes and the object does not have to be fetched again.
            if self
                .state
                .handoffs
                .get(&item.key)
                .and_then(|state| state.etag.as_deref())
                == Some(item.etag.as_str())
            {
                continue;
            }
            wanted.push((item.key, item.etag, origin, name));
        }
        let fetched = parallel_map(&wanted, self.concurrency, |(key, _etag, _origin, _name)| {
            self.backend.get(key)
        })?;
        let mut pulled = Vec::new();
        for ((key, etag, origin, name), body) in wanted.iter().zip(fetched) {
            self.ensure_active()?;
            let Some(body) = body else { continue };
            if body.len() as u64 > MAX_OBJECT_BYTES {
                return Err(Error::InvalidInput(format!(
                    "workspace handoff {key} is {} bytes; maximum is {MAX_OBJECT_BYTES}",
                    body.len()
                )));
            }
            // The directory comes from remote data, so both halves of the
            // target are checked as single path components before it is used.
            layout::check_component(origin)?;
            layout::check_component(name)?;
            let target = outdir.join(origin).join(name);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            crate::backend::write_atomically(&target, &body)?;
            let digest = crate::digest(&body);
            self.state.handoffs.insert(
                key.clone(),
                HandoffState {
                    sha256: Some(digest.clone()),
                    etag: Some(etag.clone()),
                    pushed_at: None,
                    pulled_at: Some(crate::timestamp()),
                },
            );
            pulled.push(HandoffPulled {
                key: key.clone(),
                path: target.display().to_string(),
                sha256: digest,
                summary: serde_json::from_slice::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("summary")
                            .and_then(|summary| summary.as_str())
                            .map(str::to_string)
                    }),
            });
        }
        self.save()?;
        Ok(pulled)
    }
}

fn walk(root: &Path, base: &Path, out: &mut BTreeSet<String>) -> Result<()> {
    for entry in std::fs::read_dir(base)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        // The reference client skips exactly one thing, and matching it matters
        // more than tidiness: a file one client tracks and the other silently
        // ignores is a sync that looks fine from both ends and never converges.
        if name == ".DS_Store" {
            continue;
        }
        let path = entry.path();
        match fsutil::entry_kind(&path)? {
            EntryKind::Symlink => {
                return Err(Error::RuleViolation(format!(
                    "the tracked tree contains symbolic link {}; tracked trees never follow links",
                    path.display()
                )));
            }
            EntryKind::Dir => {
                // Two directories hold material that never leaves a machine: the
                // repository's own history, and AWR's local runtime. `track` is the
                // operator's explicit boundary, so this is a refusal to act on it
                // rather than a quiet exception to it.
                if matches!(name.as_str(), ".git" | ".awr") {
                    return Err(Error::RuleViolation(format!(
                        "the tracked tree contains {}; narrow project.track so that neither a Git \
                         history nor AWR's local runtime is published",
                        path.display()
                    )));
                }
                walk(root, &path, out)?;
            }
            EntryKind::File => {
                let relpath = path
                    .strip_prefix(root)
                    .map_err(|_| {
                        Error::InvalidInput(format!(
                            "{} is outside the project root",
                            path.display()
                        ))
                    })?
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/");
                layout::check_relpath(&relpath)?;
                out.insert(relpath);
                if out.len() > MAX_INDEX_FILES {
                    return Err(Error::InvalidInput(format!(
                        "workspace track expands to more than {MAX_INDEX_FILES} files; narrow project.track"
                    )));
                }
            }
            EntryKind::Other => {}
        }
    }
    Ok(())
}

fn validate_entry(relpath: &str, entry: &ManifestEntry) -> Result<()> {
    if !is_sha256_hex(&entry.sha256) {
        return Err(Error::SourceUnavailable(format!(
            "workspace index entry {relpath} has a malformed sha256"
        )));
    }
    if entry.size > MAX_OBJECT_BYTES {
        return Err(Error::SourceUnavailable(format!(
            "workspace index entry {relpath} claims {} bytes; maximum is {MAX_OBJECT_BYTES}",
            entry.size
        )));
    }
    if let Some(host) = entry.pushed_by.as_deref() {
        layout::check_component(host)?;
    }
    Ok(())
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn short(digest: Option<&str>) -> String {
    digest.unwrap_or("").chars().take(12).collect()
}

/// The default place inbound handoffs land, relative to the project root.
pub const DEFAULT_HANDOFF_DIR: &str = "infra/handoffs";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Addressing, DEFAULT_CONCURRENCY, StoreConfig};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fixture {
        dir: PathBuf,
        mirror: PathBuf,
    }

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "awr workspace sync {}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("store")).unwrap();
            let mirror = dir.join("store");
            Self { dir, mirror }
        }

        fn root(&self, name: &str) -> PathBuf {
            let root = self.dir.join(name);
            std::fs::create_dir_all(&root).unwrap();
            root
        }

        fn config(&self, root: &Path, host: &str) -> WorkspaceConfig {
            WorkspaceConfig {
                project_key: "poc-infra".to_string(),
                root: root.to_path_buf(),
                host: host.to_string(),
                track: vec!["work-ledger.yaml".to_string(), "infra".to_string()],
                state: root.join(".awr/workspace.json"),
                credentials: root.join(".awr/workspace-credentials.json"),
                store: StoreConfig {
                    backend: "local".to_string(),
                    allow_insecure: false,
                    path: Some(self.mirror.clone()),
                    endpoint: String::new(),
                    bucket: String::new(),
                    region: "auto".to_string(),
                    addressing: Addressing::Auto,
                    prefix: String::new(),
                    concurrency: DEFAULT_CONCURRENCY,
                },
            }
        }

        fn workspace(&self, root: &Path, host: &str) -> Workspace {
            let config = self.config(root, host);
            let store = LocalStore::new(self.mirror.clone()).unwrap();
            Workspace::open(&config, Box::new(store)).unwrap()
        }

        fn workspace_with_track(&self, root: &Path, host: &str, track: Vec<String>) -> Workspace {
            let mut config = self.config(root, host);
            config.track = track;
            let store = LocalStore::new(self.mirror.clone()).unwrap();
            Workspace::open(&config, Box::new(store)).unwrap()
        }

        fn write(&self, root: &Path, relpath: &str, body: &str) {
            let path = root.join(relpath);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        }

        fn read(&self, root: &Path, relpath: &str) -> Option<String> {
            std::fs::read_to_string(root.join(relpath)).ok()
        }
    }

    fn states(report: &StatusReport) -> BTreeMap<String, String> {
        report
            .files
            .iter()
            .map(|row| (row.path.clone(), row.state.clone()))
            .collect()
    }

    /// Publishing from one host and pulling on the other is the whole point.
    #[test]
    fn a_publish_is_visible_to_the_peer_and_matches_everywhere() {
        let fixture = Fixture::new();
        let dev = fixture.root("dev");
        let mini = fixture.root("mini");
        fixture.write(&dev, "work-ledger.yaml", "work_items: []\n");
        fixture.write(&dev, "infra/evidence/dump.json", "{\"ok\":true}\n");

        let mut publisher = fixture.workspace(&dev, "macbook-codex");
        let report = publisher.publish().unwrap();
        assert_eq!(report.conflicts, Vec::new());
        // The first publish has no manifest to read, so it rebuilds the index
        // from the pointers and leaves the manifest behind for the next run.
        assert_eq!(report.index_source, "pointers");
        assert_eq!(report.commit_mode, "cas");
        assert_eq!(report.pushed.len(), 2);
        assert!(report.pushed.iter().all(|file| file.content_uploaded));
        assert_eq!(report.pushed[0].sha256.len(), 64);
        assert_eq!(publisher.status(false).unwrap().index_source, "manifest");

        let mut peer = fixture.workspace(&mini, "mac-mini");
        let sync = peer.sync(&mini.join("infra/handoffs")).unwrap();
        assert_eq!(sync.pulled.len(), 2);
        assert!(sync.changed);
        assert_eq!(
            fixture.read(&mini, "infra/evidence/dump.json").as_deref(),
            Some("{\"ok\":true}\n")
        );
        assert!(
            peer.status(false)
                .unwrap()
                .files
                .iter()
                .all(|row| row.state == "in_sync")
        );
    }

    /// A publish that changes nothing is not a second upload.
    #[test]
    fn republishing_identical_bytes_uploads_no_content() {
        let fixture = Fixture::new();
        let dev = fixture.root("dev");
        fixture.write(&dev, "work-ledger.yaml", "one\n");
        let mut publisher = fixture.workspace(&dev, "host-a");
        assert_eq!(publisher.publish().unwrap().pushed.len(), 1);
        let again = publisher.publish().unwrap();
        assert_eq!(again.pushed.len(), 0);
        assert_eq!(again.unchanged, vec!["work-ledger.yaml".to_string()]);
    }

    /// Both hosts change one path: reported, never merged, and the local bytes
    /// survive the report.
    #[test]
    fn a_file_changed_on_both_sides_is_a_conflict() {
        let fixture = Fixture::new();
        let dev = fixture.root("dev");
        let mini = fixture.root("mini");
        fixture.write(&dev, "work-ledger.yaml", "one\n");
        let mut publisher = fixture.workspace(&dev, "host-a");
        publisher.publish().unwrap();
        let mut peer = fixture.workspace(&mini, "host-b");
        peer.pull().unwrap();

        fixture.write(&dev, "work-ledger.yaml", "dev version\n");
        publisher.publish().unwrap();
        fixture.write(&mini, "work-ledger.yaml", "mini version\n");
        let report = peer.pull().unwrap();
        assert_eq!(report.conflicts.len(), 1);
        assert_eq!(report.conflicts[0].path, "work-ledger.yaml");
        assert_eq!(
            fixture.read(&mini, "work-ledger.yaml").as_deref(),
            Some("mini version\n"),
            "a conflict must not overwrite the local file"
        );
    }

    /// Two hosts publishing different paths at the same moment both land.
    #[test]
    fn concurrent_publishes_of_different_files_both_commit() {
        let fixture = Fixture::new();
        let dev = fixture.root("dev");
        let mini = fixture.root("mini");
        fixture.write(&dev, "work-ledger.yaml", "one\n");
        fixture.write(&mini, "work-ledger.yaml", "one\n");
        let mut first = fixture.workspace(&dev, "host-a");
        first.publish().unwrap();
        let mut second = fixture.workspace(&mini, "host-b");
        second.pull().unwrap();

        fixture.write(&dev, "infra/a.yaml", "a\n");
        fixture.write(&mini, "infra/b.yaml", "b\n");
        let left = first.publish().unwrap();
        let right = second.publish().unwrap();
        assert_eq!(left.conflicts, Vec::new());
        assert_eq!(right.conflicts, Vec::new());

        // Both ends converge on both files.
        first.pull().unwrap();
        second.pull().unwrap();
        assert_eq!(
            states(&first.status(false).unwrap())["infra/b.yaml"],
            "in_sync"
        );
        assert_eq!(
            states(&second.status(false).unwrap())["infra/a.yaml"],
            "in_sync"
        );
    }

    /// The manifest is the index; the pointer mirror is derived and can be
    /// rebuilt from it.
    #[test]
    fn the_manifest_and_the_pointer_mirror_agree() {
        let fixture = Fixture::new();
        let dev = fixture.root("dev");
        fixture.write(&dev, "work-ledger.yaml", "one\n");
        fixture.write(&dev, "infra/x.yaml", "x\n");
        let mut publisher = fixture.workspace(&dev, "host-a");
        publisher.publish().unwrap();

        let report = publisher.verify_index().unwrap();
        assert_eq!(report.manifest_files, 2);
        assert_eq!(report.pointer_files, 2);
        assert_eq!(report.drift_count, 0);

        // The pre-manifest layout rebuilds the same index.
        let from_pointers = publisher.status(true).unwrap();
        assert_eq!(from_pointers.index_source, "pointers");
        assert!(from_pointers.files.iter().all(|row| row.state == "in_sync"));
    }

    #[cfg(unix)]
    #[test]
    fn publish_refuses_a_tracked_file_symlink() {
        let fixture = Fixture::new();
        let dev = fixture.root("dev");
        let outside = fixture.dir.join("outside-secret.txt");
        std::fs::write(&outside, b"top-secret-credential").unwrap();
        fixture.write(&dev, "work-ledger.yaml", "one\n");
        let link = dev.join("linked.txt");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let mut config = fixture.config(&dev, "host-a");
        config.track = vec!["work-ledger.yaml".into(), "linked.txt".into()];
        let mut publisher = Workspace::open(
            &config,
            Box::new(LocalStore::new(fixture.mirror.clone()).unwrap()),
        )
        .unwrap();
        let error = publisher.publish().unwrap_err().to_string();
        assert!(error.contains("symbolic link"), "{error}");
        // Nothing under the content prefix should hold the secret.
        let leaked = std::fs::read_dir(fixture.mirror.join("projects/poc-infra/files"))
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok())
            .any(|entry| {
                let path = entry.path();
                path.is_file()
                    && std::fs::read(&path)
                        .ok()
                        .is_some_and(|body| body == b"top-secret-credential")
            });
        assert!(!leaked, "symlink target bytes must not enter the store");
    }

    #[cfg(unix)]
    #[test]
    fn publish_refuses_a_directory_symlink_in_the_tracked_tree() {
        let fixture = Fixture::new();
        let dev = fixture.root("dev");
        let outside = fixture.dir.join("outside-dir");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"dir-secret").unwrap();
        fixture.write(&dev, "work-ledger.yaml", "one\n");
        std::os::unix::fs::symlink(&outside, dev.join("infra")).unwrap();
        let mut publisher = fixture.workspace(&dev, "host-a");
        let error = publisher.publish().unwrap_err().to_string();
        assert!(error.contains("symbolic link"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn pull_refuses_to_write_through_a_parent_symlink() {
        let fixture = Fixture::new();
        let dev = fixture.root("dev");
        let mini = fixture.root("mini");
        fixture.write(&dev, "work-ledger.yaml", "one\n");
        fixture.write(&dev, "infra/a.yaml", "a\n");
        fixture.workspace(&dev, "host-a").publish().unwrap();

        // Seed the peer with only the ledger so pull still needs infra/a.yaml.
        fixture.write(&mini, "work-ledger.yaml", "one\n");
        let mut peer = fixture.workspace(&mini, "host-b");
        peer.publish().unwrap();

        let outside = fixture.dir.join("outside-mini");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, mini.join("infra")).unwrap();
        let error = peer.pull().unwrap_err().to_string();
        assert!(error.contains("symbolic link"), "{error}");
        assert!(
            !outside.join("a.yaml").exists(),
            "pull must not materialize bytes outside the project root"
        );
    }

    #[test]
    fn publish_fails_when_an_existing_content_object_has_wrong_bytes() {
        let fixture = Fixture::new();
        let dev = fixture.root("dev");
        fixture.write(&dev, "work-ledger.yaml", "honest\n");
        let digest = crate::digest(b"honest\n");
        let key = layout::content("poc-infra", "work-ledger.yaml", &digest);
        // Plant a wrong body under the content-addressed key.
        let store = LocalStore::new(fixture.mirror.clone()).unwrap();
        store
            .put(&key, b"forged-bytes", Precondition::None)
            .unwrap();
        let mut publisher =
            Workspace::open(&fixture.config(&dev, "host-a"), Box::new(store)).unwrap();
        let error = publisher.publish().unwrap_err().to_string();
        assert!(
            error.contains("already holds") || error.contains("not"),
            "{error}"
        );
        // Manifest must not claim the forged object.
        assert!(
            !fixture
                .mirror
                .join("projects/poc-infra/manifest.json")
                .exists()
        );
    }

    #[test]
    fn handoff_push_rejects_a_name_with_a_slash() {
        let fixture = Fixture::new();
        let dev = fixture.root("dev");
        fixture.write(&dev, "work-ledger.yaml", "one\n");
        let body = dev.join("note.json");
        std::fs::write(&body, br#"{"summary":"x"}"#).unwrap();
        let mut author = fixture.workspace(&dev, "host-a");
        let error = author
            .handoff_push(&body, "foo/bar")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("path component") || error.contains("foo/bar"),
            "{error}"
        );
    }

    #[test]
    fn pointer_index_fails_closed_on_malformed_json() {
        let fixture = Fixture::new();
        let dev = fixture.root("dev");
        fixture.write(&dev, "work-ledger.yaml", "one\n");
        let store = LocalStore::new(fixture.mirror.clone()).unwrap();
        store
            .put(
                "projects/poc-infra/files/work-ledger.yaml/current.json",
                b"{not-json",
                Precondition::None,
            )
            .unwrap();
        let workspace = Workspace::open(&fixture.config(&dev, "host-a"), Box::new(store)).unwrap();
        let error = workspace.status(true).unwrap_err().to_string();
        assert!(
            error.contains("not valid JSON") || error.contains("pointer"),
            "{error}"
        );
    }

    #[test]
    fn cancel_flag_stops_pull_before_local_writes() {
        let fixture = Fixture::new();
        let dev = fixture.root("dev");
        let mini = fixture.root("mini");
        fixture.write(&dev, "work-ledger.yaml", "from-dev\n");
        fixture.workspace(&dev, "host-a").publish().unwrap();
        let cancel = Arc::new(AtomicBool::new(true));
        let mut peer = Workspace::open(
            &fixture.config(&mini, "host-b"),
            Box::new(LocalStore::new(fixture.mirror.clone()).unwrap()),
        )
        .unwrap()
        .with_cancel(cancel);
        let error = peer.pull().unwrap_err().to_string();
        assert!(error.contains("cancelled"), "{error}");
        assert!(fixture.read(&mini, "work-ledger.yaml").is_none());
    }

    /// A store without compare-and-swap still commits atomically: the manifest
    /// is one object, so a peer sees all of a publish or none of it.
    #[test]
    fn a_guarded_commit_still_lands_exactly_once() {
        let fixture = Fixture::new();
        let dev = fixture.root("dev");
        fixture.write(&dev, "work-ledger.yaml", "one\n");
        let config = fixture.config(&dev, "host-a");
        let (store, index_reads) =
            GuardedStore::counting(LocalStore::new(fixture.mirror.clone()).unwrap());
        let mut publisher = Workspace::open(&config, Box::new(store)).unwrap();
        let report = publisher.publish().unwrap();
        assert_eq!(report.commit_mode, "guarded");
        assert_eq!(report.pushed.len(), 1);
        assert_eq!(publisher.verify_index().unwrap().drift_count, 0);
        // Creating the manifest is unguarded: there is no index to be stale
        // against, so the first publish reads nothing around its write.
        assert_eq!(index_reads.load(Ordering::Relaxed), 0);

        // Updating it reads the index twice: once before the write, which is
        // the check that narrows the window, and once after it, which confirms
        // the write landed.
        fixture.write(&dev, "work-ledger.yaml", "two\n");
        assert_eq!(publisher.publish().unwrap().pushed.len(), 1);
        assert_eq!(index_reads.load(Ordering::Relaxed), 2);
    }

    /// A peer that commits between this host's index read and its write is
    /// merged, not dropped: the commit checks the index immediately before
    /// writing, so the stale body never becomes the current one.
    #[test]
    fn a_guarded_commit_merges_a_peer_that_lands_inside_the_window() {
        let fixture = Fixture::new();
        let dev = fixture.root("dev");
        let mini = fixture.root("mini");
        fixture.write(&mini, "work-ledger.yaml", "one\n");
        fixture.write(&mini, "infra/b.yaml", "b\n");
        let mut peer = fixture.workspace(&mini, "host-b");
        peer.publish().unwrap();

        let manifest = fixture.mirror.join("projects/poc-infra/manifest.json");
        let seen_by_dev = std::fs::read(&manifest).unwrap();
        fixture.write(&mini, "infra/c.yaml", "c\n");
        peer.publish().unwrap();
        let written_by_peer = std::fs::read(&manifest).unwrap();
        // Rewind the store to the index this host is about to read, and replay
        // the peer's commit the moment the commit checks the index.
        std::fs::write(&manifest, &seen_by_dev).unwrap();

        let store = GuardedStore::new(LocalStore::new(fixture.mirror.clone()).unwrap());
        store.peer_commits_first("projects/poc-infra/manifest.json", &written_by_peer);
        fixture.write(&dev, "infra/a.yaml", "a\n");
        let mut publisher =
            Workspace::open(&fixture.config(&dev, "host-a"), Box::new(store)).unwrap();
        let report = publisher.publish().unwrap();

        assert_eq!(report.commit_mode, "guarded");
        assert_eq!(report.conflicts, Vec::new());
        let committed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
        let mut paths: Vec<&str> = committed["files"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        paths.sort();
        assert_eq!(
            paths,
            [
                "infra/a.yaml",
                "infra/b.yaml",
                "infra/c.yaml",
                "work-ledger.yaml"
            ]
        );
    }

    /// A commit that loses every attempt is this host's work still owed, not a
    /// tangle with the peer: nothing was overwritten, so the remedy is to
    /// publish again. Reporting it as a conflict would send the operator to
    /// mediate a change nobody else made.
    #[test]
    fn a_commit_that_loses_every_attempt_is_reported_as_still_to_publish() {
        let fixture = Fixture::new();
        let dev = fixture.root("dev");
        fixture.write(&dev, "work-ledger.yaml", "one\n");
        fixture.workspace(&dev, "host-a").publish().unwrap();

        let manifest = fixture.mirror.join("projects/poc-infra/manifest.json");
        fixture.write(&dev, "infra/a.yaml", "a\n");

        // Every attempt to commit the index finds a peer's commit already
        // there, so the guarded check rejects all three of them.
        let store = GuardedStore::new(LocalStore::new(fixture.mirror.clone()).unwrap());
        for revision in 2..=4 {
            let mut body: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
            body["revision"] = serde_json::json!(revision);
            store.peer_commits_first(
                "projects/poc-infra/manifest.json",
                &serde_json::to_vec_pretty(&body).unwrap(),
            );
        }
        let mut publisher =
            Workspace::open(&fixture.config(&dev, "host-a"), Box::new(store)).unwrap();
        let report = publisher.publish().unwrap();

        assert_eq!(report.contended, vec!["infra/a.yaml".to_string()]);
        assert_eq!(report.conflicts, Vec::new());
        assert!(report.pushed.is_empty());
        let status = publisher.status(false).unwrap();
        assert_eq!(states(&status)["infra/a.yaml"], "local_ahead");
        assert!(status.conflicted.is_empty());

        // And publishing again is the whole remedy: the file is still this
        // host's to publish, and the next attempt lands it.
        let mut retry = fixture.workspace(&dev, "host-a");
        let healed = retry.publish().unwrap();
        assert_eq!(healed.contended, Vec::<String>::new());
        assert_eq!(
            healed
                .pushed
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            ["infra/a.yaml"]
        );
        assert_eq!(
            states(&retry.status(false).unwrap())["infra/a.yaml"],
            "in_sync"
        );
    }

    /// A dropped index entry that then loses every commit is still unpublished,
    /// so the report must not claim it was re-registered.
    #[test]
    fn a_dropped_entry_that_loses_every_commit_is_not_reported_as_republished() {
        let fixture = Fixture::new();
        let dev = fixture.root("dev");
        fixture.write(&dev, "work-ledger.yaml", "one\n");
        fixture.write(&dev, "infra/a.yaml", "a\n");
        fixture.workspace(&dev, "host-a").publish().unwrap();

        let manifest = fixture.mirror.join("projects/poc-infra/manifest.json");
        let mut body: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
        body["files"]
            .as_object_mut()
            .unwrap()
            .remove("infra/a.yaml");
        std::fs::write(&manifest, serde_json::to_vec(&body).unwrap()).unwrap();

        let store = GuardedStore::new(LocalStore::new(fixture.mirror.clone()).unwrap());
        for revision in 2..=4 {
            let mut next: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
            next["revision"] = serde_json::json!(revision);
            store.peer_commits_first(
                "projects/poc-infra/manifest.json",
                &serde_json::to_vec_pretty(&next).unwrap(),
            );
        }
        let mut publisher =
            Workspace::open(&fixture.config(&dev, "host-a"), Box::new(store)).unwrap();
        let report = publisher.publish().unwrap();
        assert_eq!(report.contended, vec!["infra/a.yaml".to_string()]);
        assert!(report.pushed.is_empty());
        assert!(
            report.republished_after_index_drop.is_empty(),
            "a commit that did not land is not a re-publish: {:?}",
            report.republished_after_index_drop
        );
    }

    /// A store that drops an index entry under a concurrent commit must be
    /// reported as "publish again", not as "the peer is ahead".
    #[test]
    fn a_dropped_index_entry_is_not_mistaken_for_a_peer_being_ahead() {
        let fixture = Fixture::new();
        let dev = fixture.root("dev");
        fixture.write(&dev, "work-ledger.yaml", "one\n");
        let mut publisher = fixture.workspace(&dev, "host-a");
        publisher.publish().unwrap();

        let manifest = fixture.mirror.join("projects/poc-infra/manifest.json");
        std::fs::write(
            &manifest,
            br#"{"schema":1,"project_key":"poc-infra","files":{}}"#,
        )
        .unwrap();

        let report = publisher.status(false).unwrap();
        assert_eq!(states(&report)["work-ledger.yaml"], "index_missing_local");
        let repaired = publisher.publish().unwrap();
        assert_eq!(
            repaired.republished_after_index_drop,
            vec!["work-ledger.yaml".to_string()]
        );
        assert!(publisher.status(false).unwrap().conflicted.is_empty());
    }

    /// A repair re-registers exactly what the index lost, and nothing else: the
    /// half-finished work of a starting session must not become a publish.
    #[test]
    fn a_repair_sends_the_dropped_entry_and_no_other_work() {
        let fixture = Fixture::new();
        let dev = fixture.root("dev");
        fixture.write(&dev, "work-ledger.yaml", "one\n");
        fixture.write(&dev, "infra/a.yaml", "a\n");
        let mut publisher = fixture.workspace(&dev, "host-a");
        publisher.publish().unwrap();

        // A peer's commit built from a stale read dropped one entry.
        let manifest = fixture.mirror.join("projects/poc-infra/manifest.json");
        let mut body: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
        body["files"]
            .as_object_mut()
            .unwrap()
            .remove("infra/a.yaml");
        std::fs::write(&manifest, serde_json::to_vec(&body).unwrap()).unwrap();

        // Work in progress that is this host's, but not this repair's.
        fixture.write(&dev, "infra/wip.yaml", "half written\n");
        let report = publisher.repair_index().unwrap();
        assert_eq!(
            report.republished_after_index_drop,
            vec!["infra/a.yaml".to_string()]
        );
        assert_eq!(
            report
                .pushed
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>(),
            ["infra/a.yaml"]
        );
        assert!(!report.pushed[0].content_uploaded);

        let committed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
        let paths: Vec<&String> = committed["files"].as_object().unwrap().keys().collect();
        assert!(!paths.iter().any(|path| *path == "infra/wip.yaml"));
        let states = states(&publisher.status(false).unwrap());
        assert_eq!(states["infra/a.yaml"], "in_sync");
        assert_eq!(states["infra/wip.yaml"], "local_ahead");
    }

    /// A repair never creates the index: an un-published project that is only
    /// repaired stays un-published, and the next session finds nothing to fix.
    #[test]
    fn a_repair_does_not_create_the_index() {
        let fixture = Fixture::new();
        let dev = fixture.root("dev");
        fixture.write(&dev, "work-ledger.yaml", "one\n");
        let mut workspace = fixture.workspace(&dev, "host-a");
        let report = workspace.repair_index().unwrap();
        assert!(report.pushed.is_empty());
        assert_eq!(report.commit_mode, "cas");
        assert!(
            !fixture
                .mirror
                .join("projects/poc-infra/manifest.json")
                .exists()
        );
        assert_eq!(
            states(&workspace.status(false).unwrap())["work-ledger.yaml"],
            "local_ahead"
        );
    }

    /// The first status has no base to compare with, and a file that only this
    /// host has is "publish this", not "start by resolving a conflict".
    #[test]
    fn the_first_status_offers_to_publish_rather_than_to_mediate() {
        let fixture = Fixture::new();
        let dev = fixture.root("dev");
        fixture.write(&dev, "work-ledger.yaml", "one\n");
        let mut workspace = fixture.workspace(&dev, "host-a");
        let report = workspace.status(false).unwrap();
        assert_eq!(states(&report)["work-ledger.yaml"], "local_ahead");
        assert!(report.conflicted.is_empty());
        // A path that exists only in the workspace is not this host's to fix.
        fixture.write(&dev, "infra/published.yaml", "remote\n");
        workspace.publish().unwrap();
        std::fs::remove_file(dev.join("infra/published.yaml")).unwrap();
        let report = workspace.status(false).unwrap();
        assert_eq!(states(&report)["infra/published.yaml"], "missing_local");
    }

    /// `track` decides what is tracked. A file inside a tracked directory is
    /// tracked whatever it is called, including a dotfile, and the reference
    /// client agrees - so the two never disagree about the set.
    #[test]
    fn everything_inside_a_tracked_directory_is_tracked() {
        let fixture = Fixture::new();
        let dev = fixture.root("dev");
        fixture.write(&dev, "infra/evidence/.keep", "");
        fixture.write(&dev, "infra/evidence/nested/.hidden", "x\n");
        fixture.write(&dev, "infra/evidence/.DS_Store", "noise");
        fixture.write(&dev, "work-ledger.yaml", "one\n");
        let mut workspace = fixture.workspace(&dev, "host-a");
        workspace.publish().unwrap();
        let paths: Vec<String> = workspace
            .status(false)
            .unwrap()
            .files
            .into_iter()
            .map(|row| row.path)
            .collect();
        assert!(
            paths.contains(&"infra/evidence/.keep".to_string()),
            "{paths:?}"
        );
        assert!(
            paths.contains(&"infra/evidence/nested/.hidden".to_string()),
            "{paths:?}"
        );
        assert!(
            !paths.iter().any(|path| path.ends_with(".DS_Store")),
            "{paths:?}"
        );
    }

    /// Neither a Git history nor AWR's own runtime is ever published, even when
    /// a tracked directory happens to contain one.
    #[test]
    fn a_private_directory_inside_a_tracked_tree_is_refused() {
        let fixture = Fixture::new();
        let dev = fixture.root("dev");
        fixture.write(&dev, "infra/evidence/dump.json", "{}\n");
        std::fs::create_dir_all(dev.join("infra/evidence/.git/objects")).unwrap();
        std::fs::write(
            dev.join("infra/evidence/.git/HEAD"),
            "ref: refs/heads/main\n",
        )
        .unwrap();
        let mut workspace = fixture.workspace(&dev, "host-a");
        let error = workspace.publish().unwrap_err();
        assert!(format!("{error}").contains(".git"), "{error}");
        assert!(
            workspace.status(false).is_err(),
            "the refusal covers reads too, not only the publish that would leak it"
        );
    }

    /// A path under `.git` or `.awr` in the index is remote data, not a local
    /// fact: pull must refuse it and leave the filesystem untouched.
    #[test]
    fn a_private_path_in_the_index_is_not_written_locally() {
        let fixture = Fixture::new();
        let mini = fixture.root("mini");
        fixture.write(&mini, "work-ledger.yaml", "one\n");

        let manifest = fixture.mirror.join("projects/poc-infra/manifest.json");
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        std::fs::write(
            &manifest,
            br#"{"schema":1,"project_key":"poc-infra","files":{".git/hooks/post-checkout":{"sha256":"abc","size":1},".awr/workspace-credentials.json":{"sha256":"def","size":1}}}"#,
        )
        .unwrap();

        let hook = mini.join(".git/hooks/post-checkout");
        let creds = mini.join(".awr/workspace-credentials.json");
        assert!(!hook.exists());
        assert!(!creds.exists());

        let mut peer = fixture.workspace(&mini, "host-b");
        let error = format!("{}", peer.pull().unwrap_err());
        assert!(error.contains("never leaves a machine"), "{error}");
        assert!(
            error.contains(".git/hooks/post-checkout") || error.contains(".awr/"),
            "{error}"
        );
        assert!(!hook.exists(), "a path under .git must never be written");
        assert!(!creds.exists(), "a path under .awr must never be written");
        // A later status must not start acting on the planted path either.
        let status_error = format!("{}", peer.status(false).unwrap_err());
        assert!(
            status_error.contains("never leaves a machine"),
            "{status_error}"
        );
    }

    /// The project root is not a tracked path: tracking `.` would walk `.awr/`
    /// and the runtime database, which never leaves the machine.
    #[test]
    fn the_project_root_itself_is_not_a_tracked_path() {
        let fixture = Fixture::new();
        let dev = fixture.root("dev");
        let mut config = fixture.config(&dev, "host-a");
        config.track = vec![".".to_string()];
        let mut workspace = Workspace::open(
            &config,
            Box::new(LocalStore::new(fixture.mirror.clone()).unwrap()),
        )
        .unwrap();
        let error = workspace.publish().unwrap_err();
        assert!(format!("{error}").contains("project-relative"), "{error}");
    }

    /// A handoff is write-once and idempotent, and the origin host does not
    /// fetch its own.
    #[test]
    fn handoffs_are_write_once_and_never_come_back_to_their_author() {
        let fixture = Fixture::new();
        let dev = fixture.root("dev");
        let mini = fixture.root("mini");
        let body = dev.join("request.json");
        std::fs::write(&body, br#"{"summary":"install postgres","to":"mac-mini"}"#).unwrap();

        let mut author = fixture.workspace(&dev, "macbook-codex");
        let first = author.handoff_push(&body, "postgres-up").unwrap();
        assert!(!first.idempotent_replay);
        let replay = author.handoff_push(&body, "postgres-up").unwrap();
        assert!(replay.idempotent_replay);
        assert!(author.handoff_pull(&dev.join("inbox")).unwrap().is_empty());

        std::fs::write(&body, br#"{"summary":"something else"}"#).unwrap();
        assert!(author.handoff_push(&body, "postgres-up").is_err());

        let mut peer = fixture.workspace(&mini, "mac-mini");
        let pulled = peer.handoff_pull(&mini.join("inbox")).unwrap();
        assert_eq!(pulled.len(), 1);
        assert_eq!(pulled[0].summary.as_deref(), Some("install postgres"));
        assert!(mixture_ends_with(
            &pulled[0].path,
            "macbook-codex/postgres-up.json"
        ));
        // Unchanged bytes are not fetched twice.
        assert!(peer.handoff_pull(&mini.join("inbox")).unwrap().is_empty());
    }

    fn mixture_ends_with(path: &str, suffix: &str) -> bool {
        path.replace(std::path::MAIN_SEPARATOR, "/")
            .ends_with(suffix)
    }

    /// The index is remote data, so a path in it may not reach outside the root.
    #[test]
    fn a_remote_index_cannot_write_outside_the_project_root() {
        let fixture = Fixture::new();
        let dev = fixture.root("dev");
        let mut publisher = fixture.workspace(&dev, "host-a");
        let harness = fixture.mirror.join("projects/poc-infra/manifest.json");
        std::fs::create_dir_all(harness.parent().unwrap()).unwrap();
        std::fs::write(
            &harness,
            br#"{"schema":1,"project_key":"poc-infra","files":{"../../escape.yaml":{"sha256":"00"}}}"#,
        )
        .unwrap();
        let error = publisher.pull().unwrap_err();
        assert!(format!("{error}").contains("project-relative"), "{error}");
        assert!(!fixture.dir.join("escape.yaml").exists());
    }

    /// A manifest that describes another project, or another schema, is refused
    /// rather than half-read.
    #[test]
    fn a_manifest_that_is_not_this_project_is_refused() {
        let fixture = Fixture::new();
        let dev = fixture.root("dev");
        let workspace = fixture.workspace(&dev, "host-a");
        let harness = fixture.mirror.join("projects/poc-infra/manifest.json");
        std::fs::create_dir_all(harness.parent().unwrap()).unwrap();
        for body in [
            r#"{"schema":2,"project_key":"poc-infra","files":{}}"#,
            r#"{"schema":1,"project_key":"another","files":{}}"#,
            r#"not json"#,
        ] {
            std::fs::write(&harness, body).unwrap();
            let error = workspace.status(false).unwrap_err();
            assert!(
                format!("{error}").contains("manifest"),
                "{body} produced {error}"
            );
        }
    }

    /// A dry-run uses the same plan as publish, HEADs content keys for an
    /// honest `content_uploaded`, and writes nothing - not even a missing
    /// manifest.
    #[test]
    fn a_publish_preview_writes_nothing_and_lists_what_would_be_sent() {
        let fixture = Fixture::new();
        let dev = fixture.root("dev");
        fixture.write(&dev, "work-ledger.yaml", "one\n");
        fixture.write(&dev, "infra/evidence/dump.json", "{\"ok\":true}\n");
        let workspace = fixture.workspace(&dev, "host-a");
        let preview = workspace.publish_preview().unwrap();
        assert!(preview.dry_run);
        let mut paths: Vec<&str> = preview
            .pushed
            .iter()
            .map(|file| file.path.as_str())
            .collect();
        paths.sort();
        assert_eq!(paths, ["infra/evidence/dump.json", "work-ledger.yaml"]);
        assert!(preview.pushed.iter().all(|file| file.content_uploaded));
        assert!(preview.conflicts.is_empty());
        assert!(
            !fixture
                .mirror
                .join("projects/poc-infra/manifest.json")
                .exists()
        );
        assert_eq!(workspace.status(false).unwrap().index_source, "pointers");
        assert_eq!(
            states(&workspace.status(false).unwrap())["work-ledger.yaml"],
            "local_ahead"
        );
        assert!(
            serde_json::to_value(&preview).unwrap()["dry_run"]
                .as_bool()
                .unwrap()
        );
    }

    /// A preview of a conflict is still a preview: the report names it, the
    /// command did not fail, and neither side was touched.
    #[test]
    fn a_publish_preview_reports_conflicts_without_writing() {
        let fixture = Fixture::new();
        let dev = fixture.root("dev");
        let mini = fixture.root("mini");
        fixture.write(&dev, "work-ledger.yaml", "one\n");
        fixture.workspace(&dev, "host-a").publish().unwrap();
        let mut peer = fixture.workspace(&mini, "host-b");
        peer.pull().unwrap();
        fixture.write(&dev, "work-ledger.yaml", "dev version\n");
        fixture.workspace(&dev, "host-a").publish().unwrap();
        fixture.write(&mini, "work-ledger.yaml", "mini version\n");
        let preview = peer.publish_preview().unwrap();
        assert!(preview.dry_run);
        assert_eq!(preview.conflicts.len(), 1);
        assert_eq!(preview.conflicts[0].path, "work-ledger.yaml");
        assert_eq!(
            fixture.read(&mini, "work-ledger.yaml").as_deref(),
            Some("mini version\n")
        );
        assert_eq!(
            fixture.read(&dev, "work-ledger.yaml").as_deref(),
            Some("dev version\n")
        );
    }

    /// Bytes already in the store are not claimed as an upload on a preview.
    #[test]
    fn a_publish_preview_heads_content_keys() {
        let fixture = Fixture::new();
        let dev = fixture.root("dev");
        fixture.write(&dev, "work-ledger.yaml", "one\n");
        fixture.write(&dev, "infra/a.yaml", "a\n");
        let mut publisher = fixture.workspace(&dev, "host-a");
        publisher.publish().unwrap();
        let manifest = fixture.mirror.join("projects/poc-infra/manifest.json");
        let mut body: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
        body["files"]
            .as_object_mut()
            .unwrap()
            .remove("infra/a.yaml");
        std::fs::write(&manifest, serde_json::to_vec(&body).unwrap()).unwrap();
        let preview = publisher.publish_preview().unwrap();
        let repaired = preview
            .pushed
            .iter()
            .find(|file| file.path == "infra/a.yaml")
            .expect("the dropped entry is in the plan");
        assert!(!repaired.content_uploaded);
        assert!(
            preview
                .republished_after_index_drop
                .contains(&"infra/a.yaml".to_string())
        );
    }

    /// Drop refuses a path still covered by track, without walking the tree
    /// and without touching the index. The file does not have to exist.
    #[test]
    fn drop_refuses_a_path_still_in_track_and_does_not_need_the_file() {
        let fixture = Fixture::new();
        let dev = fixture.root("dev");
        fixture.write(&dev, "work-ledger.yaml", "one\n");
        let mut publisher = fixture.workspace(&dev, "host-a");
        publisher.publish().unwrap();
        std::fs::remove_file(dev.join("work-ledger.yaml")).unwrap();
        let error = publisher
            .drop_paths(&["work-ledger.yaml".to_string()])
            .unwrap_err();
        assert!(
            matches!(error, Error::InvalidInput(_)),
            "unexpected error: {error:?}"
        );
        assert!(format!("{error}").contains("project.track"), "{error}");
        let index: serde_json::Value = serde_json::from_slice(
            &std::fs::read(fixture.mirror.join("projects/poc-infra/manifest.json")).unwrap(),
        )
        .unwrap();
        assert!(index["files"].get("work-ledger.yaml").is_some(), "{index}");
    }

    /// After the path leaves track, drop removes it from the index and the
    /// pointer mirror. The local file stays, content objects stay, and a peer
    /// sync does not restore it.
    #[test]
    fn drop_untracks_a_path_the_peer_then_cannot_restore() {
        let fixture = Fixture::new();
        let dev = fixture.root("dev");
        let mini = fixture.root("mini");
        fixture.write(&dev, "work-ledger.yaml", "one\n");
        fixture.write(&dev, "infra/evidence/dump.json", "{\"ok\":true}\n");
        fixture.workspace(&dev, "host-a").publish().unwrap();
        let mut peer = fixture.workspace(&mini, "host-b");
        peer.sync(&mini.join("infra/handoffs")).unwrap();
        assert_eq!(
            fixture.read(&mini, "infra/evidence/dump.json").as_deref(),
            Some("{\"ok\":true}\n")
        );

        let mut publisher =
            fixture.workspace_with_track(&dev, "host-a", vec!["work-ledger.yaml".to_string()]);
        let report = publisher
            .drop_paths(&["infra/evidence/dump.json".to_string()])
            .unwrap();
        assert_eq!(report.dropped, vec!["infra/evidence/dump.json".to_string()]);
        assert!(report.contended.is_empty());
        assert_eq!(
            fixture.read(&dev, "infra/evidence/dump.json").as_deref(),
            Some("{\"ok\":true}\n")
        );

        let verified = publisher.verify_index().unwrap();
        assert_eq!(verified.drift_count, 0);
        assert_eq!(verified.manifest_files, 1);
        assert_eq!(verified.pointer_files, 1);

        std::fs::remove_file(mini.join("infra/evidence/dump.json")).unwrap();
        let mut peer = fixture.workspace(&mini, "host-b");
        peer.sync(&mini.join("infra/handoffs")).unwrap();
        assert!(
            fixture.read(&mini, "infra/evidence/dump.json").is_none(),
            "a dropped path must not come back from the workspace"
        );
        assert_eq!(
            fixture.read(&dev, "infra/evidence/dump.json").as_deref(),
            Some("{\"ok\":true}\n")
        );
        // Content objects are left behind; only the pointer and the index entry
        // are removed.
        let sha = crate::digest(b"{\"ok\":true}\n");
        assert!(
            fixture
                .mirror
                .join(layout::content(
                    "poc-infra",
                    "infra/evidence/dump.json",
                    &sha
                ))
                .exists()
        );
        assert!(
            !fixture
                .mirror
                .join(layout::pointer("poc-infra", "infra/evidence/dump.json"))
                .exists()
        );
    }

    /// A drop that loses every commit is the same kind of unfinished work as a
    /// publish that does: the index is unchanged, the local file is unchanged,
    /// and the report is contended rather than a conflict.
    #[test]
    fn a_drop_that_loses_every_commit_is_contended() {
        let fixture = Fixture::new();
        let dev = fixture.root("dev");
        fixture.write(&dev, "work-ledger.yaml", "one\n");
        fixture.write(&dev, "infra/a.yaml", "a\n");
        fixture.workspace(&dev, "host-a").publish().unwrap();

        let manifest = fixture.mirror.join("projects/poc-infra/manifest.json");
        let store = GuardedStore::new(LocalStore::new(fixture.mirror.clone()).unwrap());
        for revision in 2..=4 {
            let mut body: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
            body["revision"] = serde_json::json!(revision);
            store.peer_commits_first(
                "projects/poc-infra/manifest.json",
                &serde_json::to_vec_pretty(&body).unwrap(),
            );
        }
        let mut workspace = Workspace::open(
            &{
                let mut config = fixture.config(&dev, "host-a");
                config.track = vec!["work-ledger.yaml".to_string()];
                config
            },
            Box::new(store),
        )
        .unwrap();
        let report = workspace.drop_paths(&["infra/a.yaml".to_string()]).unwrap();
        assert_eq!(report.contended, vec!["infra/a.yaml".to_string()]);
        assert!(report.dropped.is_empty());
        let index: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
        assert!(index["files"].get("infra/a.yaml").is_some(), "{index}");
        assert_eq!(fixture.read(&dev, "infra/a.yaml").as_deref(), Some("a\n"));
    }

    /// A store that answers `If-Match` with success but does not enforce it is
    /// the OSS case: the commit is confirmed by reading its own write back.
    /// Tests can inject a peer commit either before the pre-write HEAD (detectable)
    /// or between HEAD and PUT (the true TOCTOU window guarded mode cannot close).
    struct GuardedStore {
        inner: LocalStore,
        peer_before_head: std::sync::Mutex<std::collections::VecDeque<(String, Vec<u8>)>>,
        peer_before_put: std::sync::Mutex<std::collections::VecDeque<(String, Vec<u8>)>>,
        /// Reads of the index, so a test can see the check around the write.
        index_reads: std::sync::Arc<AtomicUsize>,
    }

    impl GuardedStore {
        fn new(inner: LocalStore) -> Self {
            Self::counting(inner).0
        }

        fn counting(inner: LocalStore) -> (Self, std::sync::Arc<AtomicUsize>) {
            let index_reads = std::sync::Arc::new(AtomicUsize::new(0));
            (
                Self {
                    inner,
                    peer_before_head: std::sync::Mutex::new(std::collections::VecDeque::new()),
                    peer_before_put: std::sync::Mutex::new(std::collections::VecDeque::new()),
                    index_reads: index_reads.clone(),
                },
                index_reads,
            )
        }

        /// Peer lands before the pre-write HEAD: guarded mode detects this.
        fn peer_commits_first(&self, key: &str, body: &[u8]) {
            self.peer_before_head
                .lock()
                .unwrap()
                .push_back((key.to_string(), body.to_vec()));
        }

        /// Peer lands after HEAD and before PUT: guarded mode overwrites silently.
        fn peer_commits_between_head_and_put(&self, key: &str, body: &[u8]) {
            self.peer_before_put
                .lock()
                .unwrap()
                .push_back((key.to_string(), body.to_vec()));
        }
    }

    impl Backend for GuardedStore {
        fn describe(&self) -> String {
            "guarded".to_string()
        }

        fn supports_if_match(&self) -> bool {
            false
        }

        fn requests(&self) -> u64 {
            self.inner.requests()
        }

        fn head(&self, key: &str) -> Result<Option<crate::backend::ObjectMeta>> {
            self.index_reads.fetch_add(1, Ordering::Relaxed);
            {
                let mut queue = self.peer_before_head.lock().unwrap();
                if queue.front().is_some_and(|(peer_key, _)| peer_key == key) {
                    let (peer_key, peer_body) = queue.pop_front().unwrap();
                    drop(queue);
                    self.inner.put(&peer_key, &peer_body, Precondition::None)?;
                }
            }
            self.inner.head(key)
        }

        fn get_meta(&self, key: &str) -> Result<Option<(Vec<u8>, String)>> {
            self.inner.get_meta(key)
        }

        fn put(
            &self,
            key: &str,
            body: &[u8],
            precondition: Precondition,
        ) -> Result<crate::backend::PutOutcome> {
            {
                let mut queue = self.peer_before_put.lock().unwrap();
                if queue.front().is_some_and(|(peer_key, _)| peer_key == key) {
                    let (peer_key, peer_body) = queue.pop_front().unwrap();
                    drop(queue);
                    self.inner.put(&peer_key, &peer_body, Precondition::None)?;
                }
            }
            match precondition {
                // The store is told to compare and swap and quietly ignores it.
                Precondition::Match(_) => self.inner.put(key, body, Precondition::None),
                other => self.inner.put(key, body, other),
            }
        }

        fn list(&self, prefix: &str) -> Result<Vec<crate::backend::Listed>> {
            self.inner.list(prefix)
        }

        fn delete(&self, key: &str) -> Result<bool> {
            self.inner.delete(key)
        }
    }

    /// Documents the real guarded-mode window: a peer PUT between our HEAD and
    /// PUT is overwritten, and readback still reports success. Personal sequential
    /// use is fine; concurrent multi-writer needs a CAS store.
    #[test]
    fn a_guarded_commit_cannot_see_a_peer_between_head_and_put() {
        let fixture = Fixture::new();
        let dev = fixture.root("dev");
        let mini = fixture.root("mini");
        fixture.write(&mini, "work-ledger.yaml", "one\n");
        fixture.write(&mini, "infra/b.yaml", "b\n");
        let mut peer = fixture.workspace(&mini, "host-b");
        peer.publish().unwrap();

        let manifest = fixture.mirror.join("projects/poc-infra/manifest.json");
        let seen_by_dev = std::fs::read(&manifest).unwrap();
        fixture.write(&mini, "infra/c.yaml", "c\n");
        peer.publish().unwrap();
        let written_by_peer = std::fs::read(&manifest).unwrap();
        std::fs::write(&manifest, &seen_by_dev).unwrap();

        let store = GuardedStore::new(LocalStore::new(fixture.mirror.clone()).unwrap());
        store.peer_commits_between_head_and_put(
            "projects/poc-infra/manifest.json",
            &written_by_peer,
        );
        fixture.write(&dev, "infra/a.yaml", "a\n");
        let mut publisher =
            Workspace::open(&fixture.config(&dev, "host-a"), Box::new(store)).unwrap();
        let report = publisher.publish().unwrap();
        assert!(report.contended.is_empty(), "{report:?}");
        assert!(report.conflicts.is_empty(), "{report:?}");
        let index: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
        // Our path landed; the peer path that arrived inside the window did not.
        assert!(index["files"].get("infra/a.yaml").is_some(), "{index}");
        assert!(
            index["files"].get("infra/c.yaml").is_none(),
            "peer write between HEAD and PUT was overwritten: {index}"
        );
    }
}
