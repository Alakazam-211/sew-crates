//! Deep links: `<scheme>://<host>[:port]/<path>?<query>#<fragment>` opens that page of a saved
//! server in the webview, and `<scheme>://<host>/native/pair#code=…` asks to sign in to that
//! server with a pairing code.
//!
//! A link names the server by its host (`myapp://chat.example.com/c/5` is
//! `https://chat.example.com/c/5`). The plugin navigates only to a saved (confirmed) server;
//! any other link, and every link when no window is open, goes to the webview as the
//! `sew-app://deep_link` event for the App to handle (or to ignore). A link never adds a
//! server by itself.
//!
//! A pairing link is never navigated to and never passed on: for a saved server the plugin
//! holds the code and sends `sew-app://pair_request` (the shell asks `Sign in to <host>?` and
//! calls `pair_confirm` or `pair_decline`); for any other host it sends
//! `sew-app://pair_refused` and redeems nothing.

use std::fmt;

use serde::{Deserialize, Serialize};
use url::Url;

use crate::lane::Secret;
use crate::origin::Origin;

/// The path of a pairing link (`<scheme>://<host>/native/pair#code=…`).
pub const PAIR_PATH: &str = "/native/pair";

/// `sew-app://deep_link`'s payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeepLinkEvent {
    /// The link as it came.
    pub url: String,
    /// The saved server it names, when it names one.
    pub server: Option<Origin>,
    /// The page on that server (path, query and fragment), when it names one.
    pub path: Option<String>,
    /// Whether the plugin navigated the main window there.
    pub navigated: bool,
}

/// What a link means.
#[derive(Clone, PartialEq, Eq)]
pub enum DeepLinkRoute {
    /// Open this URL on a saved server.
    Open {
        /// The server.
        server: Origin,
        /// The page (`https://<server>/<path>…`).
        url: Url,
    },
    /// A pairing link: ask before redeeming (only a saved server's).
    Pair {
        /// The host (and port) it names, lowercase.
        host: String,
        /// The saved server with that host, when there is one.
        server: Option<Origin>,
        /// The fragment's `code`, when it has one.
        code: Option<Secret>,
    },
    /// Not one of ours to navigate: tell the webview.
    Event,
}

impl fmt::Debug for DeepLinkRoute {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeepLinkRoute::Open { server, url } => f
                .debug_struct("Open")
                .field("server", server)
                .field("url", url)
                .finish(),
            DeepLinkRoute::Pair { host, server, code } => f
                .debug_struct("Pair")
                .field("host", host)
                .field("server", server)
                .field("code", code)
                .finish(),
            DeepLinkRoute::Event => f.write_str("Event"),
        }
    }
}

/// Routes `link` for `scheme`: a link of another scheme, or naming no saved server, is
/// [`DeepLinkRoute::Event`]; a pairing link is [`DeepLinkRoute::Pair`] whatever server it
/// names.
pub fn route(link: &Url, scheme: &str, saved: &[Origin]) -> DeepLinkRoute {
    if !link.scheme().eq_ignore_ascii_case(scheme) {
        return DeepLinkRoute::Event;
    }
    let Some(host) = link.host_str() else {
        return DeepLinkRoute::Event;
    };
    let authority = match link.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.to_owned(),
    };
    let server = saved
        .iter()
        .find(|o| o.authority().eq_ignore_ascii_case(&authority))
        .cloned();
    if link.path().trim_end_matches('/') == PAIR_PATH {
        let code = link.fragment().and_then(|f| {
            url::form_urlencoded::parse(f.as_bytes())
                .find(|(k, _)| k == "code")
                .map(|(_, v)| Secret::new(v.into_owned()))
        });
        return DeepLinkRoute::Pair {
            host: authority.to_ascii_lowercase(),
            server,
            code,
        };
    }
    let Some(server) = server else {
        return DeepLinkRoute::Event;
    };
    let mut path = link.path().to_owned();
    if path.is_empty() {
        path.push('/');
    }
    if let Some(q) = link.query() {
        path.push('?');
        path.push_str(q);
    }
    if let Some(f) = link.fragment() {
        path.push('#');
        path.push_str(f);
    }
    match server.join(&path) {
        Ok(url) => DeepLinkRoute::Open { server, url },
        Err(_) => DeepLinkRoute::Event,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn saved() -> Vec<Origin> {
        vec![
            Origin::parse("https://chat.example.com", false).unwrap(),
            Origin::parse("https://other.example.com:8443", false).unwrap(),
        ]
    }

    #[test]
    fn a_link_to_a_saved_server_opens_its_page() {
        let l = Url::parse("myapp://chat.example.com/c/5?m=9#top").unwrap();
        match route(&l, "myapp", &saved()) {
            DeepLinkRoute::Open { server, url } => {
                assert_eq!(server.as_str(), "https://chat.example.com");
                assert_eq!(url.as_str(), "https://chat.example.com/c/5?m=9#top");
            }
            r => panic!("{r:?}"),
        }
        let l = Url::parse("myapp://other.example.com:8443").unwrap();
        match route(&l, "myapp", &saved()) {
            DeepLinkRoute::Open { url, .. } => {
                assert_eq!(url.as_str(), "https://other.example.com:8443/");
            }
            r => panic!("{r:?}"),
        }
    }

    #[test]
    fn anything_else_is_only_an_event() {
        for l in [
            "myapp://evil.example/c/5",
            "other://chat.example.com/c/5",
            "myapp:no-host",
            "myapp://other.example.com/x",
            "other://chat.example.com/native/pair#code=abc",
        ] {
            let l = Url::parse(l).unwrap();
            assert_eq!(route(&l, "myapp", &saved()), DeepLinkRoute::Event, "{l}");
        }
    }

    #[test]
    fn a_pairing_link_is_never_a_page_and_its_code_never_prints() {
        let l = Url::parse("myapp://Chat.Example.com/native/pair#code=abc_DEF-123").unwrap();
        let r = route(&l, "myapp", &saved());
        match &r {
            DeepLinkRoute::Pair { host, server, code } => {
                assert_eq!(host, "chat.example.com");
                assert_eq!(
                    server.as_ref().unwrap().as_str(),
                    "https://chat.example.com"
                );
                assert_eq!(code.as_ref().unwrap().expose(), "abc_DEF-123");
            }
            r => panic!("{r:?}"),
        }
        assert!(
            !format!("{r:?}").contains("abc_DEF"),
            "Debug prints no code"
        );
        // An unsaved host is still a pairing link (refused later), never a page or an event.
        let l = Url::parse("myapp://evil.example/native/pair/#code=x").unwrap();
        match route(&l, "myapp", &saved()) {
            DeepLinkRoute::Pair { host, server, code } => {
                assert_eq!(host, "evil.example");
                assert!(server.is_none());
                assert_eq!(code.unwrap().expose(), "x");
            }
            r => panic!("{r:?}"),
        }
        // No code.
        let l = Url::parse("myapp://other.example.com:8443/native/pair").unwrap();
        match route(&l, "myapp", &saved()) {
            DeepLinkRoute::Pair { host, server, code } => {
                assert_eq!(host, "other.example.com:8443");
                assert!(server.is_some());
                assert!(code.is_none());
            }
            r => panic!("{r:?}"),
        }
    }
}
