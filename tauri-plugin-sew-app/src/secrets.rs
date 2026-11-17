//! Where the device bearer lives: the [`SecretStore`] trait and its stores.
//!
//! - `KeyringStore` (feature `keyring-store`, the default): the OS credential store (macOS
//!   Keychain, Windows Credential Manager, the Secret Service on Linux), service
//!   `sew-app.<app id>`, one entry per server and account.
//! - [`FileStore`]: a 0600 JSON file under the app's data directory. Used only when the app
//!   opts in ([`SecretChoice::OsOrFile`] or [`SecretChoice::File`]); the plugin then emits the
//!   `secret_storage_file` warning so the UI can say so.
//! - [`MemoryStore`]: tests and the fake mode.
//! - the host's own ([`SecretChoice::Host`]).
//!
//! Every method may block (a keychain prompt, D-Bus): the plugin calls them on a blocking
//! thread.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use crate::lane::Secret;

/// One entry: the server (its origin) and the account (the install's device id for it).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SecretKey {
    /// The App server's origin.
    pub server: String,
    /// The account's key on that server.
    pub account: String,
}

impl SecretKey {
    /// The entry's name in a store keyed by one string: `<origin> <account>` (a space never
    /// occurs in either).
    pub fn entry_name(&self) -> String {
        format!("{} {}", self.server, self.account)
    }
}

/// Why a store failed: what it was doing and the platform's kind of failure, never a secret.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct SecretError(pub String);

/// The device bearer's store. Every method may block.
pub trait SecretStore: Send + Sync + fmt::Debug + 'static {
    /// The secret, or `None` when there is none.
    fn get(&self, key: &SecretKey) -> Result<Option<Secret>, SecretError>;
    /// Writes (or replaces) the secret.
    fn set(&self, key: &SecretKey, secret: &Secret) -> Result<(), SecretError>;
    /// Deletes it; deleting a missing entry is not an error.
    fn delete(&self, key: &SecretKey) -> Result<(), SecretError>;
    /// Whether the store answers at all (a probe round trip). The default: yes.
    fn available(&self) -> bool {
        true
    }
    /// The store's kind for the UI and logs: `keychain`, `file`, `memory`, `host`.
    fn kind(&self) -> &'static str;
}

/// Phone keychains, for later: the access group (iOS) a store shares with the App's
/// extensions (a Notification Service Extension, a ReplayKit extension), so they can read
/// the device bearer. Desktop keychains have no groups; the desktop stores ignore it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeychainOptions {
    /// `kSecAttrAccessGroup` (`<team id>.<group>`) on iOS. Ignored on desktop.
    pub access_group: Option<String>,
}

/// Which store the plugin uses (`Builder::secret_store`).
#[derive(Clone, Default)]
pub enum SecretChoice {
    /// The OS credential store; when none answers, signing in fails with
    /// `secret_store_unavailable` (the default).
    #[default]
    Os,
    /// The OS credential store, else a 0600 file under the data directory, with the
    /// `secret_storage_file` warning. Only when the app opts in.
    OsOrFile,
    /// Always the 0600 file (a kiosk or a test box), with the warning.
    File,
    /// In memory only: every sign-in ends with the process (the fake mode, tests).
    Memory,
    /// The host's own store (a vault, K2's keyring).
    Host(Arc<dyn SecretStore>),
}

impl fmt::Debug for SecretChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SecretChoice::Os => "Os",
            SecretChoice::OsOrFile => "OsOrFile",
            SecretChoice::File => "File",
            SecretChoice::Memory => "Memory",
            SecretChoice::Host(_) => "Host",
        })
    }
}

/// The store a choice makes: the store, and whether it is the file fallback (warn).
///
/// `app_id` names the keychain service (`sew-app.<app id>`); `dir` holds the file store's
/// `secrets.json`. Blocking (it probes the OS store).
pub fn open_store(
    choice: &SecretChoice,
    app_id: &str,
    dir: &Path,
    keychain: &KeychainOptions,
) -> Result<(Arc<dyn SecretStore>, bool), SecretError> {
    let file = || -> Arc<dyn SecretStore> { Arc::new(FileStore::new(dir.join("secrets.json"))) };
    match choice {
        SecretChoice::Host(s) => Ok((Arc::clone(s), false)),
        SecretChoice::Memory => Ok((Arc::new(MemoryStore::default()), false)),
        SecretChoice::File => Ok((file(), true)),
        SecretChoice::Os | SecretChoice::OsOrFile => {
            let os = os_store(app_id, keychain);
            match os {
                Some(s) if s.available() => Ok((s, false)),
                _ if matches!(choice, SecretChoice::OsOrFile) => Ok((file(), true)),
                _ => Err(SecretError(
                    "no OS credential store answers (and the app did not opt in to the file store)"
                        .into(),
                )),
            }
        }
    }
}

#[cfg(feature = "keyring-store")]
fn os_store(app_id: &str, keychain: &KeychainOptions) -> Option<Arc<dyn SecretStore>> {
    Some(Arc::new(KeyringStore::new(app_id, keychain.clone())))
}

#[cfg(not(feature = "keyring-store"))]
fn os_store(_app_id: &str, _keychain: &KeychainOptions) -> Option<Arc<dyn SecretStore>> {
    None
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// In memory.
#[derive(Debug, Default)]
pub struct MemoryStore {
    map: Mutex<BTreeMap<SecretKey, Secret>>,
}

impl MemoryStore {
    /// The keys it holds (tests).
    pub fn keys(&self) -> Vec<SecretKey> {
        lock(&self.map).keys().cloned().collect()
    }
}

impl SecretStore for MemoryStore {
    fn get(&self, key: &SecretKey) -> Result<Option<Secret>, SecretError> {
        Ok(lock(&self.map).get(key).cloned())
    }
    fn set(&self, key: &SecretKey, secret: &Secret) -> Result<(), SecretError> {
        lock(&self.map).insert(key.clone(), secret.clone());
        Ok(())
    }
    fn delete(&self, key: &SecretKey) -> Result<(), SecretError> {
        lock(&self.map).remove(key);
        Ok(())
    }
    fn kind(&self) -> &'static str {
        "memory"
    }
}

/// A 0600 JSON file (`{"<origin> <account>": "<secret>"}`), written whole through a temporary
/// file and a rename. On Unix the file is created mode 0600; on Windows it inherits the
/// profile directory's ACL.
#[derive(Debug)]
pub struct FileStore {
    path: PathBuf,
    guard: Mutex<()>,
}

impl FileStore {
    /// The store at `path` (created on the first write).
    pub fn new(path: impl Into<PathBuf>) -> FileStore {
        FileStore {
            path: path.into(),
            guard: Mutex::new(()),
        }
    }

    /// Where it lives.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read(&self) -> Result<BTreeMap<String, String>, SecretError> {
        match std::fs::read(&self.path) {
            Ok(b) => serde_json::from_slice(&b)
                .map_err(|_| SecretError("the secret file does not parse".into())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(SecretError(format!("read the secret file: {}", e.kind()))),
        }
    }

    fn write(&self, map: &BTreeMap<String, String>) -> Result<(), SecretError> {
        let bytes = serde_json::to_vec(map).map_err(|_| SecretError("encode".into()))?;
        crate::servers::write_private(&self.path, &bytes)
            .map_err(|e| SecretError(format!("write the secret file: {}", e.kind())))
    }
}

impl SecretStore for FileStore {
    fn get(&self, key: &SecretKey) -> Result<Option<Secret>, SecretError> {
        let _g = lock(&self.guard);
        Ok(self.read()?.get(&key.entry_name()).map(Secret::new))
    }
    fn set(&self, key: &SecretKey, secret: &Secret) -> Result<(), SecretError> {
        let _g = lock(&self.guard);
        let mut map = self.read()?;
        map.insert(key.entry_name(), secret.expose().to_owned());
        self.write(&map)
    }
    fn delete(&self, key: &SecretKey) -> Result<(), SecretError> {
        let _g = lock(&self.guard);
        let mut map = self.read()?;
        if map.remove(&key.entry_name()).is_some() {
            self.write(&map)?;
        }
        Ok(())
    }
    fn kind(&self) -> &'static str {
        "file"
    }
}

/// The OS credential store through `keyring` (feature `keyring-store`): service
/// `sew-app.<app id>`, account `<origin> <account>`.
#[cfg(feature = "keyring-store")]
#[derive(Debug, Clone)]
pub struct KeyringStore {
    service: String,
    options: KeychainOptions,
}

#[cfg(feature = "keyring-store")]
impl KeyringStore {
    /// The store of service `sew-app.<app_id>`. `options.access_group` is for phone keychains
    /// and ignored here.
    pub fn new(app_id: &str, options: KeychainOptions) -> KeyringStore {
        KeyringStore {
            service: format!("sew-app.{app_id}"),
            options,
        }
    }

    /// The service name.
    pub fn service(&self) -> &str {
        &self.service
    }

    /// The options it was made with.
    pub fn options(&self) -> &KeychainOptions {
        &self.options
    }

    fn entry(&self, account: &str) -> Result<keyring::Entry, SecretError> {
        keyring::Entry::new(&self.service, account)
            .map_err(|e| SecretError(format!("os store: {}", kind_of(&e))))
    }
}

#[cfg(feature = "keyring-store")]
fn kind_of(e: &keyring::Error) -> &'static str {
    match e {
        keyring::Error::NoEntry => "no entry",
        keyring::Error::NoStorageAccess(_) => "no storage access",
        keyring::Error::PlatformFailure(_) => "platform failure",
        keyring::Error::BadEncoding(_) => "bad encoding",
        keyring::Error::TooLong(..) => "too long",
        keyring::Error::Invalid(..) => "invalid",
        keyring::Error::Ambiguous(_) => "ambiguous",
        _ => "other",
    }
}

#[cfg(feature = "keyring-store")]
impl SecretStore for KeyringStore {
    fn get(&self, key: &SecretKey) -> Result<Option<Secret>, SecretError> {
        match self.entry(&key.entry_name())?.get_password() {
            Ok(s) => Ok(Some(Secret::new(s))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(SecretError(format!("os store get: {}", kind_of(&e)))),
        }
    }
    fn set(&self, key: &SecretKey, secret: &Secret) -> Result<(), SecretError> {
        self.entry(&key.entry_name())?
            .set_password(secret.expose())
            .map_err(|e| SecretError(format!("os store set: {}", kind_of(&e))))
    }
    fn delete(&self, key: &SecretKey) -> Result<(), SecretError> {
        match self.entry(&key.entry_name())?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(SecretError(format!("os store delete: {}", kind_of(&e)))),
        }
    }
    fn available(&self) -> bool {
        let Ok(e) = self.entry("probe") else {
            return false;
        };
        match e.get_password() {
            Ok(_) | Err(keyring::Error::NoEntry) => true,
            Err(_) => false,
        }
    }
    fn kind(&self) -> &'static str {
        "keychain"
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn key(a: &str) -> SecretKey {
        SecretKey {
            server: "https://chat.example.com".into(),
            account: a.into(),
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let mut b = [0u8; 8];
        getrandom::fill(&mut b).unwrap();
        let d = std::env::temp_dir().join(format!(
            "sew-app-{name}-{}",
            b.iter().map(|x| format!("{x:02x}")).collect::<String>()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_file_store_round_trips_and_is_private() {
        let dir = scratch("file");
        let s = FileStore::new(dir.join("secrets.json"));
        assert_eq!(s.get(&key("a")).unwrap(), None);
        s.set(&key("a"), &Secret::new("sewdv_one")).unwrap();
        s.set(&key("b"), &Secret::new("sewdv_two")).unwrap();
        assert_eq!(s.get(&key("a")).unwrap(), Some(Secret::new("sewdv_one")));
        s.delete(&key("a")).unwrap();
        s.delete(&key("a")).unwrap();
        assert_eq!(s.get(&key("a")).unwrap(), None);
        assert_eq!(s.get(&key("b")).unwrap(), Some(Secret::new("sewdv_two")));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(s.path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "the secret file must be 0600");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_choice_picks_the_store_and_says_when_it_is_the_file() {
        let dir = scratch("choice");
        let (s, warn) =
            open_store(&SecretChoice::File, "x", &dir, &KeychainOptions::default()).unwrap();
        assert_eq!((s.kind(), warn), ("file", true));
        let (s, warn) = open_store(
            &SecretChoice::Memory,
            "x",
            &dir,
            &KeychainOptions::default(),
        )
        .unwrap();
        assert_eq!((s.kind(), warn), ("memory", false));
        let host: Arc<dyn SecretStore> = Arc::new(MemoryStore::default());
        let (s, warn) = open_store(
            &SecretChoice::Host(host),
            "x",
            &dir,
            &KeychainOptions::default(),
        )
        .unwrap();
        assert_eq!((s.kind(), warn), ("memory", false));
        // Without the keyring feature there is no OS store: Os refuses, OsOrFile falls back.
        #[cfg(not(feature = "keyring-store"))]
        {
            assert!(open_store(&SecretChoice::Os, "x", &dir, &KeychainOptions::default()).is_err());
            let (s, warn) = open_store(
                &SecretChoice::OsOrFile,
                "x",
                &dir,
                &KeychainOptions::default(),
            )
            .unwrap();
            assert_eq!((s.kind(), warn), ("file", true));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
