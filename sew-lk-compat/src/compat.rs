// SPDX-License-Identifier: Apache-2.0
// Ported from livekit/client-sdk-js src/room/utils.ts, src/api/SignalClient.ts @ 53dd840 (Apache-2.0, © 2021 LiveKit, Inc.).
// Modified by Alakazam Labs, 2026.

//! JS SDK rules that Sew's tests evaluate against the engine: `compareVersions`
//! (`src/room/utils.ts` lines 426–443 at v2.22.3) and the validate-response classifier of
//! `SignalClient.handleConnectionError` (lines 1281–1321).

use std::cmp::Ordering;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// JS `parseInt(s, 10)`: leading whitespace, optional sign, leading digits; `None` = NaN.
fn parse_int(s: &str) -> Option<i64> {
    let t = s.trim_start();
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let end = digits
        .bytes()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(digits.len());
    if end == 0 {
        return None;
    }
    let v: i64 = digits[..end].parse().ok()?;
    Some(if neg { -v } else { v })
}

/// `compareVersions(v1, v2)`: -1, 0 or 1, with the JS semantics (NaN parts compare false).
pub fn compare_versions(v1: &str, v2: &str) -> i32 {
    let parts1: Vec<&str> = v1.split('.').collect();
    let parts2: Vec<&str> = v2.split('.').collect();
    let k = parts1.len().min(parts2.len());
    for i in 0..k {
        let p1 = parse_int(parts1[i]);
        let p2 = parse_int(parts2[i]);
        if let (Some(a), Some(b)) = (p1, p2) {
            match a.cmp(&b) {
                Ordering::Greater => return 1,
                Ordering::Less => return -1,
                Ordering::Equal if i == k - 1 => return 0,
                Ordering::Equal => {}
            }
        }
    }
    if v1.is_empty() && !v2.is_empty() {
        return -1;
    } else if v2.is_empty() {
        return 1;
    }
    match parts1.len().cmp(&parts2.len()) {
        Ordering::Equal => 0,
        Ordering::Less => -1,
        Ordering::Greater => 1,
    }
}

/// The JS `ConnectionErrorReason` the SDK reports after asking the validate endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ValidateClass {
    /// 401/403, or 404 with "requested room does not exist": `NotAllowed` with the body.
    NotAllowed {
        /// HTTP status.
        status: u16,
        /// The body text.
        message: String,
    },
    /// 404 otherwise: `ServiceNotFound` ("v1 RTC path not found", context `v0-rtc`), which
    /// makes the SDK fall back to `/rtc` and dual PC.
    ServiceNotFound,
    /// Any other status: the original websocket error (`InternalError` with the status).
    Internal {
        /// HTTP status.
        status: u16,
    },
    /// The validate request itself failed: `ServerUnreachable`.
    ServerUnreachable(String),
}

/// Classifies a validate response exactly as `handleConnectionError` does.
pub fn classify_validate(status: u16, body: &str) -> ValidateClass {
    match status {
        404 if body.contains("requested room does not exist") => ValidateClass::NotAllowed {
            status,
            message: body.to_owned(),
        },
        404 => ValidateClass::ServiceNotFound,
        401 | 403 => ValidateClass::NotAllowed {
            status,
            message: body.to_owned(),
        },
        s => ValidateClass::Internal { status: s },
    }
}

/// GETs an `http://` validate URL (loopback test engines only; no TLS) and classifies it.
pub async fn validate(url: &str) -> ValidateClass {
    match http_get(url).await {
        Ok((status, body)) => classify_validate(status, &body),
        Err(e) => ValidateClass::ServerUnreachable(e),
    }
}

/// A minimal HTTP/1.1 GET over plain TCP: (status, body).
pub async fn http_get(url: &str) -> Result<(u16, String), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("only http:// URLs: {url}"))?;
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let fut = async {
        let mut s = TcpStream::connect(host).await.map_err(|e| e.to_string())?;
        let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
        s.write_all(req.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.map_err(|e| e.to_string())?;
        let text = String::from_utf8_lossy(&buf).into_owned();
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        let status = head
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| format!("bad status line in {head:?}"))?;
        let body = if head
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked")
        {
            dechunk(body)
        } else {
            body.to_owned()
        };
        Ok((status, body))
    };
    tokio::time::timeout(Duration::from_secs(5), fut)
        .await
        .map_err(|_| "validate request timed out".to_owned())?
}

fn dechunk(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    while let Some((size, tail)) = rest.split_once("\r\n") {
        let Ok(n) = usize::from_str_radix(size.trim(), 16) else {
            break;
        };
        if n == 0 || tail.len() < n {
            break;
        }
        out.push_str(&tail[..n]);
        rest = tail[n..].trim_start_matches("\r\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compare_versions_js_cases() {
        assert_eq!(compare_versions("1.13.7", "1.13.6"), 1);
        assert_eq!(compare_versions("1.8.0", "1.8.0"), 0);
        assert_eq!(
            compare_versions("1.8", "1.8.0"),
            0,
            "equal at the last shared part"
        );
        assert_eq!(compare_versions("1.7.9", "1.8.0"), -1);
        assert_eq!(compare_versions("2", "10"), -1);
        assert_eq!(compare_versions("", "1"), -1);
        assert_eq!(compare_versions("1", ""), 1);
        assert_eq!(compare_versions("1.13.7-sew", "1.13.6"), 1);
        assert_eq!(compare_versions("x.1", "y.1"), 0, "NaN parts compare false");
        assert_eq!(compare_versions("x", "y"), 0);
        assert_eq!(
            compare_versions("x", "y.z"),
            -1,
            "falls through to the length rule"
        );
    }

    #[test]
    fn validate_classifier_matches_js() {
        assert_eq!(
            classify_validate(404, "requested room does not exist"),
            ValidateClass::NotAllowed {
                status: 404,
                message: "requested room does not exist".to_owned()
            }
        );
        assert_eq!(
            classify_validate(404, "404 page not found"),
            ValidateClass::ServiceNotFound
        );
        assert!(matches!(
            classify_validate(401, "bad"),
            ValidateClass::NotAllowed { status: 401, .. }
        ));
        assert!(matches!(
            classify_validate(403, ""),
            ValidateClass::NotAllowed { status: 403, .. }
        ));
        assert_eq!(
            classify_validate(503, "draining"),
            ValidateClass::Internal { status: 503 }
        );
        assert_eq!(dechunk("5\r\nhello\r\n0\r\n\r\n"), "hello");
    }
}
