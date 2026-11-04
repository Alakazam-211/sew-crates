//! The key session: everything between a join answer and active call keys.
//!
//! The order after a join answer:
//! 1. open the call socket and read READY's `session_id` (the join's `e2ee.gateway_session`) and
//!    the trust anchors (TOFU-pinned in the device file);
//! 2. load or enrol the device ([`crate::device::DeviceVault::ensure_device`]);
//! 3. upload a KeyPackage for `call:<channel_id>`;
//! 4. key-join: the key join route with the credential's `join_id` ([`MlsSession::key_join`],
//!    the deferred key join and every move), or inline in the voice join (the host's join carried
//!    [`MlsSession::prepare_join`]'s `e2ee`, then [`MlsSession::begin`]);
//! 5. drive `sew-e2ee`'s `CallSession`: `call_keys_delivered` in, the call key routes out,
//!    `KeyEvent`s into [`MediaKeys`], the media path's reports back as `report` inputs.
//!
//! Recording (Sew 0.7.3): the call socket's own `recording_updated` and READY's
//! `recording` reach the owner as [`KeySessionEvent::Recording`]; the owner (the engine)
//! decides which recording is the call's and feeds it back with [`MlsSession::set_recording`],
//! which `sew-e2ee` turns into the recorder key release (only for `granted`) or a rekey on a
//! withdrawal. The last state is applied again to every new key session (a move, a re-join), so
//! a new session never starts from a stale `granted`: it starts `pending` until fed.
//!
//! The credential is renewed at half its life and the socket re-authenticated (REAUTH). A device
//! the server refuses (revoked, or its certificate failed) is forgotten, enrolled once and the
//! step retried once (Sew 0.7.1's rule); the pins are kept. A pin mismatch ends with
//! `ca_pin_mismatch` and is never re-pinned.
//!
//! The session and the bridge sit behind a short-held mutex; HTTP runs with it released.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sew_e2ee::keypackage::on_join_failure;
use sew_e2ee::{
    ApiFailure, CallSession, E2eeClient, E2eeStatus, Input, JoinId, KeyEvent, KeySink as _,
    LeftReason, LkIdentity, Output, PreparedKp, SessionState, SinkAck, SoftwareSigner,
    TrustAnchors, UnknownCallBuffer,
};
use sew_proto::e2ee::{CallKeysInit, CallScope, ConsentState, E2eeJoin};
use sew_proto::id::Id;
use sew_proto::recording::Recording;
use sew_proto::voice::{VoiceServerUpdated, VoiceStateUpdated};
use tokio::sync::mpsc;
use url::Url;

use super::credential::{Bearer, CredentialGrant, RenewRefused, SharedBearer, renew_at, unix_ms};
use super::frame::FrameCrypto;
use super::keys::{MediaKeys, MediaReport, engine_caps, lock};
use super::transport::{KeyTransport, TransportError};
use crate::Clock;
use crate::device::{DeviceError, DeviceVault, DeviceWarning, VaultConfig, is_device_refused};
use crate::gateway::{CallReadyInfo, CallSocket, SocketClosed, SocketError, SocketEvent};

/// How long [`MlsSession::connect`] waits for the call socket's READY.
pub const READY_TIMEOUT: Duration = Duration::from_secs(15);
/// The driver wakes at least this often (timers, the media path's reports).
pub const TICK: Duration = Duration::from_millis(100);

/// What the key session needs.
#[derive(Clone)]
pub struct KeySessionConfig {
    /// The door (`JoinAnswer.api_base`).
    pub api_base: Url,
    /// The bearer: the call credential's token (a `sews_` session token only in tests).
    pub bearer: Bearer,
    /// When the credential expires (unix ms); `None`: never renewed (a `sews_` test bearer).
    pub credential_expires_at_ms: Option<u64>,
    /// The credential's `device_scope` (the stored device is keyed on it).
    pub device_scope: String,
    /// Where the device lives.
    pub vault: VaultConfig,
    /// The install id when the call socket's READY does not carry one.
    pub install_id: Option<u64>,
    /// The clock (unix ms).
    pub clock: Clock,
    /// The TLS trust of the key routes and the call socket (`None`: the platform's, as the
    /// engine's own connections).
    pub tls: Option<Arc<rustls::ClientConfig>>,
}

impl fmt::Debug for KeySessionConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeySessionConfig")
            .field("host", &self.api_base.host_str())
            .field("bearer", &self.bearer)
            .field("device_scope", &self.device_scope)
            .finish_non_exhaustive()
    }
}

impl KeySessionConfig {
    /// From the join answer's credential.
    pub fn from_grant(
        api_base: Url,
        grant: &CredentialGrant,
        vault: VaultConfig,
        clock: Clock,
    ) -> KeySessionConfig {
        KeySessionConfig {
            api_base,
            bearer: Bearer::new(grant.token.clone()),
            credential_expires_at_ms: Some(unix_ms(grant.expires_at)),
            device_scope: grant.device_scope.clone(),
            vault,
            install_id: None,
            clock,
            tls: None,
        }
    }
}

/// Why the key session failed (the `errors` codes in brackets).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum KeyError {
    /// The door or the client could not be set up.
    #[error("transport: {0}")]
    Transport(String),
    /// The call socket closed before READY, or READY was unreadable.
    #[error("the call socket did not become ready")]
    NoReady,
    /// READY carried no trust anchors (`call_keys`).
    #[error("the call socket's READY has no call_keys")]
    NoTrustAnchors,
    /// No install id (neither READY nor the config).
    #[error("no install id")]
    NoInstallId,
    /// A pinned server key changed (`ca_pin_mismatch`). Only `reset_device` clears the pins.
    #[error("the server's call identity changed ({0})")]
    PinMismatch(&'static str),
    /// READY's trust anchors are malformed.
    #[error("trust anchors refused: {0}")]
    Trust(String),
    /// The device store or enrolment failed.
    #[error("device: {0}")]
    Device(#[from] DeviceError),
    /// The server refused the key join for good (`e2ee_refused`).
    #[error("key join refused ({status} {reason:?})")]
    Refused {
        /// HTTP status.
        status: u16,
        /// `details.reason`.
        reason: Option<String>,
    },
    /// The MLS session could not start or failed.
    #[error("session: {0}")]
    Session(String),
    /// The key session is closed.
    #[error("closed")]
    Closed,
}

impl From<TransportError> for KeyError {
    fn from(e: TransportError) -> Self {
        KeyError::Transport(e.to_string())
    }
}

impl From<SocketError> for KeyError {
    fn from(e: SocketError) -> Self {
        KeyError::Transport(e.to_string())
    }
}

impl KeyError {
    /// The plugin's `errors.code` / `ended.reason`.
    pub fn code(&self) -> &'static str {
        match self {
            KeyError::PinMismatch(_) => "ca_pin_mismatch",
            KeyError::Refused { .. } | KeyError::Session(_) => "e2ee_refused",
            KeyError::Device(_) => "e2ee_refused",
            KeyError::Closed => "credential_lost",
            _ => "network",
        }
    }
}

fn refused(f: &ApiFailure) -> KeyError {
    KeyError::Refused {
        status: f.status,
        reason: f.reason.clone(),
    }
}

/// Why the key session ended by itself.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum KeyEnd {
    /// The credential is gone (`4011`, or a renewal refused): `credential_lost`. `rejoin` is
    /// true when the engine may re-join through the `JoinProvider` (`expired`).
    CredentialLost {
        /// Why.
        reason: String,
        /// Whether a re-join may follow.
        rejoin: bool,
    },
    /// Sew ended this join and said why (Sew 0.7.2): the `4011` close of the call socket named
    /// the reason ([`SocketClosed::Ended`]); `reason` is Sew's voice leave reason as it
    /// arrived (`replaced`: another device took the call over).
    Ended {
        /// Why.
        reason: String,
    },
    /// The call socket ended for good (`4004`, a protocol error).
    Socket(SocketClosed),
    /// The MLS session left (removed, or failed for good).
    Left(LeftReason),
}

/// What the key session tells the engine.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum KeySessionEvent {
    /// The E2EE status changed (`connected` needs `Active`).
    Status(E2eeStatus),
    /// The verified roster changed: re-check tracks that were `SubscribeDecision::Wait`.
    Roster,
    /// `voice_server_updated` (a move: reconnect signalling with its token; the key session
    /// key-joins the new `join_id` by itself).
    VoiceServerUpdated(Box<VoiceServerUpdated>),
    /// `voice_state_updated` of the call's channel (the plugin's `participants`).
    VoiceStateUpdated(Box<VoiceStateUpdated>),
    /// A recording as this member sees it (the socket's own `recording_updated`, or the call
    /// socket's READY `recording`); `None`: READY says the call has no active recording. Never
    /// fed to the keys by the session itself: see [`MlsSession::set_recording`].
    Recording(Option<Box<Recording>>),
    /// A warning for the host (`key_storage_file`, `pins_reset`).
    Warning(DeviceWarning),
    /// A move's key join failed.
    KeyJoinFailed(KeyError),
    /// The key session ended by itself.
    Ended(KeyEnd),
}

struct Core<C: FrameCrypto> {
    session: Option<CallSession>,
    sink: MediaKeys<C>,
    client: E2eeClient,
    signer: Arc<SoftwareSigner>,
    prepared: BTreeMap<[u8; 32], PreparedKp>,
    buffer: UnknownCallBuffer,
    gateway_session: String,
    voice_members: BTreeSet<Id>,
    channel: Option<Id>,
    last_verified: BTreeSet<LkIdentity>,
    /// The recording state the owner fed last ([`MlsSession::set_recording`]): applied to every
    /// new `CallSession`.
    recording: Option<(Option<Id>, ConsentState)>,
    ended: bool,
}

struct Shared<C: FrameCrypto> {
    core: Mutex<Core<C>>,
    http: KeyTransport,
    vault: tokio::sync::Mutex<DeviceVault>,
    clock: Clock,
    events: mpsc::UnboundedSender<KeySessionEvent>,
    member: Id,
    changed: tokio::sync::Notify,
}

/// One call's key session.
pub struct MlsSession<C: FrameCrypto> {
    shared: Arc<Shared<C>>,
    socket: Arc<CallSocket>,
    stop: Arc<tokio::sync::Notify>,
}

impl<C: FrameCrypto> fmt::Debug for MlsSession<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MlsSession")
            .field("member", &self.shared.member)
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

impl<C: FrameCrypto> Drop for MlsSession<C> {
    fn drop(&mut self) {
        self.stop.notify_one();
        self.socket.close();
    }
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, DeviceError> + Send + 'static,
) -> Result<T, DeviceError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| DeviceError::Store("blocking task failed".to_owned()))?
}

/// Builds the trust anchors from READY against the pins (TOFU; a change is refused) and pages
/// the CRL when READY's is partial.
async fn trust_from_ready(
    ready: &CallReadyInfo,
    install: u64,
    vault: &mut DeviceVault,
    http: &KeyTransport,
) -> Result<TrustAnchors, KeyError> {
    let ck = ready.call_keys.as_ref().ok_or(KeyError::NoTrustAnchors)?;
    let mut trust =
        TrustAnchors::from_ready(ck, install, vault.file_mut()).map_err(|e| match e {
            sew_e2ee::TrustError::ServerIdentityChanged { what } => KeyError::PinMismatch(what),
            other => KeyError::Trust(other.to_string()),
        })?;
    if trust.crl_partial() {
        let mut after = None;
        for _ in 0..64 {
            let Ok(page) = http.crl_page(after).await else {
                break;
            };
            trust.revoke(&page.items.iter().map(|s| s.get()).collect::<Vec<_>>());
            after = page.items.last().copied();
            if !page.has_more || after.is_none() {
                break;
            }
        }
    }
    Ok(trust)
}

impl<C: FrameCrypto> MlsSession<C> {
    /// Steps 1–2: opens the call socket, reads READY, pins the trust anchors, loads or enrols
    /// the device, and starts the driver. Events arrive on the returned receiver.
    pub async fn connect(
        cfg: KeySessionConfig,
        keys: MediaKeys<C>,
    ) -> Result<(MlsSession<C>, mpsc::UnboundedReceiver<KeySessionEvent>), KeyError> {
        let bearer = SharedBearer::new(cfg.bearer.clone());
        let tls = match &cfg.tls {
            Some(t) => t.clone(),
            None => {
                crate::config::platform_tls().map_err(|e| KeyError::Transport(e.to_string()))?
            }
        };
        let http = KeyTransport::new(&cfg.api_base, bearer.clone(), &tls)?;
        let (socket, mut socket_rx) = CallSocket::open(&cfg.api_base, bearer.clone(), tls)?;
        let ready = match tokio::time::timeout(READY_TIMEOUT, socket_rx.recv()).await {
            Ok(Some(SocketEvent::Ready(r))) => *r,
            Ok(Some(SocketEvent::Closed(SocketClosed::Revoked | SocketClosed::Ended { .. }))) => {
                return Err(KeyError::Closed);
            }
            _ => return Err(KeyError::NoReady),
        };
        let install = ready
            .install_id
            .map(|i| i.get())
            .or(cfg.install_id)
            .ok_or(KeyError::NoInstallId)?;
        let member = ready.member_id.ok_or(KeyError::NoReady)?;
        tracing::debug!(target: "sew_call_client::keys", member = %member, install, "call socket ready");
        let vcfg = cfg.vault.clone();
        let scope = cfg.device_scope.clone();
        let mut vault = blocking(move || DeviceVault::open(&vcfg, install, member, &scope)).await?;
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        for w in vault.take_warnings() {
            let _ = ev_tx.send(KeySessionEvent::Warning(w));
        }
        if let Some(r) = ready.recording.clone() {
            let _ = ev_tx.send(KeySessionEvent::Recording(r.map(Box::new)));
        }
        let trust = trust_from_ready(&ready, install, &mut vault, &http).await?;
        let now = (cfg.clock)();
        let (identity, signer) = vault.ensure_device(&http, &trust, member, now).await?;
        let params = trust.params().map_err(|e| KeyError::Trust(e.to_string()))?;
        let client = E2eeClient::new(identity, trust);
        let buffer = UnknownCallBuffer::new(&params, client.config());
        let shared = Arc::new(Shared {
            core: Mutex::new(Core {
                session: None,
                sink: keys,
                client,
                signer: Arc::new(signer),
                prepared: BTreeMap::new(),
                buffer,
                gateway_session: ready.session_id.clone(),
                voice_members: BTreeSet::new(),
                channel: None,
                last_verified: BTreeSet::new(),
                recording: None,
                ended: false,
            }),
            http,
            vault: tokio::sync::Mutex::new(vault),
            clock: cfg.clock.clone(),
            events: ev_tx,
            member,
            changed: tokio::sync::Notify::new(),
        });
        let socket = Arc::new(socket);
        let stop = Arc::new(tokio::sync::Notify::new());
        let renew = cfg
            .credential_expires_at_ms
            .filter(|_| cfg.bearer.is_call_credential())
            .map(|exp| renew_at(now, exp));
        tokio::spawn(drive(
            Arc::downgrade(&shared),
            socket.clone(),
            socket_rx,
            stop.clone(),
            renew,
        ));
        Ok((
            MlsSession {
                shared,
                socket,
                stop,
            },
            ev_rx,
        ))
    }

    /// The call socket's `session_id` (the join's `e2ee.gateway_session`).
    pub fn gateway_session(&self) -> String {
        lock(&self.shared.core).gateway_session.clone()
    }

    /// The member.
    pub fn member_id(&self) -> Id {
        self.shared.member
    }

    /// The key state the media path reads.
    pub fn media_keys(&self) -> MediaKeys<C> {
        lock(&self.shared.core).sink.clone()
    }

    /// The device in use (its id).
    pub fn device_id(&self) -> Id {
        lock(&self.shared.core).client.identity().device_id()
    }

    /// The last E2EE status.
    pub fn status(&self) -> Option<E2eeStatus> {
        lock(&self.shared.core)
            .session
            .as_ref()
            .and_then(|s| s.status().cloned())
    }

    /// The MLS session state.
    pub fn state(&self) -> Option<SessionState> {
        lock(&self.shared.core).session.as_ref().map(|s| s.state())
    }

    /// Whether a key epoch is active and the local sender has a key (`connected`).
    pub fn is_active(&self) -> bool {
        let c = lock(&self.shared.core);
        let active = c.session.as_ref().is_some_and(|s| {
            matches!(s.state(), SessionState::Active { .. }) && s.active_epoch().is_some()
        });
        active && c.sink.local_slot().is_some()
    }

    /// The local engine identity of the current join.
    pub fn lk_identity(&self) -> Option<LkIdentity> {
        lock(&self.shared.core)
            .session
            .as_ref()
            .map(|s| s.lk_identity().clone())
    }

    /// Waits until `pred` holds or `timeout` passes.
    pub async fn wait_until(&self, timeout: Duration, pred: impl Fn(&Self) -> bool) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if pred(self) {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            let n = self.shared.changed.notified();
            let _ = tokio::time::timeout(Duration::from_millis(50), n).await;
        }
    }

    /// Steps 3 and 4 inline: a KeyPackage for `call:<channel>` uploaded, and the `e2ee` object
    /// of a voice or DM call join. Hand the join's `CallKeysInit` to [`MlsSession::begin`].
    pub async fn prepare_join(&self, channel: Id) -> Result<E2eeJoin, KeyError> {
        let kp = self.prepared_kp(CallScope::call(channel)).await?;
        let mut guard = lock(&self.shared.core);
        let c = &mut *guard;
        let join = kp
            .join_field(&engine_caps(), &c.gateway_session)
            .map_err(|e| KeyError::Session(e.to_string()))?;
        c.channel = Some(channel);
        c.prepared.insert(kp.key_package_ref(), kp);
        Ok(join)
    }

    /// Starts the MLS session from a voice join answer (`VoiceJoinResponse.e2ee`).
    pub async fn begin(&self, init: CallKeysInit) -> Result<(), KeyError> {
        start_session(&self.shared, init, None).await
    }

    /// Steps 3–4 for a deferred key join or a move: a KeyPackage for `call:<channel>`, then the key
    /// join with `join_id`, then the session. A stale ref is re-uploaded once; a refused device is
    /// forgotten, enrolled once and the join retried once.
    pub async fn key_join(&self, channel: Id, join_id: JoinId) -> Result<(), KeyError> {
        key_join(&self.shared, channel, join_id).await
    }

    /// Leaves: the session zeroizes its secrets, every remote key is dropped, the socket closes.
    /// (The voice leave itself is `POST /api/v1/me/voice/leave`, the engine's.)
    pub async fn leave(&self) {
        let now = (self.shared.clock)();
        {
            let mut guard = lock(&self.shared.core);
            let c = &mut *guard;
            if let Some(s) = c.session.as_mut() {
                s.leave(now);
            }
            let _ = c.sink.apply(KeyEvent::Clear);
            c.ended = true;
        }
        pump(&self.shared).await;
        self.stop.notify_one();
        self.socket.close();
    }

    /// The host's explicit `reset_device` (a Rust API, never a webview command): forgets the
    /// device, its key and the pins.
    pub async fn reset_device(&self) -> Result<(), KeyError> {
        let mut v = self.shared.vault.lock().await;
        v.reset().await?;
        Ok(())
    }

    /// The live bearer (renewed in place).
    pub fn bearer(&self) -> Bearer {
        self.shared.http.bearer().get()
    }

    /// The engine renewed the call credential: the key routes use the new token from now on,
    /// and the call socket re-authenticates with it (REAUTH keeps the `session_id`).
    pub fn renewed(&self, token: Bearer) {
        self.shared.http.bearer().set(token);
        self.socket.reauth();
    }

    /// The call's recording state as Sew said it (never local state): the live recording of
    /// the call's room (`None`: none) and this member's consent for it. `sew-e2ee` releases this
    /// device's key to the recorder only while the recording is announced, live and
    /// `Granted`, and asks for an immediate rekey when `Granted` ends (a withdrawal). Kept and
    /// applied again to each new key session of this socket.
    pub async fn set_recording(&self, recording_id: Option<Id>, my_consent: ConsentState) {
        {
            let mut guard = lock(&self.shared.core);
            let c = &mut *guard;
            if c.recording == Some((recording_id, my_consent)) {
                return;
            }
            c.recording = Some((recording_id, my_consent));
        }
        tracing::debug!(target: "sew_call_client::keys", recording = ?recording_id, consent = ?my_consent, "recording state");
        input(
            &self.shared,
            Input::Recording {
                recording_id,
                my_consent,
            },
        )
        .await;
    }

    /// The recording state last given to [`MlsSession::set_recording`].
    pub fn recording(&self) -> Option<(Option<Id>, ConsentState)> {
        lock(&self.shared.core).recording
    }

    /// The active key epoch, when the session has one.
    pub fn active_epoch(&self) -> Option<u64> {
        lock(&self.shared.core)
            .session
            .as_ref()
            .and_then(|s| s.active_epoch())
    }

    /// The engine identities bound to a leaf of the verified tree and the member each is, the
    /// local one included (the `participants` event's `member_id`).
    pub fn members(&self) -> Vec<(String, String)> {
        lock(&self.shared.core)
            .session
            .as_ref()
            .map(|s| {
                s.roster()
                    .iter()
                    .filter(|e| e.leaf_index.is_some())
                    .map(|e| (e.lk_identity.as_str().to_owned(), e.member_id.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// For tests: the derived keys are not exposed, but the IKMs the session holds are, so a
    /// test can grep logs for them.
    #[doc(hidden)]
    pub fn live_ikms_for_test(&self) -> Vec<[u8; 32]> {
        let c = lock(&self.shared.core);
        let Some(s) = c.session.as_ref() else {
            return Vec::new();
        };
        s.key_ring().live().map(|(_, _, k)| *k.expose()).collect()
    }
}

impl<C: FrameCrypto> Shared<C> {
    fn emit(&self, e: KeySessionEvent) {
        let _ = self.events.send(e);
    }
}

/// Prepares and uploads one KeyPackage for `scope`, recovering a refused device once.
async fn upload_kp<C: FrameCrypto>(
    shared: &Arc<Shared<C>>,
    scope: CallScope,
) -> Result<PreparedKp, KeyError> {
    let mut recovered = false;
    loop {
        let now = (shared.clock)();
        let (pending, msg, signer) = {
            let mut guard = lock(&shared.core);
            let c = &mut *guard;
            let (pending, req) = c
                .client
                .prepare_key_package(scope, now)
                .map_err(|e| KeyError::Session(e.to_string()))?;
            (pending, req.message, c.signer.clone())
        };
        let mut kp = pending
            .finish(signer.sign(&msg))
            .map_err(|e| KeyError::Session(e.to_string()))?;
        match shared.http.upload_key_package(&mut kp).await {
            Ok(()) => return Ok(kp),
            Err(f) if !recovered && is_device_refused(&f) => {
                recovered = true;
                recover_device(shared).await?;
            }
            Err(f) => return Err(refused(&f)),
        }
    }
}

/// Sew 0.7.1's recovery: forget the refused device (keep the pins), enrol once.
async fn recover_device<C: FrameCrypto>(shared: &Arc<Shared<C>>) -> Result<(), KeyError> {
    tracing::info!(target: "sew_call_client::keys", "the device was refused; enrolling again");
    let trust = lock(&shared.core).client.trust().clone();
    let now = (shared.clock)();
    let (identity, signer) = {
        let mut v = shared.vault.lock().await;
        v.forget_device().await?;
        v.enroll(&shared.http, &trust, shared.member, now).await?
    };
    let mut guard = lock(&shared.core);
    let c = &mut *guard;
    let cfg = c.client.config().clone();
    c.client = E2eeClient::new(identity, trust).with_config(cfg);
    c.signer = Arc::new(signer);
    // Packages signed by the old device are useless now.
    c.prepared.clear();
    Ok(())
}

impl<C: FrameCrypto> MlsSession<C> {
    async fn prepared_kp(&self, scope: CallScope) -> Result<PreparedKp, KeyError> {
        upload_kp(&self.shared, scope).await
    }
}

async fn key_join<C: FrameCrypto>(
    shared: &Arc<Shared<C>>,
    channel: Id,
    join_id: JoinId,
) -> Result<(), KeyError> {
    let scope = CallScope::call(channel);
    let mut ref_retried = false;
    let mut device_retried = false;
    loop {
        let kp = upload_kp(shared, scope).await?;
        let body = {
            let c = lock(&shared.core);
            kp.key_join_request(join_id.clone(), &engine_caps(), &c.gateway_session)
                .map_err(|e| KeyError::Session(e.to_string()))?
        };
        match shared.http.key_join(&body).await {
            Ok(init) => {
                lock(&shared.core).channel = Some(channel);
                return start_session(shared, init, Some(kp)).await;
            }
            Err(f) if !device_retried && is_device_refused(&f) => {
                device_retried = true;
                recover_device(shared).await?;
            }
            Err(f) => match on_join_failure(&f, ref_retried) {
                Ok(()) => ref_retried = true,
                Err(_) => return Err(refused(&f)),
            },
        }
    }
}

async fn start_session<C: FrameCrypto>(
    shared: &Arc<Shared<C>>,
    init: CallKeysInit,
    kp: Option<PreparedKp>,
) -> Result<(), KeyError> {
    let now = (shared.clock)();
    {
        let mut guard = lock(&shared.core);
        let c = &mut *guard;
        let kp = match kp {
            Some(k) => k,
            None => c
                .prepared
                .pop_last()
                .map(|(_, k)| k)
                .ok_or_else(|| KeyError::Session("no prepared key package".to_owned()))?,
        };
        let call_id = init.call_id.clone();
        if let Some(old) = c.session.as_mut() {
            // A move: the old call's keys go (its leave is the server's).
            old.leave(now);
        }
        let _ = c.sink.apply(KeyEvent::Clear);
        let session = kp
            .into_session(&c.client, init, &c.gateway_session, now)
            .map_err(|e| KeyError::Session(e.to_string()))?;
        tracing::debug!(target: "sew_call_client::keys", call = %call_id.as_str(), "key session started");
        c.session = Some(session);
        c.prepared.clear();
        let held = c.buffer.take(now, &call_id);
        let recording = c.recording;
        if let Some(s) = c.session.as_mut() {
            for m in held {
                let _ = s.handle(now, Input::Server(m));
            }
            // A new session starts `pending`: the recording state as last fed.
            if let Some((recording_id, my_consent)) = recording {
                let _ = s.handle(
                    now,
                    Input::Recording {
                        recording_id,
                        my_consent,
                    },
                );
            }
        }
    }
    pump(shared).await;
    Ok(())
}

/// Applies key events, answers signatures and self-checks, and POSTs every request (lock
/// released), feeding the answers back, until the session is quiet.
async fn pump<C: FrameCrypto>(shared: &Arc<Shared<C>>) {
    loop {
        let (sends, roster_changed, ended) = {
            let mut guard = lock(&shared.core);
            let c = &mut *guard;
            let now = (shared.clock)();
            let mut sends = Vec::new();
            let mut roster = false;
            let Some(session) = c.session.as_mut() else {
                return;
            };
            while let Some(o) = session.poll_output() {
                match o {
                    Output::Send(id, req) => sends.push((id, req)),
                    Output::Key(ev) => {
                        let fence = match &ev {
                            KeyEvent::Fence { id } => Some(*id),
                            _ => None,
                        };
                        match c.sink.apply(ev) {
                            Ok(SinkAck::Now) => {
                                if let Some(f) = fence {
                                    let _ = session.handle(now, Input::Installed { fence: f });
                                }
                            }
                            Ok(SinkAck::Later(_)) => {}
                            Err(e) => {
                                tracing::warn!(target: "sew_call_client::keys", error = %e, "the bridge refused a key event");
                            }
                        }
                    }
                    Output::NeedDeviceSignature { request, message } => {
                        let sig = c.signer.sign(&message);
                        let _ = session.handle(now, Input::DeviceSignature { request, sig });
                    }
                    Output::CheckSenderSlot { .. } => {
                        if let Some(observed) = c.sink.local_slot() {
                            let _ = session.handle(now, Input::SenderSlotObserved { observed });
                        }
                    }
                    Output::Status(s) => shared.emit(KeySessionEvent::Status(s)),
                    Output::Roster => roster = true,
                }
            }
            let me = session.lk_identity().clone();
            let verified: BTreeSet<LkIdentity> = session
                .roster()
                .iter()
                .filter(|e| e.leaf_index.is_some() && e.lk_identity != me)
                .map(|e| e.lk_identity.clone())
                .collect();
            if verified != c.last_verified {
                c.sink.set_verified(verified.clone());
                c.last_verified = verified;
                roster = true;
            }
            let ended = match session.state() {
                SessionState::Left { reason } if !c.ended => {
                    c.ended = true;
                    Some(reason)
                }
                _ => None,
            };
            (sends, roster, ended)
        };
        if roster_changed {
            shared.emit(KeySessionEvent::Roster);
        }
        if let Some(r) = ended
            && !matches!(r, LeftReason::Left)
        {
            shared.emit(KeySessionEvent::Ended(KeyEnd::Left(r)));
        }
        shared.changed.notify_waiters();
        if sends.is_empty() {
            return;
        }
        for (id, req) in sends {
            let r = shared.http.key_request(&req).await;
            let now = (shared.clock)();
            let mut guard = lock(&shared.core);
            let c = &mut *guard;
            if let Some(s) = c.session.as_mut() {
                let _ = s.handle(now, Input::Reply(id, r));
            }
        }
    }
}

/// Feeds one input to the session (if any), then pumps.
async fn input<C: FrameCrypto>(shared: &Arc<Shared<C>>, i: Input) {
    {
        let now = (shared.clock)();
        let mut guard = lock(&shared.core);
        let c = &mut *guard;
        if let Some(s) = c.session.as_mut()
            && let Err(e) = s.handle(now, i)
        {
            tracing::debug!(target: "sew_call_client::keys", error = %e, "input refused");
        }
    }
    pump(shared).await;
}

fn report_input(r: MediaReport) -> Input {
    match r {
        MediaReport::Attack { kind, identity } => Input::EngineAttack { kind, identity },
        MediaReport::Undecryptable {
            identity,
            since,
            failures,
            reason,
            slot,
        } => Input::SdkUndecryptable {
            identity,
            since,
            failures,
            reason,
            missing_index: slot,
        },
        MediaReport::DecryptOk { identity } => Input::SdkDecryptOk { identity },
    }
}

async fn on_socket<C: FrameCrypto>(shared: &Arc<Shared<C>>, ev: SocketEvent) -> Option<KeyEnd> {
    match ev {
        SocketEvent::CallKeys(m) => {
            let m = *m;
            let now = (shared.clock)();
            let known = {
                let mut guard = lock(&shared.core);
                let c = &mut *guard;
                let call = sew_e2ee::session::call_id_of(&m).cloned();
                let current = c.session.as_ref().map(|s| s.call_id().clone());
                match (call, current) {
                    (Some(call), Some(cur)) if call != cur => {
                        let size = serde_json::to_vec(&m).map(|v| v.len()).unwrap_or(1024);
                        c.buffer.push(now, call, m, size);
                        None
                    }
                    (Some(call), None) => {
                        let size = serde_json::to_vec(&m).map(|v| v.len()).unwrap_or(1024);
                        c.buffer.push(now, call, m, size);
                        None
                    }
                    _ => Some(m),
                }
            };
            if let Some(m) = known {
                input(shared, Input::Server(m)).await;
            }
            None
        }
        SocketEvent::Ready(r) => {
            // A new gateway session (RESUME refused): re-bind with the `state` route.
            if let Some(rec) = r.recording.clone() {
                shared.emit(KeySessionEvent::Recording(rec.map(Box::new)));
            }
            {
                let mut guard = lock(&shared.core);
                let c = &mut *guard;
                c.gateway_session = r.session_id.clone();
                if let Some(ck) = &r.call_keys
                    && let Err(e) = c.client.trust_mut().refresh(ck)
                {
                    tracing::warn!(target: "sew_call_client::keys", error = %e, "READY's trust anchors refused");
                }
            }
            input(
                shared,
                Input::GatewayReidentified {
                    gateway_session: r.session_id,
                },
            )
            .await;
            None
        }
        SocketEvent::Resumed => {
            input(shared, Input::GatewayResumed).await;
            None
        }
        SocketEvent::VoiceServerUpdated(u) => {
            shared.emit(KeySessionEvent::VoiceServerUpdated(u.clone()));
            let s = shared.clone();
            let (channel, join) = (u.channel_id, u.e2ee.join_id.clone());
            tokio::spawn(async move {
                if let Err(e) = key_join(&s, channel, join).await {
                    s.emit(KeySessionEvent::KeyJoinFailed(e));
                }
            });
            None
        }
        SocketEvent::VoiceStateUpdated(u) => {
            let members = {
                let mut guard = lock(&shared.core);
                let c = &mut *guard;
                let here = c.channel;
                if u.channel_id.is_some() && u.channel_id == here {
                    c.voice_members.insert(u.member_id);
                } else {
                    c.voice_members.remove(&u.member_id);
                }
                c.voice_members.iter().copied().collect::<Vec<_>>()
            };
            shared.emit(KeySessionEvent::VoiceStateUpdated(u));
            input(shared, Input::VoiceMembers(members)).await;
            None
        }
        SocketEvent::RecordingUpdated(r) => {
            shared.emit(KeySessionEvent::Recording(Some(r)));
            None
        }
        SocketEvent::Closed(SocketClosed::Revoked) => Some(KeyEnd::CredentialLost {
            reason: "session_revoked".to_owned(),
            rejoin: false,
        }),
        SocketEvent::Closed(SocketClosed::Ended { reason }) => Some(KeyEnd::Ended { reason }),
        SocketEvent::Closed(SocketClosed::Closed) => None,
        SocketEvent::Closed(other) => Some(KeyEnd::Socket(other)),
    }
}

async fn drive<C: FrameCrypto>(
    weak: std::sync::Weak<Shared<C>>,
    socket: Arc<CallSocket>,
    mut rx: mpsc::Receiver<SocketEvent>,
    stop: Arc<tokio::sync::Notify>,
    mut renew: Option<u64>,
) {
    loop {
        let next = {
            let Some(shared) = weak.upgrade() else {
                return;
            };
            let c = lock(&shared.core);
            let now = (shared.clock)();
            let mut next = now + TICK.as_millis() as u64;
            if let Some(t) = c.session.as_ref().and_then(|s| s.poll_timeout()) {
                next = next.min(t);
            }
            if let Some(t) = renew {
                next = next.min(t);
            }
            next.saturating_sub(now)
        };
        let ev = tokio::select! {
            _ = stop.notified() => return,
            e = rx.recv() => match e {
                Some(e) => Some(e),
                None => return,
            },
            _ = tokio::time::sleep(Duration::from_millis(next)) => None,
        };
        let Some(shared) = weak.upgrade() else {
            return;
        };
        if let Some(e) = ev
            && let Some(end) = on_socket(&shared, e).await
        {
            shared.emit(KeySessionEvent::Ended(end));
            return;
        }
        // Timers.
        let now = (shared.clock)();
        {
            let mut guard = lock(&shared.core);
            let c = &mut *guard;
            if let Some(s) = c.session.as_mut()
                && s.poll_timeout().is_some_and(|t| t <= now)
            {
                s.handle_timeout(now);
            }
            c.buffer.expire(now);
        }
        // The media path's reports (the `report` route, the receiver check).
        let reports = lock(&shared.core).sink.take_reports();
        for r in reports {
            input(&shared, report_input(r)).await;
        }
        pump(&shared).await;
        // The credential.
        if let Some(at) = renew
            && now >= at
        {
            match shared.http.renew_credential().await {
                Ok(a) => {
                    socket.reauth();
                    renew = Some(renew_at(now, unix_ms(a.expires_at)));
                }
                Err(f) if f.is_retryable() => {
                    renew = Some(now + 5_000);
                }
                Err(f) => {
                    let r = RenewRefused::from_failure(&f);
                    shared.emit(KeySessionEvent::Ended(KeyEnd::CredentialLost {
                        reason: f.reason.clone().unwrap_or_else(|| f.status.to_string()),
                        rejoin: r.allows_rejoin(),
                    }));
                    return;
                }
            }
        }
    }
}
