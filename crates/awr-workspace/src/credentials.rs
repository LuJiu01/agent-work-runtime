//! Where the store credentials live, and where they must not live.
//!
//! Not in the config file: that one is portable, shared and may be committed.
//! Not on the command line: arguments are copied into shell history and are
//! visible in process listings, and this repository already refuses sensitive
//! command arguments outright. They live in a file this module creates with
//! owner-only permissions, and nothing here ever renders a value.
use awr_core::{Error, Result};
use serde::{Deserialize, Serialize};
use std::{fmt, path::Path};

/// Relative to the project root, next to AWR's own local state.
pub const DEFAULT_PATH: &str = ".awr/workspace-credentials.json";

#[derive(Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct Credentials {
    pub access_key: String,
    pub secret_key: String,
    /// Temporary credentials only; the header carrying it is part of the signature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_token: Option<String>,
}

impl Credentials {
    pub fn is_empty(&self) -> bool {
        self.access_key.is_empty() && self.secret_key.is_empty()
    }

    pub fn session_token(&self) -> Option<&str> {
        self.session_token
            .as_deref()
            .filter(|token| !token.is_empty())
    }

    pub fn missing_fields(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if self.access_key.is_empty() {
            missing.push("access_key");
        }
        if self.secret_key.is_empty() {
            missing.push("secret_key");
        }
        missing
    }
}

fn redact(value: &str) -> String {
    if value.is_empty() {
        "<empty>".to_string()
    } else {
        format!("<{} chars>", value.chars().count())
    }
}

/// Debug is shared by every error path, so it redacts by construction.
impl fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Credentials")
            .field("access_key", &redact(&self.access_key))
            .field("secret_key", &redact(&self.secret_key))
            .field("session_token", &self.session_token().map(redact))
            .finish()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CredentialStatus {
    pub path: String,
    pub present: bool,
    pub complete: bool,
    pub access_key: bool,
    pub secret_key: bool,
    pub session_token: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

#[cfg(unix)]
fn mode_of(path: &Path) -> Result<u32> {
    use std::os::unix::fs::PermissionsExt;
    Ok(std::fs::metadata(path)?.permissions().mode() & 0o777)
}

/// Owner-only, checked rather than assumed: a credential file that other users
/// can read is a finding, not a detail to print and move past.
#[cfg(unix)]
fn enforce_owner_only(path: &Path) -> Result<()> {
    let mode = mode_of(path)?;
    if mode & 0o077 != 0 {
        return Err(Error::RuleViolation(format!(
            "workspace credentials {} are readable beyond the owner (mode {mode:03o}); \
             rerun `awr workspace credential set` or chmod 600",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn enforce_owner_only(_path: &Path) -> Result<()> {
    Ok(())
}

pub fn save(path: &Path, credentials: &Credentials) -> Result<()> {
    let missing = credentials.missing_fields();
    if !missing.is_empty() {
        return Err(Error::InvalidInput(format!(
            "credentials require {}",
            missing.join(" and ")
        )));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(credentials)?;
    // Write through a sibling temp file and rename so a crash mid-write cannot
    // leave a truncated credentials file that later reads as JSON garbage.
    let scratch = path.with_extension("json.tmp");
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&scratch)?;
        file.write_all(&body)?;
        file.sync_all()?;
        std::fs::set_permissions(&scratch, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(&scratch, path)?;
        // A pre-existing destination keeps its mode across some rename cases.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        // Windows does not expose a portable 0600 equivalent here. The file
        // lives under `.awr/`, and operators must keep that directory
        // owner-only via NTFS ACL; this write path still uses temp+rename so a
        // crash cannot leave truncated JSON.
        std::fs::write(&scratch, &body)?;
        std::fs::rename(&scratch, path)?;
    }
    Ok(())
}

pub fn load(path: &Path) -> Result<Option<Credentials>> {
    if !path.exists() {
        return Ok(None);
    }
    enforce_owner_only(path)?;
    let body = std::fs::read(path)?;
    let credentials: Credentials = serde_json::from_slice(&body).map_err(|error| {
        Error::InvalidInput(format!("workspace credentials {}: {error}", path.display()))
    })?;
    Ok(Some(credentials))
}

pub fn status(path: &Path) -> Result<CredentialStatus> {
    let present = path.exists();
    let credentials = if present {
        let body = std::fs::read(path)?;
        serde_json::from_slice::<Credentials>(&body).ok()
    } else {
        None
    };
    let credentials = credentials.unwrap_or_default();
    #[cfg(unix)]
    let mode = if present {
        mode_of(path).ok().map(|mode| format!("{mode:03o}"))
    } else {
        None
    };
    #[cfg(not(unix))]
    let mode = None;
    Ok(CredentialStatus {
        path: path.display().to_string(),
        present,
        complete: !credentials.is_empty() && credentials.missing_fields().is_empty(),
        access_key: !credentials.access_key.is_empty(),
        secret_key: !credentials.secret_key.is_empty(),
        session_token: credentials.session_token().is_some(),
        mode,
    })
}

pub fn clear(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    std::fs::remove_file(path)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("awr workspace creds {}", awr_core::Id::new()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(".awr/workspace-credentials.json")
    }

    fn sample() -> Credentials {
        Credentials {
            access_key: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_key: "synthetic-secret-not-a-key".into(),
            session_token: Some("temporary-token".into()),
        }
    }

    #[test]
    fn a_saved_credential_round_trips_with_owner_only_permissions() {
        let path = scratch();
        save(&path, &sample()).unwrap();
        let loaded = load(&path).unwrap().unwrap();
        assert_eq!(loaded, sample());
        assert_eq!(loaded.session_token(), Some("temporary-token"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "credentials must not be readable by others");
        }
    }

    #[test]
    fn resolving_and_updating_an_existing_file_keeps_it_owner_only() {
        let path = scratch();
        save(&path, &sample()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // A file that already exists keeps its own mode, so the writer has
            // to set it rather than rely on create-time permissions.
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        save(&path, &sample()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[cfg(unix)]
    #[test]
    fn credentials_readable_beyond_the_owner_are_refused_not_used() {
        use std::os::unix::fs::PermissionsExt;
        let path = scratch();
        save(&path, &sample()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = load(&path).unwrap_err();
        assert!(matches!(error, Error::RuleViolation(_)), "{error:?}");
        assert!(format!("{error}").contains("600"), "{error}");
    }

    #[test]
    fn nothing_renderable_ever_carries_a_value() {
        let credentials = sample();
        let debug = format!("{credentials:?}");
        assert!(!debug.contains("AKIAIOSFODNN"), "{debug}");
        assert!(!debug.contains("synthetic-secret"), "{debug}");
        assert!(!debug.contains("temporary-token"), "{debug}");
        // Every field is replaced by its length, so the shape is inspectable and
        // the value is not - whatever the value happens to be.
        assert!(
            debug.contains(&format!(
                "<{} chars>",
                credentials.access_key.chars().count()
            )),
            "{debug}"
        );
        assert!(debug.contains("<15 chars>"), "{debug}");

        let path = scratch();
        save(&path, &credentials).unwrap();
        let status = status(&path).unwrap();
        let rendered = serde_json::to_string(&status).unwrap();
        assert!(!rendered.contains("AKIAIOSFODNN"), "{rendered}");
        assert!(!rendered.contains("synthetic-secret"), "{rendered}");
        assert!(status.present && status.complete);
        assert!(status.access_key && status.secret_key && status.session_token);
    }

    #[test]
    fn an_absent_file_is_not_an_error_and_clears_cleanly() {
        let path = scratch();
        assert!(load(&path).unwrap().is_none());
        let status = status(&path).unwrap();
        assert!(!status.present && !status.complete);
        assert!(!clear(&path).unwrap());
        save(&path, &sample()).unwrap();
        assert!(clear(&path).unwrap());
        assert!(!path.exists());
    }

    #[test]
    fn a_long_lived_key_without_a_session_token_is_complete() {
        let path = scratch();
        let long_lived = Credentials {
            session_token: None,
            ..sample()
        };
        save(&path, &long_lived).unwrap();
        let status = status(&path).unwrap();
        assert!(status.complete);
        assert!(!status.session_token);
    }

    #[test]
    fn a_half_written_credential_is_refused() {
        let path = scratch();
        let partial = Credentials {
            access_key: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_key: String::new(),
            session_token: None,
        };
        let error = save(&path, &partial).unwrap_err();
        assert!(format!("{error}").contains("secret_key"), "{error}");
        assert!(
            !path.exists(),
            "a rejected credential is not written at all"
        );
    }
}
