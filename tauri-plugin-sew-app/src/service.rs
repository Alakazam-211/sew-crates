//! The plugin's core: saved servers, sign-in, sign-out, the device sessions and the handoff,
//! with no Tauri in it (the commands in `commands.rs` are thin wrappers).

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri_plugin_sew_voice::BoxFuture;
use url::Url;

use crate::error::{AppError, code};
use crate::gateway::{GatewayTable, SocketConnector};
use crate::lane::{
    DeviceName, HostDoorFailure, LANE_VERSION, LaneClient, LaneFailure, LaneInfo, Secret,
    SessionInfo, SignIn, route,
};
use crate::origin::Origin;
use crate::secrets::{KeychainOptions, SecretChoice, SecretStore, open_store};
use crate::servers::{
    AccountKey, AccountState, AccountView, Book, BookStore, Server, ServersView,
    account_from_session,
};

/// How long an added server waits for its confirm.
pub const PENDING_TTL: Duration = Duration::from_secs(600);

/// How long a pairing link waits for the person's confirm (a pairing code's own life).
pub const PAIRING_TTL: Duration = Duration::from_secs(120);

/// The longest a door begin is kept for a retry (the lane's own 10 minutes, less a margin).
pub const DOOR_BEGIN_TTL: Duration = Duration::from_secs(570);

/// The core's settings (`Builder` fills them in).
#[derive(Debug, Clone)]
pub struct AppConfig {
    /// The app's identifier (the keychain service is `sew-app.<app id>`).
    pub app_id: String,
    /// Where `servers.json` (and the opt-in `secrets.json`) live; `None`: memory only.
    pub data_dir: Option<PathBuf>,
    /// The device name sign-ins send (what the person's device list shows).
    pub device_name: String,
    /// Allow `http://` servers on this machine's loopback (development only).
    pub allow_loopback_http: bool,
    /// The device bearer's store.
    pub secrets: SecretChoice,
    /// Phone keychain options (ignored on desktop).
    pub keychain: KeychainOptions,
    /// The App's page that redeems a handoff code from its fragment (`#code=…`), e.g.
    /// `/native/handoff`. `None`: `open` loads the page without a handoff.
    pub handoff_page: Option<String>,
    /// The shell's version (`x.y.z`), checked against a server's `info.shell.min` before a
    /// sign-in. Default: this plugin's version; `Builder` sets the app's own.
    pub shell_version: String,
}

impl AppConfig {
    /// Settings for `app_id`, with everything else at its default (memory only, the OS
    /// keychain, `Desktop` as the device name).
    pub fn new(app_id: impl Into<String>) -> AppConfig {
        AppConfig {
            app_id: app_id.into(),
            data_dir: None,
            device_name: default_device_name(),
            allow_loopback_http: false,
            secrets: SecretChoice::Os,
            keychain: KeychainOptions::default(),
            handoff_page: None,
            shell_version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}

/// `Desktop (macOS)`, `Desktop (Windows)`, `Desktop (Linux)`.
pub fn default_device_name() -> String {
    let os = match std::env::consts::OS {
        "macos" => "macOS",
        "windows" => "Windows",
        "linux" => "Linux",
        "ios" => "iOS",
        "android" => "Android",
        other => other,
    };
    format!("Desktop ({os})")
}

/// What the plugin tells the webview (`sew-app://<name>`).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum AppEvent {
    /// The server list or an account changed: the whole list (`sew-app://servers`).
    Servers(ServersView),
    /// The server ended an account's session (a revoke, the idle limit, a replacement, an
    /// admin): `sew-app://signed_out` `{key, reason}`. The bearer is already deleted.
    SignedOut {
        /// The account.
        key: AccountKey,
        /// Why (`detail.reason` of the lane's `signed_out`).
        reason: String,
    },
    /// A warning (`sew-app://warning`), e.g. `secret_storage_file`.
    Warning(AppError),
    /// A deep link the plugin did not navigate itself (`sew-app://deep_link`).
    DeepLink(crate::deeplink::DeepLinkEvent),
    /// A pairing link for a confirmed server came (`sew-app://pair_request`
    /// `{server, host, expires_in_ms}`): ask `Sign in to <host>?`, then `pair_confirm` or
    /// `pair_decline`. The code stays in the plugin.
    PairRequest {
        /// The confirmed server the link names.
        server: Origin,
        /// Its host, for the question.
        host: String,
        /// How long the confirm may wait.
        expires_in_ms: u64,
    },
    /// A pairing link was refused and nothing was redeemed (`sew-app://pair_refused`
    /// `{host, reason}`): `unknown_server` (not a confirmed server) or `bad_link` (no code).
    PairRefused {
        /// The host the link names (empty when it names none).
        host: String,
        /// Why.
        reason: String,
    },
}

impl AppEvent {
    /// The event's name after `sew-app://`.
    pub fn name(&self) -> &'static str {
        match self {
            AppEvent::Servers(_) => "servers",
            AppEvent::SignedOut { .. } => "signed_out",
            AppEvent::Warning(_) => "warning",
            AppEvent::DeepLink(_) => "deep_link",
            AppEvent::PairRequest { .. } => "pair_request",
            AppEvent::PairRefused { .. } => "pair_refused",
        }
    }

    /// `sew-app://<name>`.
    pub fn event_name(&self) -> String {
        format!("{}{}", crate::EVENT_PREFIX, self.name())
    }

    /// The JSON payload.
    pub fn payload(&self) -> serde_json::Value {
        let v = match self {
            AppEvent::Servers(s) => serde_json::to_value(s),
            AppEvent::SignedOut { key, reason } => {
                Ok(serde_json::json!({ "key": key, "reason": reason }))
            }
            AppEvent::Warning(w) => serde_json::to_value(w),
            AppEvent::DeepLink(d) => serde_json::to_value(d),
            AppEvent::PairRequest {
                server,
                host,
                expires_in_ms,
            } => Ok(serde_json::json!({
                "server": server, "host": host, "expires_in_ms": expires_in_ms
            })),
            AppEvent::PairRefused { host, reason } => {
                Ok(serde_json::json!({ "host": host, "reason": reason }))
            }
        };
        v.unwrap_or(serde_json::Value::Null)
    }
}

/// Where the core's events go.
pub trait AppEventSink: Send + Sync + 'static {
    /// One event.
    fn emit(&self, event: &AppEvent);
}

/// A sink that drops everything.
#[derive(Debug, Default)]
pub struct NoEvents;

impl AppEventSink for NoEvents {
    fn emit(&self, _event: &AppEvent) {}
}

/// The voice plugin's `forget_server`, called when the last signed-in account on a Sew install
/// is gone (a sign-out, a revoke): it deletes the call engine's device keys and pins of that
/// install. The Tauri glue wires it to `tauri-plugin-sew-voice`'s `VoiceControl`.
pub trait VoiceForget: Send + Sync + 'static {
    /// Forget install `install_id`.
    fn forget(&self, install_id: u64) -> BoxFuture<'_, Result<(), AppError>>;
}

/// An added server waiting for its confirm.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingServer {
    /// Its origin (what the confirm names).
    pub origin: Origin,
    /// Its host (and port), for the confirm screen.
    pub host: String,
    /// What it offers.
    pub info: LaneInfo,
    /// Whether it is saved already (a confirm then refreshes it).
    pub saved: bool,
    /// How long the confirm may wait.
    pub expires_in_ms: u64,
}

/// `sign_out`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignOutResult {
    /// Whether the server confirmed the session's end (`false`: it could not be reached; the
    /// session then ends by itself on the server's idle limit).
    pub server_signed_out: bool,
    /// The Sew install whose call device keys were forgotten (the last account on it), as a
    /// decimal string.
    pub forgot_install: Option<String>,
}

/// `handoff`'s answer: the code and where the page redeems it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffView {
    /// The single-use code (60 s; `Debug` never prints it).
    pub code: Secret,
    /// How long it lives.
    pub expires_in_ms: u64,
    /// `POST` it here on the cookie lane: `{code}`, the exact Origin and the App's CSRF header.
    pub redeem_url: String,
}

impl fmt::Debug for AppService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppService")
            .field("app_id", &self.inner.cfg.app_id)
            .finish_non_exhaustive()
    }
}

struct Pending {
    info: LaneInfo,
    at: Instant,
}

/// One sign-in through the host's door, held between the person's tries: a refused password
/// uses nothing, so a retry (or the new password) goes with the same challenge.
struct DoorAttempt {
    device: DeviceName,
    id: Secret,
    challenge: Secret,
    until: Instant,
    username: String,
    change_token: Option<Secret>,
}

/// A door attempt's key: the server and the account it signs in again (`None`: a new one).
type DoorKey = (Origin, Option<String>);

struct State {
    book: Book,
    pending: HashMap<Origin, Pending>,
    secret_kind: Option<&'static str>,
    doors: HashMap<DoorKey, DoorAttempt>,
    /// The host door's `Retry-After`: no password goes there before it.
    door_wait: HashMap<Origin, Instant>,
    /// Pairing links waiting for the person's confirm.
    pairs: HashMap<Origin, (Secret, Instant)>,
}

struct Inner {
    cfg: AppConfig,
    lane: LaneClient,
    store: BookStore,
    state: Mutex<State>,
    secrets: tokio::sync::OnceCell<Arc<dyn SecretStore>>,
    sink: Arc<dyn AppEventSink>,
    forget: Mutex<Option<Arc<dyn VoiceForget>>>,
    /// The account of the call the voice provider joined (re-joins and consent use it).
    call_account: Mutex<Option<AccountKey>>,
    /// The bearers read from the secret store, so each API call doesn't ask the keychain.
    bearers: Mutex<HashMap<AccountKey, Secret>>,
    /// One keychain read at a time: a burst of calls on a cold cache reads each bearer once.
    bearer_read: tokio::sync::Mutex<()>,
    /// The open gateway sockets.
    gateways: GatewayTable,
    /// Opens the gateway sockets.
    connector: Mutex<Option<Arc<dyn SocketConnector>>>,
}

/// The core. Cheap to clone (an `Arc`).
#[derive(Clone)]
pub struct AppService {
    inner: Arc<Inner>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Unix seconds, read through the voice SDK's system clock (the one wall-clock source).
fn now_secs() -> u64 {
    (tauri_plugin_sew_voice::sew_call_client::system_clock())() / 1000
}

/// A new account id: 16 random bytes in base64url (22 characters).
pub fn new_account_id() -> Result<String, AppError> {
    use base64::Engine as _;
    let mut b = [0u8; 16];
    getrandom::fill(&mut b)
        .map_err(|_| AppError::with_detail(code::SECRET_STORE, "no random source"))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b))
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, AppError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| AppError::with_detail(code::SECRET_STORE, "the store's thread failed"))
}

/// Whether `s` reads as a pairing code (1 to 256 base64url characters).
fn is_pairing_code(s: &str) -> bool {
    (1..=256).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn info_failure(f: LaneFailure) -> AppError {
    match f {
        LaneFailure::Unreachable(e) => AppError::with_detail(code::UNREACHABLE, e.0),
        LaneFailure::BadAnswer(_) => AppError::new(code::NOT_A_SEW_APP),
        LaneFailure::Refused(r) if r.status == 404 => AppError::new(code::NOT_A_SEW_APP),
        LaneFailure::Refused(r) => r.to_error(),
    }
}

impl AppService {
    /// The core over `lane`, reading the saved book (an unreadable book is an error: the
    /// plugin never overwrites what it can't read).
    pub fn new(
        cfg: AppConfig,
        lane: LaneClient,
        sink: Arc<dyn AppEventSink>,
    ) -> Result<AppService, AppError> {
        let store = match &cfg.data_dir {
            Some(d) => BookStore::File(d.join("servers.json")),
            None => BookStore::Memory,
        };
        let book = store
            .load()
            .map_err(|e| AppError::with_detail(code::STORE, e.to_string()))?;
        Ok(AppService {
            inner: Arc::new(Inner {
                cfg,
                lane,
                store,
                state: Mutex::new(State {
                    book,
                    pending: HashMap::new(),
                    secret_kind: None,
                    doors: HashMap::new(),
                    door_wait: HashMap::new(),
                    pairs: HashMap::new(),
                }),
                secrets: tokio::sync::OnceCell::new(),
                sink,
                forget: Mutex::new(None),
                call_account: Mutex::new(None),
                bearers: Mutex::new(HashMap::new()),
                bearer_read: tokio::sync::Mutex::new(()),
                gateways: GatewayTable::default(),
                connector: Mutex::new(None),
            }),
        })
    }

    /// The settings.
    pub fn config(&self) -> &AppConfig {
        &self.inner.cfg
    }

    /// The lane client.
    pub fn lane(&self) -> &LaneClient {
        &self.inner.lane
    }

    /// Wires the voice plugin's `forget_server`.
    pub fn set_voice_forget(&self, forget: Arc<dyn VoiceForget>) {
        *lock(&self.inner.forget) = Some(forget);
    }

    /// What opens the gateway sockets (`gateway_open`). `Builder` sets the WebSocket one
    /// (feature `http`) or the fake lane's.
    pub fn set_socket_connector(&self, connector: Arc<dyn SocketConnector>) {
        *lock(&self.inner.connector) = Some(connector);
    }

    pub(crate) fn socket_connector(&self) -> Option<Arc<dyn SocketConnector>> {
        lock(&self.inner.connector).clone()
    }

    pub(crate) fn gateways(&self) -> &GatewayTable {
        &self.inner.gateways
    }

    /// Sends an event.
    pub fn emit(&self, event: &AppEvent) {
        self.inner.sink.emit(event);
    }

    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.inner.state)
    }

    /// Saves the book and tells the webview.
    fn commit(&self, st: &MutexGuard<'_, State>) -> Result<(), AppError> {
        let r = self
            .inner
            .store
            .save(&st.book)
            .map_err(|e| AppError::with_detail(code::STORE, e.to_string()));
        self.inner
            .sink
            .emit(&AppEvent::Servers(st.book.view(st.secret_kind)));
        r
    }

    async fn secrets(&self) -> Result<Arc<dyn SecretStore>, AppError> {
        let s = self
            .inner
            .secrets
            .get_or_try_init(|| async {
                let cfg = self.inner.cfg.clone();
                let dir = cfg
                    .data_dir
                    .clone()
                    .unwrap_or_else(|| std::env::temp_dir().join("sew-app-unused"));
                let opened =
                    blocking(move || open_store(&cfg.secrets, &cfg.app_id, &dir, &cfg.keychain))
                        .await?;
                let (store, file) = opened
                    .map_err(|e| AppError::with_detail(code::SECRET_STORE_UNAVAILABLE, e.0))?;
                if file {
                    tracing::warn!("sew-app: the device bearer is kept in a 0600 file");
                    self.emit(&AppEvent::Warning(AppError::with_detail(
                        code::SECRET_STORAGE_FILE,
                        "no OS credential store answers: the device session is kept in a \
                         file only this user can read",
                    )));
                }
                self.state().secret_kind = Some(store.kind());
                Ok::<_, AppError>(store)
            })
            .await?;
        Ok(Arc::clone(s))
    }

    async fn secret_get(&self, key: &AccountKey) -> Result<Option<Secret>, AppError> {
        if let Some(s) = lock(&self.inner.bearers).get(key) {
            return Ok(Some(s.clone()));
        }
        // Single flight: the calls that found the cache cold wait here, and all but the first
        // find it filled.
        let _one = self.inner.bearer_read.lock().await;
        if let Some(s) = lock(&self.inner.bearers).get(key) {
            return Ok(Some(s.clone()));
        }
        let store = self.secrets().await?;
        let k = key.secret_key();
        let got = blocking(move || store.get(&k))
            .await?
            .map_err(|e| AppError::with_detail(code::SECRET_STORE, e.0))?;
        if let Some(s) = &got {
            lock(&self.inner.bearers).insert(key.clone(), s.clone());
        }
        Ok(got)
    }

    async fn secret_set(&self, key: &AccountKey, s: Secret) -> Result<(), AppError> {
        // Under the read lock, so a read that began before this write can't cache the old one.
        let _one = self.inner.bearer_read.lock().await;
        lock(&self.inner.bearers).remove(key);
        let store = self.secrets().await?;
        let k = key.secret_key();
        let kept = s.clone();
        blocking(move || store.set(&k, &s))
            .await?
            .map_err(|e| AppError::with_detail(code::SECRET_STORE, e.0))?;
        lock(&self.inner.bearers).insert(key.clone(), kept);
        Ok(())
    }

    async fn secret_delete(&self, key: &AccountKey) -> Result<(), AppError> {
        let _one = self.inner.bearer_read.lock().await;
        lock(&self.inner.bearers).remove(key);
        let store = self.secrets().await?;
        let k = key.secret_key();
        blocking(move || store.delete(&k))
            .await?
            .map_err(|e| AppError::with_detail(code::SECRET_STORE, e.0))
    }

    // ── servers ──

    /// The saved servers and accounts.
    pub fn servers(&self) -> ServersView {
        let st = self.state();
        st.book.view(st.secret_kind)
    }

    /// A copy of the saved book.
    pub fn book(&self) -> Book {
        self.state().book.clone()
    }

    /// Reads `input` as a server address, asks it what it offers, and holds it for
    /// [`AppService::confirm_server`] (the person checks the host first; nothing is saved yet).
    pub async fn add_server(&self, input: &str) -> Result<PendingServer, AppError> {
        let origin = Origin::parse(input, self.inner.cfg.allow_loopback_http)?;
        let info = self.inner.lane.info(&origin).await.map_err(info_failure)?;
        if info.lane < LANE_VERSION {
            return Err(AppError::with_detail(
                code::LANE_TOO_OLD,
                serde_json::json!({ "lane": info.lane, "needs": LANE_VERSION }),
            ));
        }
        let mut st = self.state();
        let saved = st.book.server(&origin).is_some();
        st.pending.retain(|_, p| p.at.elapsed() < PENDING_TTL);
        st.pending.insert(
            origin.clone(),
            Pending {
                info: info.clone(),
                at: Instant::now(),
            },
        );
        Ok(PendingServer {
            host: origin.authority().to_owned(),
            origin,
            info,
            saved,
            expires_in_ms: u64::try_from(PENDING_TTL.as_millis()).unwrap_or(u64::MAX),
        })
    }

    /// Saves a server [`AppService::add_server`] holds (`not_pending` when it is not held, or
    /// held longer than [`PENDING_TTL`]).
    pub fn confirm_server(&self, origin: &Origin) -> Result<ServersView, AppError> {
        let mut st = self.state();
        let Some(p) = st.pending.remove(origin) else {
            return Err(AppError::new(code::NOT_PENDING));
        };
        if p.at.elapsed() >= PENDING_TTL {
            return Err(AppError::new(code::NOT_PENDING));
        }
        match st.book.server_mut(origin) {
            Some(s) => s.info = p.info,
            None => st.book.servers.push(Server {
                origin: origin.clone(),
                info: p.info,
                confirmed_at: now_secs(),
                accounts: Vec::new(),
            }),
        }
        self.commit(&st)?;
        Ok(st.book.view(st.secret_kind))
    }

    /// Signs every account of the server out (best effort) and forgets the server.
    pub async fn remove_server(&self, origin: &Origin) -> Result<ServersView, AppError> {
        let keys: Vec<AccountKey> = {
            let st = self.state();
            let Some(s) = st.book.server(origin) else {
                return Err(AppError::new(code::UNKNOWN_SERVER));
            };
            s.accounts
                .iter()
                .map(|a| AccountKey {
                    server: origin.clone(),
                    account: a.id.clone(),
                })
                .collect()
        };
        for k in keys {
            if let Err(e) = self.sign_out(&k).await {
                tracing::warn!(code = %e.code, "sew-app: sign-out while removing a server");
            }
        }
        let mut st = self.state();
        st.book.servers.retain(|s| &s.origin != origin);
        if st.book.active.as_ref().is_some_and(|a| &a.server == origin) {
            st.book.active = st.book.next_active(origin);
        }
        self.commit(&st)?;
        Ok(st.book.view(st.secret_kind))
    }

    /// Asks a saved server what it offers now (and saves the answer).
    pub async fn refresh_info(&self, origin: &Origin) -> Result<LaneInfo, AppError> {
        self.saved_server(origin)?;
        let info = self.inner.lane.info(origin).await.map_err(info_failure)?;
        let mut st = self.state();
        if let Some(s) = st.book.server_mut(origin)
            && s.info != info
        {
            s.info = info.clone();
            self.commit(&st)?;
        }
        Ok(info)
    }

    fn saved_server(&self, origin: &Origin) -> Result<Server, AppError> {
        self.state()
            .book
            .server(origin)
            .cloned()
            .ok_or_else(|| AppError::new(code::UNKNOWN_SERVER))
    }

    /// Makes `key` the active account (the voice provider's and `open`'s default).
    pub fn set_active(&self, key: &AccountKey) -> Result<ServersView, AppError> {
        let mut st = self.state();
        if st.book.account(key).is_none() {
            return Err(AppError::new(code::UNKNOWN_ACCOUNT));
        }
        st.book.active = Some(key.clone());
        self.commit(&st)?;
        Ok(st.book.view(st.secret_kind))
    }

    /// The active account.
    pub fn active(&self) -> Option<AccountKey> {
        self.state().book.active.clone()
    }

    /// `key`, else the active account (`no_account`).
    pub fn account_or_active(&self, key: Option<AccountKey>) -> Result<AccountKey, AppError> {
        key.or_else(|| self.active())
            .ok_or_else(|| AppError::new(code::NO_ACCOUNT))
    }

    // ── sign-in ──

    /// The device id for a sign-in on `origin`: `account`'s own (signing it in again), or a
    /// new one.
    fn device_for(&self, origin: &Origin, account: Option<&str>) -> Result<DeviceName, AppError> {
        let id = match account {
            Some(a) => {
                let st = self.state();
                let key = AccountKey {
                    server: origin.clone(),
                    account: a.to_owned(),
                };
                if st.book.account(&key).is_none() {
                    return Err(AppError::new(code::UNKNOWN_ACCOUNT));
                }
                a.to_owned()
            }
            None => new_account_id()?,
        };
        let d = DeviceName {
            id,
            name: self.inner.cfg.device_name.clone(),
        };
        d.validate()?;
        Ok(d)
    }

    /// Signs in with the server's password form (`password_unavailable` when it has none).
    /// `account` signs that account in again (keeping its device id); `None` adds one.
    pub async fn sign_in_password(
        &self,
        origin: &Origin,
        username: &str,
        password: Secret,
        account: Option<&str>,
    ) -> Result<AccountView, AppError> {
        if username.is_empty() || password.expose().is_empty() {
            return Err(AppError::invalid_args("a username and a password"));
        }
        let info = self.refresh_info(origin).await?;
        self.check_shell(&info)?;
        if !info.signin.password {
            return Err(AppError::new(code::PASSWORD_UNAVAILABLE));
        }
        let device = self.device_for(origin, account)?;
        let s = self
            .inner
            .lane
            .sign_in_password(origin, username, &password, &device)
            .await?;
        self.finish_sign_in(origin, device, s, Some(username)).await
    }

    /// Redeems a pairing code a signed-in page showed (`pairing_unavailable` when the server
    /// has no pairing).
    pub async fn pair(
        &self,
        origin: &Origin,
        code: Secret,
        account: Option<&str>,
    ) -> Result<AccountView, AppError> {
        if code.expose().is_empty() {
            return Err(AppError::invalid_args("a pairing code"));
        }
        let info = self.refresh_info(origin).await?;
        self.check_shell(&info)?;
        if !info.signin.pairing {
            return Err(AppError::new(code::PAIRING_UNAVAILABLE));
        }
        let device = self.device_for(origin, account)?;
        let s = self.inner.lane.pair_redeem(origin, &code, &device).await?;
        self.finish_sign_in(origin, device, s, None).await
    }

    /// `shell_too_old` when the server's `info.shell.min` is above this shell's version.
    fn check_shell(&self, info: &LaneInfo) -> Result<(), AppError> {
        let version = &self.inner.cfg.shell_version;
        match info.shell.too_old(version) {
            Some(false) => Ok(()),
            Some(true) => Err(AppError::with_detail(
                code::SHELL_TOO_OLD,
                serde_json::json!({ "min": info.shell.min, "version": version }),
            )),
            None => {
                // A version that doesn't read can't be compared; the server decides.
                tracing::warn!(min = ?info.shell.min, %version, "sew-app: shell.min unreadable");
                Ok(())
            }
        }
    }

    // ── the host's sign-in door ──

    /// Signs in through the host's sign-in door, all in Rust: checks the host's
    /// `/.k2/auth/ping`, asks the lane for a begin (a one-time id and a PKCE challenge) and
    /// checks that its `door` is this server's own `/.k2/auth/sign-in`, posts
    /// `{username, password, challenge}` there (no Origin), and trades the door's code for a
    /// device session at the lane's finish. The password, the code, the id and the challenge
    /// never leave the plugin.
    ///
    /// Refusals: `signin_door_unavailable` (not offered, or the ping isn't `{"k2": true}`: no
    /// password was sent), `signin_door_mismatch`, `shell_too_old`, and the host's own codes:
    /// `bad_login`, `locked`, `rate_limited` (`detail.retry_after_ms`; until then the plugin
    /// sends nothing there), `password_change_required` (then
    /// [`AppService::sign_in_door_set_password`]). A refused password keeps the begin, so a
    /// retry goes with the same challenge.
    pub async fn sign_in_door(
        &self,
        origin: &Origin,
        username: &str,
        password: Secret,
        account: Option<&str>,
    ) -> Result<AccountView, AppError> {
        if username.is_empty() || password.expose().is_empty() {
            return Err(AppError::invalid_args("a username and a password"));
        }
        let info = self.refresh_info(origin).await?;
        self.check_shell(&info)?;
        if !info.signin.door {
            return Err(AppError::new(code::SIGNIN_DOOR_UNAVAILABLE));
        }
        self.door_wait(origin)?;
        let ping = self
            .inner
            .lane
            .host_door_ping(origin)
            .await
            .map_err(|e| AppError::with_detail(code::UNREACHABLE, e.0))?;
        if !ping {
            return Err(AppError::with_detail(
                code::SIGNIN_DOOR_UNAVAILABLE,
                serde_json::json!({ "reason": "ping" }),
            ));
        }
        let key: DoorKey = (origin.clone(), account.map(str::to_owned));
        let held = self.state().doors.remove(&key);
        let mut attempt = match held {
            Some(a) if a.until > Instant::now() && a.change_token.is_none() => a,
            _ => self.door_begin(origin, account, username).await?,
        };
        attempt.username = username.to_owned();
        let r = self
            .inner
            .lane
            .host_door_sign_in(origin, username, &password, &attempt.challenge)
            .await;
        match r {
            Ok(door_code) => self.door_finish(origin, attempt, door_code).await,
            Err(f) => Err(self.door_refused(origin, key, attempt, f)),
        }
    }

    /// After `password_change_required`: sets the person's first password at the host's
    /// `/.k2/auth/set-password` with the held change token and challenge, then finishes the
    /// sign-in as [`AppService::sign_in_door`] does. `server` and `account` are the ones the
    /// sign-in named. Refusals: `no_password_change`, `password_set_no_signin` (the password
    /// is set but the person can't use this App), and the host's own (`password_too_short`,
    /// `password_too_long`, `password_unchanged`, `bad_token`, `rate_limited`, ...).
    pub async fn sign_in_door_set_password(
        &self,
        origin: &Origin,
        new_password: Secret,
        account: Option<&str>,
    ) -> Result<AccountView, AppError> {
        if new_password.expose().is_empty() {
            return Err(AppError::invalid_args("a new password"));
        }
        self.saved_server(origin)?;
        self.door_wait(origin)?;
        let key: DoorKey = (origin.clone(), account.map(str::to_owned));
        let held = self.state().doors.remove(&key);
        let attempt = match held {
            Some(a) if a.until > Instant::now() && a.change_token.is_some() => a,
            _ => return Err(AppError::new(code::NO_PASSWORD_CHANGE)),
        };
        let Some(token) = attempt.change_token.clone() else {
            return Err(AppError::new(code::NO_PASSWORD_CHANGE));
        };
        let r = self
            .inner
            .lane
            .host_door_set_password(origin, &token, &new_password, &attempt.challenge)
            .await;
        match r {
            Ok(Some(door_code)) => self.door_finish(origin, attempt, door_code).await,
            Ok(None) => Err(AppError::new(code::PASSWORD_SET_NO_SIGNIN)),
            Err(f) => Err(self.door_refused(origin, key, attempt, f)),
        }
    }

    /// `rate_limited` while the host door's `Retry-After` runs for `origin`.
    fn door_wait(&self, origin: &Origin) -> Result<(), AppError> {
        let mut st = self.state();
        let now = Instant::now();
        st.door_wait.retain(|_, until| *until > now);
        match st.door_wait.get(origin) {
            Some(until) => {
                let ms = u64::try_from((*until - now).as_millis())
                    .unwrap_or(u64::MAX)
                    .max(1);
                Err(AppError::with_detail(
                    "rate_limited",
                    serde_json::json!({ "retry_after_ms": ms }),
                ))
            }
            None => Ok(()),
        }
    }

    /// A new begin at the lane, its `door` checked against the confirmed server.
    async fn door_begin(
        &self,
        origin: &Origin,
        account: Option<&str>,
        username: &str,
    ) -> Result<DoorAttempt, AppError> {
        let device = self.device_for(origin, account)?;
        let start = self.inner.lane.door_begin(origin, &device).await?;
        let own = origin.join(route::HOST_DOOR_SIGN_IN)?;
        if Url::parse(&start.door).ok().as_ref() != Some(&own) {
            return Err(AppError::with_detail(
                code::SIGNIN_DOOR_MISMATCH,
                serde_json::json!({ "expected": own.as_str() }),
            ));
        }
        let ttl = start
            .expires_in_ms
            .map(|ms| Duration::from_millis(ms.saturating_sub(30_000)))
            .unwrap_or(DOOR_BEGIN_TTL)
            .min(DOOR_BEGIN_TTL);
        Ok(DoorAttempt {
            device,
            id: start.id,
            challenge: start.challenge,
            until: Instant::now() + ttl,
            username: username.to_owned(),
            change_token: None,
        })
    }

    /// The lane's finish (the id is spent, whatever it answers), then the account.
    async fn door_finish(
        &self,
        origin: &Origin,
        attempt: DoorAttempt,
        door_code: Secret,
    ) -> Result<AccountView, AppError> {
        let s = self
            .inner
            .lane
            .door_finish(origin, &attempt.id, &door_code, &attempt.device)
            .await?;
        self.finish_sign_in(origin, attempt.device, s, Some(&attempt.username))
            .await
    }

    /// A host door refusal: keeps the begin for a retry (a refused password uses nothing),
    /// with the change token when the host asks for a new password; honours `Retry-After`.
    fn door_refused(
        &self,
        origin: &Origin,
        key: DoorKey,
        mut attempt: DoorAttempt,
        f: HostDoorFailure,
    ) -> AppError {
        let e = f.failure.to_error();
        let Some(r) = f.failure.refusal() else {
            // Unreachable or unreadable: the challenge may still be good.
            self.state().doors.insert(key, attempt);
            return e;
        };
        if r.status == 429 || r.status == 503 || e.code == "rate_limited" {
            let wait = Duration::from_millis(r.retry_after_ms.unwrap_or(1000));
            self.state()
                .door_wait
                .insert(origin.clone(), Instant::now() + wait);
        }
        match e.code.as_str() {
            code::PASSWORD_CHANGE_REQUIRED => match f.change_token {
                Some(t) => attempt.change_token = Some(t),
                // Nothing to set it with: begin again next time.
                None => return e,
            },
            // The change token is dead: sign in again.
            "bad_token" | "challenge_required" => return e,
            _ => {}
        }
        self.state().doors.insert(key, attempt);
        e
    }

    // ── pairing links ──

    /// A pairing link (`<scheme>://<host>/native/pair#code=…`) came: for a confirmed server,
    /// holds the code for [`PAIRING_TTL`] and sends `pair_request` (the shell asks
    /// `Sign in to <host>?`); otherwise sends `pair_refused` and redeems nothing. The code never reaches
    /// the webview.
    pub fn offer_pairing(&self, host: &str, server: Option<&Origin>, code: Option<Secret>) {
        let refused = |reason: &str| AppEvent::PairRefused {
            host: host.to_owned(),
            reason: reason.to_owned(),
        };
        let Some(server) = server.filter(|o| self.state().book.server(o).is_some()) else {
            self.emit(&refused("unknown_server"));
            return;
        };
        let Some(code) = code.filter(|c| is_pairing_code(c.expose())) else {
            self.emit(&refused("bad_link"));
            return;
        };
        {
            let mut st = self.state();
            st.pairs.retain(|_, (_, at)| at.elapsed() < PAIRING_TTL);
            st.pairs.insert(server.clone(), (code, Instant::now()));
        }
        self.emit(&AppEvent::PairRequest {
            server: server.clone(),
            host: server.authority().to_owned(),
            expires_in_ms: u64::try_from(PAIRING_TTL.as_millis()).unwrap_or(u64::MAX),
        });
    }

    /// The person confirmed a `pair_request`: redeems the held code ([`AppService::pair`]).
    /// `pairing_not_pending` when none is held for `origin` (or it is too old).
    pub async fn pair_confirm(
        &self,
        origin: &Origin,
        account: Option<&str>,
    ) -> Result<AccountView, AppError> {
        let held = self.state().pairs.remove(origin);
        match held {
            Some((code, at)) if at.elapsed() < PAIRING_TTL => {
                self.pair(origin, code, account).await
            }
            _ => Err(AppError::new(code::PAIRING_NOT_PENDING)),
        }
    }

    /// The person declined a `pair_request`: forgets the held code. Whether one was held.
    pub fn pair_decline(&self, origin: &Origin) -> bool {
        self.state().pairs.remove(origin).is_some()
    }

    /// Keeps a new device session: the bearer into the secret store (else the session is
    /// ended again), whoami for the install and the name, the account into the book (the same
    /// person's older account on this server is signed out and replaced), active.
    async fn finish_sign_in(
        &self,
        origin: &Origin,
        device: DeviceName,
        s: SignIn,
        username: Option<&str>,
    ) -> Result<AccountView, AppError> {
        let key = AccountKey {
            server: origin.clone(),
            account: device.id.clone(),
        };
        if let Err(e) = self.secret_set(&key, s.token.clone()).await {
            // Never keep a session the device can't hold.
            let _ = self.inner.lane.sign_out(origin, &s.token).await;
            return Err(e);
        }
        let who = self.inner.lane.whoami(origin, &s.token).await;
        let (install_id, label) = match &who {
            Ok(w) => (w.install_id(), w.display_name()),
            Err(e) => {
                tracing::warn!(code = %e.to_error().code, "sew-app: whoami after sign-in");
                (None, None)
            }
        };
        let label = label.or_else(|| username.map(str::to_owned));
        // The same person signed in under another account id here: replace it.
        let older = {
            let st = self.state();
            st.book
                .member_account(origin, &s.member_id)
                .filter(|k| k != &key)
        };
        if let Some(old) = older {
            if let Ok(Some(b)) = self.secret_get(&old).await {
                let _ = self.inner.lane.sign_out(origin, &b).await;
            }
            let _ = self.secret_delete(&old).await;
            self.state().book.remove_account(&old);
        }
        let account = account_from_session(
            device.id.clone(),
            s.member_id.clone(),
            label,
            install_id,
            &s.session,
        );
        let mut st = self.state();
        let Some(server) = st.book.server_mut(origin) else {
            return Err(AppError::new(code::UNKNOWN_SERVER));
        };
        match server.accounts.iter_mut().find(|a| a.id == device.id) {
            Some(a) => *a = account,
            None => server.accounts.push(account),
        }
        st.book.active = Some(key.clone());
        self.commit(&st)?;
        st.book
            .view(st.secret_kind)
            .servers
            .into_iter()
            .flat_map(|s| s.accounts)
            .find(|a| a.key == key)
            .ok_or_else(|| AppError::new(code::UNKNOWN_ACCOUNT))
    }

    // ── the device session ──

    /// The account's bearer (`signed_out` when its session ended or its secret is gone).
    pub async fn bearer(&self, key: &AccountKey) -> Result<Secret, AppError> {
        let state = {
            let st = self.state();
            st.book
                .account(key)
                .map(|a| a.state.clone())
                .ok_or_else(|| AppError::new(code::UNKNOWN_ACCOUNT))?
        };
        if let AccountState::SignedOut { reason } = state {
            return Err(AppError::with_detail(
                code::SIGNED_OUT,
                serde_json::json!({ "reason": reason }),
            ));
        }
        match self.secret_get(key).await? {
            Some(b) => Ok(b),
            None => {
                self.session_ended(key, "missing").await;
                Err(AppError::with_detail(
                    code::SIGNED_OUT,
                    serde_json::json!({ "reason": "missing" }),
                ))
            }
        }
    }

    /// The server ended `key`'s session: delete its bearer, mark it signed out, move the
    /// active account, tell the webview, and forget the install's call keys when no signed-in
    /// account is left on it.
    pub async fn session_ended(&self, key: &AccountKey, reason: &str) {
        self.inner.gateways.close_account(key);
        if let Err(e) = self.secret_delete(key).await {
            tracing::warn!(code = %e.code, "sew-app: deleting an ended session's bearer");
        }
        let install = {
            let mut st = self.state();
            let Some(a) = st.book.account_mut(key) else {
                return;
            };
            if a.state != AccountState::SignedIn {
                return;
            }
            a.state = AccountState::SignedOut {
                reason: reason.to_owned(),
            };
            let install = a.install_id;
            if st.book.active.as_ref() == Some(key) {
                st.book.active = st.book.next_active(&key.server).or(Some(key.clone()));
            }
            if let Err(e) = self.commit(&st) {
                tracing::warn!(code = %e.code, "sew-app: saving an ended session");
            }
            install.filter(|i| !st.book.install_in_use(*i))
        };
        self.emit(&AppEvent::SignedOut {
            key: key.clone(),
            reason: reason.to_owned(),
        });
        if let Some(i) = install {
            self.forget_install(i).await;
        }
    }

    async fn forget_install(&self, install_id: u64) -> bool {
        let f = lock(&self.inner.forget).clone();
        let Some(f) = f else {
            return false;
        };
        match f.forget(install_id).await {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(code = %e.code, "sew-app: the voice plugin's forget_server");
                false
            }
        }
    }

    /// Runs a lane call with `key`'s bearer; a `401` ends the account's session
    /// ([`AppService::session_ended`]) and answers `signed_out`.
    pub async fn with_bearer<T, F, Fut>(&self, key: &AccountKey, f: F) -> Result<T, AppError>
    where
        F: FnOnce(LaneClient, Secret) -> Fut,
        Fut: std::future::Future<Output = Result<T, LaneFailure>>,
    {
        let bearer = self.bearer(key).await?;
        match f(self.inner.lane.clone(), bearer).await {
            Ok(t) => Ok(t),
            Err(LaneFailure::Refused(r)) if r.is_signed_out() => {
                let reason = r.signed_out_reason();
                self.session_ended(key, &reason).await;
                let mut e = r.to_error();
                e.code = code::SIGNED_OUT.to_owned();
                e.detail = serde_json::json!({ "reason": reason });
                Err(e)
            }
            Err(f) => Err(f.to_error()),
        }
    }

    /// Signs the account out: ends its device session on the server, deletes its bearer,
    /// removes it, and forgets the install's call keys when no signed-in account is left on
    /// that install.
    pub async fn sign_out(&self, key: &AccountKey) -> Result<SignOutResult, AppError> {
        let install = {
            let st = self.state();
            st.book
                .account(key)
                .ok_or_else(|| AppError::new(code::UNKNOWN_ACCOUNT))?
                .install_id
        };
        self.inner.gateways.close_account(key);
        let bearer = self.secret_get(key).await?;
        let server_signed_out = match bearer {
            None => true,
            Some(b) => match self.inner.lane.sign_out(&key.server, &b).await {
                Ok(()) => true,
                // Already ended.
                Err(LaneFailure::Refused(r)) if r.status == 401 => true,
                Err(e) => {
                    tracing::warn!(code = %e.to_error().code, "sew-app: sign-out on the server");
                    false
                }
            },
        };
        self.secret_delete(key).await?;
        let forget = {
            let mut st = self.state();
            st.book.remove_account(key);
            self.commit(&st)?;
            install.filter(|i| !st.book.install_in_use(*i))
        };
        let mut forgot_install = None;
        if let Some(i) = forget
            && self.forget_install(i).await
        {
            forgot_install = Some(i.to_string());
        }
        if self.call_account().as_ref() == Some(key) {
            *lock(&self.inner.call_account) = None;
        }
        Ok(SignOutResult {
            server_signed_out,
            forgot_install,
        })
    }

    /// This person's device sessions on that server.
    pub async fn devices(&self, key: &AccountKey) -> Result<Vec<SessionInfo>, AppError> {
        let origin = key.server.clone();
        self.with_bearer(
            key,
            |lane, b| async move { lane.devices(&origin, &b).await },
        )
        .await
    }

    /// Revokes one of this person's device sessions; revoking the account's own is a
    /// sign-out.
    pub async fn revoke_device(&self, key: &AccountKey, session_id: &str) -> Result<(), AppError> {
        let own = {
            let st = self.state();
            st.book
                .account(key)
                .ok_or_else(|| AppError::new(code::UNKNOWN_ACCOUNT))?
                .session_id
                .clone()
        };
        if own.as_deref() == Some(session_id) {
            self.sign_out(key).await?;
            return Ok(());
        }
        let origin = key.server.clone();
        let id = session_id.to_owned();
        self.with_bearer(
            key,
            |lane, b| async move { lane.revoke(&origin, &b, &id).await },
        )
        .await
    }

    // ── the webview ──

    /// A handoff code for the account's server page. `caller` is the asking page's URL: it
    /// must be on the account's server (`wrong_origin` otherwise), since the code is a
    /// credential for that origin's cookie.
    pub async fn handoff(
        &self,
        key: &AccountKey,
        caller: Option<&Url>,
    ) -> Result<HandoffView, AppError> {
        if let Some(u) = caller
            && !key.server.contains(u)
        {
            return Err(AppError::new(code::WRONG_ORIGIN));
        }
        let origin = key.server.clone();
        let h = self
            .with_bearer(
                key,
                |lane, b| async move { lane.handoff(&origin, &b).await },
            )
            .await?;
        Ok(HandoffView {
            code: h.code,
            expires_in_ms: h.expires_in_ms,
            redeem_url: key
                .server
                .join(crate::lane::route::HANDOFF_REDEEM)?
                .to_string(),
        })
    }

    /// The URL that opens `path` on the account's server signed in: with a handoff page
    /// configured, `<page>#code=<code>&next=<path>` (the page redeems the code, drops the
    /// fragment and goes to `next`); without one, `path` itself.
    pub async fn open_url(&self, key: &AccountKey, path: &str) -> Result<Url, AppError> {
        let target = key.server.join(path)?;
        let Some(page) = self.inner.cfg.handoff_page.clone() else {
            return Ok(target);
        };
        let h = self.handoff(key, None).await?;
        let mut url = key.server.join(&page)?;
        let next = format!(
            "{}{}",
            target.path(),
            target.query().map(|q| format!("?{q}")).unwrap_or_default()
        );
        let frag: String = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("code", h.code.expose())
            .append_pair("next", &next)
            .finish();
        url.set_fragment(Some(&frag));
        Ok(url)
    }

    // ── the voice provider's account ──

    /// The account the current call joined with.
    pub fn call_account(&self) -> Option<AccountKey> {
        lock(&self.inner.call_account).clone()
    }

    /// Pins the call's account (the provider's first join).
    pub(crate) fn set_call_account(&self, key: Option<AccountKey>) {
        *lock(&self.inner.call_account) = key;
    }
}
