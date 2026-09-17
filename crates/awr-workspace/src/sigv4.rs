//! AWS Signature Version 4, dependency-light and separable.
//!
//! Kept away from the workspace semantics so the signing path can be checked
//! against the official AWS test vectors without credentials or a network.
//!
//! Aliyun OSS accepts this dialect as-is: measured against the live service,
//! `AWS4-HMAC-SHA256` with service `s3` and a real region is accepted, so no
//! second signing dialect is needed.
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

pub const ALGORITHM: &str = "AWS4-HMAC-SHA256";
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

type HmacSha256 = Hmac<Sha256>;

pub fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC takes a key of any length");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~')
}

/// Percent-encode, never normalising. `safe` names bytes that stay literal.
pub(crate) fn percent_encode(value: &str, safe: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if is_unreserved(byte) || (safe.contains(byte as char) && byte.is_ascii()) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

pub(crate) fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                out.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn canonical_query(raw: &str) -> String {
    if raw.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<String> = raw
        .split('&')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let (name, value) = part.split_once('=').unwrap_or((part, ""));
            format!(
                "{}={}",
                percent_encode(&percent_decode(name), ""),
                percent_encode(&percent_decode(value), "")
            )
        })
        .collect();
    pairs.sort();
    pairs.join("&")
}

/// Returns the canonical header block (newline terminated) and its signed names.
pub fn canonical_headers(headers: &[(String, String)]) -> (String, String) {
    let mut merged: Vec<(String, Vec<String>)> = Vec::new();
    for (name, value) in headers {
        let key = name.to_ascii_lowercase();
        // Values are whitespace-collapsed; a repeated name is joined with ','.
        let collapsed = value.split_whitespace().collect::<Vec<_>>().join(" ");
        match merged.iter_mut().find(|(seen, _)| *seen == key) {
            Some((_, values)) => values.push(collapsed),
            None => merged.push((key, vec![collapsed])),
        }
    }
    merged.sort_by(|left, right| left.0.cmp(&right.0));
    let block = merged
        .iter()
        .map(|(name, values)| format!("{name}:{}", values.join(",")))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let signed = merged
        .iter()
        .map(|(name, _)| name.clone())
        .collect::<Vec<_>>()
        .join(";");
    (block, signed)
}

/// Encode the path as sent: decode once, then encode, and never normalise.
pub fn canonical_uri(path: &str) -> String {
    let path = if path.is_empty() { "/" } else { path };
    percent_encode(&percent_decode(path), "/")
}

pub fn canonical_request(
    method: &str,
    path: &str,
    raw_query: &str,
    headers: &[(String, String)],
    payload_sha256: &str,
) -> String {
    let (header_block, signed) = canonical_headers(headers);
    [
        method,
        &canonical_uri(path),
        &canonical_query(raw_query),
        &header_block,
        &signed,
        payload_sha256,
    ]
    .join("\n")
}

pub fn string_to_sign(amz_date: &str, scope: &str, canonical: &str) -> String {
    [
        ALGORITHM,
        amz_date,
        scope,
        &hex_sha256(canonical.as_bytes()),
    ]
    .join("\n")
}

pub fn signing_key(secret: &str, date_stamp: &str, region: &str, service: &str) -> [u8; 32] {
    let key = hmac(format!("AWS4{secret}").as_bytes(), date_stamp.as_bytes());
    let key = hmac(&key, region.as_bytes());
    let key = hmac(&key, service.as_bytes());
    hmac(&key, b"aws4_request")
}

/// Returns the string to sign and the Authorization header value.
pub fn authorization(
    access_key: &str,
    secret_key: &str,
    region: &str,
    service: &str,
    amz_date: &str,
    canonical: &str,
    signed: &str,
) -> (String, String) {
    let date_stamp = &amz_date[..amz_date.len().min(8)];
    let scope = format!("{date_stamp}/{region}/{service}/aws4_request");
    let to_sign = string_to_sign(amz_date, &scope, canonical);
    let signature = hex(&hmac(
        &signing_key(secret_key, date_stamp, region, service),
        to_sign.as_bytes(),
    ));
    (
        to_sign,
        format!(
            "{ALGORITHM} Credential={access_key}/{scope}, SignedHeaders={signed}, Signature={signature}"
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The suite's `normalize-path` group states the *non-S3* expectation:
    /// services that are not S3 normalise `/example/..` away. S3 signs the path
    /// as sent, so the rule this client has to hold is the round trip - the
    /// signed path decodes back to exactly the path that was sent, dots and
    /// doubled slashes included.
    #[test]
    fn signed_path_is_the_path_as_sent() {
        for (sent, signed) in [
            ("/example/..", "/example/.."),
            ("/example1/example2/../..", "/example1/example2/../.."),
            ("/./", "/./"),
            ("/./example", "/./example"),
            ("//", "//"),
            ("//example//", "//example//"),
            ("/example space/", "/example%20space/"),
        ] {
            assert_eq!(canonical_uri(sent), signed, "canonical uri for {sent:?}");
            assert_eq!(
                percent_decode(&canonical_uri(sent)),
                sent,
                "round trip for {sent:?}"
            );
        }
    }

    #[test]
    fn an_empty_path_signs_as_root() {
        assert_eq!(canonical_uri(""), "/");
    }

    #[test]
    fn query_pairs_are_sorted_and_re_encoded() {
        assert_eq!(canonical_query(""), "");
        assert_eq!(
            canonical_query("b=2&a=1"),
            "a=1&b=2",
            "pairs sort by encoded name"
        );
        assert_eq!(canonical_query("k"), "k=", "a bare key gains its separator");
        assert_eq!(
            canonical_query("a=%2F"),
            "a=%2F",
            "an encoded slash stays encoded in a query value"
        );
    }
}
