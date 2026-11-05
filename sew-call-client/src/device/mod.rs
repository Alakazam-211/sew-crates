//! The device store: the native equivalent of the browser's IndexedDB `sew-e2ee`
//! store.
//!
//! - **A host's own store** ([`DeviceKeyStore`], [`StoreChoice::Host`]): the host keeps the
//!   device's private key **and** its public record ([`DeviceKeyRecord`]: device id,
//!   certificate) itself, keyed by [`DeviceKeyId`] (install id, member id, `device_scope`), in
//!   its own keyring or vault. Nothing of the device is written under the data directory then;
//!   only the pins are. No `key_storage_file` warning.
//! - **The built-in stores.** The private key (the P-256 scalar) goes in a [`SecretStore`]. By
//!   default ([`StoreChoice::Auto`] without feature `keyring-store`, or [`StoreChoice::File`])
//!   that is `device-<device_scope>.key` beside the public data, mode 0600 in a 0700 directory
//!   ([`FileSecretStore`]), and the engine warns `key_storage_file` once. No pretend
//!   encryption: a key next to its own decryption key protects nothing. With the optional
//!   feature `keyring-store` (off by default), `Auto` prefers the OS credential store
//!   (`OsSecretStore`: macOS login Keychain, Windows Credential Manager, Linux Secret Service
//!   over `keyring`'s `async-secret-service`), service `sew-voice.<app identifier>`, account
//!   `<install_id>:<device_scope>`, and falls back to the file when it does not answer.
//! - **A hardware key** ([`HardwareDeviceKey`], [`StoreChoice::Hardware`]): the private key is
//!   a non-exportable P-256 key in the Secure Enclave or StrongBox/TEE, behind the host's
//!   trait; the public record stays in the [`DeviceFile`]. No software fallback: an error from
//!   the key fails the call (`e2ee_refused`); a key that is gone enrols a new device
//!   ([`hardware`]).
//! - **The public data** ([`DeviceFile`]): the TOFU `Pins` (CA, DS, `external_sender`) and,
//!   with the built-in stores, the `StoredDevice` (device id, certificate) in
//!   `<data dir>/sew-voice/<install_id>/device-<device_scope>.json`, 0600 in a 0700 directory,
//!   written atomically (temp file, fsync, rename). It implements `sew_e2ee::DeviceStore` for
//!   that one install and scope.
//! - **Losses** ([`DeviceVault::ensure_device`]): key lost or data lost, a new `device_scope` (a
//!   new login lineage), a certificate refused or revoked: enrol again (`POST /api/v1/devices`),
//!   keep the pins. Pins lost while a key survives: re-pinned on first sight, with a `pins_reset`
//!   warning. A pin mismatch is never re-pinned silently: only [`DeviceVault::reset`] (the host's
//!   explicit `reset_device`, a Rust API) clears the pins.

use std::fmt;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sew_e2ee::{
    DeviceIdentity, DeviceStore, Enrollment, MemberId, Pins, SoftwareSigner, TrustAnchors,
};
/// The device's platform in its enrolment (Sew's `DeviceEnrollRequest.platform`).
pub use sew_proto::e2ee::DevicePlatform;
use sew_proto::e2ee::reasons;
use zeroize::Zeroizing;

use crate::e2ee::transport::KeyTransport;

pub mod hardware;

#[cfg(feature = "sim-software-key")]
pub use hardware::SimSoftwareKey;
pub use hardware::{DeviceKey, HardwareDeviceKey, p1363_from_der};

// `sim-software-key` is for the iOS simulator and the x86 Android emulator only. An arm64
// Android emulator cannot be told from a phone at compile time, so it is refused too.
#[cfg(all(
    feature = "sim-software-key",
    not(any(
        all(target_os = "ios", target_abi = "sim"),
        all(target_os = "ios", target_arch = "x86_64"),
        all(
            target_os = "android",
            any(target_arch = "x86_64", target_arch = "x86")
        ),
    ))
))]
compile_error!(
    "sew-call-client: feature `sim-software-key` builds only for the iOS simulator \
     (aarch64-apple-ios-sim, x86_64-apple-ios) and the x86 Android emulator \
     (x86_64-linux-android, i686-linux-android); never ship it"
);

/// The platform this build enrols as, from `target_os`: iOS, Android, otherwise desktop.
pub const fn default_platform() -> DevicePlatform {
    if cfg!(target_os = "ios") {
        DevicePlatform::Ios
    } else if cfg!(target_os = "android") {
        DevicePlatform::Android
    } else {
        DevicePlatform::Desktop
    }
}

/// The device's public record in a [`DeviceKeyRecord`] (sew-e2ee's), re-exported so a host's
/// [`DeviceKeyStore`] needs no sew-e2ee dependency of its own.
pub use sew_e2ee::StoredDevice;

/// Where the device key is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum KeyStorage {
    /// The host's own store ([`DeviceKeyStore`]).
    Host,
    /// The OS credential store (feature `keyring-store`).
    Os,
    /// A 0600 file beside the public data (the fallback; warned as `key_storage_file`).
    File,
    /// Process memory only (tests, a host that keeps keys itself).
    Memory,
    /// The host's hardware key ([`HardwareDeviceKey`]: Secure Enclave, StrongBox/TEE).
    Hardware,
}

/// A warning for the host (the `errors` event: a warning, not a failure).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DeviceWarning {
    /// `key_storage_file`: no OS credential store; the key is in a file.
    KeyStorageFile,
    /// `pins_reset`: the pins were lost while the key survived; re-pinned on first sight.
    PinsReset,
}

impl DeviceWarning {
    /// The `errors.code`.
    pub fn code(self) -> &'static str {
        match self {
            DeviceWarning::KeyStorageFile => "key_storage_file",
            DeviceWarning::PinsReset => "pins_reset",
        }
    }
}

/// Why the device store failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DeviceError {
    /// Reading or writing the store failed.
    #[error("device store: {0}")]
    Store(String),
    /// The server refused the enrolment for good.
    #[error("enrolment refused ({status} {reason:?})")]
    EnrollRefused {
        /// HTTP status.
        status: u16,
        /// `details.reason`.
        reason: Option<String>,
    },
    /// The certificate Sew answered does not verify against the pinned CA.
    #[error("certificate refused: {0}")]
    Certificate(String),
    /// The hardware key could not answer (a platform error, a signature that is not DER or does
    /// not verify). The call's key setup fails (`e2ee_refused`); there is no software fallback.
    #[error("hardware key unavailable: {0}")]
    Unavailable(String),
}

/// Which device a [`DeviceKeyStore`] entry belongs to: one install of Sew, one member, one login
/// lineage (the call credential's `device_scope`; a new sign-in is a new scope and enrols a new
/// device).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceKeyId {
    /// Sew's install id (READY's, the trust anchors').
    pub install_id: u64,
    /// The member the device is enrolled for.
    pub member_id: MemberId,
    /// The login lineage (32 hex characters from Sew; sanitized to `[A-Za-z0-9_-]`, at most 64).
    pub device_scope: String,
}

impl DeviceKeyId {
    /// A stable text form, `<install_id>:<member_id>:<device_scope>`, for a host that keys a
    /// keyring or vault entry by one string.
    pub fn account(&self) -> String {
        format!(
            "{}:{}:{}",
            self.install_id, self.member_id, self.device_scope
        )
    }
}

/// What a [`DeviceKeyStore`] keeps for one device: the private key and the public record Sew
/// answered at enrolment (or the last renewal).
#[derive(Clone)]
pub struct DeviceKeyRecord {
    /// The device's P-256 private scalar (secret).
    pub key: Zeroizing<[u8; 32]>,
    /// The public record: device id, public key (SEC1), certificate, install and member.
    pub device: StoredDevice,
}

impl fmt::Debug for DeviceKeyRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceKeyRecord")
            .field("key", &"<redacted>")
            .field("install_id", &self.device.install_id)
            .field("member_id", &self.device.member_id)
            .field("device_id", &self.device.device_id)
            .finish()
    }
}

/// A host's own store for the device's private key and public record, e.g. K2's
/// keyring or vault. Give it to the engine with [`StoreChoice::Host`]
/// (`ClientConfig::with_device_key_store`, the Tauri plugin's `Builder::device_key_store`).
///
/// The engine calls it on a blocking thread (`tokio::task::spawn_blocking`), so calls may block
/// (D-Bus, the Keychain, a vault round trip). Rules:
///
/// - `load` answers `Ok(None)` for "no device" (never stored, cleared, or unreadable: a damaged
///   entry is a lost device, and the engine enrols again). An `Err` fails the call's key setup,
///   so keep it for a store that cannot answer at all.
/// - `store` replaces the whole entry (enrolment, renewal).
/// - `clear` removes the entry and succeeds when there is none. The engine clears a device Sew
///   refused or revoked and enrols again (the pins stay), and on the host's `reset_device`.
///
/// The pins (the server's CA, DS key and `external_sender`, TOFU) stay in the engine's data
/// directory either way.
pub trait DeviceKeyStore: Send + Sync + 'static {
    /// The device of `id`, if stored.
    fn load(&self, id: &DeviceKeyId) -> Result<Option<DeviceKeyRecord>, DeviceError>;
    /// Saves (replaces) the device of `id`.
    fn store(&self, id: &DeviceKeyId, record: &DeviceKeyRecord) -> Result<(), DeviceError>;
    /// Forgets the device of `id` (no error when absent).
    fn clear(&self, id: &DeviceKeyId) -> Result<(), DeviceError>;
}

/// An in-memory [`DeviceKeyStore`] (tests, and an example of the trait; never the engine's
/// default).
#[derive(Default)]
pub struct MemoryDeviceKeyStore {
    entries: std::sync::Mutex<std::collections::BTreeMap<DeviceKeyId, DeviceKeyRecord>>,
}

impl fmt::Debug for MemoryDeviceKeyStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MemoryDeviceKeyStore(***)")
    }
}

impl MemoryDeviceKeyStore {
    /// An empty store.
    pub fn new() -> MemoryDeviceKeyStore {
        MemoryDeviceKeyStore::default()
    }

    /// How many devices it holds.
    pub fn len(&self) -> usize {
        crate::e2ee::keys::lock(&self.entries).len()
    }

    /// Whether it holds none.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl DeviceKeyStore for MemoryDeviceKeyStore {
    fn load(&self, id: &DeviceKeyId) -> Result<Option<DeviceKeyRecord>, DeviceError> {
        Ok(crate::e2ee::keys::lock(&self.entries).get(id).cloned())
    }

    fn store(&self, id: &DeviceKeyId, record: &DeviceKeyRecord) -> Result<(), DeviceError> {
        crate::e2ee::keys::lock(&self.entries).insert(id.clone(), record.clone());
        Ok(())
    }

    fn clear(&self, id: &DeviceKeyId) -> Result<(), DeviceError> {
        crate::e2ee::keys::lock(&self.entries).remove(id);
        Ok(())
    }
}

/// A place for the device's private scalar alone, for the built-in stores (the public record
/// stays in [`DeviceFile`]). A host that keeps the whole device implements [`DeviceKeyStore`]
/// instead. Calls may block (D-Bus, the Keychain): the key
/// session runs them on a blocking thread.
pub trait SecretStore: Send + Sync + 'static {
    /// Which kind of store this is.
    fn kind(&self) -> KeyStorage;
    /// The scalar of `account`, if any.
    fn load(&self, account: &str) -> Result<Option<Zeroizing<[u8; 32]>>, DeviceError>;
    /// Saves (replaces) the scalar of `account`.
    fn save(&self, account: &str, scalar: &[u8; 32]) -> Result<(), DeviceError>;
    /// Deletes the scalar of `account` (no error when absent).
    fn delete(&self, account: &str) -> Result<(), DeviceError>;
}

/// An in-memory store (tests; never chosen by `StoreChoice::Auto`).
#[derive(Default)]
pub struct MemorySecretStore {
    keys: std::sync::Mutex<std::collections::BTreeMap<String, Zeroizing<[u8; 32]>>>,
}

impl fmt::Debug for MemorySecretStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MemorySecretStore(***)")
    }
}

impl MemorySecretStore {
    /// An empty store.
    pub fn new() -> MemorySecretStore {
        MemorySecretStore::default()
    }
}

impl SecretStore for MemorySecretStore {
    fn kind(&self) -> KeyStorage {
        KeyStorage::Memory
    }

    fn load(&self, account: &str) -> Result<Option<Zeroizing<[u8; 32]>>, DeviceError> {
        Ok(crate::e2ee::keys::lock(&self.keys).get(account).cloned())
    }

    fn save(&self, account: &str, scalar: &[u8; 32]) -> Result<(), DeviceError> {
        crate::e2ee::keys::lock(&self.keys).insert(account.to_owned(), Zeroizing::new(*scalar));
        Ok(())
    }

    fn delete(&self, account: &str) -> Result<(), DeviceError> {
        crate::e2ee::keys::lock(&self.keys).remove(account);
        Ok(())
    }
}

/// The file fallback: `device-<device_scope>.key` (64 hex characters), 0600.
#[derive(Debug, Clone)]
pub struct FileSecretStore {
    dir: PathBuf,
}

impl FileSecretStore {
    /// Keys in `dir` (the install's directory).
    pub fn new(dir: impl Into<PathBuf>) -> FileSecretStore {
        FileSecretStore { dir: dir.into() }
    }

    fn path(&self, account: &str) -> PathBuf {
        // `account` is `<install_id>:<device_scope>`; the install is the directory already.
        let scope = account.rsplit(':').next().unwrap_or(account);
        self.dir.join(format!("device-{}.key", sanitize(scope)))
    }
}

impl SecretStore for FileSecretStore {
    fn kind(&self) -> KeyStorage {
        KeyStorage::File
    }

    fn load(&self, account: &str) -> Result<Option<Zeroizing<[u8; 32]>>, DeviceError> {
        let p = self.path(account);
        let text = match std::fs::read_to_string(&p) {
            Ok(t) => Zeroizing::new(t),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(DeviceError::Store(format!("read key: {}", e.kind()))),
        };
        let mut out = Zeroizing::new([0u8; 32]);
        match hex::decode_to_slice(text.trim(), out.as_mut()) {
            Ok(()) => Ok(Some(out)),
            // A damaged key file is a lost key: enrol again.
            Err(_) => Ok(None),
        }
    }

    fn save(&self, account: &str, scalar: &[u8; 32]) -> Result<(), DeviceError> {
        ensure_dir(&self.dir)?;
        let text = Zeroizing::new(hex::encode(scalar));
        write_atomic(&self.path(account), text.as_bytes())
    }

    fn delete(&self, account: &str) -> Result<(), DeviceError> {
        match std::fs::remove_file(self.path(account)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(DeviceError::Store(format!("delete key: {}", e.kind()))),
        }
    }
}

/// The OS credential store through `keyring` (feature `keyring-store`).
#[cfg(feature = "keyring-store")]
#[derive(Debug, Clone)]
pub struct OsSecretStore {
    service: String,
}

#[cfg(feature = "keyring-store")]
impl OsSecretStore {
    /// The store of service `sew-voice.<app_id>`.
    pub fn new(app_id: &str) -> OsSecretStore {
        OsSecretStore {
            service: format!("sew-voice.{app_id}"),
        }
    }

    fn entry(&self, account: &str) -> Result<keyring::Entry, DeviceError> {
        keyring::Entry::new(&self.service, account)
            .map_err(|e| DeviceError::Store(format!("os store: {}", kind_of(&e))))
    }

    /// Whether the store answers at all (a round trip on a probe account). Blocking.
    pub fn available(&self) -> bool {
        let Ok(e) = self.entry("probe") else {
            return false;
        };
        match e.get_secret() {
            Ok(_) | Err(keyring::Error::NoEntry) => true,
            Err(_) => false,
        }
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
impl SecretStore for OsSecretStore {
    fn kind(&self) -> KeyStorage {
        KeyStorage::Os
    }

    fn load(&self, account: &str) -> Result<Option<Zeroizing<[u8; 32]>>, DeviceError> {
        match self.entry(account)?.get_secret() {
            Ok(v) => {
                let v = Zeroizing::new(v);
                let a: Option<[u8; 32]> = v.as_slice().try_into().ok();
                Ok(a.map(Zeroizing::new))
            }
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(DeviceError::Store(format!("os store: {}", kind_of(&e)))),
        }
    }

    fn save(&self, account: &str, scalar: &[u8; 32]) -> Result<(), DeviceError> {
        self.entry(account)?
            .set_secret(scalar)
            .map_err(|e| DeviceError::Store(format!("os store: {}", kind_of(&e))))
    }

    fn delete(&self, account: &str) -> Result<(), DeviceError> {
        match self.entry(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(DeviceError::Store(format!("os store: {}", kind_of(&e)))),
        }
    }
}

fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect()
}

fn ensure_dir(dir: &Path) -> Result<(), DeviceError> {
    std::fs::create_dir_all(dir).map_err(|e| DeviceError::Store(format!("mkdir: {}", e.kind())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| DeviceError::Store(format!("chmod dir: {}", e.kind())))?;
    }
    Ok(())
}

/// Writes `bytes` to `path` atomically: a 0600 temp file beside it, fsync, rename, fsync of the
/// directory (unix).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), DeviceError> {
    let dir = path
        .parent()
        .ok_or_else(|| DeviceError::Store("no parent directory".to_owned()))?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| DeviceError::Store("bad file name".to_owned()))?;
    let tmp = dir.join(format!(".{name}.{:016x}.tmp", rand::random::<u64>()));
    let io = |what: &str, e: std::io::Error| DeviceError::Store(format!("{what}: {}", e.kind()));
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp).map_err(|e| io("create", e))?;
        f.write_all(bytes).map_err(|e| io("write", e))?;
        f.sync_all().map_err(|e| io("fsync", e))?;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(io("rename", e));
    }
    #[cfg(unix)]
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

#[derive(Serialize, Deserialize, Default)]
struct FileData {
    v: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    device: Option<DeviceWire>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pins: Option<PinsWire>,
}

#[derive(Serialize, Deserialize, Clone)]
struct DeviceWire {
    member_id: MemberId,
    device_id: MemberId,
    pubkey: String,
    cert: String,
}

#[derive(Serialize, Deserialize, Clone)]
struct PinsWire {
    ca_public: String,
    ds_public: String,
    external_sender: String,
}

fn h<const N: usize>(s: &str) -> Option<[u8; N]> {
    let mut out = [0u8; N];
    hex::decode_to_slice(s, &mut out).ok()?;
    Some(out)
}

/// The public data of one install and `device_scope` (a `sew_e2ee::DeviceStore`).
pub struct DeviceFile {
    path: PathBuf,
    install_id: u64,
    data: FileData,
    existed: bool,
}

impl fmt::Debug for DeviceFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceFile")
            .field("install_id", &self.install_id)
            .field("device", &self.data.device.is_some())
            .field("pins", &self.data.pins.is_some())
            .finish()
    }
}

impl DeviceFile {
    /// Opens (or starts) `<dir>/device-<device_scope>.json`.
    pub fn open(
        dir: &Path,
        install_id: u64,
        device_scope: &str,
    ) -> Result<DeviceFile, DeviceError> {
        let path = dir.join(format!("device-{}.json", sanitize(device_scope)));
        let (data, existed) = match std::fs::read(&path) {
            Ok(b) => (serde_json::from_slice(&b).unwrap_or_default(), true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (FileData::default(), false),
            Err(e) => return Err(DeviceError::Store(format!("read: {}", e.kind()))),
        };
        Ok(DeviceFile {
            path,
            install_id,
            data,
            existed,
        })
    }

    /// The file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn flush(&self) -> Result<(), String> {
        let dir = self.path.parent().ok_or("no parent directory")?;
        ensure_dir(dir).map_err(|e| e.to_string())?;
        let mut data = serde_json::to_vec_pretty(&FileData {
            v: 1,
            device: self.data.device.clone(),
            pins: self.data.pins.clone(),
        })
        .map_err(|e| e.to_string())?;
        data.push(b'\n');
        write_atomic(&self.path, &data).map_err(|e| e.to_string())
    }

    /// Forgets the device and keeps the pins (a lost or refused device).
    pub fn forget_device(&mut self) -> Result<(), DeviceError> {
        self.data.device = None;
        self.flush().map_err(DeviceError::Store)
    }
}

impl DeviceStore for DeviceFile {
    fn load(&self, install_id: u64) -> Option<StoredDevice> {
        if install_id != self.install_id {
            return None;
        }
        let d = self.data.device.as_ref()?;
        Some(StoredDevice {
            install_id,
            member_id: d.member_id,
            device_id: d.device_id,
            pubkey: h::<65>(&d.pubkey)?,
            cert: hex::decode(&d.cert).ok()?,
        })
    }

    fn save(&mut self, dev: &StoredDevice) -> Result<(), String> {
        if dev.install_id != self.install_id {
            return Err("another install".to_owned());
        }
        self.data.device = Some(DeviceWire {
            member_id: dev.member_id,
            device_id: dev.device_id,
            pubkey: hex::encode(dev.pubkey),
            cert: hex::encode(&dev.cert),
        });
        self.flush()
    }

    fn clear(&mut self, install_id: u64) {
        if install_id == self.install_id {
            self.data.device = None;
            self.data.pins = None;
            let _ = self.flush();
        }
    }

    fn load_pins(&self, install_id: u64) -> Option<Pins> {
        if install_id != self.install_id {
            return None;
        }
        let p = self.data.pins.as_ref()?;
        Some(Pins {
            install_id,
            ca_public: h::<32>(&p.ca_public)?,
            ds_public: h::<32>(&p.ds_public)?,
            external_sender: hex::decode(&p.external_sender).ok()?,
        })
    }

    fn save_pins(&mut self, pins: &Pins) -> Result<(), String> {
        if pins.install_id != self.install_id {
            return Err("another install".to_owned());
        }
        self.data.pins = Some(PinsWire {
            ca_public: hex::encode(pins.ca_public),
            ds_public: hex::encode(pins.ds_public),
            external_sender: hex::encode(&pins.external_sender),
        });
        self.flush()
    }
}

/// Which device store to use.
#[derive(Clone, Default)]
#[non_exhaustive]
pub enum StoreChoice {
    /// The default. The 0600 key file (with `key_storage_file`); with feature `keyring-store`,
    /// the OS credential store when it answers, else the file.
    #[default]
    Auto,
    /// The file store, always (warned as `key_storage_file`).
    File,
    /// A key-only store the host provides; the public record stays in the data directory.
    Custom(Arc<dyn SecretStore>),
    /// The host's own store for the key and the public record ([`DeviceKeyStore`]).
    Host(Arc<dyn DeviceKeyStore>),
    /// The host's hardware key ([`HardwareDeviceKey`]); the public record stays in the data
    /// directory. Never falls back to a software key.
    Hardware(Arc<dyn HardwareDeviceKey>),
}

impl fmt::Debug for StoreChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreChoice::Auto => f.write_str("Auto"),
            StoreChoice::File => f.write_str("File"),
            StoreChoice::Custom(s) => write!(f, "Custom({:?})", s.kind()),
            StoreChoice::Host(_) => f.write_str("Host"),
            StoreChoice::Hardware(_) => f.write_str("Hardware"),
        }
    }
}

/// Where the vault keeps the key and the public record.
#[derive(Clone)]
enum Keys {
    /// The key in a [`SecretStore`], the record in the [`DeviceFile`].
    Local(Arc<dyn SecretStore>),
    /// Both in the host's [`DeviceKeyStore`].
    Host(Arc<dyn DeviceKeyStore>),
    /// The key in the host's hardware, the record in the [`DeviceFile`].
    Hardware(Arc<dyn HardwareDeviceKey>),
}

/// Where a vault lives.
#[derive(Debug, Clone)]
pub struct VaultConfig {
    /// The host's app data directory.
    pub data_dir: PathBuf,
    /// The host's app identifier (with `keyring-store`, the OS store's service is
    /// `sew-voice.<app_id>`).
    pub app_id: String,
    /// The secret store.
    pub store: StoreChoice,
    /// The device label Sew shows ("K2 on macOS"); trimmed to Sew's limit.
    pub label: String,
    /// The platform the device enrols as (default [`default_platform`], from `target_os`).
    pub platform: DevicePlatform,
}

/// One install's and login lineage's device: the public file, the secret store, and the
/// enrolment and recovery rules.
pub struct DeviceVault {
    file: DeviceFile,
    keys: Keys,
    account: String,
    id: DeviceKeyId,
    label: String,
    platform: DevicePlatform,
    warnings: Vec<DeviceWarning>,
}

impl fmt::Debug for DeviceVault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceVault")
            .field("file", &self.file)
            .field("storage", &self.storage())
            .finish_non_exhaustive()
    }
}

/// The platform's name in the default label.
pub fn platform_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macOS",
        "windows" => "Windows",
        "linux" => "Linux",
        "ios" => "iOS",
        "android" => "Android",
        _ => "desktop",
    }
}

impl DeviceVault {
    /// Opens the vault of `install_id`, `member` and `device_scope`. Blocking (the OS store
    /// probe, a host store's `load`): call it on a blocking thread from async code. With a
    /// hardware key the `pins_reset` probe (the key survived, the data did not) needs the key's
    /// async `public_key`: [`DeviceVault::open_async`] does both.
    pub fn open(
        cfg: &VaultConfig,
        install_id: u64,
        member: MemberId,
        device_scope: &str,
    ) -> Result<DeviceVault, DeviceError> {
        let dir = cfg.data_dir.join("sew-voice").join(install_id.to_string());
        let mut warnings = Vec::new();
        let keys = match &cfg.store {
            StoreChoice::Host(s) => Keys::Host(s.clone()),
            StoreChoice::Custom(s) => Keys::Local(s.clone()),
            StoreChoice::File => {
                warnings.push(DeviceWarning::KeyStorageFile);
                Keys::Local(Arc::new(FileSecretStore::new(dir.clone())))
            }
            StoreChoice::Auto => Keys::Local(auto_store(&cfg.app_id, &dir, &mut warnings)),
            StoreChoice::Hardware(k) => Keys::Hardware(k.clone()),
        };
        let file = DeviceFile::open(&dir, install_id, device_scope)?;
        let scope = sanitize(device_scope);
        let account = format!("{install_id}:{scope}");
        let id = DeviceKeyId {
            install_id,
            member_id: member,
            device_scope: scope,
        };
        let survives = match &keys {
            Keys::Local(s) => s.load(&account)?.is_some(),
            Keys::Host(s) => s.load(&id)?.is_some(),
            // Probed by `open_async`.
            Keys::Hardware(_) => false,
        };
        if !file.existed && survives {
            warnings.push(DeviceWarning::PinsReset);
        }
        Ok(DeviceVault {
            file,
            keys,
            account,
            id,
            label: cfg.label.clone(),
            platform: cfg.platform,
            warnings,
        })
    }

    /// [`DeviceVault::open`] on a blocking thread, then, with a hardware key, its `pins_reset`
    /// probe (the key's `public_key`). What the key session uses.
    pub async fn open_async(
        cfg: &VaultConfig,
        install_id: u64,
        member: MemberId,
        device_scope: &str,
    ) -> Result<DeviceVault, DeviceError> {
        let (cfg2, scope) = (cfg.clone(), device_scope.to_owned());
        let mut vault =
            blocking(move || DeviceVault::open(&cfg2, install_id, member, &scope)).await?;
        if let Keys::Hardware(k) = &vault.keys
            && !vault.file.existed
            && k.public_key(&vault.id).await?.is_some()
        {
            vault.warnings.push(DeviceWarning::PinsReset);
        }
        Ok(vault)
    }

    /// The platform the device enrols as.
    pub fn platform(&self) -> DevicePlatform {
        self.platform
    }

    /// Warnings for the host, once each.
    pub fn take_warnings(&mut self) -> Vec<DeviceWarning> {
        std::mem::take(&mut self.warnings)
    }

    /// Where the key is kept.
    pub fn storage(&self) -> KeyStorage {
        match &self.keys {
            Keys::Local(s) => s.kind(),
            Keys::Host(_) => KeyStorage::Host,
            Keys::Hardware(_) => KeyStorage::Hardware,
        }
    }

    /// The device this vault is for (the [`DeviceKeyStore`] key).
    pub fn id(&self) -> &DeviceKeyId {
        &self.id
    }

    /// The public data (the pins for `TrustAnchors::from_ready`).
    pub fn file_mut(&mut self) -> &mut DeviceFile {
        &mut self.file
    }

    /// The public data.
    pub fn file(&self) -> &DeviceFile {
        &self.file
    }

    /// The software device key, if stored (on a blocking thread). Always `None` with a hardware
    /// key, whose private half never leaves the hardware: see [`DeviceVault::device_key`].
    pub async fn signer(&self) -> Result<Option<SoftwareSigner>, DeviceError> {
        if matches!(self.keys, Keys::Hardware(_)) {
            return Ok(None);
        }
        Ok(self.stored().await?.1.and_then(DeviceKey::into_software))
    }

    /// The device key, software or hardware, if stored.
    pub async fn device_key(&self) -> Result<Option<DeviceKey>, DeviceError> {
        Ok(self.stored().await?.1)
    }

    /// The stored public record and key, each if present (on a blocking thread). A host record
    /// for another install or member is no device; a hardware key whose `public_key` answers
    /// `None` is a lost key.
    async fn stored(&self) -> Result<(Option<StoredDevice>, Option<DeviceKey>), DeviceError> {
        match &self.keys {
            Keys::Local(s) => {
                let (s, a) = (s.clone(), self.account.clone());
                let signer =
                    blocking(move || Ok(s.load(&a)?.and_then(|k| SoftwareSigner::from_bytes(&k))))
                        .await?;
                Ok((
                    self.file.load(self.file.install_id),
                    signer.map(DeviceKey::software),
                ))
            }
            Keys::Hardware(k) => {
                let key = match k.public_key(&self.id).await? {
                    Some(public) => Some(DeviceKey::hardware(k.clone(), self.id.clone(), public)?),
                    None => None,
                };
                Ok((self.file.load(self.file.install_id), key))
            }
            Keys::Host(s) => {
                let (s, id) = (s.clone(), self.id.clone());
                let rec = blocking(move || s.load(&id)).await?;
                Ok(match rec {
                    Some(r)
                        if r.device.install_id == self.id.install_id
                            && r.device.member_id == self.id.member_id =>
                    {
                        let signer = SoftwareSigner::from_bytes(&r.key).map(DeviceKey::software);
                        (Some(r.device), signer)
                    }
                    _ => (None, None),
                })
            }
        }
    }

    /// Saves the device and, when it is new (`key_changed`), its key: an enrolment or a
    /// renewal. A host store gets the whole record each time.
    async fn save_device(
        &mut self,
        stored: &StoredDevice,
        signer: &DeviceKey,
        key_changed: bool,
    ) -> Result<(), DeviceError> {
        let scalar = || {
            signer
                .software_bytes()
                .ok_or_else(|| DeviceError::Store("a hardware key in a software store".to_owned()))
        };
        match &self.keys {
            Keys::Local(s) => {
                if key_changed {
                    let (s, a) = (s.clone(), self.account.clone());
                    let k = scalar()?;
                    blocking(move || s.save(&a, &k)).await?;
                }
                self.file.save(stored).map_err(DeviceError::Store)
            }
            Keys::Host(s) => {
                let (s, id) = (s.clone(), self.id.clone());
                let rec = DeviceKeyRecord {
                    key: scalar()?,
                    device: stored.clone(),
                };
                blocking(move || s.store(&id, &rec)).await
            }
            // The key is the hardware's already (`generate`); only the record is ours.
            Keys::Hardware(_) => self.file.save(stored).map_err(DeviceError::Store),
        }
    }

    /// A new key for an enrolment: a software key, or the hardware's (`generate`, replacing
    /// any old one).
    async fn new_key(&self) -> Result<DeviceKey, DeviceError> {
        match &self.keys {
            Keys::Hardware(k) => {
                let public = k.generate(&self.id).await?;
                DeviceKey::hardware(k.clone(), self.id.clone(), public)
            }
            Keys::Local(_) | Keys::Host(_) => Ok(DeviceKey::software(SoftwareSigner::generate(
                &mut rand::rngs::OsRng,
            ))),
        }
    }

    async fn delete_key(&self) -> Result<(), DeviceError> {
        match &self.keys {
            Keys::Local(s) => {
                let (s, a) = (s.clone(), self.account.clone());
                blocking(move || s.delete(&a)).await
            }
            Keys::Host(s) => {
                let (s, id) = (s.clone(), self.id.clone());
                blocking(move || s.clear(&id)).await
            }
            Keys::Hardware(k) => k.delete(&self.id).await,
        }
    }

    /// Forgets the device and its key, keeping the pins (a refused or revoked device).
    pub async fn forget_device(&mut self) -> Result<(), DeviceError> {
        self.delete_key().await?;
        match self.keys {
            Keys::Local(_) | Keys::Hardware(_) => self.file.forget_device(),
            // Nothing of the device is in the file; the pins stay.
            Keys::Host(_) => Ok(()),
        }
    }

    /// The host's explicit `reset_device`: the device, its key **and** the pins. The next call
    /// re-pins on first sight.
    pub async fn reset(&mut self) -> Result<(), DeviceError> {
        self.delete_key().await?;
        let install = self.file.install_id;
        self.file.clear(install);
        Ok(())
    }

    /// The verified device of `me`, enrolling or renewing its certificate as needed:
    ///
    /// - a stored device with its key whose certificate verifies (pinned CA, member, key, not
    ///   in the CRL): used, renewed silently when under 7 days are left; a renewal answered
    ///   `422 device_revoked` or `404` enrols again;
    /// - anything else (key lost, data lost, refused, revoked): enrol again with a new key,
    ///   keeping the pins. `409 device_key_in_use` retries once with another new key.
    pub async fn ensure_device(
        &mut self,
        http: &KeyTransport,
        trust: &TrustAnchors,
        me: MemberId,
        now: u64,
    ) -> Result<(DeviceIdentity, DeviceKey), DeviceError> {
        if let (Some(stored), Some(signer)) = self.stored().await? {
            let key_matches = signer.public_key_sec1() == stored.pubkey;
            match DeviceIdentity::from_stored(stored, trust, me, now) {
                Ok(id) if key_matches => {
                    if !Enrollment::needs_renewal(&id, now) {
                        return Ok((id, signer));
                    }
                    match http.renew_device(id.device_id()).await {
                        Ok(resp) => {
                            let stored =
                                Enrollment::accept(resp, trust, me, &signer.public_key_sec1(), now)
                                    .map_err(|e| DeviceError::Certificate(e.to_string()))?;
                            self.save_device(&stored, &signer, false).await?;
                            let id = DeviceIdentity::from_stored(stored, trust, me, now)
                                .map_err(|e| DeviceError::Certificate(e.to_string()))?;
                            return Ok((id, signer));
                        }
                        Err(f) if is_device_refused(&f) || f.status == 404 => {
                            tracing::info!(target: "sew_call_client::device", "device refused on renew; enrolling again");
                        }
                        // Not renewed yet (transport, busy): the certificate is still valid.
                        Err(_) => return Ok((id, signer)),
                    }
                }
                _ => {
                    tracing::info!(target: "sew_call_client::device", "stored device unusable; enrolling again");
                }
            }
        }
        self.forget_device().await?;
        self.enroll(http, trust, me, now).await
    }

    /// Enrols with a fresh key (one retry on `409 device_key_in_use`) as
    /// [`DeviceVault::platform`]. With a hardware key the fresh key is the hardware's
    /// (`generate`).
    pub async fn enroll(
        &mut self,
        http: &KeyTransport,
        trust: &TrustAnchors,
        me: MemberId,
        now: u64,
    ) -> Result<(DeviceIdentity, DeviceKey), DeviceError> {
        let install = trust.install_id();
        let mut retried = false;
        loop {
            let signer = self.new_key().await?;
            let pubkey = signer.public_key_sec1();
            let (draft, req) = Enrollment::begin(&pubkey, self.platform, &self.label, install, me);
            let body = Enrollment::request(draft, signer.sign(&req.message).await?);
            let resp = match http.enroll(&body).await {
                Ok(r) => r,
                Err(f) if !retried && f.status == 409 && f.is(reasons::DEVICE_KEY_IN_USE) => {
                    retried = true;
                    continue;
                }
                Err(f) => {
                    return Err(DeviceError::EnrollRefused {
                        status: f.status,
                        reason: f.reason,
                    });
                }
            };
            let stored = Enrollment::accept(resp, trust, me, &pubkey, now)
                .map_err(|e| DeviceError::Certificate(e.to_string()))?;
            self.save_device(&stored, &signer, true).await?;
            let id = DeviceIdentity::from_stored(stored, trust, me, now)
                .map_err(|e| DeviceError::Certificate(e.to_string()))?;
            return Ok((id, signer));
        }
    }
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, DeviceError> + Send + 'static,
) -> Result<T, DeviceError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| DeviceError::Store("blocking task failed".to_owned()))?
}

fn auto_store(app_id: &str, dir: &Path, warnings: &mut Vec<DeviceWarning>) -> Arc<dyn SecretStore> {
    #[cfg(feature = "keyring-store")]
    {
        let os = OsSecretStore::new(app_id);
        if os.available() {
            return Arc::new(os);
        }
    }
    #[cfg(not(feature = "keyring-store"))]
    let _ = app_id;
    warnings.push(DeviceWarning::KeyStorageFile);
    Arc::new(FileSecretStore::new(dir.to_path_buf()))
}

/// Whether a key or device route refused the device itself (revoked, or its certificate
/// failed): Sew 0.7.1's rule is to forget it, enrol once and retry once.
pub fn is_device_refused(f: &sew_e2ee::ApiFailure) -> bool {
    use reasons::key_package_check as kc;
    (f.status == 422 && f.is(reasons::DEVICE_REVOKED))
        || (f.status == 422
            && f.is(reasons::E2EE_KEY_PACKAGE_INVALID)
            && matches!(f.detail.as_deref(), Some(d) if d == kc::DEVICE_CERT || d == kc::DEVICE_SIG))
}
