//! Transport layer: one façade over HTTP/3 (quinn + h3) and HTTP/1.1-2
//! (reqwest), plus the shared header parsing both paths need.

pub mod altsvc;
pub mod gauge;
pub mod h3;
pub mod http12;
pub mod stack;
pub mod tls;

pub use altsvc::{AltSvcEntry, AltSvcStore, MemAltSvc};
pub use gauge::{EngineConnections, ProtocolGauge};
pub use stack::{Body, NetStack, Opened, ProbeInfo, RequestSpec};

use http::header::{
    HeaderMap, HeaderName, ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE,
    CONTENT_TYPE, ETAG, LAST_MODIFIED,
};
pub use http::Method;

use crate::store::records::TransportKind;
use crate::util;

/// `Accept-Encoding` we always send: a downloader must receive *identity*
/// bytes, otherwise `Content-Length` and `Range` stop meaning anything.
pub const ACCEPT_ENCODING_IDENTITY: &str = "identity";

/// Look up a header as a trimmed string.
pub fn header_str(headers: &HeaderMap, name: &HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// `Content-Length`, when present and parseable.
pub fn content_length(headers: &HeaderMap) -> Option<u64> {
    header_str(headers, &CONTENT_LENGTH)?.parse().ok()
}

/// Does the server accept `Range` requests?
///
/// `accept-ranges: bytes` is the explicit signal; some servers omit it but
/// still answer `206` - callers therefore also treat a `Content-Range` in a
/// 206 response as proof.
pub fn accepts_ranges(headers: &HeaderMap) -> bool {
    match header_str(headers, &ACCEPT_RANGES) {
        Some(v) => v.to_ascii_lowercase().contains("bytes"),
        None => headers.contains_key(CONTENT_RANGE),
    }
}

/// ETag exactly as sent, weak prefix included. Callers that build `If-Range`
/// keep only a strong tag; see [`strong_etag`].
pub fn etag(headers: &HeaderMap) -> Option<String> {
    header_str(headers, &ETAG)
}

/// `Last-Modified` verbatim. It stays a stored hint. It is not an `If-Range`
/// validator: a clock value is not a representation identity.
pub fn last_modified(headers: &HeaderMap) -> Option<String> {
    header_str(headers, &LAST_MODIFIED)
}

pub fn content_encoding(headers: &HeaderMap) -> Option<String> {
    header_str(headers, &http::header::CONTENT_ENCODING)
}

pub fn vary(headers: &HeaderMap) -> Option<String> {
    header_str(headers, &http::header::VARY)
}

/// RFC 9110 weak indicator: the two octets `W/`. Anything else, including a
/// different quote style, is not rewritten into a strong tag.
pub fn is_weak_etag(raw: &str) -> bool {
    let trimmed = raw.trim();
    trimmed.len() >= 2 && trimmed.as_bytes()[0] == b'W' && trimmed.as_bytes()[1] == b'/'
}

/// The bytes that may be stored as `If-Range` or as a representation bound.
/// A weak tag returns `None`. The strong tag is returned verbatim, not
/// canonicalized.
pub fn strong_etag(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || is_weak_etag(trimmed) {
        None
    } else {
        Some(trimmed)
    }
}

/// `Content-Encoding` absent, empty, or only `identity` (possibly repeated).
/// Any other coding, including `gzip` beside `identity`, is not identity.
pub fn content_coding_is_identity(value: Option<&str>) -> bool {
    match value {
        None => true,
        Some(raw) => raw.split(',').all(|part| {
            let part = part.trim();
            part.is_empty() || part.eq_ignore_ascii_case("identity")
        }),
    }
}

/// `Vary` absent, empty, or listing only `Accept-Encoding` may bind with
/// header scope `none`. Any other field, or `*`, may not.
pub fn vary_allows_none_scope(value: Option<&str>) -> bool {
    match value {
        None => true,
        Some(raw) => raw
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .all(|part| part.eq_ignore_ascii_case("accept-encoding")),
    }
}

pub fn content_type(headers: &HeaderMap) -> Option<String> {
    header_str(headers, &CONTENT_TYPE).map(|v| v.split(';').next().unwrap_or("").trim().to_string())
}

/// Filename suggested by `Content-Disposition`.
pub fn filename_hint(headers: &HeaderMap) -> Option<String> {
    header_str(headers, &CONTENT_DISPOSITION).and_then(|v| util::parse_content_disposition(&v))
}

/// `Content-Range` of a 206 response -> `(start, end, total)`.
pub fn content_range(headers: &HeaderMap) -> Option<(u64, u64, Option<u64>)> {
    header_str(headers, &CONTENT_RANGE).and_then(|v| util::parse_content_range(&v))
}

/// Learn an HTTP/3 endpoint from an `Alt-Svc` header, if it advertises one.
pub fn alt_svc_from(headers: &HeaderMap, origin_host: &str, origin_port: u16) -> Option<AltSvcEntry> {
    let value = header_str(headers, &http::header::ALT_SVC)?;
    let (port, ma) = util::parse_alt_svc_h3(&value, origin_port)?;
    Some(AltSvcEntry::new(origin_host, port, ma))
}

/// Flatten a header map into an owned, sorted list (for `udm-cli probe`).
pub fn headers_to_vec(headers: &HeaderMap) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = headers
        .iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|s| (k.as_str().to_string(), s.to_string())))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    out
}

/// Map a `reqwest::Version` to our [`TransportKind`].
pub fn transport_from_version(version: reqwest::Version) -> TransportKind {
    TransportKind::from_version_text(&format!("{version:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::header::HeaderValue;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn parses_range_support() {
        assert!(accepts_ranges(&headers(&[("accept-ranges", "bytes")])));
        assert!(!accepts_ranges(&headers(&[("accept-ranges", "none")])));
        assert!(accepts_ranges(&headers(&[("content-range", "bytes 0-1/2")])));
        assert!(!accepts_ranges(&headers(&[])));
    }

    #[test]
    fn parses_common_headers() {
        let h = headers(&[
            ("content-length", "12345"),
            ("etag", "\"abc123\""),
            ("last-modified", "Wed, 21 Oct 2025 07:28:00 GMT"),
            ("content-type", "application/octet-stream; charset=utf-8"),
            ("content-disposition", "attachment; filename=\"thing.iso\""),
            ("alt-svc", "h3=\":443\"; ma=3600"),
        ]);
        assert_eq!(content_length(&h), Some(12345));
        assert_eq!(etag(&h).as_deref(), Some("\"abc123\""));
        assert_eq!(
            last_modified(&h).as_deref(),
            Some("Wed, 21 Oct 2025 07:28:00 GMT")
        );
        assert_eq!(content_type(&h).as_deref(), Some("application/octet-stream"));
        assert_eq!(filename_hint(&h).as_deref(), Some("thing.iso"));
        let learned = alt_svc_from(&h, "cdn.example", 443).unwrap();
        assert_eq!(learned.origin, "cdn.example:443");
        assert_eq!(learned.port, 443);
        assert_eq!(learned.max_age_secs, 3600);
    }

    #[test]
    fn headers_to_vec_is_sorted() {
        let v = headers_to_vec(&headers(&[("z", "1"), ("a", "2")]));
        assert_eq!(v[0].0, "a");
    }

    #[test]
    fn strong_etag_rejects_a_weak_prefix_and_keeps_quotes() {
        assert_eq!(strong_etag("\"abc\""), Some("\"abc\""));
        assert_eq!(strong_etag(" W/\"abc\""), None);
        assert!(is_weak_etag("W/\"abc\""));
        assert!(!is_weak_etag("\"W/not-a-prefix\""));
        assert_eq!(strong_etag(""), None);
    }

    #[test]
    fn identity_coding_and_vary_scope() {
        assert!(content_coding_is_identity(None));
        assert!(content_coding_is_identity(Some("identity")));
        assert!(content_coding_is_identity(Some("identity, identity")));
        assert!(!content_coding_is_identity(Some("gzip")));
        assert!(!content_coding_is_identity(Some("gzip, identity")));
        assert!(vary_allows_none_scope(None));
        assert!(vary_allows_none_scope(Some("")));
        assert!(vary_allows_none_scope(Some("Accept-Encoding")));
        assert!(vary_allows_none_scope(Some("accept-encoding, Accept-Encoding")));
        assert!(!vary_allows_none_scope(Some("*")));
        assert!(!vary_allows_none_scope(Some("Accept-Encoding, User-Agent")));
    }
}
