//! The whole surface a store has to offer: HEAD, GET, PUT, DELETE and LIST,
//! with PUT carrying two optional preconditions.
//!
//! Everything vendor specific stays behind this trait, so the semantics above
//! it never learn the name of a provider. What the semantics need to know is
//! not "is this R2" but "can this store compare and swap" - one capability bit,
//! `supports_if_match`, which the commit path reads to choose between mutual
//! exclusion and a read-back that detects a lost race instead.
use crate::MAX_OBJECT_BYTES;
use awr_core::{Error, Result};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectMeta {
    pub etag: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub key: String,
    pub etag: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Precondition {
    /// Last writer wins. Only ever used for the derived pointer mirror.
    None,
    /// Create only: `If-None-Match: *`.
    Absent,
    /// Compare and swap: `If-Match: <etag>`.
    Match(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutOutcome {
    /// False when a precondition rejected the write; the caller re-reads.
    pub created: bool,
    pub etag: Option<String>,
}

pub trait Backend: Send + Sync {
    /// A short label for reports, never a credential.
    fn describe(&self) -> String;
    /// Whether `Precondition::Match` is enforced by the store.
    fn supports_if_match(&self) -> bool;
    /// Requests actually spent, so "how far away is the store" is observable.
    fn requests(&self) -> u64;
    fn head(&self, key: &str) -> Result<Option<ObjectMeta>>;
    fn get_meta(&self, key: &str) -> Result<Option<(Vec<u8>, String)>>;
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.get_meta(key)?.map(|(body, _etag)| body))
    }
    fn put(&self, key: &str, body: &[u8], precondition: Precondition) -> Result<PutOutcome>;
    fn list(&self, prefix: &str) -> Result<Vec<Listed>>;
    /// Remove `key`. `true` when it was there, `false` when it was already gone.
    fn delete(&self, key: &str) -> Result<bool>;
}

/// Fetch `items` with at most `workers` requests in flight, keeping input order.
///
/// The round trip to a store outside the region dominates cost, so the work is
/// bounded by the number of requests and never by the bytes. Failures are
/// reported in input order rather than completion order, so the same run always
/// names the same first problem.
pub(crate) fn parallel_map<T, R, F>(items: &[T], workers: usize, work: F) -> Result<Vec<R>>
where
    T: Sync,
    R: Send,
    F: Fn(&T) -> Result<R> + Sync,
{
    let workers = workers.clamp(1, crate::config::DEFAULT_CONCURRENCY_CAP);
    if workers == 1 || items.len() <= 1 {
        return items.iter().map(&work).collect();
    }
    let workers = workers.min(items.len());
    let slots: Vec<Mutex<Option<Result<R>>>> = items.iter().map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= items.len() {
                        break;
                    }
                    let outcome = work(&items[index]);
                    let mut slot = slots[index]
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    *slot = Some(outcome);
                }
            });
        }
    });
    let mut collected = Vec::with_capacity(items.len());
    for slot in slots {
        match slot.into_inner().unwrap_or_else(|error| error.into_inner()) {
            Some(outcome) => collected.push(outcome?),
            None => {
                return Err(Error::Storage(
                    "a workspace worker returned no result".into(),
                ));
            }
        }
    }
    Ok(collected)
}

/// A directory standing in for an object store.
///
/// Used by the test suites and by a single-writer demo, and it is honest about
/// what it does not provide: the compare and swap is guarded by an in-process
/// lock, so two *processes* pointing at the same directory can lose an update.
/// Anything with more than one writer needs a real store.
pub struct LocalStore {
    root: PathBuf,
    requests: AtomicU64,
    lock: Mutex<()>,
}

impl LocalStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            requests: AtomicU64::new(0),
            lock: Mutex::new(()),
        })
    }

    fn path(&self, key: &str) -> Result<PathBuf> {
        crate::layout::check_relpath(key)?;
        Ok(self.root.join(key))
    }
}

impl Backend for LocalStore {
    fn describe(&self) -> String {
        format!("local {}", self.root.display())
    }

    fn supports_if_match(&self) -> bool {
        true
    }

    fn requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    fn head(&self, key: &str) -> Result<Option<ObjectMeta>> {
        let Some((body, etag)) = self.get_meta(key)? else {
            return Ok(None);
        };
        Ok(Some(ObjectMeta {
            etag,
            size: body.len() as u64,
        }))
    }

    fn get_meta(&self, key: &str) -> Result<Option<(Vec<u8>, String)>> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        let path = self.path(key)?;
        match std::fs::read(&path) {
            Ok(body) => {
                let etag = crate::digest(&body);
                Ok(Some((body, etag)))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(Error::Io(error)),
        }
    }

    fn put(&self, key: &str, body: &[u8], precondition: Precondition) -> Result<PutOutcome> {
        if body.len() as u64 > MAX_OBJECT_BYTES {
            return Err(Error::InvalidInput(format!(
                "workspace object {key} is {} bytes; maximum is {MAX_OBJECT_BYTES}",
                body.len()
            )));
        }

        self.requests.fetch_add(1, Ordering::Relaxed);
        let path = self.path(key)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _guard = self.lock.lock().unwrap_or_else(|error| error.into_inner());
        // Read, decide and write under one lock: a compare and swap that is not
        // atomic is worse than none, because it reports a conflict it did not
        // have and misses one it did.
        let current = std::fs::read(&path).ok().map(|body| crate::digest(&body));
        match (&precondition, current.as_deref()) {
            (Precondition::Absent, Some(etag)) => {
                return Ok(PutOutcome {
                    created: false,
                    etag: Some(etag.to_string()),
                });
            }
            (Precondition::Match(expected), Some(etag)) if etag == expected => {}
            (Precondition::Match(_), other) => {
                return Ok(PutOutcome {
                    created: false,
                    etag: other.map(str::to_string),
                });
            }
            _ => {}
        }
        write_atomically(&path, body)?;
        Ok(PutOutcome {
            created: true,
            etag: Some(crate::digest(body)),
        })
    }

    fn list(&self, prefix: &str) -> Result<Vec<Listed>> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        let mut out = Vec::new();
        collect(&self.root, &self.root, prefix, &mut out)?;
        out.sort_by(|left, right| left.key.cmp(&right.key));
        Ok(out)
    }

    fn delete(&self, key: &str) -> Result<bool> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        let path = self.path(key)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(Error::Io(error)),
        }
    }
}

fn collect(root: &Path, base: &Path, prefix: &str, out: &mut Vec<Listed>) -> Result<()> {
    for entry in std::fs::read_dir(base)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        if entry.file_type()?.is_dir() {
            collect(root, &path, prefix, out)?;
            continue;
        }
        let key = path
            .strip_prefix(root)
            .expect("every walked path is under the root")
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        if !key.starts_with(prefix) {
            continue;
        }
        let body = std::fs::read(&path)?;
        out.push(Listed {
            key,
            etag: crate::digest(&body),
            size: body.len() as u64,
        });
    }
    Ok(())
}

/// Replace a file with its whole new content, or leave the old one untouched.
pub(crate) fn write_atomically(path: &Path, body: &[u8]) -> Result<()> {
    use std::io::Write;
    let scratch = scratch_name(path);
    {
        let mut handle = std::fs::File::create(&scratch)?;
        handle.write_all(body)?;
        handle.sync_all()?;
    }
    std::fs::rename(&scratch, path)?;
    Ok(())
}

/// Per process and thread: concurrent writers share one directory, so a fixed
/// scratch name would let two of them truncate each other's file.
fn scratch_name(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(
        ".{}.{:?}.tmp",
        std::process::id(),
        std::thread::current().id()
    ));
    path.with_file_name(name)
}

/// An in-memory store, for testing the semantics without a filesystem.
#[derive(Default)]
pub struct MemoryStore {
    objects: Mutex<BTreeMap<String, Vec<u8>>>,
    requests: AtomicU64,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Backend for MemoryStore {
    fn describe(&self) -> String {
        "memory".to_string()
    }

    fn supports_if_match(&self) -> bool {
        true
    }

    fn requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    fn head(&self, key: &str) -> Result<Option<ObjectMeta>> {
        Ok(self.get_meta(key)?.map(|(body, etag)| ObjectMeta {
            etag,
            size: body.len() as u64,
        }))
    }

    fn get_meta(&self, key: &str) -> Result<Option<(Vec<u8>, String)>> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        Ok(self
            .objects
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(key)
            .map(|body| (body.clone(), crate::digest(body))))
    }

    fn put(&self, key: &str, body: &[u8], precondition: Precondition) -> Result<PutOutcome> {
        if body.len() as u64 > MAX_OBJECT_BYTES {
            return Err(Error::InvalidInput(format!(
                "workspace object {key} is {} bytes; maximum is {MAX_OBJECT_BYTES}",
                body.len()
            )));
        }

        self.requests.fetch_add(1, Ordering::Relaxed);
        let mut objects = self
            .objects
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let current = objects.get(key).map(|body| crate::digest(body));
        match (&precondition, current.as_deref()) {
            (Precondition::Absent, Some(etag)) => {
                return Ok(PutOutcome {
                    created: false,
                    etag: Some(etag.to_string()),
                });
            }
            (Precondition::Match(expected), Some(etag)) if etag == expected => {}
            (Precondition::Match(_), other) => {
                return Ok(PutOutcome {
                    created: false,
                    etag: other.map(str::to_string),
                });
            }
            _ => {}
        }
        objects.insert(key.to_string(), body.to_vec());
        Ok(PutOutcome {
            created: true,
            etag: Some(crate::digest(body)),
        })
    }

    fn list(&self, prefix: &str) -> Result<Vec<Listed>> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        Ok(self
            .objects
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .filter(|(key, _)| key.starts_with(prefix))
            .map(|(key, body)| Listed {
                key: key.clone(),
                etag: crate::digest(body),
                size: body.len() as u64,
            })
            .collect())
    }

    fn delete(&self, key: &str) -> Result<bool> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        Ok(self
            .objects
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(key)
            .is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("awr workspace backend {}", awr_core::Id::new()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_create_only_write_is_rejected_once_the_object_exists() {
        for backend in [
            Box::new(MemoryStore::new()) as Box<dyn Backend>,
            Box::new(LocalStore::new(scratch()).unwrap()),
        ] {
            assert!(backend.head("k").unwrap().is_none());
            assert!(
                backend
                    .put("k", b"one", Precondition::Absent)
                    .unwrap()
                    .created
            );
            let second = backend.put("k", b"two", Precondition::Absent).unwrap();
            assert!(!second.created);
            assert_eq!(backend.get("k").unwrap().unwrap(), b"one");
            assert_eq!(backend.head("k").unwrap().unwrap().size, 3);
        }
    }

    #[test]
    fn a_compare_and_swap_needs_the_current_etag() {
        for backend in [
            Box::new(MemoryStore::new()) as Box<dyn Backend>,
            Box::new(LocalStore::new(scratch()).unwrap()),
        ] {
            backend.put("k", b"one", Precondition::Absent).unwrap();
            let stale = crate::digest(b"stale");
            let refused = backend
                .put("k", b"two", Precondition::Match(stale))
                .unwrap();
            assert!(!refused.created);
            assert_eq!(backend.get("k").unwrap().unwrap(), b"one");

            let current = backend.head("k").unwrap().unwrap().etag;
            assert!(
                backend
                    .put("k", b"two", Precondition::Match(current))
                    .unwrap()
                    .created
            );
            assert_eq!(backend.get("k").unwrap().unwrap(), b"two");
        }
    }

    #[test]
    fn a_key_that_leaves_the_root_never_reaches_the_filesystem() {
        let backend = LocalStore::new(scratch()).unwrap();
        for key in ["../escape", "a/../../escape", "/absolute", ""] {
            assert!(backend.get_meta(key).is_err(), "{key:?} was accepted");
        }
    }

    #[test]
    fn listing_returns_keys_with_the_prefix_in_order() {
        let backend = LocalStore::new(scratch()).unwrap();
        for key in ["p/a", "p/b", "q/c"] {
            backend
                .put(key, key.as_bytes(), Precondition::Absent)
                .unwrap();
        }
        let listed: Vec<String> = backend
            .list("p/")
            .unwrap()
            .into_iter()
            .map(|item| item.key)
            .collect();
        assert_eq!(listed, vec!["p/a".to_string(), "p/b".to_string()]);
    }

    #[test]
    fn delete_removes_an_object_and_is_false_when_it_was_already_gone() {
        for backend in [
            Box::new(MemoryStore::new()) as Box<dyn Backend>,
            Box::new(LocalStore::new(scratch()).unwrap()),
        ] {
            assert!(!backend.delete("k").unwrap());
            backend.put("k", b"one", Precondition::Absent).unwrap();
            assert!(backend.delete("k").unwrap());
            assert!(backend.get("k").unwrap().is_none());
            assert!(!backend.delete("k").unwrap());
        }
        let backend = LocalStore::new(scratch()).unwrap();
        for key in ["../escape", "a/../../escape", "/absolute", ""] {
            assert!(backend.delete(key).is_err(), "{key:?} was accepted");
        }
    }

    #[test]
    fn parallel_map_keeps_input_order_and_reports_the_first_failure() {
        let items: Vec<u32> = (0..40).collect();
        let doubled = parallel_map(&items, 8, |item| Ok(item * 2)).unwrap();
        assert_eq!(
            doubled,
            items.iter().map(|item| item * 2).collect::<Vec<_>>()
        );

        let failed = parallel_map(&items, 8, |item| {
            if *item == 3 {
                Err(Error::InvalidInput("item three".into()))
            } else {
                Ok(*item)
            }
        });
        assert!(format!("{}", failed.unwrap_err()).contains("item three"));
    }
}
