use crate::error::{PgError, PgResult};

pub const MAX_SOURCE_FILES: usize = 64;
pub const MAX_FILE_BYTES: usize = 1024 * 1024;
pub const MAX_PACKAGE_BYTES: usize = 4 * 1024 * 1024;

/// Reject path traversal, absolute paths, NUL, and platform aliases before
/// any file bytes are persisted. This runs outside the project lock.
pub fn validate_source_path(path: &str) -> PgResult<()> {
    if path.is_empty() {
        return Err(PgError::UnsafeSourcePath("empty path".into()));
    }
    if path.contains('\0') {
        return Err(PgError::UnsafeSourcePath("NUL in path".into()));
    }
    if path.contains('\\') {
        return Err(PgError::UnsafeSourcePath(path.into()));
    }
    if path.starts_with('/') || path.starts_with("./") {
        return Err(PgError::UnsafeSourcePath(path.into()));
    }
    if path.chars().nth(1) == Some(':') {
        return Err(PgError::UnsafeSourcePath(path.into()));
    }
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(PgError::UnsafeSourcePath(path.into()));
        }
        if segment.ends_with(' ') || segment.ends_with('.') {
            return Err(PgError::UnsafeSourcePath(path.into()));
        }
    }
    Ok(())
}

pub fn validate_package(files: &[(String, Vec<u8>)]) -> PgResult<()> {
    if files.is_empty() {
        return Err(PgError::Protocol("source package is empty".into()));
    }
    if files.len() > MAX_SOURCE_FILES {
        return Err(PgError::UnsafeSourcePath(format!(
            "too many files: {}",
            files.len()
        )));
    }
    let mut total = 0usize;
    let mut seen = std::collections::BTreeSet::new();
    for (path, bytes) in files {
        validate_source_path(path)?;
        if bytes.len() > MAX_FILE_BYTES {
            return Err(PgError::UnsafeSourcePath(format!(
                "{path} exceeds {MAX_FILE_BYTES} bytes"
            )));
        }
        total = total.saturating_add(bytes.len());
        if total > MAX_PACKAGE_BYTES {
            return Err(PgError::UnsafeSourcePath(
                "package exceeds size limit".into(),
            ));
        }
        if !seen.insert(path.clone()) {
            return Err(PgError::UnsafeSourcePath(format!("duplicate path {path}")));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_file_is_allowed() {
        validate_source_path("work-ledger.yaml").unwrap();
        validate_source_path("rules/hard.md").unwrap();
    }

    #[test]
    fn traversal_and_absolute_paths_are_rejected() {
        for path in [
            "",
            "../secret",
            "foo/../bar",
            "/etc/passwd",
            "C:windows",
            "foo\\bar",
            "./hidden",
            "foo//bar",
            "foo/./bar",
            "foo/\0bar",
            "foo/bar.",
        ] {
            assert!(
                validate_source_path(path).is_err(),
                "should reject {path:?}"
            );
        }
    }
}
