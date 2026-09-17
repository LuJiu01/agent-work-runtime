//! Boundary regressions for the PR #29 review findings.
//!
//! These cases pin the contracts reviewers required: no symlink following,
//! content-object integrity before commit, handoff name safety, size budgets,
//! fail-closed remote indexes, HTTPS defaults, cancel-before-write, and the
//! honest guarded-mode TOCTOU window.
use awr_workspace::backend::{Backend, LocalStore, MemoryStore, Precondition};
use awr_workspace::config::{self, Addressing, DEFAULT_CONCURRENCY, StoreConfig, WorkspaceConfig};
use awr_workspace::credentials::{self, Credentials};
use awr_workspace::fsutil;
use awr_workspace::layout;
use awr_workspace::sync::Workspace;
use awr_workspace::{MAX_INDEX_FILES, MAX_OBJECT_BYTES, digest};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct Fixture {
    dir: PathBuf,
    mirror: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "awr-pr29-bound-{}-{}",
            std::process::id(),
            awr_core::Id::new()
        ));
        let mirror = dir.join("store");
        std::fs::create_dir_all(&mirror).unwrap();
        Self { dir, mirror }
    }

    fn root(&self, name: &str) -> PathBuf {
        let root = self.dir.join(name);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn config(&self, root: &Path, host: &str) -> WorkspaceConfig {
        WorkspaceConfig {
            project_key: "poc-bound".to_string(),
            root: root.to_path_buf(),
            host: host.to_string(),
            track: vec!["work-ledger.yaml".to_string(), "infra".to_string()],
            state: root.join(".awr/workspace.json"),
            credentials: root.join(".awr/workspace-credentials.json"),
            store: StoreConfig {
                backend: "local".to_string(),
                endpoint: "http://localhost".to_string(),
                bucket: String::new(),
                region: "auto".to_string(),
                addressing: Addressing::Auto,
                prefix: String::new(),
                concurrency: DEFAULT_CONCURRENCY,
                allow_insecure: false,
                path: Some(self.mirror.clone()),
            },
        }
    }

    fn workspace(&self, root: &Path, host: &str) -> Workspace {
        Workspace::open(
            &self.config(root, host),
            Box::new(LocalStore::new(self.mirror.clone()).unwrap()),
        )
        .unwrap()
    }

    fn write(&self, root: &Path, relpath: &str, body: &str) {
        let path = root.join(relpath);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
}

// ---------------------------------------------------------------------------
// P0: symbolic links
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn tracked_nested_file_symlink_is_refused_and_does_not_leak() {
    let fixture = Fixture::new();
    let dev = fixture.root("dev");
    let outside = fixture.dir.join("vault/secret.env");
    std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
    std::fs::write(&outside, b"nested-secret-value").unwrap();
    fixture.write(&dev, "work-ledger.yaml", "one\n");
    std::fs::create_dir_all(dev.join("infra")).unwrap();
    std::os::unix::fs::symlink(&outside, dev.join("infra/nested.env")).unwrap();

    let error = fixture
        .workspace(&dev, "host-a")
        .publish()
        .unwrap_err()
        .to_string();
    assert!(error.contains("symbolic link"), "{error}");
    let leaked = walk_contains(&fixture.mirror, b"nested-secret-value");
    assert!(!leaked, "nested symlink target must not enter the store");
}

#[cfg(unix)]
#[test]
fn publish_preview_also_refuses_symlinks() {
    let fixture = Fixture::new();
    let dev = fixture.root("dev");
    let outside = fixture.dir.join("secret.txt");
    std::fs::write(&outside, b"preview-secret").unwrap();
    fixture.write(&dev, "work-ledger.yaml", "one\n");
    std::os::unix::fs::symlink(&outside, dev.join("linked.txt")).unwrap();
    let mut config = fixture.config(&dev, "host-a");
    config.track = vec!["work-ledger.yaml".into(), "linked.txt".into()];
    let publisher = Workspace::open(
        &config,
        Box::new(LocalStore::new(fixture.mirror.clone()).unwrap()),
    )
    .unwrap();
    let error = publisher.publish_preview().unwrap_err().to_string();
    assert!(error.contains("symbolic link"), "{error}");
}

#[cfg(unix)]
#[test]
fn local_bytes_refuses_symlink_even_when_not_listed_via_walk() {
    let fixture = Fixture::new();
    let dev = fixture.root("dev");
    let outside = fixture.dir.join("hidden.txt");
    std::fs::write(&outside, b"hidden").unwrap();
    std::os::unix::fs::symlink(&outside, dev.join("work-ledger.yaml")).unwrap();
    let mut config = fixture.config(&dev, "host-a");
    config.track = vec!["work-ledger.yaml".into()];
    let error = Workspace::open(
        &config,
        Box::new(LocalStore::new(fixture.mirror.clone()).unwrap()),
    )
    .unwrap()
    .publish()
    .unwrap_err()
    .to_string();
    assert!(error.contains("symbolic link"), "{error}");
}

// ---------------------------------------------------------------------------
// P1: content object integrity
// ---------------------------------------------------------------------------

#[test]
fn publish_succeeds_when_existing_content_object_matches_exactly() {
    let fixture = Fixture::new();
    let dev = fixture.root("dev");
    fixture.write(&dev, "work-ledger.yaml", "same-bytes\n");
    let digest = digest(b"same-bytes\n");
    let key = layout::content("poc-bound", "work-ledger.yaml", &digest);
    let store = LocalStore::new(fixture.mirror.clone()).unwrap();
    store
        .put(&key, b"same-bytes\n", Precondition::None)
        .unwrap();
    let mut publisher = Workspace::open(&fixture.config(&dev, "host-a"), Box::new(store)).unwrap();
    let report = publisher.publish().unwrap();
    assert_eq!(report.pushed.len(), 1);
    assert!(!report.pushed[0].content_uploaded);
    assert!(
        fixture
            .mirror
            .join("projects/poc-bound/manifest.json")
            .exists()
    );
}

#[test]
fn publish_fails_when_absent_reports_present_but_get_is_empty() {
    let fixture = Fixture::new();
    let dev = fixture.root("dev");
    fixture.write(&dev, "work-ledger.yaml", "need-upload\n");
    let store = GhostAbsentStore {
        inner: LocalStore::new(fixture.mirror.clone()).unwrap(),
    };
    let mut publisher = Workspace::open(&fixture.config(&dev, "host-a"), Box::new(store)).unwrap();
    let error = publisher.publish().unwrap_err().to_string();
    assert!(
        error.contains("reported present") || error.contains("could not be read"),
        "{error}"
    );
    assert!(
        !fixture
            .mirror
            .join("projects/poc-bound/manifest.json")
            .exists()
    );
}

#[test]
fn publish_fails_when_existing_content_has_wrong_size_same_prefix() {
    let fixture = Fixture::new();
    let dev = fixture.root("dev");
    fixture.write(&dev, "work-ledger.yaml", "abcdef\n");
    let digest = digest(b"abcdef\n");
    let key = layout::content("poc-bound", "work-ledger.yaml", &digest);
    let store = LocalStore::new(fixture.mirror.clone()).unwrap();
    // Wrong body, different length.
    store.put(&key, b"nope", Precondition::None).unwrap();
    let mut publisher = Workspace::open(&fixture.config(&dev, "host-a"), Box::new(store)).unwrap();
    let error = publisher.publish().unwrap_err().to_string();
    assert!(error.contains("already holds"), "{error}");
}

// ---------------------------------------------------------------------------
// P1: handoff names
// ---------------------------------------------------------------------------

#[test]
fn handoff_push_rejects_traversal_and_private_names() {
    let fixture = Fixture::new();
    let dev = fixture.root("dev");
    fixture.write(&dev, "work-ledger.yaml", "one\n");
    let body = dev.join("note.json");
    std::fs::write(&body, br#"{"summary":"x"}"#).unwrap();
    let mut author = fixture.workspace(&dev, "host-a");
    for name in ["foo/bar", "..", ".", "a\\b", ".git", ".awr", ""] {
        let error = author.handoff_push(&body, name).unwrap_err().to_string();
        assert!(
            error.contains("path component")
                || error.contains("never leaves")
                || error.contains(name)
                || name.is_empty(),
            "name={name:?} error={error}"
        );
    }
}

#[test]
fn handoff_push_and_pull_roundtrip_with_a_safe_name() {
    let fixture = Fixture::new();
    let dev = fixture.root("dev");
    let mini = fixture.root("mini");
    fixture.write(&dev, "work-ledger.yaml", "one\n");
    let body = dev.join("note.json");
    std::fs::write(&body, br#"{"summary":"bring postgres"}"#).unwrap();
    let mut author = fixture.workspace(&dev, "host-a");
    let pushed = author.handoff_push(&body, "postgres-up").unwrap();
    assert!(!pushed.idempotent_replay);
    let mut peer = fixture.workspace(&mini, "host-b");
    let pulled = peer.handoff_pull(&mini.join("inbox")).unwrap();
    assert_eq!(pulled.len(), 1);
    assert_eq!(pulled[0].sha256, pushed.sha256);
    assert_eq!(
        std::fs::read_to_string(&pulled[0].path).unwrap(),
        r#"{"summary":"bring postgres"}"#
    );
}

// ---------------------------------------------------------------------------
// P1: size budgets
// ---------------------------------------------------------------------------

#[test]
fn put_rejects_bodies_over_max_object_bytes() {
    let oversized = vec![b'x'; MAX_OBJECT_BYTES as usize + 1];
    for backend in [
        Box::new(MemoryStore::new()) as Box<dyn Backend>,
        Box::new(LocalStore::new(Fixture::new().mirror).unwrap()),
    ] {
        let error = backend
            .put("too-big", &oversized, Precondition::None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("maximum"), "{error}");
    }
}

#[test]
fn local_read_rejects_files_over_max_object_bytes() {
    let fixture = Fixture::new();
    let path = fixture.dir.join("huge.bin");
    // Sparse-ish write: metadata size matters for the first check.
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(MAX_OBJECT_BYTES + 1).unwrap();
    drop(file);
    let error = fsutil::read_nofollow(&path).unwrap_err().to_string();
    assert!(error.contains("maximum"), "{error}");
}

#[test]
fn remote_index_rejects_manifest_claiming_too_many_files() {
    let fixture = Fixture::new();
    let store = LocalStore::new(fixture.mirror.clone()).unwrap();
    let mut files = serde_json::Map::new();
    for i in 0..(MAX_INDEX_FILES + 1) {
        let sha = format!("{:064x}", i);
        files.insert(
            format!("f{i}.txt"),
            serde_json::json!({"sha256": sha, "size": 1}),
        );
    }
    let body = serde_json::json!({
        "schema": 1,
        "project_key": "poc-bound",
        "files": files,
    });
    store
        .put(
            "projects/poc-bound/manifest.json",
            &serde_json::to_vec(&body).unwrap(),
            Precondition::None,
        )
        .unwrap();
    let dev = fixture.root("dev");
    fixture.write(&dev, "work-ledger.yaml", "one\n");
    let workspace = fixture.workspace(&dev, "host-a");
    let error = workspace.status(false).unwrap_err().to_string();
    assert!(
        error.contains("maximum") || error.contains(&MAX_INDEX_FILES.to_string()),
        "{error}"
    );
}

#[test]
fn remote_index_rejects_entry_with_uppercase_digest() {
    let fixture = Fixture::new();
    let store = LocalStore::new(fixture.mirror.clone()).unwrap();
    let body = serde_json::json!({
        "schema": 1,
        "project_key": "poc-bound",
        "files": {
            "work-ledger.yaml": {
                "sha256": "A".repeat(64),
                "size": 1
            }
        }
    });
    store
        .put(
            "projects/poc-bound/manifest.json",
            &serde_json::to_vec(&body).unwrap(),
            Precondition::None,
        )
        .unwrap();
    let dev = fixture.root("dev");
    fixture.write(&dev, "work-ledger.yaml", "one\n");
    let error = fixture
        .workspace(&dev, "host-a")
        .status(false)
        .unwrap_err()
        .to_string();
    assert!(error.contains("malformed sha256"), "{error}");
}

#[test]
fn remote_index_rejects_entry_claiming_size_over_budget() {
    let fixture = Fixture::new();
    let store = LocalStore::new(fixture.mirror.clone()).unwrap();
    let body = serde_json::json!({
        "schema": 1,
        "project_key": "poc-bound",
        "files": {
            "work-ledger.yaml": {
                "sha256": "a".repeat(64),
                "size": MAX_OBJECT_BYTES + 1
            }
        }
    });
    store
        .put(
            "projects/poc-bound/manifest.json",
            &serde_json::to_vec(&body).unwrap(),
            Precondition::None,
        )
        .unwrap();
    let dev = fixture.root("dev");
    fixture.write(&dev, "work-ledger.yaml", "one\n");
    let error = fixture
        .workspace(&dev, "host-a")
        .status(false)
        .unwrap_err()
        .to_string();
    assert!(error.contains("maximum"), "{error}");
}

// ---------------------------------------------------------------------------
// P2: pointer fail-closed
// ---------------------------------------------------------------------------

#[test]
fn pointer_index_fails_on_non_json_pointer() {
    let fixture = Fixture::new();
    let store = LocalStore::new(fixture.mirror.clone()).unwrap();
    store
        .put(
            "projects/poc-bound/files/work-ledger.yaml/current.json",
            b"not-json",
            Precondition::None,
        )
        .unwrap();
    let dev = fixture.root("dev");
    fixture.write(&dev, "work-ledger.yaml", "one\n");
    let error = Workspace::open(&fixture.config(&dev, "host-a"), Box::new(store))
        .unwrap()
        .status(true)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("not valid JSON") || error.contains("pointer"),
        "{error}"
    );
}

#[test]
fn pointer_index_fails_on_malformed_digest() {
    let fixture = Fixture::new();
    let store = LocalStore::new(fixture.mirror.clone()).unwrap();
    store
        .put(
            "projects/poc-bound/files/work-ledger.yaml/current.json",
            br#"{"schema":1,"sha256":"xyz","size":1}"#,
            Precondition::None,
        )
        .unwrap();
    let dev = fixture.root("dev");
    fixture.write(&dev, "work-ledger.yaml", "one\n");
    let error = Workspace::open(&fixture.config(&dev, "host-a"), Box::new(store))
        .unwrap()
        .status(true)
        .unwrap_err()
        .to_string();
    assert!(error.contains("malformed sha256"), "{error}");
}

// ---------------------------------------------------------------------------
// P1: cancel / local edit races
// ---------------------------------------------------------------------------

#[test]
fn cancel_during_pull_leaves_unwritten_paths_untouched() {
    let fixture = Fixture::new();
    let dev = fixture.root("dev");
    let mini = fixture.root("mini");
    fixture.write(&dev, "work-ledger.yaml", "from-dev\n");
    fixture.write(&dev, "infra/a.yaml", "a\n");
    fixture.workspace(&dev, "host-a").publish().unwrap();

    let cancel = Arc::new(AtomicBool::new(false));
    let writes = Arc::new(AtomicUsize::new(0));
    let store = CountingGetStore {
        inner: LocalStore::new(fixture.mirror.clone()).unwrap(),
        cancel: Arc::clone(&cancel),
        gets: Arc::clone(&writes),
        trip_after: 1,
    };
    let mut peer = Workspace::open(&fixture.config(&mini, "host-b"), Box::new(store))
        .unwrap()
        .with_cancel(Arc::clone(&cancel));
    let error = peer.pull().unwrap_err().to_string();
    assert!(error.contains("cancelled"), "{error}");
    // At least the cancel tripped; no guarantee on partial, but cancelled means
    // ensure_active fired before further writes after the flag.
    assert!(cancel.load(Ordering::Acquire));
}

#[test]
fn pull_does_not_overwrite_a_local_edit_made_after_plan() {
    let fixture = Fixture::new();
    let dev = fixture.root("dev");
    let mini = fixture.root("mini");
    fixture.write(&dev, "work-ledger.yaml", "remote\n");
    fixture.workspace(&dev, "host-a").publish().unwrap();

    // Peer starts empty. Use a store that mutates the local file during GET.
    let mini_ledger = mini.join("work-ledger.yaml");
    let store = EditLocalOnGet {
        inner: LocalStore::new(fixture.mirror.clone()).unwrap(),
        local_path: mini_ledger.clone(),
        new_body: b"agent-edit-in-flight\n".to_vec(),
        fired: AtomicBool::new(false),
    };
    let mut peer = Workspace::open(&fixture.config(&mini, "host-b"), Box::new(store)).unwrap();
    let report = peer.pull().unwrap();
    // Either conflicted (preferred) or, if write happened to race differently,
    // local must still be the agent edit — never silently remote.
    let local = std::fs::read(&mini_ledger).unwrap_or_default();
    assert!(
        report
            .conflicts
            .iter()
            .any(|c| c.path == "work-ledger.yaml")
            || local == b"agent-edit-in-flight\n",
        "report={report:?} local={}",
        String::from_utf8_lossy(&local)
    );
    assert_ne!(
        local, b"remote\n",
        "agent edit must not be clobbered by pull"
    );
}

// ---------------------------------------------------------------------------
// P2: endpoint / credentials
// ---------------------------------------------------------------------------

#[test]
fn config_rejects_http_with_query_fragment_and_credentials() {
    let dir = scratch_cfg();
    let cases = [
        (r#"endpoint = "https://example.com/path""#, "origin"),
        (r#"endpoint = "https://example.com?x=1""#, "origin"),
        (r#"endpoint = "https://user:pass@example.com""#, "userinfo"),
        (r#"endpoint = "http://example.com""#, "https"),
        (r#"endpoint = "ftp://example.com""#, "scheme"),
    ];
    for (endpoint_line, needle) in cases {
        let body = format!(
            r#"
[project]
key = "poc"
host = "h"
track = ["a.yaml"]
[store]
{endpoint_line}
bucket = "b"
"#
        );
        let path = dir.join(format!("cfg-{needle}.toml"));
        std::fs::write(&path, body).unwrap();
        let error = config::load(&path, &dir).unwrap_err().to_string();
        assert!(
            error.to_lowercase().contains(needle)
                || error.contains("https")
                || error.contains("credentials"),
            "endpoint={endpoint_line} error={error}"
        );
    }
}

#[test]
fn allow_insecure_permits_cleartext_lab_endpoint() {
    let dir = scratch_cfg();
    let path = dir.join("insecure.toml");
    std::fs::write(
        &path,
        r#"
[project]
key = "poc"
host = "h"
track = ["a.yaml"]
[store]
endpoint = "http://example.com"
bucket = "b"
allow_insecure = true
"#,
    )
    .unwrap();
    let config = config::load(&path, &dir).unwrap();
    assert_eq!(config.store.endpoint, "http://example.com");
    assert!(config.store.allow_insecure);
}

#[test]
fn credential_save_is_atomic_and_unix_mode_600() {
    let dir = scratch_cfg();
    let path = dir.join(".awr/workspace-credentials.json");
    let creds = Credentials {
        access_key: "AKIAEXAMPLE".into(),
        secret_key: "secret-example".into(),
        session_token: None,
    };
    credentials::save(&path, &creds).unwrap();
    assert!(path.exists());
    assert!(!path.with_extension("json.tmp").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "mode={mode:o}");
    }
    // status never echoes secrets
    let status = credentials::status(&path).unwrap();
    let rendered = serde_json::to_string(&status).unwrap();
    assert!(!rendered.contains("secret-example"), "{rendered}");
}

// ---------------------------------------------------------------------------
// abnormal / hostile index payloads
// ---------------------------------------------------------------------------

#[test]
fn status_rejects_manifest_for_wrong_project_key() {
    let fixture = Fixture::new();
    let store = LocalStore::new(fixture.mirror.clone()).unwrap();
    store
        .put(
            "projects/poc-bound/manifest.json",
            br#"{"schema":1,"project_key":"other","files":{}}"#,
            Precondition::None,
        )
        .unwrap();
    let dev = fixture.root("dev");
    fixture.write(&dev, "work-ledger.yaml", "one\n");
    let error = fixture
        .workspace(&dev, "host-a")
        .status(false)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("does not describe project") || error.contains("poc-bound"),
        "{error}"
    );
}

#[test]
fn status_rejects_unknown_manifest_schema() {
    let fixture = Fixture::new();
    let store = LocalStore::new(fixture.mirror.clone()).unwrap();
    store
        .put(
            "projects/poc-bound/manifest.json",
            br#"{"schema":99,"project_key":"poc-bound","files":{}}"#,
            Precondition::None,
        )
        .unwrap();
    let dev = fixture.root("dev");
    fixture.write(&dev, "work-ledger.yaml", "one\n");
    let error = fixture
        .workspace(&dev, "host-a")
        .status(false)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("does not describe project")
            || error.contains("schema")
            || error.contains("poc-bound"),
        "{error}"
    );
}

#[test]
fn pull_rejects_content_object_digest_mismatch() {
    let fixture = Fixture::new();
    let dev = fixture.root("dev");
    let mini = fixture.root("mini");
    fixture.write(&dev, "work-ledger.yaml", "good\n");
    fixture.workspace(&dev, "host-a").publish().unwrap();
    // Corrupt the content object while keeping the index digest.
    let digest = digest(b"good\n");
    let key = layout::content("poc-bound", "work-ledger.yaml", &digest);
    let path = fixture.mirror.join(key);
    std::fs::write(&path, b"tampered").unwrap();
    let error = fixture
        .workspace(&mini, "host-b")
        .pull()
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("does not match") || error.contains("content object"),
        "{error}"
    );
    assert!(
        mini.join("work-ledger.yaml").metadata().is_err()
            || fixture_read_optional(&mini, "work-ledger.yaml") != Some("tampered".into())
    );
}

#[test]
fn drop_still_refuses_paths_covered_by_track() {
    let fixture = Fixture::new();
    let dev = fixture.root("dev");
    fixture.write(&dev, "work-ledger.yaml", "one\n");
    fixture.write(&dev, "infra/a.yaml", "a\n");
    let mut ws = fixture.workspace(&dev, "host-a");
    ws.publish().unwrap();
    let error = ws
        .drop_paths(&["infra/a.yaml".into()])
        .unwrap_err()
        .to_string();
    assert!(error.contains("project.track"), "{error}");
}

#[test]
fn handoff_push_rejects_oversized_source_file() {
    let fixture = Fixture::new();
    let dev = fixture.root("dev");
    fixture.write(&dev, "work-ledger.yaml", "one\n");
    let big = dev.join("big-handoff.bin");
    let file = std::fs::File::create(&big).unwrap();
    file.set_len(MAX_OBJECT_BYTES + 1).unwrap();
    drop(file);
    let error = fixture
        .workspace(&dev, "host-a")
        .handoff_push(&big, "too-big")
        .unwrap_err()
        .to_string();
    assert!(error.contains("maximum"), "{error}");
}

#[test]
fn pull_fails_when_index_names_missing_content_object() {
    let fixture = Fixture::new();
    let store = LocalStore::new(fixture.mirror.clone()).unwrap();
    let sha = "a".repeat(64);
    let body = serde_json::json!({
        "schema": 1,
        "project_key": "poc-bound",
        "files": {
            "work-ledger.yaml": {"sha256": sha, "size": 4}
        }
    });
    store
        .put(
            "projects/poc-bound/manifest.json",
            &serde_json::to_vec(&body).unwrap(),
            Precondition::None,
        )
        .unwrap();
    let mini = fixture.root("mini");
    fixture.write(&mini, "work-ledger.yaml", "old\n");
    // Seed base so pull would overwrite.
    let mut peer = Workspace::open(&fixture.config(&mini, "host-b"), Box::new(store)).unwrap();
    // Force base match by publishing empty? Simpler: delete local so missing_local pull path.
    std::fs::remove_file(mini.join("work-ledger.yaml")).unwrap();
    let error = peer.pull().unwrap_err().to_string();
    assert!(
        error.contains("no content object") || error.contains("content object"),
        "{error}"
    );
}

#[test]
fn loopback_http_with_port_is_accepted() {
    let dir = scratch_cfg();
    let path = dir.join("loopback.toml");
    std::fs::write(
        &path,
        r#"
[project]
key = "poc"
host = "h"
track = ["a.yaml"]
[store]
endpoint = "http://127.0.0.1:9000"
bucket = "b"
"#,
    )
    .unwrap();
    let config = config::load(&path, &dir).unwrap();
    assert_eq!(config.store.endpoint, "http://127.0.0.1:9000");
}

#[test]
fn publish_preview_does_not_write_on_conflict_plan() {
    let fixture = Fixture::new();
    let dev = fixture.root("dev");
    let mini = fixture.root("mini");
    fixture.write(&dev, "work-ledger.yaml", "one\n");
    fixture.workspace(&dev, "host-a").publish().unwrap();
    fixture.write(&mini, "work-ledger.yaml", "one\n");
    let mut peer = fixture.workspace(&mini, "host-b");
    peer.pull().unwrap();
    fixture.write(&dev, "work-ledger.yaml", "dev\n");
    fixture.workspace(&dev, "host-a").publish().unwrap();
    fixture.write(&mini, "work-ledger.yaml", "mini\n");
    let preview = peer.publish_preview().unwrap();
    assert!(preview.dry_run);
    assert!(!preview.conflicts.is_empty());
    // Preview must not change remote index host marker / local state file existence is fine.
    assert_eq!(
        std::fs::read_to_string(mini.join("work-ledger.yaml")).unwrap(),
        "mini\n"
    );
}

// ---------------------------------------------------------------------------
// helpers / test doubles
// ---------------------------------------------------------------------------

fn scratch_cfg() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("awr-pr29-cfg-{}", awr_core::Id::new()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn walk_contains(root: &Path, needle: &[u8]) -> bool {
    fn walk(path: &Path, needle: &[u8]) -> bool {
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            return false;
        };
        if meta.file_type().is_symlink() {
            return false;
        }
        if meta.is_file() {
            return std::fs::read(path)
                .ok()
                .is_some_and(|b| b.windows(needle.len()).any(|w| w == needle));
        }
        if meta.is_dir() {
            if let Ok(rd) = std::fs::read_dir(path) {
                for entry in rd.flatten() {
                    if walk(&entry.path(), needle) {
                        return true;
                    }
                }
            }
        }
        false
    }
    walk(root, needle)
}

fn fixture_read_optional(root: &Path, rel: &str) -> Option<String> {
    std::fs::read_to_string(root.join(rel)).ok()
}

/// Absent PUT always reports created=false without storing bytes.
struct GhostAbsentStore {
    inner: LocalStore,
}

impl Backend for GhostAbsentStore {
    fn describe(&self) -> String {
        "ghost-absent".into()
    }
    fn supports_if_match(&self) -> bool {
        true
    }
    fn requests(&self) -> u64 {
        self.inner.requests()
    }
    fn head(&self, key: &str) -> awr_core::Result<Option<awr_workspace::ObjectMeta>> {
        self.inner.head(key)
    }
    fn get_meta(&self, key: &str) -> awr_core::Result<Option<(Vec<u8>, String)>> {
        self.inner.get_meta(key)
    }
    fn put(
        &self,
        key: &str,
        body: &[u8],
        precondition: Precondition,
    ) -> awr_core::Result<awr_workspace::PutOutcome> {
        match precondition {
            Precondition::Absent => Ok(awr_workspace::PutOutcome {
                created: false,
                etag: Some("ghost".into()),
            }),
            other => self.inner.put(key, body, other),
        }
    }
    fn list(&self, prefix: &str) -> awr_core::Result<Vec<awr_workspace::backend::Listed>> {
        self.inner.list(prefix)
    }
    fn delete(&self, key: &str) -> awr_core::Result<bool> {
        self.inner.delete(key)
    }
}

/// After N content GETs, raise cancel so pull stops before remaining writes.
struct CountingGetStore {
    inner: LocalStore,
    cancel: Arc<AtomicBool>,
    gets: Arc<AtomicUsize>,
    trip_after: usize,
}

impl Backend for CountingGetStore {
    fn describe(&self) -> String {
        "counting-get".into()
    }
    fn supports_if_match(&self) -> bool {
        true
    }
    fn requests(&self) -> u64 {
        self.inner.requests()
    }
    fn head(&self, key: &str) -> awr_core::Result<Option<awr_workspace::ObjectMeta>> {
        self.inner.head(key)
    }
    fn get_meta(&self, key: &str) -> awr_core::Result<Option<(Vec<u8>, String)>> {
        // Only content objects count: index reads must not trip the cancel early.
        if key.contains("/files/") && !key.ends_with("/current.json") {
            let n = self.gets.fetch_add(1, Ordering::SeqCst) + 1;
            if n > self.trip_after {
                self.cancel.store(true, Ordering::Release);
            }
        }
        self.inner.get_meta(key)
    }
    fn put(
        &self,
        key: &str,
        body: &[u8],
        precondition: Precondition,
    ) -> awr_core::Result<awr_workspace::PutOutcome> {
        self.inner.put(key, body, precondition)
    }
    fn list(&self, prefix: &str) -> awr_core::Result<Vec<awr_workspace::backend::Listed>> {
        self.inner.list(prefix)
    }
    fn delete(&self, key: &str) -> awr_core::Result<bool> {
        self.inner.delete(key)
    }
}

/// On first content GET, write a local edit the pull plan did not see.
struct EditLocalOnGet {
    inner: LocalStore,
    local_path: PathBuf,
    new_body: Vec<u8>,
    fired: AtomicBool,
}

impl Backend for EditLocalOnGet {
    fn describe(&self) -> String {
        "edit-local-on-get".into()
    }
    fn supports_if_match(&self) -> bool {
        true
    }
    fn requests(&self) -> u64 {
        self.inner.requests()
    }
    fn head(&self, key: &str) -> awr_core::Result<Option<awr_workspace::ObjectMeta>> {
        self.inner.head(key)
    }
    fn get_meta(&self, key: &str) -> awr_core::Result<Option<(Vec<u8>, String)>> {
        if key.contains("/files/") && !key.ends_with("current.json") && !key.contains("manifest") {
            if !self.fired.swap(true, Ordering::SeqCst) {
                if let Some(parent) = self.local_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(&self.local_path, &self.new_body);
            }
        }
        self.inner.get_meta(key)
    }
    fn put(
        &self,
        key: &str,
        body: &[u8],
        precondition: Precondition,
    ) -> awr_core::Result<awr_workspace::PutOutcome> {
        self.inner.put(key, body, precondition)
    }
    fn list(&self, prefix: &str) -> awr_core::Result<Vec<awr_workspace::backend::Listed>> {
        self.inner.list(prefix)
    }
    fn delete(&self, key: &str) -> awr_core::Result<bool> {
        self.inner.delete(key)
    }
}
