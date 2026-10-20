use awr_core::{Error, EventDraft, Id};
use awr_source::{Manifest, index_project, source_state_fingerprint, verify_read_snapshot};
use awr_store::Store;
use std::{fs, path::PathBuf};

const MANIFEST: &str = "[project]\nname='Read snapshot fixture'\ncontext_profile='minimal'\n[[sources]]\ndomain='ledger'\nrole='primary'\npath='work.yaml'\nadapter='yaml-ledger-v1'\n";
const LEDGER: &str = "work_items:\n- id: W\n  title: Review the guide\n  status: planned\n  evidence_level: unrecognized\n";

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("awr-read-snapshot-{}", Id::new()));
        fs::create_dir_all(root.join(".awr")).unwrap();
        let fixture = Self(root.canonicalize().unwrap());
        fs::write(fixture.0.join(".awr/project.toml"), MANIFEST).unwrap();
        fs::write(fixture.0.join("work.yaml"), LEDGER).unwrap();
        let mut store = Store::open(&fixture.database()).unwrap();
        let report = index_project(
            &mut store,
            &fixture.0,
            &Manifest::load(&fixture.0).unwrap(),
            false,
        )
        .unwrap();
        assert!(report.ok);
        assert_eq!(report.sources[0].warnings.len(), 1);
        fixture
    }
    fn database(&self) -> PathBuf {
        self.0.join(".awr/state.db")
    }
    fn snapshot(&self) -> Store {
        Store::read_snapshot(&self.database(), 256 * 1024 * 1024).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn source_verification_preserves_warnings_and_ignores_origin_runtime_events() {
    let f = Fixture::new();
    let mut snapshot = f.snapshot();
    let project = snapshot.project_by_root(&f.0).unwrap();
    let fingerprint = source_state_fingerprint(&snapshot, project.id).unwrap();
    let before = verify_read_snapshot(&mut snapshot, &f.0, &project).unwrap();
    let mut origin = Store::open_existing(&f.database()).unwrap();
    let advanced = origin
        .append_event(
            project.id,
            project.project_revision,
            EventDraft::new("work.observed", "Another reader inspected the guide"),
        )
        .unwrap();
    let after = verify_read_snapshot(&mut snapshot, &f.0, &project).unwrap();
    assert_eq!(before.sources[0].warnings, after.sources[0].warnings);
    assert_eq!(after.sources[0].warnings.len(), 1);
    assert_eq!(after.project_revision, project.project_revision);
    assert_eq!(
        source_state_fingerprint(&snapshot, project.id).unwrap(),
        fingerprint
    );
    assert_eq!(
        source_state_fingerprint(&origin, project.id).unwrap(),
        fingerprint
    );
    assert_eq!(
        origin.project(project.id).unwrap().project_revision,
        advanced.project_revision
    );
    assert!(advanced.project_revision > project.project_revision);
    assert_eq!(fs::read_to_string(f.0.join("work.yaml")).unwrap(), LEDGER);
}

#[test]
fn source_and_registration_drift_fail_before_or_after_read_without_persisting_refresh() {
    for previously_verified in [false, true] {
        for drift in ["content", "missing", "mapping", "options", "adapter"] {
            let f = Fixture::new();
            let mut snapshot = f.snapshot();
            let project = snapshot.project_by_root(&f.0).unwrap();
            if previously_verified {
                verify_read_snapshot(&mut snapshot, &f.0, &project).unwrap();
            }
            let database = fs::read(f.database()).unwrap();
            match drift {
                "content" => fs::write(
                    f.0.join("work.yaml"),
                    LEDGER.replace("Review the guide", "Review new requirements"),
                )
                .unwrap(),
                "missing" => fs::remove_file(f.0.join("work.yaml")).unwrap(),
                "mapping" => {
                    fs::write(f.0.join("other.yaml"), LEDGER).unwrap();
                    fs::write(
                        f.0.join(".awr/project.toml"),
                        MANIFEST.replace("work.yaml", "other.yaml"),
                    )
                    .unwrap();
                }
                "options" => fs::write(
                    f.0.join(".awr/project.toml"),
                    format!("{MANIFEST}[sources.options.status_map]\nqueued='planned'\n"),
                )
                .unwrap(),
                "adapter" => fs::write(
                    f.0.join(".awr/project.toml"),
                    MANIFEST.replace("yaml-ledger-v1", "yaml-workstream-ledger-v1"),
                )
                .unwrap(),
                _ => unreachable!(),
            }
            assert!(
                matches!(
                    verify_read_snapshot(&mut snapshot, &f.0, &project),
                    Err(Error::SourceStale(_))
                ),
                "{drift}, previously_verified={previously_verified}"
            );
            assert_eq!(fs::read(f.database()).unwrap(), database, "{drift}");
            let origin = f.snapshot();
            assert_eq!(
                origin.project(project.id).unwrap().project_revision,
                project.project_revision
            );
            assert_eq!(
                origin.work_items(project.id).unwrap()[0].item.title,
                "Review the guide"
            );
        }
    }
}

#[test]
fn verification_refuses_a_snapshot_mutated_during_the_read() {
    let f = Fixture::new();
    let mut snapshot = f.snapshot();
    let project = snapshot.project_by_root(&f.0).unwrap();
    verify_read_snapshot(&mut snapshot, &f.0, &project).unwrap();
    let database = fs::read(f.database()).unwrap();
    snapshot
        .append_event(
            project.id,
            project.project_revision,
            EventDraft::new(
                "work.observed",
                "Unexpected mutation of the private read view",
            ),
        )
        .unwrap();
    assert!(matches!(
        verify_read_snapshot(&mut snapshot, &f.0, &project),
        Err(Error::SourceStale(_))
    ));
    assert_eq!(fs::read(f.database()).unwrap(), database);
}
