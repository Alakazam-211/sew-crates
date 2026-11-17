//! The saved servers and their accounts (no secret: the bearers are in the [`SecretStore`]).
//!
//! Everything is keyed by server (its [`Origin`]) and account (an [`AccountKey`]): a person
//! may keep several servers, and several accounts on one server (an admin's two identities, a
//! shared tablet). Each account has its own device id, which is the account's key and what its
//! sign-in sends as `device.id`, so two accounts on one server are two device sessions and
//! signing one in again replaces only its own.
//!
//! The book is a JSON file (`servers.json`, 0600) under the app's data directory, written whole
//! through a temporary file and a rename.
//!
//! [`SecretStore`]: crate::secrets::SecretStore

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::lane::{LaneInfo, SessionInfo};
use crate::origin::Origin;
use crate::secrets::SecretKey;

/// The book's format version.
pub const BOOK_VERSION: u32 = 1;

/// One account on one server: `{server, account}` in JSON.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AccountKey {
    /// The server's origin.
    pub server: Origin,
    /// The account's id on this install (its device id).
    pub account: String,
}

impl AccountKey {
    /// Its secret store entry.
    pub fn secret_key(&self) -> SecretKey {
        SecretKey {
            server: self.server.as_str().to_owned(),
            account: self.account.clone(),
        }
    }
}

/// Whether an account's device session is live.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AccountState {
    /// Signed in: the bearer is in the secret store.
    SignedIn,
    /// The server ended the session (`reason`: `signed_out`, `revoked`, `replaced`, `idle`,
    /// `ended`, `unauthenticated`); the bearer is deleted. Shown until the person signs in
    /// again or removes the account.
    SignedOut {
        /// Why.
        reason: String,
    },
}

/// One account (what the book saves; never its bearer).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// Its id on this install: the device id its sign-ins send.
    pub id: String,
    /// Sew's member id.
    pub member_id: String,
    /// What to show (the person's display name, else the sign-in name).
    #[serde(default)]
    pub label: Option<String>,
    /// The Sew install the server fronts (`whoami.install_id`), for the voice plugin's
    /// `forget_server`.
    #[serde(default)]
    pub install_id: Option<u64>,
    /// The device session's public id (for the device list and a revoke).
    #[serde(default)]
    pub session_id: Option<String>,
    /// The device name the session shows.
    pub device_name: String,
    /// When it signed in (the lane's `created_at`).
    #[serde(default)]
    pub signed_in_at: Option<String>,
    /// Live or ended.
    #[serde(flatten)]
    pub state: AccountState,
}

/// One saved server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Server {
    /// Its origin.
    pub origin: Origin,
    /// What the lane offered when it was confirmed (refreshed on each sign-in).
    pub info: LaneInfo,
    /// When it was confirmed (seconds since the Unix epoch).
    pub confirmed_at: u64,
    /// Its accounts.
    #[serde(default)]
    pub accounts: Vec<Account>,
}

/// The whole book.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Book {
    /// [`BOOK_VERSION`].
    pub version: u32,
    /// The saved servers, in the order they were added.
    #[serde(default)]
    pub servers: Vec<Server>,
    /// The account the voice provider and `open` use when none is named.
    #[serde(default)]
    pub active: Option<AccountKey>,
}

impl Default for Book {
    fn default() -> Book {
        Book {
            version: BOOK_VERSION,
            servers: Vec::new(),
            active: None,
        }
    }
}

impl Book {
    /// The server with this origin.
    pub fn server(&self, origin: &Origin) -> Option<&Server> {
        self.servers.iter().find(|s| &s.origin == origin)
    }

    /// The server with this origin, to change.
    pub fn server_mut(&mut self, origin: &Origin) -> Option<&mut Server> {
        self.servers.iter_mut().find(|s| &s.origin == origin)
    }

    /// The account with this key.
    pub fn account(&self, key: &AccountKey) -> Option<&Account> {
        self.server(&key.server)?
            .accounts
            .iter()
            .find(|a| a.id == key.account)
    }

    /// The account with this key, to change.
    pub fn account_mut(&mut self, key: &AccountKey) -> Option<&mut Account> {
        self.server_mut(&key.server)?
            .accounts
            .iter_mut()
            .find(|a| a.id == key.account)
    }

    /// Every account, with its key.
    pub fn accounts(&self) -> impl Iterator<Item = (AccountKey, &Account)> {
        self.servers.iter().flat_map(|s| {
            s.accounts.iter().map(move |a| {
                (
                    AccountKey {
                        server: s.origin.clone(),
                        account: a.id.clone(),
                    },
                    a,
                )
            })
        })
    }

    /// Removes an account; answers it. The active account moves to another signed-in one
    /// (the same server's first), else none.
    pub fn remove_account(&mut self, key: &AccountKey) -> Option<Account> {
        let server = self.server_mut(&key.server)?;
        let at = server.accounts.iter().position(|a| a.id == key.account)?;
        let gone = server.accounts.remove(at);
        if self.active.as_ref() == Some(key) {
            self.active = self.next_active(&key.server);
        }
        Some(gone)
    }

    /// A signed-in account to make active: one of `prefer`'s, else any.
    pub fn next_active(&self, prefer: &Origin) -> Option<AccountKey> {
        let signed_in = |(_, a): &(AccountKey, &Account)| a.state == AccountState::SignedIn;
        self.accounts()
            .filter(signed_in)
            .find(|(k, _)| &k.server == prefer)
            .or_else(|| self.accounts().find(signed_in))
            .map(|(k, _)| k)
    }

    /// Whether a signed-in account on any server is on Sew install `install_id` (the call
    /// engine's device keys of that install are still needed).
    pub fn install_in_use(&self, install_id: u64) -> bool {
        self.accounts()
            .any(|(_, a)| a.install_id == Some(install_id) && a.state == AccountState::SignedIn)
    }

    /// The account of `member_id` on `server`, when one is saved.
    pub fn member_account(&self, server: &Origin, member_id: &str) -> Option<AccountKey> {
        self.accounts()
            .find(|(k, a)| &k.server == server && a.member_id == member_id)
            .map(|(k, _)| k)
    }
}

/// What the webview sees of a server (never a secret).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerView {
    /// Its origin.
    pub origin: Origin,
    /// Its host (and port), to show.
    pub host: String,
    /// What it offers.
    pub info: LaneInfo,
    /// Its accounts.
    pub accounts: Vec<AccountView>,
}

/// What the webview sees of an account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountView {
    /// `{server, account}`.
    pub key: AccountKey,
    /// Sew's member id.
    pub member_id: String,
    /// What to show.
    pub label: Option<String>,
    /// The device name.
    pub device_name: String,
    /// Whether it is the active account.
    pub active: bool,
    /// Live or ended.
    #[serde(flatten)]
    pub state: AccountState,
}

/// The server list as the webview sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServersView {
    /// The saved servers.
    pub servers: Vec<ServerView>,
    /// The active account.
    pub active: Option<AccountKey>,
    /// The secret store's kind (`keychain`, `file`, `memory`, `host`); `null` before it opened.
    pub secret_store: Option<String>,
}

impl Book {
    /// The webview's view.
    pub fn view(&self, secret_store: Option<&str>) -> ServersView {
        ServersView {
            servers: self
                .servers
                .iter()
                .map(|s| ServerView {
                    origin: s.origin.clone(),
                    host: s.origin.authority().to_owned(),
                    info: s.info.clone(),
                    accounts: s
                        .accounts
                        .iter()
                        .map(|a| {
                            let key = AccountKey {
                                server: s.origin.clone(),
                                account: a.id.clone(),
                            };
                            AccountView {
                                active: self.active.as_ref() == Some(&key),
                                key,
                                member_id: a.member_id.clone(),
                                label: a.label.clone(),
                                device_name: a.device_name.clone(),
                                state: a.state.clone(),
                            }
                        })
                        .collect(),
                })
                .collect(),
            active: self.active.clone(),
            secret_store: secret_store.map(str::to_owned),
        }
    }
}

/// Where the book lives: a file, or memory only.
#[derive(Debug, Clone)]
pub enum BookStore {
    /// `servers.json` at this path.
    File(std::path::PathBuf),
    /// Memory only (tests, the fake mode).
    Memory,
}

impl BookStore {
    /// Reads the book (an empty one when there is no file yet).
    pub fn load(&self) -> std::io::Result<Book> {
        let BookStore::File(path) = self else {
            return Ok(Book::default());
        };
        match std::fs::read(path) {
            Ok(b) => {
                let book: Book = serde_json::from_slice(&b)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                if book.version > BOOK_VERSION {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "servers.json is version {} (this plugin reads {BOOK_VERSION})",
                            book.version
                        ),
                    ));
                }
                Ok(book)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Book::default()),
            Err(e) => Err(e),
        }
    }

    /// Writes the book.
    pub fn save(&self, book: &Book) -> std::io::Result<()> {
        let BookStore::File(path) = self else {
            return Ok(());
        };
        let bytes = serde_json::to_vec_pretty(book).map_err(std::io::Error::other)?;
        write_private(path, &bytes)
    }
}

/// Writes `bytes` to `path` through `<path>.tmp` and a rename, mode 0600 on Unix.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)
}

/// A session's account fields from a sign-in.
pub(crate) fn account_from_session(
    id: String,
    member_id: String,
    label: Option<String>,
    install_id: Option<u64>,
    session: &SessionInfo,
) -> Account {
    Account {
        id,
        member_id,
        label,
        install_id,
        session_id: Some(session.id.clone()),
        device_name: session.name.clone(),
        signed_in_at: session.created_at.clone(),
        state: AccountState::SignedIn,
    }
}
