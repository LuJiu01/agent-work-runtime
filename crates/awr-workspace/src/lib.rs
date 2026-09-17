//! The workspace exchange plane: moving the files a project shares between
//! machines that do not share a filesystem.
//!
//! Machines run independent agents against one project, so the plane moves
//! *bytes*, never authority. Each host keeps its own authoritative sources and
//! its own local runtime; what travels is the tracked source bytes, the
//! evidence built from them, and the handoffs that carry work between hosts.
//!
//! The layering is deliberate, and each layer can be replaced on its own:
//!
//! - [`layout`] is the key scheme both ends must agree on.
//! - [`backend`] is HEAD, GET, PUT, DELETE and LIST; a provider is one implementation.
//! - [`s3`] is one provider family; [`sigv4`] is the signing dialect next to it,
//!   checked against the official AWS vectors.
//! - [`sync`] is the semantics: an index, a compare-and-swap commit, conflicts.
//! - [`config`] and [`credentials`] are what a machine has to say about itself.
pub mod backend;
pub mod config;
pub mod credentials;
pub mod fsutil;
pub mod layout;
pub mod s3;
pub mod sigv4;
pub mod sync;

pub use backend::{Backend, LocalStore, MemoryStore, ObjectMeta, Precondition, PutOutcome};
pub use config::{StoreConfig, WorkspaceConfig};
pub use sync::{Workspace, open_backend};

/// Hard ceiling for one content object, one handoff, and one local read.
///
/// The exchange plane is for project sources and evidence, not bulk media. The
/// same budget bounds response bodies from the store and bodies leaving the
/// local filesystem, so a single oversized file cannot pin memory on either end.
pub const MAX_OBJECT_BYTES: u64 = 64 * 1024 * 1024;

/// Maximum number of paths a workspace index may name.
///
/// A corrupted or hostile manifest must not force this host to materialize an
/// unbounded path set into memory or onto disk.
pub const MAX_INDEX_FILES: usize = 10_000;

/// The content address of an object, and the identity of a tracked file.
pub fn digest(body: &[u8]) -> String {
    sigv4::hex_sha256(body)
}

/// Whole seconds since the epoch, the unit the state file has always used.
///
/// A clock that cannot be read is not worth failing a sync over: the field is
/// informational, and the content hashes are what identify anything.
pub(crate) fn timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}
