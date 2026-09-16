//! Object keys: the one thing both ends of an exchange plane must agree on.
//!
//! A project-relative path travels as a single key segment, percent-encoded in
//! full, so `infra/evidence/dump.json` is one opaque segment instead of nested
//! prefixes. That is deliberate: a store that normalises or splits on `/` would
//! otherwise be able to fold two different paths into one. The reference client
//! encodes the same way, so a Rust host and a Python host converge on the same
//! objects rather than on two parallel copies of the same project.
use crate::sigv4::{percent_decode, percent_encode};
use awr_core::{Error, Result};

/// The shape of the per-machine state file, and of a pointer object.
pub const SCHEMA: u32 = 1;
/// The shape of the manifest. A host that finds another schema refuses the
/// workspace instead of guessing what the fields mean.
pub const MANIFEST_SCHEMA: u32 = 1;

pub fn encode(component: &str) -> String {
    percent_encode(component, "")
}

pub fn decode(component: &str) -> String {
    percent_decode(component)
}

/// A project key becomes a path element, so it is restricted to what cannot
/// change the shape of a key: no separators, no escapes, nothing to decode.
pub fn validate_project_key(key: &str) -> Result<()> {
    let usable = !key.is_empty()
        && key.len() <= 128
        && key.bytes().any(|byte| byte.is_ascii_alphanumeric())
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if usable {
        Ok(())
    } else {
        Err(Error::InvalidInput(format!(
            "workspace project.key {key:?} must be 1..128 characters of letters, digits, '-' or '_'"
        )))
    }
}

/// A tracked path is relative to the project root, and stays inside it.
///
/// The index is remote data, so a path in it is a claim about this filesystem
/// rather than a fact about it. Both directions check before touching a file:
/// a publish must not read outside the root, and a pull must never write there.
/// `.git` and `.awr` never leave a machine, so a component of either name is
/// refused the same way a traversal is.
pub fn is_safe_relpath(relpath: &str) -> bool {
    !relpath.is_empty()
        && !relpath.starts_with('/')
        && !relpath.contains('\\')
        && !relpath.contains('\0')
        && !relpath.contains(':')
        && relpath.split('/').all(is_safe_component)
}

fn is_safe_component(part: &str) -> bool {
    !part.is_empty() && part != "." && part != ".." && !is_private_component(part)
}

fn is_private_component(part: &str) -> bool {
    matches!(part, ".git" | ".awr")
}

pub fn check_relpath(relpath: &str) -> Result<()> {
    if is_safe_relpath(relpath) {
        Ok(())
    } else if relpath.split('/').any(is_private_component) {
        Err(Error::RuleViolation(format!(
            "workspace path {relpath:?} names a Git history or AWR runtime that never leaves a machine"
        )))
    } else {
        Err(Error::RuleViolation(format!(
            "workspace path {relpath:?} is not a project-relative path inside the project root"
        )))
    }
}

/// A single name that is safe to join under a directory: no separators, no
/// traversal, nothing that names a drive, and not a Git history or AWR runtime.
/// Used for host names and for the two halves of a handoff key, which are
/// remote data used to build a local path.
pub fn check_component(value: &str) -> Result<()> {
    if value.is_empty()
        || value.contains('/')
        || value.contains('\\')
        || value.contains(':')
        || value == "."
        || value == ".."
        || value.contains('\0')
    {
        Err(Error::RuleViolation(format!(
            "workspace name {value:?} is not a single path component"
        )))
    } else if is_private_component(value) {
        Err(Error::RuleViolation(format!(
            "workspace name {value:?} names a Git history or AWR runtime that never leaves a machine"
        )))
    } else {
        Ok(())
    }
}

pub fn content(project_key: &str, relpath: &str, sha256: &str) -> String {
    format!(
        "projects/{project_key}/files/{}/{}",
        encode(relpath),
        sha256
    )
}

pub fn pointer(project_key: &str, relpath: &str) -> String {
    format!(
        "projects/{project_key}/files/{}/current.json",
        encode(relpath)
    )
}

pub fn manifest(project_key: &str) -> String {
    format!("projects/{project_key}/manifest.json")
}

pub fn files_prefix(project_key: &str) -> String {
    format!("projects/{project_key}/files/")
}

pub fn handoffs_prefix(project_key: &str) -> String {
    format!("projects/{project_key}/handoffs/")
}

pub fn handoff(project_key: &str, host: &str, name: &str) -> String {
    format!(
        "projects/{project_key}/handoffs/{}/{}.json",
        encode(host),
        encode(name)
    )
}

const POINTER_SUFFIX: &str = "/current.json";

/// The tracked path a pointer object stands for, or `None` if the key is not a
/// pointer of this project at all.
pub fn relpath_of_pointer(project_key: &str, key: &str) -> Option<String> {
    let prefix = files_prefix(project_key);
    let encoded = key
        .strip_prefix(prefix.as_str())?
        .strip_suffix(POINTER_SUFFIX)?;
    Some(decode(encoded))
}

/// The two halves of a handoff key: which host wrote it, and what it is called.
pub fn handoff_parts(project_key: &str, key: &str) -> Option<(String, String)> {
    let prefix = handoffs_prefix(project_key);
    let rest = key.strip_prefix(prefix.as_str())?;
    let (origin, name) = rest.split_once('/')?;
    if origin.is_empty() || name.is_empty() {
        return None;
    }
    Some((decode(origin), decode(name)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference client's spelling, character for character. A divergence
    /// here does not fail loudly; it silently splits one project into two.
    #[test]
    fn keys_match_the_reference_client() {
        assert_eq!(
            content("poc-infra", "infra/evidence/dump.json", "ab12"),
            "projects/poc-infra/files/infra%2Fevidence%2Fdump.json/ab12"
        );
        assert_eq!(
            pointer("poc-infra", "work-ledger.yaml"),
            "projects/poc-infra/files/work-ledger.yaml/current.json"
        );
        assert_eq!(manifest("poc-infra"), "projects/poc-infra/manifest.json");
        assert_eq!(
            handoff("poc-infra", "macbook-codex", "postgres-up"),
            "projects/poc-infra/handoffs/macbook-codex/postgres-up.json"
        );
        // A space, a plus and a non-ASCII byte all survive one round trip.
        let key = content("poc-infra", "infra/evidence/a b+c 图.png", "ff");
        assert_eq!(
            key,
            "projects/poc-infra/files/infra%2Fevidence%2Fa%20b%2Bc%20%E5%9B%BE.png/ff"
        );
        assert_eq!(
            relpath_of_pointer(
                "poc-infra",
                &pointer("poc-infra", "infra/evidence/a b+c 图.png")
            ),
            Some("infra/evidence/a b+c 图.png".to_string())
        );
    }

    #[test]
    fn a_pointer_key_from_another_project_is_not_ours() {
        assert_eq!(
            relpath_of_pointer("poc-infra", "projects/other/files/x/current.json"),
            None
        );
        assert_eq!(
            relpath_of_pointer("poc-infra", "projects/poc-infra/manifest.json"),
            None
        );
        // The content object of a path is not a pointer.
        assert_eq!(
            relpath_of_pointer("poc-infra", &content("poc-infra", "a.yaml", "ab")),
            None
        );
    }

    #[test]
    fn paths_that_leave_the_root_are_refused() {
        for unsafe_path in [
            "../escape.yaml",
            "infra/../../escape.yaml",
            "/etc/passwd",
            "infra//double.yaml",
            "infra/./same.yaml",
            "C:/windows/x",
            "infra\\windows.yaml",
            "",
            "trailing/",
            ".git/hooks/post-checkout",
            "infra/.git/HEAD",
            ".awr/workspace-credentials.json",
            "infra/.awr/state.db",
        ] {
            assert!(
                !is_safe_relpath(unsafe_path),
                "{unsafe_path:?} was accepted"
            );
        }
        for safe_path in [
            "work-ledger.yaml",
            "infra/evidence/dump.json",
            ".gitkeep",
            ".gitignore",
        ] {
            assert!(is_safe_relpath(safe_path), "{safe_path:?} was refused");
        }
        let error = format!("{}", check_relpath(".git/hooks/post-checkout").unwrap_err());
        assert!(error.contains("never leaves a machine"), "{error}");
        assert!(error.contains(".git/hooks/post-checkout"), "{error}");
    }

    #[test]
    fn a_project_key_that_could_change_a_key_shape_is_refused() {
        assert!(validate_project_key("poc-infra").is_ok());
        assert!(validate_project_key("team.a_b-2").is_ok());
        for key in ["", "a/b", "a%2Fb", "a b", "..", ".", "a\n"] {
            assert!(validate_project_key(key).is_err(), "{key:?} was accepted");
        }
    }

    #[test]
    fn a_component_that_is_really_a_path_is_refused() {
        assert!(check_component("mac-mini").is_ok());
        assert!(check_component("host-a").is_ok());
        assert!(check_component("macbook-codex").is_ok());
        assert!(check_component("postgres-up.json").is_ok());
        for component in ["", ".", "..", "a/b", "a\\b", "../escape", ".git", ".awr"] {
            assert!(
                check_component(component).is_err(),
                "{component:?} was accepted"
            );
        }
        let error = format!("{}", check_component(".git").unwrap_err());
        assert!(error.contains("never leaves a machine"), "{error}");
    }

    #[test]
    fn handoff_keys_split_back_into_their_parts() {
        let key = handoff("poc-infra", "mac-mini", "postgres 16 up");
        assert_eq!(
            handoff_parts("poc-infra", &key),
            Some(("mac-mini".to_string(), "postgres 16 up.json".to_string()))
        );
        assert_eq!(
            handoff_parts("poc-infra", "projects/poc-infra/files/x/current.json"),
            None
        );
    }
}
