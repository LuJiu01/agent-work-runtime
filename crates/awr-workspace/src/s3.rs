//! An S3-compatible object store: R2, S3, OSS and MinIO are one implementation.
//!
//! Two provider differences are real and measured, so they are handled here and
//! nowhere else:
//!
//! - Aliyun OSS refuses path-style requests on its public endpoints, so the
//!   bucket has to be in the host. That is what `addressing` decides.
//! - Aliyun OSS `PutObject` has no `If-Match`/`If-None-Match`: it answers
//!   `NotImplemented` to both, and silently ignores `x-oss-if-match`. It has its
//!   own create-only spelling (`x-oss-forbid-overwrite`) and no compare and
//!   swap at all, which is why [`Backend::supports_if_match`] exists and why the
//!   commit path has a second strategy above this layer.
use crate::backend::{Backend, Listed, ObjectMeta, Precondition, PutOutcome};
use crate::config::StoreConfig;
use crate::credentials::Credentials;
use crate::sigv4;
use awr_core::{Error, Result};
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use ureq::http::Request;

/// A single object is read into memory, so its size is bounded. Content objects
/// for tracked sources and evidence sit far below this; a bigger one is a
/// mistake to surface rather than a file to stream.
pub use crate::MAX_OBJECT_BYTES;
/// A store outside the region answers in about a second; the timeout is what
/// stops one slow object from stalling a bounded fan-out for minutes.
const TIMEOUT: Duration = Duration::from_secs(60);

pub struct S3Store {
    config: StoreConfig,
    credentials: Credentials,
    agent: ureq::Agent,
    requests: AtomicU64,
}

struct Reply {
    status: u16,
    etag: Option<String>,
    content_length: Option<u64>,
    body: Vec<u8>,
}

impl S3Store {
    pub fn new(config: StoreConfig, credentials: Credentials) -> Result<Self> {
        if !credentials.missing_fields().is_empty() {
            return Err(Error::InvalidInput(format!(
                "the {} store needs credentials; run `awr workspace credential set --stdin`",
                config.host()
            )));
        }
        let agent = ureq::Agent::config_builder()
            // A 404, 409 or 412 is this layer's control flow, not a failure.
            .http_status_as_error(false)
            .timeout_global(Some(TIMEOUT))
            .user_agent(concat!("awr/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Ok(Self {
            config,
            credentials,
            agent,
            requests: AtomicU64::new(0),
        })
    }

    /// The path of an object key, encoded exactly once and never normalised.
    fn object_path(&self, key: &str) -> String {
        let mut parts = Vec::new();
        if !self.config.virtual_hosted() {
            parts.push(self.config.bucket.clone());
        }
        if !self.config.prefix.is_empty() {
            parts.push(self.config.prefix.clone());
        }
        parts.push(key.to_string());
        format!(
            "/{}",
            parts
                .iter()
                .map(|part| crate::layout::encode(part))
                .collect::<Vec<_>>()
                .join("/")
        )
    }

    fn bucket_path(&self) -> String {
        if self.config.virtual_hosted() {
            "/".to_string()
        } else {
            format!("/{}", self.config.bucket)
        }
    }

    fn send(
        &self,
        method: &str,
        path: &str,
        query: &str,
        body: &[u8],
        extra: &[(&str, String)],
    ) -> Result<Reply> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        let amz_date = amz_date()?;
        let payload_sha = sigv4::hex_sha256(body);
        let host = self.config.request_host();
        let mut headers: Vec<(String, String)> = vec![
            ("Host".to_string(), host.clone()),
            ("x-amz-content-sha256".to_string(), payload_sha.clone()),
            ("x-amz-date".to_string(), amz_date.clone()),
        ];
        if let Some(token) = self.credentials.session_token() {
            headers.push(("x-amz-security-token".to_string(), token.to_string()));
        }
        for (name, value) in extra {
            headers.push((name.to_string(), value.clone()));
        }
        let canonical = sigv4::canonical_request(method, path, query, &headers, &payload_sha);
        let (_block, signed) = sigv4::canonical_headers(&headers);
        let (_to_sign, authorization) = sigv4::authorization(
            &self.credentials.access_key,
            &self.credentials.secret_key,
            &self.config.region,
            "s3",
            &amz_date,
            &canonical,
            &signed,
        );
        let url = format!(
            "{}://{}{}{}",
            self.config.scheme(),
            host,
            path,
            if query.is_empty() {
                String::new()
            } else {
                format!("?{query}")
            }
        );
        let mut builder = Request::builder().method(method).uri(url.as_str());
        for (name, value) in &headers {
            // The Host header is the URI's job; sending it twice is worse than
            // not sending it at all.
            if !name.eq_ignore_ascii_case("host") {
                builder = builder.header(name, value);
            }
        }
        let request = builder
            .header("Authorization", authorization)
            .body(body)
            .map_err(|error| Error::SourceUnavailable(format!("workspace request: {error}")))?;
        let response = self
            .agent
            .run(request)
            .map_err(|error| Error::SourceUnavailable(format!("workspace store: {error}")))?;
        let status = response.status().as_u16();
        let etag = header(&response, "etag");
        let content_length =
            header(&response, "content-length").and_then(|value| value.parse::<u64>().ok());
        let mut body = response.into_body();
        let body = body
            .with_config()
            .limit(MAX_OBJECT_BYTES)
            .read_to_vec()
            .map_err(|error| {
                Error::SourceUnavailable(format!(
                    "workspace store response is not readable: {error}"
                ))
            })?;
        Ok(Reply {
            status,
            etag,
            content_length,
            body,
        })
    }

    fn put_with(
        &self,
        key: &str,
        body: &[u8],
        if_none: bool,
        if_match: Option<&str>,
    ) -> Result<PutOutcome> {
        let mut extra: Vec<(&str, String)> = Vec::new();
        if if_none {
            // The same create-only guarantee in the store's own spelling.
            if self.config.is_oss() {
                extra.push(("x-oss-forbid-overwrite", "true".to_string()));
            } else {
                extra.push(("If-None-Match", "*".to_string()));
            }
        }
        if let Some(etag) = if_match {
            if !self.supports_if_match() {
                return Err(Error::Unsupported(format!(
                    "{} cannot compare and swap an existing object, so a workspace there cannot \
                     commit safely",
                    self.config.host()
                )));
            }
            extra.push(("If-Match", etag.to_string()));
        }
        let reply = self.send("PUT", &self.object_path(key), "", body, &extra)?;
        match reply.status {
            200 | 201 => Ok(PutOutcome {
                created: true,
                etag: reply.etag,
            }),
            // 412 is the S3 answer to a failed precondition; OSS answers 409 to
            // its own create-only header.
            409 | 412 => Ok(PutOutcome {
                created: false,
                etag: reply.etag,
            }),
            status => Err(Error::SourceUnavailable(format!(
                "workspace store refused PUT {key}: HTTP {status} {}",
                first_line(&reply.body)
            ))),
        }
    }

    fn list_page(
        &self,
        prefix: &str,
        token: Option<&str>,
    ) -> Result<(Vec<Listed>, Option<String>)> {
        let mut pairs = vec![("list-type", "2"), ("prefix", prefix)];
        if let Some(token) = token {
            pairs.push(("continuation-token", token));
        }
        pairs.sort();
        let query = sigv4::canonical_query(
            &pairs
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>()
                .join("&"),
        );
        let reply = self.send("GET", &self.bucket_path(), &query, b"", &[])?;
        if reply.status != 200 {
            return Err(Error::SourceUnavailable(format!(
                "workspace store refused LIST: HTTP {} {}",
                reply.status,
                first_line(&reply.body)
            )));
        }
        let xml = String::from_utf8_lossy(&reply.body).into_owned();
        let mut listed = Vec::new();
        for block in blocks(&xml, "Contents") {
            let Some(key) = text_of(block, "Key") else {
                continue;
            };
            listed.push(Listed {
                key: unescape(&key),
                // Quotes are HTTP's, not the etag's; the reference client
                // compares the bare value here.
                etag: unescape(&text_of(block, "ETag").unwrap_or_default())
                    .trim_matches('"')
                    .to_string(),
                size: text_of(block, "Size")
                    .and_then(|value| value.trim().parse::<u64>().ok())
                    .unwrap_or(0),
            });
        }
        let next = (text_of(&xml, "IsTruncated").as_deref() == Some("true"))
            .then(|| text_of(&xml, "NextContinuationToken"))
            .flatten()
            .filter(|token| !token.is_empty());
        Ok((listed, next))
    }
}

impl Backend for S3Store {
    fn describe(&self) -> String {
        format!("s3 {}", self.config.host())
    }

    fn supports_if_match(&self) -> bool {
        !self.config.is_oss()
    }

    fn requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    fn head(&self, key: &str) -> Result<Option<ObjectMeta>> {
        let reply = self.send("HEAD", &self.object_path(key), "", b"", &[])?;
        match reply.status {
            200 => Ok(Some(ObjectMeta {
                etag: reply.etag.ok_or_else(|| self.no_etag(key))?,
                size: reply.content_length.unwrap_or(0),
            })),
            404 => Ok(None),
            status => Err(Error::SourceUnavailable(format!(
                "workspace store refused HEAD {key}: HTTP {status}"
            ))),
        }
    }

    fn get_meta(&self, key: &str) -> Result<Option<(Vec<u8>, String)>> {
        let reply = self.send("GET", &self.object_path(key), "", b"", &[])?;
        match reply.status {
            200 => Ok(Some((
                reply.body,
                reply.etag.ok_or_else(|| self.no_etag(key))?,
            ))),
            404 => Ok(None),
            status => Err(Error::SourceUnavailable(format!(
                "workspace store refused GET {key}: HTTP {status} {}",
                first_line(&reply.body)
            ))),
        }
    }

    fn put(&self, key: &str, body: &[u8], precondition: Precondition) -> Result<PutOutcome> {
        if body.len() as u64 > MAX_OBJECT_BYTES {
            return Err(Error::InvalidInput(format!(
                "workspace object {key} is {} bytes; maximum is {MAX_OBJECT_BYTES}",
                body.len()
            )));
        }
        match precondition {
            Precondition::None => self.put_with(key, body, false, None),
            Precondition::Absent => self.put_with(key, body, true, None),
            Precondition::Match(etag) => self.put_with(key, body, false, Some(&etag)),
        }
    }

    fn list(&self, prefix: &str) -> Result<Vec<Listed>> {
        // The store's prefix is a path, and this client's keys keep every
        // project path inside one segment, so the prefix is used verbatim.
        let full = if self.config.prefix.is_empty() {
            prefix.to_string()
        } else {
            format!("{}/{prefix}", self.config.prefix)
        };
        let mut out = Vec::new();
        let mut token = None;
        loop {
            let (mut page, next) = self.list_page(&full, token.as_deref())?;
            out.append(&mut page);
            match next {
                Some(next) => token = Some(next),
                None => break,
            }
        }
        out.sort_by(|left, right| left.key.cmp(&right.key));
        Ok(out)
    }

    fn delete(&self, key: &str) -> Result<bool> {
        let reply = self.send("DELETE", &self.object_path(key), "", b"", &[])?;
        match reply.status {
            200 | 204 => Ok(true),
            404 => Ok(false),
            status => Err(Error::SourceUnavailable(format!(
                "workspace store refused DELETE {key}: HTTP {status} {}",
                first_line(&reply.body)
            ))),
        }
    }
}

impl S3Store {
    fn no_etag(&self, key: &str) -> Error {
        Error::SourceUnavailable(format!(
            "workspace store returned no ETag for {key}; without one a commit cannot tell its \
             own write from a peer's"
        ))
    }
}

fn header(response: &ureq::http::Response<ureq::Body>, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn first_line(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    line.chars().take(200).collect()
}

/// `YYYYMMDDTHHMMSSZ`, the only timestamp the signing protocol accepts.
fn amz_date() -> Result<String> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| Error::InvalidInput(error.to_string()))?
        .as_secs();
    let (year, month, day, hour, minute, second) = utc_parts(seconds);
    Ok(format!(
        "{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z"
    ))
}

/// Days to civil date, by Howard Hinnant's `civil_from_days`.
fn utc_parts(seconds: u64) -> (i64, u32, u32, u32, u32, u32) {
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (
        year,
        month,
        day,
        (rest / 3_600) as u32,
        ((rest % 3_600) / 60) as u32,
        (rest % 60) as u32,
    )
}

// --------------------------------------------------------------------------
// The little of XML a `ListObjectsV2` response uses.
//
// A dependency-free reader for a fixed, machine-generated shape: elements, no
// attributes that matter, no repeated names inside one element. It is written
// to refuse rather than to guess - a tag it cannot match ends the scan instead
// of returning half a key.
// --------------------------------------------------------------------------
fn open_tag(xml: &str, name: &str, from: usize) -> Option<usize> {
    let mut index = from;
    while let Some(offset) = xml[index..].find('<') {
        let start = index + offset;
        let rest = &xml[start + 1..];
        let end = rest.find(|c: char| c == '>' || c == '/' || c.is_whitespace())?;
        let local = rest[..end].rsplit(':').next().unwrap_or(&rest[..end]);
        if local == name && rest.as_bytes().get(end) == Some(&b'>') {
            return Some(start + end + 2);
        }
        index = start + 1;
    }
    None
}

fn close_tag(xml: &str, name: &str, from: usize) -> Option<usize> {
    let mut index = from;
    while let Some(offset) = xml[index..].find("</") {
        let start = index + offset;
        let rest = &xml[start + 2..];
        let end = rest.find('>')?;
        let local = rest[..end].rsplit(':').next().unwrap_or(&rest[..end]);
        if local == name {
            return Some(start);
        }
        index = start + 2;
    }
    None
}

fn blocks<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(text_start) = open_tag(xml, name, from) {
        let Some(end) = close_tag(xml, name, text_start) else {
            break;
        };
        out.push(&xml[text_start..end]);
        from = end;
    }
    out
}

fn text_of(block: &str, name: &str) -> Option<String> {
    let start = open_tag(block, name, 0)?;
    let end = close_tag(block, name, start)?;
    (end >= start).then(|| block[start..end].to_string())
}

fn unescape(value: &str) -> String {
    if !value.contains('&') {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(index) = rest.find('&') {
        out.push_str(&rest[..index]);
        let tail = &rest[index..];
        let Some(end) = tail.find(';') else {
            out.push_str(tail);
            return out;
        };
        let entity = &tail[1..end];
        match entity {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            _ => match entity.strip_prefix('#') {
                Some(code) => match u32::from_str_radix(
                    code.trim_start_matches(['x', 'X']),
                    if code.starts_with(['x', 'X']) { 16 } else { 10 },
                )
                .ok()
                .and_then(char::from_u32)
                {
                    Some(character) => out.push(character),
                    None => {
                        out.push_str(&tail[..end + 1]);
                    }
                },
                None => out.push_str(&tail[..end + 1]),
            },
        }
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Addressing;

    fn store(endpoint: &str, addressing: Addressing) -> S3Store {
        S3Store::new(
            StoreConfig {
                backend: "s3".to_string(),
                allow_insecure: false,
                path: None,
                endpoint: endpoint.to_string(),
                bucket: "awr-workspace-project".to_string(),
                region: "cn-beijing".to_string(),
                addressing,
                prefix: String::new(),
                concurrency: 8,
            },
            Credentials {
                access_key: "synthetic".to_string(),
                secret_key: "synthetic".to_string(),
                session_token: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn the_object_path_is_encoded_once_for_the_style_in_use() {
        let oss = store("https://oss-cn-beijing.aliyuncs.com", Addressing::Virtual);
        assert_eq!(
            oss.object_path("projects/p/files/a%2Fb/ab12"),
            "/projects%2Fp%2Ffiles%2Fa%252Fb%2Fab12"
        );
        assert_eq!(oss.bucket_path(), "/");

        let r2 = store("https://account.r2.cloudflarestorage.com", Addressing::Path);
        assert_eq!(
            r2.object_path("projects/p/manifest.json"),
            "/awr-workspace-project/projects%2Fp%2Fmanifest.json"
        );
        assert_eq!(r2.bucket_path(), "/awr-workspace-project");
    }

    #[test]
    fn only_a_store_without_compare_and_swap_loses_the_capability() {
        assert!(
            !store("https://oss-cn-beijing.aliyuncs.com", Addressing::Virtual).supports_if_match()
        );
        assert!(
            store("https://account.r2.cloudflarestorage.com", Addressing::Path).supports_if_match()
        );
        assert!(store("https://s3.eu-west-1.amazonaws.com", Addressing::Path).supports_if_match());
    }

    #[test]
    fn a_list_response_is_read_as_the_store_wrote_it() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>awr-workspace-project</Name><Prefix>projects/p/files/</Prefix>
  <KeyCount>2</KeyCount><MaxKeys>1000</MaxKeys><IsTruncated>false</IsTruncated>
  <Contents><Key>projects/p/files/a.yaml/current.json</Key>
    <LastModified>2026-09-16T04:51:12.000Z</LastModified>
    <ETag>&quot;d41d8cd98f00b204e9800998ecf8427e&quot;</ETag><Size>412</Size>
    <StorageClass>STANDARD</StorageClass></Contents>
  <Contents><Key>projects/p/files/infra%2Fevidence%2Fdump.json/current.json</Key>
    <ETag>&quot;0cc175b9c0f1b6a831c399e269772661&quot;</ETag><Size>9</Size></Contents>
</ListBucketResult>"#;
        let listed: Vec<(String, String, u64)> = blocks(xml, "Contents")
            .iter()
            .filter_map(|block| {
                Some((
                    unescape(&text_of(block, "Key")?),
                    unescape(&text_of(block, "ETag")?)
                        .trim_matches('"')
                        .to_string(),
                    text_of(block, "Size")?.parse::<u64>().ok()?,
                ))
            })
            .collect();
        assert_eq!(
            listed,
            vec![
                (
                    "projects/p/files/a.yaml/current.json".to_string(),
                    "d41d8cd98f00b204e9800998ecf8427e".to_string(),
                    412
                ),
                (
                    "projects/p/files/infra%2Fevidence%2Fdump.json/current.json".to_string(),
                    "0cc175b9c0f1b6a831c399e269772661".to_string(),
                    9
                ),
            ]
        );
        assert_eq!(text_of(xml, "IsTruncated").as_deref(), Some("false"));
        assert_eq!(text_of(xml, "NextContinuationToken"), None);
    }

    #[test]
    fn a_paginated_list_reports_its_token() {
        let xml = "<ListBucketResult><IsTruncated>true</IsTruncated>\
                   <NextContinuationToken>1ueGcxLPRx</NextContinuationToken></ListBucketResult>";
        assert_eq!(
            text_of(xml, "NextContinuationToken").as_deref(),
            Some("1ueGcxLPRx")
        );
    }

    #[test]
    fn entities_and_numeric_references_survive_the_reader() {
        assert_eq!(unescape("a&amp;b"), "a&b");
        assert_eq!(unescape("a&#37;b"), "a%b");
        assert_eq!(unescape("a&#x25;b"), "a%b");
        assert_eq!(unescape("trailing &no-semicolon"), "trailing &no-semicolon");
        assert_eq!(unescape("plain"), "plain");
    }

    #[test]
    fn a_timestamp_is_the_wire_format() {
        // 2026-09-16T04:51:12Z, and the epoch itself.
        assert_eq!(
            utc_parts(1_789_534_272),
            (2026, 9, 16, 4, 51, 12),
            "civil conversion"
        );
        assert_eq!(utc_parts(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(utc_parts(951_782_400), (2000, 2, 29, 0, 0, 0));
        assert!(amz_date().unwrap().ends_with('Z'));
    }
}
