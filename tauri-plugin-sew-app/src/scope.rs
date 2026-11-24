//! Who calls a command, and what it may act on.
//!
//! A command from the app's bundled UI acts on every saved server. A command from a saved
//! server's live site acts only on that server and its accounts: the server comes from the
//! calling page's origin, never from an argument, and a page naming another server or one of
//! its accounts is refused (`wrong_origin`). Adding and confirming a server answer only the
//! bundled UI (`bundled_only`). A page that is neither (another origin a host capability let
//! in) acts on no server.

use std::fmt;

use url::Url;

use crate::error::{AppError, code};
use crate::origin::Origin;
use crate::servers::{AccountKey, Book, ServersView};

/// The caller of a command, from its page's URL.
#[derive(Clone, PartialEq, Eq)]
pub enum Caller {
    /// The app's bundled UI (`tauri://localhost`, `http(s)://tauri.localhost`, or an origin
    /// the host listed, such as a development server's): every saved server.
    Bundled,
    /// A saved server's live site: that server only.
    Site(Origin),
    /// Any other page: no server.
    Other,
}

impl fmt::Debug for Caller {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Caller::Bundled => f.write_str("Bundled"),
            Caller::Site(o) => write!(f, "Site({o})"),
            Caller::Other => f.write_str("Other"),
        }
    }
}

/// Whether `url` is a page of the bundled UI: `tauri://localhost`, `http(s)://tauri.localhost`,
/// or one of `extra` (origins, such as a development server's).
pub fn is_bundled(url: &Url, extra: &[String]) -> bool {
    let bundled = match url.scheme() {
        "tauri" => url.host_str() == Some("localhost"),
        "http" | "https" => url.host_str() == Some("tauri.localhost"),
        _ => false,
    };
    bundled
        || extra
            .iter()
            .any(|o| o.trim_end_matches('/') == url.origin().ascii_serialization())
}

fn wrong_origin() -> AppError {
    AppError {
        code: code::WRONG_ORIGIN.to_owned(),
        detail: serde_json::Value::Null,
        hint: Some("A server's page acts only on its own server.".to_owned()),
    }
}

impl Caller {
    /// The caller at `url`: the bundled UI (see [`is_bundled`]), else the saved server of
    /// `book` whose origin is the page's, else [`Caller::Other`].
    pub fn of(url: &Url, bundled_extra: &[String], book: &Book) -> Caller {
        if is_bundled(url, bundled_extra) {
            return Caller::Bundled;
        }
        book.servers
            .iter()
            .find(|s| s.origin.contains(url))
            .map_or(Caller::Other, |s| Caller::Site(s.origin.clone()))
    }

    /// Ok for the bundled UI only (`bundled_only`).
    pub fn bundled_only(&self) -> Result<(), AppError> {
        match self {
            Caller::Bundled => Ok(()),
            _ => Err(AppError {
                code: code::BUNDLED_ONLY.to_owned(),
                detail: serde_json::Value::Null,
                hint: Some("Only the app's own pages add or confirm a server.".to_owned()),
            }),
        }
    }

    /// Ok when the caller may act on `server`: the bundled UI, or that server's own site
    /// (`wrong_origin`).
    pub fn server(&self, server: &Origin) -> Result<(), AppError> {
        match self {
            Caller::Bundled => Ok(()),
            Caller::Site(o) if o == server => Ok(()),
            _ => Err(wrong_origin()),
        }
    }

    /// The account a command acts on: `key` when the caller may act on its server, else
    /// (`None`) the active account, which for a live site must be on its own server
    /// (`no_account` otherwise).
    pub fn key(
        &self,
        key: Option<AccountKey>,
        active: Option<AccountKey>,
    ) -> Result<AccountKey, AppError> {
        match key {
            Some(k) => {
                self.server(&k.server)?;
                Ok(k)
            }
            None => {
                let a = active.ok_or_else(|| AppError::new(code::NO_ACCOUNT))?;
                match self {
                    Caller::Bundled => Ok(a),
                    Caller::Site(o) if *o == a.server => Ok(a),
                    Caller::Site(_) => Err(AppError::new(code::NO_ACCOUNT)),
                    Caller::Other => Err(wrong_origin()),
                }
            }
        }
    }

    /// What the caller sees of the saved servers: everything for the bundled UI; its own
    /// server and accounts for a live site (the active account only when it is one of them);
    /// nothing for any other page.
    pub fn view(&self, mut v: ServersView) -> ServersView {
        match self {
            Caller::Bundled => v,
            Caller::Site(o) => {
                v.servers.retain(|s| &s.origin == o);
                if v.active.as_ref().is_some_and(|a| &a.server != o) {
                    v.active = None;
                }
                v
            }
            Caller::Other => {
                v.servers.clear();
                v.active = None;
                v
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::lane::LaneInfo;
    use crate::servers::Server;

    fn o(s: &str) -> Origin {
        Origin::parse(s, false).unwrap()
    }

    fn book() -> Book {
        let mut b = Book::default();
        for s in ["https://a.example.com", "https://b.example.com"] {
            b.servers.push(Server {
                origin: o(s),
                info: serde_json::from_value::<LaneInfo>(serde_json::json!({ "lane": 1 })).unwrap(),
                confirmed_at: 0,
                accounts: Vec::new(),
            });
        }
        b
    }

    #[test]
    fn the_caller_is_read_from_the_page() {
        let b = book();
        let u = |s: &str| Url::parse(s).unwrap();
        assert_eq!(
            Caller::of(&u("tauri://localhost/index.html"), &[], &b),
            Caller::Bundled
        );
        assert_eq!(
            Caller::of(&u("http://tauri.localhost/"), &[], &b),
            Caller::Bundled
        );
        assert_eq!(
            Caller::of(
                &u("http://localhost:1420/x"),
                &["http://localhost:1420".into()],
                &b
            ),
            Caller::Bundled
        );
        assert_eq!(
            Caller::of(&u("https://a.example.com/app/c/5"), &[], &b),
            Caller::Site(o("https://a.example.com"))
        );
        assert_eq!(
            Caller::of(&u("https://a.example.com:8443/"), &[], &b),
            Caller::Other
        );
        assert_eq!(
            Caller::of(&u("https://evil.example/"), &[], &b),
            Caller::Other
        );
    }

    #[test]
    fn a_site_acts_only_on_its_own_server() {
        let a = Caller::Site(o("https://a.example.com"));
        let key = |s: &str| AccountKey {
            server: o(s),
            account: "x".into(),
        };
        assert!(a.server(&o("https://a.example.com")).is_ok());
        assert_eq!(
            a.server(&o("https://b.example.com")).unwrap_err().code,
            code::WRONG_ORIGIN
        );
        assert_eq!(
            a.key(Some(key("https://b.example.com")), None)
                .unwrap_err()
                .code,
            code::WRONG_ORIGIN
        );
        // Without a key: the active account only when it is the site's own.
        assert_eq!(
            a.key(None, Some(key("https://b.example.com")))
                .unwrap_err()
                .code,
            code::NO_ACCOUNT
        );
        assert!(a.key(None, Some(key("https://a.example.com"))).is_ok());
        assert_eq!(a.bundled_only().unwrap_err().code, code::BUNDLED_ONLY);
        assert!(Caller::Bundled.bundled_only().is_ok());
        assert!(
            Caller::Bundled
                .key(None, Some(key("https://b.example.com")))
                .is_ok()
        );
        assert_eq!(
            Caller::Other
                .server(&o("https://a.example.com"))
                .unwrap_err()
                .code,
            code::WRONG_ORIGIN
        );
    }

    #[test]
    fn a_site_lists_only_its_own_server() {
        let mut b = book();
        b.active = Some(AccountKey {
            server: o("https://b.example.com"),
            account: "x".into(),
        });
        let v = Caller::Site(o("https://a.example.com")).view(b.view(None));
        assert_eq!(v.servers.len(), 1);
        assert_eq!(v.servers[0].origin, o("https://a.example.com"));
        assert_eq!(v.active, None);
        assert_eq!(Caller::Bundled.view(b.view(None)).servers.len(), 2);
        assert!(Caller::Other.view(b.view(None)).servers.is_empty());
    }
}
