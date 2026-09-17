//! The per-machine workspace config.
//!
//! Convention over configuration: a config only states what is genuinely
//! machine specific. Everything else has a measured or conventional default,
//! and an unknown key is an error rather than a silent default, because a
//! typo in a shared config must never change behaviour quietly.
use crate::credentials::DEFAULT_PATH as DEFAULT_CREDENTIALS;
use awr_core::{Error, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const DEFAULT_CONCURRENCY: usize = 8;
/// Measured: beyond this the extra sockets buy latency back in retries.
pub const DEFAULT_CONCURRENCY_CAP: usize = 32;
pub const DEFAULT_STATE: &str = ".awr/workspace.json";
pub const DEFAULT_REGION: &str = "auto";
pub const DEFAULT_HOSTS_SUFFIX: &str = ".aliyuncs.com";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Addressing {
    /// Bucket in the path unless the endpoint already names the bucket.
    #[default]
    Auto,
    /// Bucket in the path: R2, MinIO, S3.
    Path,
    /// Bucket in the host: required by Aliyun OSS public endpoints, which
    /// answer SecondLevelDomainForbidden to a path-style request.
    Virtual,
}

impl Addressing {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim() {
            "auto" => Ok(Self::Auto),
            "path" => Ok(Self::Path),
            "virtual" => Ok(Self::Virtual),
            other => Err(Error::InvalidInput(format!(
                "addressing must be auto, path or virtual, not {other:?}"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Path => "path",
            Self::Virtual => "virtual",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreConfig {
    /// `s3` for an object store, `local` for a directory. Inferred from which
    /// of `endpoint` and `path` the config sets.
    pub backend: String,
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub addressing: Addressing,
    pub prefix: String,
    pub concurrency: usize,
    /// When true, non-loopback `http://` endpoints are accepted. Default false.
    pub allow_insecure: bool,
    /// The directory a `local` store uses. Test suites and a single writer.
    pub path: Option<PathBuf>,
}

impl StoreConfig {
    /// Virtual-hosted addressing is what the endpoint itself asks for.
    pub fn virtual_hosted(&self) -> bool {
        match self.addressing {
            Addressing::Virtual => true,
            Addressing::Path => false,
            Addressing::Auto => self.host().starts_with(&format!("{}.", self.bucket)),
        }
    }

    pub fn host(&self) -> String {
        let rest = self
            .endpoint
            .split_once("://")
            .map(|(_scheme, rest)| rest)
            .unwrap_or(&self.endpoint);
        rest.trim_end_matches('/').to_string()
    }

    pub fn scheme(&self) -> &str {
        self.endpoint
            .split_once("://")
            .map(|(scheme, _rest)| scheme)
            .unwrap_or("https")
    }

    /// The Host header, and therefore the name that ends up signed.
    pub fn request_host(&self) -> String {
        if !self.virtual_hosted() {
            return self.host();
        }
        let host = self.host();
        if host.starts_with(&format!("{}.", self.bucket)) {
            host
        } else {
            format!("{}.{}", self.bucket, host)
        }
    }

    /// A store that speaks the S3 dialect minus conditional writes.
    pub fn is_oss(&self) -> bool {
        self.host().ends_with(DEFAULT_HOSTS_SUFFIX)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceConfig {
    pub project_key: String,
    pub root: PathBuf,
    pub host: String,
    pub track: Vec<String>,
    pub state: PathBuf,
    pub credentials: PathBuf,
    pub store: StoreConfig,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    project: Option<RawProject>,
    store: Option<RawStore>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProject {
    key: Option<String>,
    root: Option<String>,
    host: Option<String>,
    state: Option<String>,
    #[serde(default)]
    track: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStore {
    backend: Option<String>,
    endpoint: Option<String>,
    bucket: Option<String>,
    region: Option<String>,
    addressing: Option<String>,
    prefix: Option<String>,
    concurrency: Option<usize>,
    #[serde(default)]
    allow_insecure: bool,
    path: Option<String>,
    // Declared only so their use is refused with a message that says where
    // credentials do belong. A config is portable and often committed, so a key
    // in it is a finding, not a configuration style.
    ak: Option<String>,
    sk: Option<String>,
    access_key: Option<String>,
    secret_key: Option<String>,
    session_token: Option<String>,
}

impl RawStore {
    fn carries_credentials(&self) -> bool {
        [
            &self.ak,
            &self.sk,
            &self.access_key,
            &self.secret_key,
            &self.session_token,
        ]
        .iter()
        .any(|value| value.is_some())
    }
}

fn resolve(base: &Path, raw: &str) -> PathBuf {
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

fn required(value: Option<String>, field: &str, path: &Path) -> Result<String> {
    match value.map(|v| v.trim().to_string()) {
        Some(v) if !v.is_empty() => Ok(v),
        _ => Err(Error::InvalidInput(format!(
            "workspace config {} is missing {field}",
            path.display()
        ))),
    }
}

/// Read a config, filling in the defaults. `project_root` supplies the root
/// when the config leaves it out, so a machine only has to name its host.
pub fn load(path: &Path, project_root: &Path) -> Result<WorkspaceConfig> {
    let text = std::fs::read_to_string(path).map_err(|error| {
        Error::SourceUnavailable(format!("workspace config {}: {error}", path.display()))
    })?;
    parse(&text, path, project_root)
}

pub fn parse(text: &str, path: &Path, project_root: &Path) -> Result<WorkspaceConfig> {
    let raw: RawConfig = toml::from_str(text).map_err(|error| {
        Error::InvalidInput(format!("workspace config {}: {error}", path.display()))
    })?;
    let project = raw.project.unwrap_or(RawProject {
        key: None,
        root: None,
        host: None,
        state: None,
        track: Vec::new(),
    });
    let store = raw.store.ok_or_else(|| {
        Error::InvalidInput(format!(
            "workspace config {} is missing the [store] section",
            path.display()
        ))
    })?;
    if store.carries_credentials() {
        return Err(Error::InvalidInput(format!(
            "workspace config {} carries credentials, and this file is portable and may be \
             committed; store them per machine with `awr workspace credential set --stdin`, \
             or set AWR_WORKSPACE_ACCESS_KEY and AWR_WORKSPACE_SECRET_KEY",
            path.display()
        )));
    }

    let project_key = required(project.key, "project.key", path)?;
    let host = required(project.host, "project.host", path)?;
    crate::layout::check_component(&host).map_err(|_| {
        Error::InvalidInput(format!(
            "workspace config {} has an invalid project.host {host:?}",
            path.display()
        ))
    })?;
    let track: Vec<String> = project
        .track
        .into_iter()
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect();
    if track.is_empty() {
        return Err(Error::InvalidInput(format!(
            "workspace config {} is missing project.track",
            path.display()
        )));
    }

    let base = path.parent().unwrap_or_else(|| Path::new("."));
    // A hand-edited config routinely carries a stray space; an endpoint is a
    // URL, so whitespace is never part of it.
    let allow_insecure = store.allow_insecure;
    let endpoint = store
        .endpoint
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| normalize_endpoint(value, allow_insecure))
        .transpose()?;
    let directory = store
        .path
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| resolve(base, value));
    // The backend is inferred from which one the config named, so a machine
    // only writes down what it actually has.
    let backend = match store.backend.as_deref().map(str::trim) {
        Some(value) if !value.is_empty() => value.to_string(),
        _ if endpoint.is_some() => "s3".to_string(),
        _ if directory.is_some() => "local".to_string(),
        _ => {
            return Err(Error::InvalidInput(format!(
                "workspace config {} needs [store] endpoint, or path for a local store",
                path.display()
            )));
        }
    };
    if backend == "s3" && endpoint.is_none() {
        return Err(Error::InvalidInput(format!(
            "workspace config {}: an s3 store needs store.endpoint",
            path.display()
        )));
    }
    if backend == "local" && directory.is_none() {
        return Err(Error::InvalidInput(format!(
            "workspace config {}: a local store needs store.path",
            path.display()
        )));
    }
    if !matches!(backend.as_str(), "s3" | "local") {
        return Err(Error::InvalidInput(format!(
            "workspace config {}: store.backend must be s3 or local, not {backend:?}",
            path.display()
        )));
    }
    let bucket = match store
        .bucket
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        Some(bucket) => bucket.to_string(),
        None if backend == "s3" => {
            return Err(Error::InvalidInput(format!(
                "workspace config {} is missing store.bucket",
                path.display()
            )));
        }
        None => String::new(),
    };
    let region = store
        .region
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_REGION.to_string());
    let addressing = match store.addressing {
        Some(raw) => Addressing::parse(&raw)?,
        None => Addressing::Auto,
    };
    let concurrency = store
        .concurrency
        .unwrap_or(DEFAULT_CONCURRENCY)
        .clamp(1, DEFAULT_CONCURRENCY_CAP);

    let root = match project.root.as_deref().map(str::trim) {
        Some(raw) if !raw.is_empty() => resolve(base, raw),
        _ => project_root.to_path_buf(),
    };
    let state = match project.state.as_deref().map(str::trim) {
        Some(raw) if !raw.is_empty() => resolve(base, raw),
        _ => root.join(DEFAULT_STATE),
    };

    // Credentials hang off the project the caller is in, not off the workspace
    // root, so they can be managed before a config exists and cannot move when
    // a config points the workspace somewhere else.
    let credentials = project_root.join(DEFAULT_CREDENTIALS);
    Ok(WorkspaceConfig {
        project_key,
        root,
        host,
        track,
        state,
        credentials,
        store: StoreConfig {
            backend,
            endpoint: endpoint.unwrap_or_else(|| "http://localhost".to_string()),
            bucket,
            region,
            addressing,
            prefix: store
                .prefix
                .unwrap_or_default()
                .trim_matches('/')
                .to_string(),
            concurrency,
            allow_insecure,
            path: directory,
        },
    })
}

fn normalize_endpoint(raw: &str, allow_insecure: bool) -> Result<String> {
    let candidate = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("https://{raw}")
    };
    let parsed = url::Url::parse(&candidate).map_err(|error| {
        Error::InvalidInput(format!(
            "workspace store.endpoint is not a valid URL: {error}"
        ))
    })?;
    match parsed.scheme() {
        "https" => {}
        "http" => {
            let host = parsed.host_str().unwrap_or("");
            let loopback = matches!(host, "localhost" | "127.0.0.1" | "::1");
            if !(allow_insecure || loopback) {
                return Err(Error::InvalidInput(
                    "workspace store.endpoint must use https; set store.allow_insecure = true only for a deliberate cleartext lab endpoint (loopback http is allowed)"
                        .into(),
                ));
            }
        }
        other => {
            return Err(Error::InvalidInput(format!(
                "workspace store.endpoint scheme must be https (or http for loopback/lab), not {other:?}"
            )));
        }
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(Error::InvalidInput(
            "workspace store.endpoint must not carry userinfo; credentials belong in              `awr workspace credential set`"
                .into(),
        ));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(Error::InvalidInput(
            "workspace store.endpoint must be an origin only (no query or fragment)".into(),
        ));
    }
    let path = parsed.path();
    if path != "/" && !path.is_empty() {
        return Err(Error::InvalidInput(
            "workspace store.endpoint must be an origin only (no path); put a shared prefix in store.prefix"
                .into(),
        ));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| Error::InvalidInput("workspace store.endpoint is missing a host".into()))?;
    let mut out = format!("{}://{host}", parsed.scheme());
    if let Some(port) = parsed.port() {
        out.push(':');
        out.push_str(&port.to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("awr workspace config {}", awr_core::Id::new()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn config_path(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("remote_workspace.toml");
        std::fs::write(&path, body).unwrap();
        path
    }

    const MINIMAL: &str = r#"
[project]
key  = "poc-infra"
host = "macbook-codex"
track = ["work-ledger.yaml", "infra"]

[store]
endpoint = "oss-cn-beijing.aliyuncs.com"
bucket   = "awr-workspace-project"
"#;

    #[test]
    fn only_the_machine_specific_fields_are_required() {
        let dir = scratch();
        let root = dir.join("project");
        let path = config_path(&dir, MINIMAL);
        let config = load(&path, &root).unwrap();
        assert_eq!(config.project_key, "poc-infra");
        assert_eq!(config.host, "macbook-codex");
        assert_eq!(config.track, vec!["work-ledger.yaml", "infra"]);
        // root defaults to the project the caller is already working in, so a
        // machine only has to name its host.
        assert_eq!(config.root, root);
        assert_eq!(config.state, root.join(DEFAULT_STATE));
        assert_eq!(config.credentials, root.join(DEFAULT_CREDENTIALS));
        assert_eq!(config.store.region, DEFAULT_REGION);
        assert_eq!(config.store.addressing, Addressing::Auto);
        assert_eq!(config.store.concurrency, DEFAULT_CONCURRENCY);
        assert_eq!(config.store.prefix, "");
    }

    #[test]
    fn the_scheme_is_supplied_and_stray_whitespace_is_not_a_host() {
        let dir = scratch();
        // A hand-edited config routinely carries a stray space, and an endpoint
        // is a URL: whitespace is never part of it.
        let body = MINIMAL.replace(
            "oss-cn-beijing.aliyuncs.com",
            "  oss-cn-beijing.aliyuncs.com ",
        );
        let config = load(&config_path(&dir, &body), &dir).unwrap();
        assert_eq!(config.store.endpoint, "https://oss-cn-beijing.aliyuncs.com");
        assert_eq!(config.store.host(), "oss-cn-beijing.aliyuncs.com");
        assert!(config.store.is_oss());
    }

    #[test]
    fn oss_takes_the_bucket_in_the_host_and_r2_in_the_path() {
        let dir = scratch();
        let oss = load(&config_path(&dir, MINIMAL), &dir).unwrap().store;
        // Auto reads the style off the endpoint: a bare region endpoint is
        // path-style, and OSS refuses that, so `virtual` has to be written out.
        assert!(!oss.virtual_hosted());
        let explicit = MINIMAL.replace(
            "bucket   = \"awr-workspace-project\"",
            "bucket     = \"awr-workspace-project\"\naddressing = \"virtual\"",
        );
        let store = load(&config_path(&dir, &explicit), &dir).unwrap().store;
        assert!(store.virtual_hosted());
        assert_eq!(
            store.request_host(),
            "awr-workspace-project.oss-cn-beijing.aliyuncs.com"
        );

        // Pasting the bucket domain into the endpoint is the other way to say
        // the same thing, and auto detects it.
        let pasted = MINIMAL.replace(
            "oss-cn-beijing.aliyuncs.com",
            "awr-workspace-project.oss-cn-beijing.aliyuncs.com",
        );
        let store = load(&config_path(&dir, &pasted), &dir).unwrap().store;
        assert!(store.virtual_hosted());
        assert_eq!(
            store.request_host(),
            "awr-workspace-project.oss-cn-beijing.aliyuncs.com"
        );

        let r2 = load(
            &config_path(
                &dir,
                &MINIMAL.replace(
                    "oss-cn-beijing.aliyuncs.com",
                    "074500c63a44cd12e391fe75601335d2.r2.cloudflarestorage.com",
                ),
            ),
            &dir,
        )
        .unwrap()
        .store;
        assert!(!r2.virtual_hosted(), "R2 keeps the bucket in the path");
        assert_eq!(
            r2.request_host(),
            "074500c63a44cd12e391fe75601335d2.r2.cloudflarestorage.com"
        );
        assert!(!r2.is_oss());
    }

    #[test]
    fn a_typo_is_an_error_rather_than_a_silent_default() {
        let dir = scratch();
        let body = MINIMAL.replace("endpoint", "endpont");
        let error = load(&config_path(&dir, &body), &dir).unwrap_err();
        assert!(
            matches!(error, Error::InvalidInput(_)),
            "unexpected error: {error:?}"
        );
    }

    #[test]
    fn credentials_in_the_config_are_refused_with_where_to_put_them() {
        let dir = scratch();
        let body = MINIMAL.replace(
            "bucket   = \"awr-workspace-project\"",
            "bucket = \"awr-workspace-project\"\nak     = \"synthetic-access-key\"",
        );
        let error = format!("{}", load(&config_path(&dir, &body), &dir).unwrap_err());
        assert!(error.contains("awr workspace credential set"), "{error}");
        assert!(!error.contains("synthetic-access-key"), "{error}");
    }

    #[test]
    fn required_fields_are_named_when_missing() {
        let dir = scratch();
        let body = MINIMAL.replace("host = \"macbook-codex\"", "");
        let error = load(&config_path(&dir, &body), &dir).unwrap_err();
        assert!(format!("{error}").contains("project.host"), "{error}");

        let body = MINIMAL.replace("track = [\"work-ledger.yaml\", \"infra\"]", "track = []");
        let error = load(&config_path(&dir, &body), &dir).unwrap_err();
        assert!(format!("{error}").contains("project.track"), "{error}");

        let error = load(&config_path(&dir, "[project]\nkey=\"k\"\n"), &dir).unwrap_err();
        assert!(format!("{error}").contains("[store]"), "{error}");
    }

    #[test]
    fn an_invalid_project_host_is_refused() {
        let dir = scratch();
        for host in ["a/b", "../x", ".git", ".awr", "..", ""] {
            let body = MINIMAL.replace("host = \"macbook-codex\"", &format!("host = \"{host}\""));
            let error = load(&config_path(&dir, &body), &dir).unwrap_err();
            assert!(
                matches!(error, Error::InvalidInput(_)),
                "{host:?} produced {error:?}"
            );
            let message = format!("{error}");
            assert!(message.contains("project.host"), "{host:?}: {message}");
        }
        for host in ["macbook-codex", "host-a", "mac-mini"] {
            let body = MINIMAL.replace("host = \"macbook-codex\"", &format!("host = \"{host}\""));
            let config = load(&config_path(&dir, &body), &dir).unwrap();
            assert_eq!(config.host, host);
        }
    }

    #[test]
    fn an_unknown_addressing_mode_is_refused() {
        assert_eq!(Addressing::parse("virtual").unwrap(), Addressing::Virtual);
        assert!(Addressing::parse("Virtualy").is_err());
        let dir = scratch();
        let body = MINIMAL.replace(
            "bucket   = \"awr-workspace-project\"",
            "bucket     = \"awr-workspace-project\"\naddressing = \"vurtual\"",
        );
        assert!(load(&config_path(&dir, &body), &dir).is_err());
    }

    #[test]
    fn concurrency_is_bounded_and_relative_paths_resolve_against_the_config() {
        let dir = scratch();
        let body = MINIMAL
            .replace(
                "bucket   = \"awr-workspace-project\"",
                "bucket      = \"awr-workspace-project\"\nconcurrency = 4096",
            )
            .replace(
                "host = \"macbook-codex\"",
                "host  = \"macbook-codex\"\nstate = \"local/workspace.json\"",
            );
        let config = load(&config_path(&dir, &body), &dir).unwrap();
        assert_eq!(config.store.concurrency, DEFAULT_CONCURRENCY_CAP);
        assert_eq!(config.state, dir.join("local/workspace.json"));

        let body = MINIMAL.replace(
            "bucket   = \"awr-workspace-project\"",
            "bucket      = \"awr-workspace-project\"\nconcurrency = 0",
        );
        assert_eq!(
            load(&config_path(&dir, &body), &dir)
                .unwrap()
                .store
                .concurrency,
            1
        );
    }
    #[test]
    fn http_endpoints_outside_loopback_are_refused() {
        let dir = scratch();
        let body = MINIMAL.replace(
            "oss-cn-beijing.aliyuncs.com",
            "http://oss-cn-beijing.aliyuncs.com",
        );
        let error = load(&config_path(&dir, &body), &dir)
            .unwrap_err()
            .to_string();
        assert!(error.contains("https"), "{error}");
    }

    #[test]
    fn loopback_http_is_allowed_without_a_flag() {
        let dir = scratch();
        let body = MINIMAL.replace("oss-cn-beijing.aliyuncs.com", "http://127.0.0.1:9000");
        let config = load(&config_path(&dir, &body), &dir).unwrap();
        assert_eq!(config.store.endpoint, "http://127.0.0.1:9000");
        assert_eq!(config.store.scheme(), "http");
    }

    #[test]
    fn endpoint_path_and_userinfo_are_refused() {
        let dir = scratch();
        let with_path =
            MINIMAL.replace("oss-cn-beijing.aliyuncs.com", "https://example.com/bucket");
        let error = load(&config_path(&dir, &with_path), &dir)
            .unwrap_err()
            .to_string();
        assert!(error.contains("origin"), "{error}");
        let with_user = MINIMAL.replace(
            "oss-cn-beijing.aliyuncs.com",
            "https://user:pass@example.com",
        );
        let error = load(&config_path(&dir, &with_user), &dir)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("userinfo") || error.contains("credentials"),
            "{error}"
        );
    }
}
