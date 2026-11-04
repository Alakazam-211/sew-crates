//! The E2EE interface: what the engine asks of the call's keys, and what the keys tell the engine.
//! This module fills it in (MLS through `sew-e2ee`, the call socket, key transport over the call
//! credential, the device store, the mandatory defences). The engine itself never holds a key.
//!
//! The rules the engine keeps on its side of the interface:
//! - **No plaintext mode.** Every outgoing Opus frame goes through [`KeySession::encrypt`];
//!   an `Err` drops the frame. There is no code path that sends a frame unencrypted, and no
//!   feature that adds one (Sew's own tests check this).
//! - **Verified receive only.** Every incoming frame goes through [`KeySession::decrypt`]; a
//!   frame that fails is counted and reported as lost, never played.
//! - **Subscribe only what the keys allow.** A remote track is kept only while
//!   [`KeySession::may_subscribe`] says [`Subscribe::Yes`]; otherwise the engine unsubscribes
//!   it (`UpdateSubscription`) and drops its frames.
//! - **`connected` needs keys.** The call state is `connected` only after
//!   [`E2eeEvent::Active`] (media up **and** a key epoch active).
//!
//! The real provider is [`MlsKeys`] (the default: `CallClient::with_mls`): the key session
//! (`session`: the call socket, READY's trust anchors, the device store, the KeyPackage upload
//! and the key join),
//! the mandatory checks (`guard`: [`PublishGuard`], [`SubscribeGuard`], [`SendGate`],
//! [`RecvGate`]) and sew-lk-compat's LkGcm ([`LkGcm`]) behind this interface (`mls`).
//!
//! `StaticKeys` (feature `testing`, never in a release build) is the loopback lanes'
//! stand-in: real LiveKit AES-GCM under one key every test client shares.

use std::fmt;
use std::sync::Arc;

use sew_proto::voice::{VoiceJoinResponse, VoiceServerUpdated, VoiceState};
use tokio::sync::{mpsc, watch};

use crate::provider::{BoxFuture, CallCredential};

pub mod credential;
pub mod frame;
pub mod guard;
pub mod keys;
pub mod lkgcm;
pub mod mls;
pub mod session;
pub mod transport;

pub use credential::{Bearer, CredentialGrant, RenewAnswer, RenewRefused, SharedBearer};
pub use frame::{FrameCrypto, IvParts, OpenError, SealError};
pub use guard::{
    PublishGuard, PublishRefused, RecvGate, RecvRefused, SendGate, SubscribeDecision,
    SubscribeGuard, SubscribeRefused,
};
pub use keys::{MediaCounters, MediaKeys, MediaReport, PublishState, engine_caps};
pub use lkgcm::LkGcm;
pub use mls::{MlsKeys, SessionJoin};
pub use session::{KeyEnd, KeyError, KeySessionConfig, KeySessionEvent, MlsSession};
pub use transport::{KeyTransport, TransportError};

/// A remote track as the engine sees it in the roster.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RemoteTrack {
    /// The engine's track sid (`TR_…`).
    pub sid: String,
    /// `TrackInfo.encryption` (LiveKit `Encryption.Type`: 1 = GCM).
    pub encryption: i32,
    /// `TrackInfo.mime_type`.
    pub mime: String,
    /// `TrackInfo.type` is audio (voice only: anything else is refused).
    pub audio: bool,
}

/// Whether a remote track may be subscribed.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Subscribe {
    /// Keep it (encrypted, and its publisher is bound in the leaf binding table).
    Yes,
    /// Not yet (the publisher is not in the roster yet): keep it, drop its frames for now.
    NotYet,
    /// Never: unsubscribe and report (the `report` route).
    No {
        /// Why (for the report and the logs).
        reason: String,
    },
}

/// Why a frame was not encrypted or decrypted.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CryptoError {
    /// No active send slot yet (the frame is dropped, never sent in the clear).
    #[error("no send key")]
    NoSendKey,
    /// No key for this sender or slot.
    #[error("no key")]
    NoKey,
    /// The frame did not verify (wrong key, tampered, malformed).
    #[error("bad frame")]
    BadFrame,
}

/// Something the keys tell the engine.
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
#[non_exhaustive]
pub enum E2eeEvent {
    /// A key epoch is active: with media up the call is `connected`.
    Active {
        /// The epoch.
        epoch: u64,
    },
    /// The keys refused the call (`e2ee_refused`, `ca_pin_mismatch`, …): the call ends.
    Refused {
        /// The `errors` code.
        code: String,
        /// Details for the host.
        detail: String,
    },
    /// A warning for the host (`key_storage_file`, `pins_reset`); the call goes on.
    Warning {
        /// The `errors` code.
        code: String,
        /// Details.
        detail: String,
    },
    /// Engine identities (`p_…`) and the members they are, from the roster (the
    /// `participants` event's `member_id`).
    Members(Vec<(String, String)>),
    /// The call channel's voice states (the call socket's `voice_state_updated`).
    VoiceStates(Vec<VoiceState>),
    /// A move or a token re-issue (the call socket's `voice_server_updated`): the engine
    /// reconnects signalling with the new token and room.
    ServerUpdated(VoiceServerUpdated),
    /// Sew no longer accepts the credential for this join (`call_credential_stale`, a swap):
    /// the call ends with `credential_lost`.
    CredentialLost,
    /// Sew ended this join and said why (Sew 0.7.2): the self copy of `voice_state_updated`
    /// leaving the call (its `reason`), or the call socket's `4011` close that named one.
    /// `reason` is Sew's voice leave reason as it arrived (`perms_lost`, `removed_from_space`,
    /// `session_revoked`, …); the call ends with it (`replaced`: `credential_lost`).
    Ended {
        /// Sew's reason.
        reason: String,
    },
}

/// The sending half the engine gives [`E2eeProvider::start`].
#[derive(Clone, Debug)]
pub struct E2eeEvents(pub(crate) mpsc::UnboundedSender<E2eeEvent>);

impl E2eeEvents {
    /// Reports an event; `false` once the call is gone.
    pub fn send(&self, ev: E2eeEvent) -> bool {
        self.0.send(ev).is_ok()
    }
}

/// The call credential as it renews: the engine renews it at half its life
/// (`POST /api/v1/me/voice/credential`) and publishes each new token here, so the key
/// transport and the call socket (`REAUTH`) always use the current one.
#[derive(Clone)]
pub struct CredentialHandle(pub(crate) watch::Receiver<Option<CallCredential>>);

impl fmt::Debug for CredentialHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CredentialHandle").finish_non_exhaustive()
    }
}

impl CredentialHandle {
    /// The current credential (`None`: the join had none).
    pub fn current(&self) -> Option<CallCredential> {
        self.0.borrow().clone()
    }

    /// The bearer token now.
    pub fn token(&self) -> Option<String> {
        self.0.borrow().as_ref().map(|c| c.token.clone())
    }

    /// Waits for the next renewal (`Err` once the call is gone).
    pub async fn changed(&mut self) -> Result<(), watch::error::RecvError> {
        self.0.changed().await
    }
}

/// What the keys get for one join.
#[derive(Clone)]
pub struct E2eeContext {
    /// Sew's join answer (`call_credential` is in [`E2eeContext::credential`]).
    pub join: VoiceJoinResponse,
    /// The door to Sew's client API (`JoinAnswer.api_base`).
    pub api_base: url::Url,
    /// The call credential, renewing.
    pub credential: CredentialHandle,
    /// An HTTP client with the engine's TLS trust (platform roots).
    pub http: reqwest::Client,
    /// The engine's TLS client config (for the call socket's WSS).
    pub tls: Arc<rustls::ClientConfig>,
    /// The host's app identifier (the device store's service name).
    pub app_id: String,
    /// The host's data directory (the device store's public data).
    pub data_dir: std::path::PathBuf,
    /// A re-join (`None` for the first join of this call).
    pub rejoin: Option<crate::provider::RejoinReason>,
}

impl fmt::Debug for E2eeContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("E2eeContext")
            .field("room", &self.join.room)
            .field("api_base", &self.api_base.as_str())
            .field("app_id", &self.app_id)
            .finish_non_exhaustive()
    }
}

/// Why the keys could not start.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {detail}")]
pub struct E2eeError {
    /// The `errors` code (`e2ee_refused`, `ca_pin_mismatch`, …).
    pub code: String,
    /// Details.
    pub detail: String,
}

/// The call's keys for one join. The engine calls the frame methods on its media path, so they
/// must be quick and never block on I/O.
pub trait KeySession: Send + Sync + 'static {
    /// Encrypts one Opus frame of the local track (`ssrc`, `rtp_ts` for the IV) into `out`.
    /// `Err` drops the frame: it is never sent unencrypted.
    fn encrypt(
        &self,
        frame: &[u8],
        ssrc: u32,
        rtp_ts: u32,
        out: &mut Vec<u8>,
    ) -> Result<(), CryptoError>;

    /// Decrypts and verifies one Opus frame from `sender` (engine identity `p_…`) into
    /// `out`. `Err`: the frame is never played.
    fn decrypt(&self, sender: &str, frame: &[u8], out: &mut Vec<u8>) -> Result<(), CryptoError>;

    /// Whether `publisher`'s `track` may be subscribed.
    fn may_subscribe(&self, publisher: &str, track: &RemoteTrack) -> Subscribe;

    /// The server confirmed the local publication (`TrackPublishedResponse.track`). Frames may
    /// leave only once a publication is confirmed encrypted (the publish check); `false`: refused
    /// for the rest of the call (a downgrade, reported).
    fn published(&self, track: &sew_lkproto::livekit::TrackInfo) -> bool {
        let _ = track;
        true
    }

    /// The engine roster changed: the remote engine identities now in the room.
    fn roster(&self, identities: &[String]) {
        let _ = identities;
    }

    /// The media session was rebuilt (`full`: a new engine participant, so re-assert the sender
    /// slot; the publish check).
    fn media_reconnected(&self, full: bool) {
        let _ = full;
    }

    /// The call ends: release the key session (best effort).
    fn close(&self) -> BoxFuture<'_, ()>;
}

/// Starts the keys of a join (the call socket, the device, a KeyPackage, the key join).
/// The engine connects signalling in parallel.
pub trait E2eeProvider: Send + Sync + 'static {
    /// Starts the keys for `ctx`; events go to `events`.
    fn start(
        &self,
        ctx: E2eeContext,
        events: E2eeEvents,
    ) -> BoxFuture<'_, Result<Arc<dyn KeySession>, E2eeError>>;
}

#[cfg(feature = "testing")]
pub use testing::StaticKeys;

/// TEST ONLY (feature `testing`): never compiled into a release build.
#[cfg(feature = "testing")]
mod testing {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use sew_lk_compat::e2ee::{Codec, FrameCryptor as _, FrameIv, FrameKey, LkGcmV1, SenderRing};

    use super::{
        CryptoError, E2eeContext, E2eeError, E2eeEvent, E2eeEvents, E2eeProvider, KeySession,
        RemoteTrack, Subscribe,
    };
    use crate::provider::BoxFuture;

    /// TEST ONLY: every client that holds the same 32-byte key decrypts every other. Real
    /// LiveKit AES-GCM frames (`LkGcmV1`, key index 0); tracks that are not GCM are refused.
    /// Emits [`E2eeEvent::Active`] at once.
    #[derive(Clone)]
    pub struct StaticKeys {
        ikm: [u8; 32],
    }

    impl std::fmt::Debug for StaticKeys {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("StaticKeys(TEST ONLY)")
        }
    }

    impl StaticKeys {
        /// TEST ONLY: keys from a shared 32-byte secret.
        pub fn new(ikm: [u8; 32]) -> StaticKeys {
            StaticKeys { ikm }
        }
    }

    struct Session {
        ring: SenderRing,
        counts: Mutex<HashMap<u32, u32>>,
    }

    impl KeySession for Session {
        fn encrypt(
            &self,
            frame: &[u8],
            ssrc: u32,
            rtp_ts: u32,
            out: &mut Vec<u8>,
        ) -> Result<(), CryptoError> {
            let Some(key) = self.ring.get(0) else {
                return Err(CryptoError::NoSendKey);
            };
            let send_count = {
                let mut c = self.counts.lock().map_err(|_| CryptoError::NoSendKey)?;
                let n = c
                    .entry(ssrc)
                    .or_insert_with(|| u32::from(rand::random::<u16>()));
                *n = n.wrapping_add(1);
                *n
            };
            let iv = FrameIv {
                ssrc,
                timestamp: rtp_ts,
                send_count,
            };
            LkGcmV1
                .encrypt(Codec::Opus, frame, key, 0, iv, out)
                .map_err(|_| CryptoError::BadFrame)
        }

        fn decrypt(
            &self,
            _sender: &str,
            frame: &[u8],
            out: &mut Vec<u8>,
        ) -> Result<(), CryptoError> {
            LkGcmV1
                .decrypt(Codec::Opus, frame, &self.ring, out)
                .map(|_| ())
                .map_err(|_| CryptoError::BadFrame)
        }

        fn may_subscribe(&self, _publisher: &str, track: &RemoteTrack) -> Subscribe {
            if track.encryption == sew_lk_compat::session::ENCRYPTION_GCM {
                Subscribe::Yes
            } else {
                Subscribe::No {
                    reason: "not_encrypted".to_owned(),
                }
            }
        }

        fn close(&self) -> BoxFuture<'_, ()> {
            Box::pin(async {})
        }
    }

    impl E2eeProvider for StaticKeys {
        fn start(
            &self,
            _ctx: E2eeContext,
            events: E2eeEvents,
        ) -> BoxFuture<'_, Result<Arc<dyn KeySession>, E2eeError>> {
            let ikm = self.ikm;
            Box::pin(async move {
                let mut ring = SenderRing::new();
                ring.set(0, FrameKey::derive(&ikm));
                let s: Arc<dyn KeySession> = Arc::new(Session {
                    ring,
                    counts: Mutex::new(HashMap::new()),
                });
                events.send(E2eeEvent::Active { epoch: 0 });
                Ok(s)
            })
        }
    }
}
