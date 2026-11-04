//! [`MlsKeys`]: the engine's real E2EE provider, and the default one (`CallClient::with_mls`).
//! It fills the engine's E2EE interface ([`E2eeProvider`] / [`KeySession`]) with the key session:
//!
//! - **start**: the call socket with the call credential (READY: `session_id`, the install id and
//!   the trust anchors, TOFU-pinned), the device (loaded or enrolled), a KeyPackage for
//!   `call:<channel>` (uploaded), then the deferred key join with the credential's `join_id`. A
//!   host that holds its own Sew session instead of a call credential prepares the key join before
//!   its voice join ([`MlsKeys::prepare_session_join`], carried inline in the voice join) and the
//!   answer's `e2ee` starts the session.
//! - **frames**: [`KeySession::encrypt`] is the [`SendGate`] (sealed with
//!   sew-lk-compat's LkGcm under the active slot, or dropped), [`KeySession::decrypt`] is the
//!   [`RecvGate`] (verified, subscribed senders only), [`KeySession::may_subscribe`] is the
//!   [`SubscribeGuard`], and the publication counts only once the server echoed GCM
//!   ([`PublishGuard`], [`KeySession::published`]).
//! - **events** to the engine: `Active{epoch}` when a key epoch is active and the local sender
//!   has its key, `Members` from the verified roster, `VoiceStates` and `ServerUpdated` from the
//!   call socket, `Warning` (`key_storage_file`, `pins_reset`), `Refused` and
//!   `CredentialLost`.
//! - **renewal**: the engine renews the credential; each new token reaches the key routes and
//!   the call socket's REAUTH here.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sew_e2ee::{JoinId, LeftReason, LkIdentity};
use sew_lkproto::livekit::{TrackInfo, TrackType};
use sew_proto::e2ee::{E2eeJoin, RemoveReason};
use sew_proto::id::Id;
use sew_proto::voice::VoiceStateUpdated;
use tokio::sync::{Notify, mpsc};

use super::credential::Bearer;
use super::guard::{
    PublishGuard, RecvGate, RecvRefused, SendGate, SubscribeDecision, SubscribeGuard,
};
use super::keys::{MediaKeys, lock};
use super::lkgcm::LkGcm;
use super::session::{KeyEnd, KeyError, KeySessionConfig, KeySessionEvent, MlsSession};
use super::{
    CredentialHandle, CryptoError, E2eeContext, E2eeError, E2eeEvent, E2eeEvents, E2eeProvider,
    KeySession, RemoteTrack, Subscribe,
};
use crate::Clock;
use crate::config::ClientConfig;
use crate::device::{StoreChoice, VaultConfig, platform_name};
use crate::provider::BoxFuture;

/// How often the bridge re-checks the key state when nothing else happened.
const BRIDGE_TICK: Duration = Duration::from_millis(100);

fn refused(code: &str, detail: impl Into<String>) -> E2eeError {
    E2eeError {
        code: code.to_owned(),
        detail: detail.into(),
    }
}

fn from_key_error(e: &KeyError) -> E2eeError {
    refused(e.code(), e.to_string())
}

/// What a host with its own Sew session (not a call credential) gives
/// [`MlsKeys::prepare_session_join`].
#[derive(Clone)]
pub struct SessionJoin {
    /// Sew's client API (the host's door).
    pub api_base: url::Url,
    /// The host's Sew session token (`sews_…`); held in Rust memory only.
    pub bearer: String,
    /// A stable 32-hex scope for this login (the stored device is keyed on it).
    pub device_scope: String,
    /// The voice channel (or DM call channel) being joined.
    pub channel: Id,
}

impl fmt::Debug for SessionJoin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionJoin")
            .field("api_base", &self.api_base.as_str())
            .field("bearer", &"<redacted>")
            .field("device_scope", &self.device_scope)
            .field("channel", &self.channel)
            .finish()
    }
}

struct Pending {
    session: MlsSession<LkGcm>,
    events: mpsc::UnboundedReceiver<KeySessionEvent>,
}

/// Sew's MLS call keys for the engine (the default [`E2eeProvider`]).
pub struct MlsKeys {
    store: StoreChoice,
    label: String,
    clock: Clock,
    pending: Mutex<Option<Pending>>,
}

impl fmt::Debug for MlsKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MlsKeys")
            .field("store", &self.store)
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

impl Default for MlsKeys {
    fn default() -> MlsKeys {
        MlsKeys::new(StoreChoice::Auto)
    }
}

impl MlsKeys {
    /// Keys with the device key in `store` (`Auto`: a 0600 file with a `key_storage_file`
    /// warning, or with feature `keyring-store` the OS credential store when it answers;
    /// `Host`: the host's own [`crate::device::DeviceKeyStore`]).
    pub fn new(store: StoreChoice) -> MlsKeys {
        MlsKeys {
            store,
            label: format!("Sew voice on {}", platform_name()),
            clock: crate::system_clock(),
            pending: Mutex::new(None),
        }
    }

    /// The device label Sew shows ("K2 on macOS").
    pub fn with_label(mut self, label: &str) -> MlsKeys {
        label.clone_into(&mut self.label);
        self
    }

    /// The wall clock the key session reads (tests run a server on a clock of their own).
    pub fn with_clock(mut self, clock: Clock) -> MlsKeys {
        self.clock = clock;
        self
    }

    fn vault(&self, app_id: &str, data_dir: &std::path::Path) -> VaultConfig {
        VaultConfig {
            data_dir: data_dir.to_path_buf(),
            app_id: app_id.to_owned(),
            store: self.store.clone(),
            label: self.label.clone(),
        }
    }

    /// For a host that joins with its own Sew session rather than a call credential (a
    /// desktop that holds a `sews_` session; the key join inline): opens the call socket, loads or
    /// enrols the device, uploads a KeyPackage for `join.channel` and answers the voice join's
    /// `e2ee` object. The host sends its join with it; the engine's next [`E2eeProvider::start`]
    /// with an answer that has `e2ee` and no `call_credential` takes this session over. One
    /// prepared join is held at a time (one call per app); a new one replaces it.
    pub async fn prepare_session_join(
        &self,
        cfg: &ClientConfig,
        join: SessionJoin,
    ) -> Result<E2eeJoin, E2eeError> {
        let tls = cfg
            .tls()
            .map_err(|e| refused("network", format!("tls: {e}")))?;
        let kcfg = KeySessionConfig {
            api_base: join.api_base.clone(),
            bearer: Bearer::new(join.bearer.clone()),
            credential_expires_at_ms: None,
            device_scope: join.device_scope.clone(),
            vault: self.vault(&cfg.app_id, &cfg.data_dir),
            install_id: None,
            clock: self.clock.clone(),
            tls: Some(tls),
        };
        let (session, events) = MlsSession::connect(kcfg, MediaKeys::new(LkGcm))
            .await
            .map_err(|e| from_key_error(&e))?;
        let e2ee = session
            .prepare_join(join.channel)
            .await
            .map_err(|e| from_key_error(&e))?;
        *lock(&self.pending) = Some(Pending { session, events });
        Ok(e2ee)
    }

    async fn start_keys(
        &self,
        ctx: E2eeContext,
        events: E2eeEvents,
    ) -> Result<Arc<dyn KeySession>, E2eeError> {
        let (session, rx) = match ctx.credential.current() {
            Some(cred) => {
                let join_id = JoinId::parse(&cred.join_id).map_err(|_| {
                    refused("e2ee_refused", "the call credential's join_id is malformed")
                })?;
                let kcfg = KeySessionConfig {
                    api_base: ctx.api_base.clone(),
                    bearer: Bearer::new(cred.token.clone()),
                    // The engine renews the credential; the bridge passes each new token on.
                    credential_expires_at_ms: None,
                    device_scope: cred.device_scope.clone(),
                    vault: self.vault(&ctx.app_id, &ctx.data_dir),
                    install_id: None,
                    clock: self.clock.clone(),
                    tls: Some(ctx.tls.clone()),
                };
                let (session, rx) = MlsSession::connect(kcfg, MediaKeys::new(LkGcm))
                    .await
                    .map_err(|e| from_key_error(&e))?;
                // The deferred key join: a KeyPackage for the call's channel, then the key join.
                session
                    .key_join(ctx.join.state.channel_id, join_id)
                    .await
                    .map_err(|e| from_key_error(&e))?;
                (session, rx)
            }
            None => {
                let pending = lock(&self.pending).take().ok_or_else(|| {
                    refused(
                        "e2ee_refused",
                        "the join answer has no call_credential and no session join was prepared",
                    )
                })?;
                let init = ctx.join.e2ee.clone().ok_or_else(|| {
                    refused(
                        "e2ee_refused",
                        "the join answer has neither call_credential nor e2ee",
                    )
                })?;
                pending
                    .session
                    .begin(init)
                    .await
                    .map_err(|e| from_key_error(&e))?;
                (pending.session, pending.events)
            }
        };
        let session = Arc::new(session);
        let keys = session.media_keys();
        let stop = Arc::new(Notify::new());
        let clock = self.clock.clone();
        let ks = Arc::new(MlsKeySession {
            session: session.clone(),
            send: SendGate::new(&keys),
            recv: RecvGate::new(&keys, clock),
            subscribe: SubscribeGuard::new(&keys),
            publish: PublishGuard::new(&keys),
            keys,
            roster: Mutex::new(BTreeSet::new()),
            stop: stop.clone(),
        });
        let me = ctx.join.state.member_id;
        tokio::spawn(bridge(
            session,
            rx,
            events,
            ctx.credential.clone(),
            stop,
            me,
        ));
        Ok(ks)
    }
}

impl E2eeProvider for MlsKeys {
    fn start(
        &self,
        ctx: E2eeContext,
        events: E2eeEvents,
    ) -> BoxFuture<'_, Result<Arc<dyn KeySession>, E2eeError>> {
        Box::pin(self.start_keys(ctx, events))
    }
}

/// One join's keys as the engine's media path sees them.
struct MlsKeySession {
    session: Arc<MlsSession<LkGcm>>,
    keys: MediaKeys<LkGcm>,
    send: SendGate<LkGcm>,
    recv: RecvGate<LkGcm>,
    subscribe: SubscribeGuard<LkGcm>,
    publish: PublishGuard<LkGcm>,
    roster: Mutex<BTreeSet<String>>,
    stop: Arc<Notify>,
}

impl fmt::Debug for MlsKeySession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MlsKeySession")
            .field("keys", &self.keys)
            .finish_non_exhaustive()
    }
}

impl KeySession for MlsKeySession {
    fn encrypt(
        &self,
        frame: &[u8],
        ssrc: u32,
        rtp_ts: u32,
        out: &mut Vec<u8>,
    ) -> Result<(), CryptoError> {
        match self.send.seal(ssrc, rtp_ts, frame) {
            Some(sealed) => {
                out.extend_from_slice(&sealed);
                Ok(())
            }
            None if self.keys.local_slot().is_none() => Err(CryptoError::NoSendKey),
            None => Err(CryptoError::BadFrame),
        }
    }

    fn decrypt(&self, sender: &str, frame: &[u8], out: &mut Vec<u8>) -> Result<(), CryptoError> {
        let Ok(id) = LkIdentity::parse(sender) else {
            return Err(CryptoError::BadFrame);
        };
        match self.recv.open(&id, frame) {
            Ok(opus) => {
                out.extend_from_slice(&opus);
                Ok(())
            }
            Err(RecvRefused::Unverified) => Err(CryptoError::NoKey),
            Err(RecvRefused::Undecryptable(_)) => Err(CryptoError::BadFrame),
        }
    }

    fn may_subscribe(&self, publisher: &str, track: &RemoteTrack) -> Subscribe {
        let info = TrackInfo {
            sid: track.sid.clone(),
            r#type: if track.audio {
                TrackType::Audio as i32
            } else {
                TrackType::Video as i32
            },
            encryption: track.encryption,
            mime_type: track.mime.clone(),
            ..TrackInfo::default()
        };
        match self.subscribe.check(publisher, &info) {
            SubscribeDecision::Subscribe => Subscribe::Yes,
            SubscribeDecision::Wait => Subscribe::NotYet,
            SubscribeDecision::Refuse(r) => Subscribe::No {
                reason: format!("{r:?}"),
            },
        }
    }

    fn published(&self, track: &TrackInfo) -> bool {
        // Every publication (the first, a full reconnect's, a move's) is confirmed afresh; a
        // refusal holds for the rest of the call.
        self.publish.on_republish();
        match self.publish.on_published(track) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(target: "sew_call_client::keys", error = %e, "the publication is refused");
                false
            }
        }
    }

    fn roster(&self, identities: &[String]) {
        let now: BTreeSet<String> = identities.iter().cloned().collect();
        let gone: Vec<String> = {
            let mut known = lock(&self.roster);
            let gone = known.difference(&now).cloned().collect();
            *known = now;
            gone
        };
        for id in gone {
            self.subscribe.unsubscribed(&id);
        }
    }

    fn media_reconnected(&self, full: bool) {
        // The sender slot is re-asserted by the key session itself (its stuck-sender check);
        // the new publication is confirmed through `published`, and until then the engine has
        // no send path.
        let _ = full;
    }

    fn close(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.stop.notify_one();
            self.session.leave().await;
        })
    }
}

impl Drop for MlsKeySession {
    fn drop(&mut self) {
        self.stop.notify_one();
    }
}

/// Turns the key session's events into the engine's, passes the engine's renewed credential to
/// the key routes and the call socket, and reports `Active` and `Members` as they change.
async fn bridge(
    session: Arc<MlsSession<LkGcm>>,
    mut rx: mpsc::UnboundedReceiver<KeySessionEvent>,
    events: E2eeEvents,
    mut credential: CredentialHandle,
    stop: Arc<Notify>,
    me: Id,
) {
    let mut last_epoch: Option<u64> = None;
    let mut last_members: Vec<(String, String)> = Vec::new();
    // Only renewals of this join's credential: a re-join publishes another credential, which
    // belongs to the next key session.
    let join_id = credential.current().map(|c| c.join_id);
    let mut renewals = join_id.is_some();
    loop {
        let ev = tokio::select! {
            () = stop.notified() => return,
            e = rx.recv() => match e {
                Some(e) => Some(e),
                None => return,
            },
            r = credential.changed(), if renewals => {
                match r {
                    Ok(()) => match credential.current() {
                        Some(c) if Some(&c.join_id) == join_id.as_ref() => {
                            session.renewed(Bearer::new(c.token));
                        }
                        _ => renewals = false,
                    },
                    Err(_) => renewals = false,
                }
                None
            }
            () = tokio::time::sleep(BRIDGE_TICK) => None,
        };
        let mut out = Vec::new();
        if let Some(ev) = ev {
            match ev {
                KeySessionEvent::Status(_) | KeySessionEvent::Roster => {}
                KeySessionEvent::VoiceServerUpdated(u) => out.push(E2eeEvent::ServerUpdated(*u)),
                KeySessionEvent::VoiceStateUpdated(u) => {
                    if let Some(reason) = self_left(&u, me) {
                        out.push(E2eeEvent::Ended { reason });
                    } else if let Some(st) = u.state {
                        out.push(E2eeEvent::VoiceStates(vec![st]));
                    }
                }
                KeySessionEvent::Warning(w) => out.push(E2eeEvent::Warning {
                    code: w.code().to_owned(),
                    detail: String::new(),
                }),
                KeySessionEvent::KeyJoinFailed(e) => out.push(E2eeEvent::Refused {
                    code: e.code().to_owned(),
                    detail: format!("the move's key join failed: {e}"),
                }),
                KeySessionEvent::Ended(KeyEnd::CredentialLost { .. }) => {
                    out.push(E2eeEvent::CredentialLost);
                }
                KeySessionEvent::Ended(KeyEnd::Ended { reason }) => {
                    out.push(E2eeEvent::Ended { reason });
                }
                KeySessionEvent::Ended(KeyEnd::Socket(why)) => out.push(E2eeEvent::Refused {
                    code: "network".to_owned(),
                    detail: format!("the call socket ended: {why:?}"),
                }),
                // The key group removed this device and said why (Sew's voice leave reason;
                // a move re-keys instead of ending the call).
                KeySessionEvent::Ended(KeyEnd::Left(LeftReason::Removed(r)))
                    if r != RemoveReason::Moved =>
                {
                    out.push(E2eeEvent::Ended {
                        reason: r.as_str().to_owned(),
                    });
                }
                KeySessionEvent::Ended(KeyEnd::Left(why)) => out.push(E2eeEvent::Refused {
                    code: "e2ee_refused".to_owned(),
                    detail: format!("the key session left: {why:?}"),
                }),
            }
        }
        let members = session.members();
        if members != last_members {
            last_members.clone_from(&members);
            out.push(E2eeEvent::Members(members));
        }
        if session.is_active() {
            let epoch = session.active_epoch();
            if epoch != last_epoch {
                last_epoch = epoch;
                out.push(E2eeEvent::Active {
                    epoch: epoch.unwrap_or(0),
                });
            }
        }
        for e in out {
            if !events.send(e) {
                return;
            }
        }
    }
}

/// The reason of the self copy of `voice_state_updated` that takes this device's member out of
/// voice (`state` absent, `reason` given: Sew sends a reason to the member only). A move keeps
/// the member in voice (`state` present) and reaches the engine as `voice_server_updated`.
fn self_left(u: &VoiceStateUpdated, me: Id) -> Option<String> {
    if u.member_id != me || u.state.is_some() {
        return None;
    }
    u.reason.map(|r| r.as_str().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sew_proto::voice::VoiceLeaveReason;

    fn left(member: u64, reason: Option<VoiceLeaveReason>) -> VoiceStateUpdated {
        VoiceStateUpdated {
            member_id: Id::new_const(member),
            space_id: None,
            channel_id: None,
            previous_channel_id: Some(Id::new_const(900)),
            state: None,
            reason,
        }
    }

    #[test]
    fn the_self_copy_of_a_leave_keeps_its_reason() {
        let me = Id::new_const(7);
        for r in [
            VoiceLeaveReason::PermsLost,
            VoiceLeaveReason::RemovedFromSpace,
            VoiceLeaveReason::TimedOut,
            VoiceLeaveReason::ChannelArchived,
            VoiceLeaveReason::AccountRemoved,
            VoiceLeaveReason::RemovedByModerator,
            VoiceLeaveReason::SessionRevoked,
            VoiceLeaveReason::DeviceRevoked,
        ] {
            assert_eq!(
                self_left(&left(7, Some(r)), me).as_deref(),
                Some(r.as_str())
            );
        }
        // Someone else's leave, a leave without a reason, and a state that stays in voice are
        // not this device's end.
        assert_eq!(
            self_left(&left(8, Some(VoiceLeaveReason::PermsLost)), me),
            None
        );
        assert_eq!(self_left(&left(7, None), me), None);
    }
}
