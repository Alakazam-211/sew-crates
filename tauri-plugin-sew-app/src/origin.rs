//! An App server's origin: `https://host[:port]`, the key of everything the plugin saves.

use std::fmt;
use std::net::IpAddr;

use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::{AppError, code};

/// An App server's origin, normalised: `https://host[:port]` (lower-case host, no default
/// port, no path), or `http://` on this machine's loopback only when the app allows it
/// (development). Serialises as that string.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Origin(String);

/// Whether `host` names this machine.
pub fn is_loopback_host(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']');
    h.eq_ignore_ascii_case("localhost") || h.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

impl Origin {
    /// Reads what a person typed: a URL or a bare host (`chat.example.com` is
    /// `https://chat.example.com`). A path, query or fragment is dropped. `http://` is refused
    /// (`insecure_url`) unless `allow_loopback_http` and the host is this machine.
    pub fn parse(input: &str, allow_loopback_http: bool) -> Result<Origin, AppError> {
        let input = input.trim();
        if input.is_empty() {
            return Err(AppError::with_detail(code::INVALID_URL, "empty"));
        }
        let with_scheme = if input.contains("://") {
            input.to_owned()
        } else {
            format!("https://{input}")
        };
        let url = Url::parse(&with_scheme)
            .map_err(|e| AppError::with_detail(code::INVALID_URL, e.to_string()))?;
        if !url.username().is_empty() || url.password().is_some() {
            return Err(AppError::with_detail(
                code::INVALID_URL,
                "a server address carries no user or password",
            ));
        }
        let Some(host) = url.host_str() else {
            return Err(AppError::with_detail(code::INVALID_URL, "no host"));
        };
        match url.scheme() {
            "https" => {}
            "http" if allow_loopback_http && is_loopback_host(host) => {}
            "http" => {
                return Err(AppError::with_detail(
                    code::INSECURE_URL,
                    "the server must be https://",
                ));
            }
            other => {
                return Err(AppError::with_detail(
                    code::INVALID_URL,
                    format!("unknown scheme {other}"),
                ));
            }
        }
        // `Url::origin` lower-cases the host and drops the default port.
        Ok(Origin(url.origin().ascii_serialization()))
    }

    /// The origin string (`https://host[:port]`).
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The origin as a URL (path `/`).
    pub fn url(&self) -> Url {
        match Url::parse(&self.0) {
            Ok(u) => u,
            // An Origin is only made from a parsed URL.
            Err(_) => unreachable!("an Origin is a valid URL"),
        }
    }

    /// The host (and `:port` when not the default), as a deep link names it.
    pub fn authority(&self) -> &str {
        self.0.split_once("://").map_or(&self.0, |(_, a)| a)
    }

    /// `path` (with its query and fragment) under this origin. A path that would leave the
    /// origin (`//other`, a scheme) is refused.
    pub fn join(&self, path: &str) -> Result<Url, AppError> {
        let path = if path.is_empty() { "/" } else { path };
        if !path.starts_with('/') || path.starts_with("//") || path.contains('\\') {
            return Err(AppError::invalid_args(
                "a path under the server starts with one /",
            ));
        }
        let url = self
            .url()
            .join(path)
            .map_err(|e| AppError::invalid_args(e.to_string()))?;
        if url.origin().ascii_serialization() != self.0 {
            return Err(AppError::invalid_args("the path leaves the server"));
        }
        Ok(url)
    }

    /// Whether `url` is on this origin.
    pub fn contains(&self, url: &Url) -> bool {
        url.origin().ascii_serialization() == self.0
    }
}

impl TryFrom<String> for Origin {
    type Error = AppError;

    /// Reads a saved origin (`https://` or a loopback `http://`).
    fn try_from(s: String) -> Result<Origin, AppError> {
        Origin::parse(&s, true)
    }
}

impl From<Origin> for String {
    fn from(o: Origin) -> String {
        o.0
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Origin({})", self.0)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn normalises_what_a_person_types() {
        let o = Origin::parse("Chat.Example.com/some/path?q#f", false).unwrap();
        assert_eq!(o.as_str(), "https://chat.example.com");
        assert_eq!(
            Origin::parse("https://chat.example.com:443/", false)
                .unwrap()
                .as_str(),
            "https://chat.example.com"
        );
        assert_eq!(
            Origin::parse("https://chat.example.com:8443", false)
                .unwrap()
                .authority(),
            "chat.example.com:8443"
        );
    }

    #[test]
    fn refuses_plain_http_off_loopback_and_odd_urls() {
        assert_eq!(
            Origin::parse("http://chat.example.com", true)
                .unwrap_err()
                .code,
            code::INSECURE_URL
        );
        assert_eq!(
            Origin::parse("http://127.0.0.1:8080", false)
                .unwrap_err()
                .code,
            code::INSECURE_URL
        );
        assert_eq!(
            Origin::parse("http://127.0.0.1:8080", true)
                .unwrap()
                .as_str(),
            "http://127.0.0.1:8080"
        );
        assert_eq!(
            Origin::parse("https://bob:pw@chat.example.com", false)
                .unwrap_err()
                .code,
            code::INVALID_URL
        );
        assert_eq!(
            Origin::parse("ftp://chat.example.com", false)
                .unwrap_err()
                .code,
            code::INVALID_URL
        );
        assert_eq!(
            Origin::parse("  ", false).unwrap_err().code,
            code::INVALID_URL
        );
    }

    #[test]
    fn join_stays_on_the_origin() {
        let o = Origin::parse("https://chat.example.com", false).unwrap();
        assert_eq!(
            o.join("/app/c/5?x=1#m").unwrap().as_str(),
            "https://chat.example.com/app/c/5?x=1#m"
        );
        assert!(o.join("//evil.example/x").is_err());
        assert!(o.join("https://evil.example/").is_err());
        assert!(o.join("x").is_err());
        assert!(o.join("/\\evil.example").is_err());
        assert!(o.contains(&Url::parse("https://chat.example.com/a").unwrap()));
        assert!(!o.contains(&Url::parse("https://chat.example.com:8443/a").unwrap()));
    }
}
