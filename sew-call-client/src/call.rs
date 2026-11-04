//! The public engine API: [`CallClient`] (one per app) and [`Call`] (one live
//! call), their events, and the frame API the audio pipeline (kessel-audio, or a host's own)
//! drives.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bytes::Bytes;
use serde::Serialize;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::config::ClientConfig;
use crate::e2ee::E2eeProvider;
use crate::engine;
use crate::provider::{JoinProvider, JoinRequest, ProviderError};

/// Frames the send queue holds (20 ms each: one second).
pub(crate) const SEND_QUEUE: usize = 50;
/// Items the receive queue holds before it drops (and counts) new frames.
pub(crate) const RECV_QUEUE: usize = 1024;

/// Why a call ended (`ended.reason`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EndReason {
    /// The host left.
    Left,
    /// The host has no Sew session.
    SigninRequired,
    /// The network or the engine is gone for good (or relay-only had no `turns:`).
    Network,
    /// The keys refused the call.
    E2eeRefused,
    /// Sew no longer accepts the call credential (another device took over: `replaced`, or a
    /// 4011 from a Sew older than 0.7.2 with no engine `Disconnect` before it).
    CredentialLost,
    /// The engine removed this participant and Sew gave no reason within 1.5 s (`kicked`).
    Kicked,
    /// The engine closed or deleted the room and Sew gave no reason within 1.5 s
    /// (`room_closed`).
    RoomClosed,
    /// Sew ended this join and said why (Sew 0.7.2): its voice leave reason as it arrived
    /// (`perms_lost`, `removed_from_space`, `timed_out`, `channel_archived`,
    /// `account_removed`, `removed_by_moderator`, `session_revoked`, `device_revoked`, …;
    /// an open set). From the call socket's self copy of `voice_state_updated`, its `4011`
    /// close, or the key group's removal.
    Removed(String),
    /// Sew refused the (re-)join; the code is in the `errors` event.
    Refused(String),
}

/// The call's state (`state` event).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
#[non_exhaustive]
pub enum CallState {
    /// Getting media and keys up.
    Connecting,
    /// Media is up **and** a key epoch is active: you can hear and be heard.
    Connected,
    /// Resuming or reconnecting (attempt counts from 1).
    Reconnecting {
        /// The attempt.
        attempt: u32,
    },
    /// Over.
    Ended {
        /// Why.
        reason: EndReason,
    },
}

/// One participant (`participants` event). Names are the host's.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct Participant {
    /// The engine identity (`p_…`, one per join).
    pub identity: String,
    /// The Sew member, once the keys' roster or the call socket named it.
    pub member_id: Option<String>,
    /// This device.
    pub is_self: bool,
    /// Muted (self or by a moderator, the track is muted, or SPEAK denied).
    pub muted: bool,
    /// Deafened.
    pub deafened: bool,
    /// Muted by a moderator.
    pub server_muted: bool,
    /// Deafened by a moderator.
    pub server_deafened: bool,
    /// The member lacks SPEAK here (Sew's `VoiceState.speak_denied`; for this device also the
    /// engine's permission): nothing is captured or published until it is granted again. Not a
    /// moderator mute (`server_muted`).
    pub speak_denied: bool,
}

/// A speaking level (`speaking` event).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[non_exhaustive]
pub struct Speaking {
    /// The engine identity.
    pub identity: String,
    /// The member, when known.
    pub member_id: Option<String>,
    /// 0..1.
    pub level: f32,
}

/// An error for the host (`errors` event): `{code, detail}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct CallErrorEvent {
    /// `turn_unreachable`, `relay_only_no_turns`, `e2ee_refused`, a provider's code, …
    pub code: String,
    /// Details.
    pub detail: String,
}

impl CallErrorEvent {
    /// An error with its code and details (for a host's own engine or a test).
    pub fn new(code: impl Into<String>, detail: impl Into<String>) -> CallErrorEvent {
        CallErrorEvent {
            code: code.into(),
            detail: detail.into(),
        }
    }
}

/// Something that happened in the call.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum CallEvent {
    /// The state changed.
    State(CallState),
    /// The participants changed.
    Participants(Vec<Participant>),
    /// Speaking levels (at most 10 Hz).
    Speaking(Vec<Speaking>),
    /// An error or warning.
    Error(CallErrorEvent),
    /// A moderator moved this member (Sew's `voice_server_updated`): the call goes on in the
    /// destination. Sent before the engine reconnects there; a re-join after it asks the
    /// provider for [`CallMoved::room`].
    Moved(CallMoved),
}

/// Where a moderator move took the call (Sew's `voice_server_updated`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct CallMoved {
    /// The destination voice channel (Sew's id, a decimal string).
    pub channel_id: String,
    /// The destination DM call, when the destination is one (Sew's id, a decimal string).
    pub call_id: Option<String>,
    /// The destination's media room (`r_...`).
    pub media_room: String,
}

impl CallMoved {
    /// The move in Sew's `voice_server_updated`.
    pub fn from_update(u: &sew_proto::voice::VoiceServerUpdated) -> CallMoved {
        CallMoved {
            channel_id: u.channel_id.to_string(),
            call_id: u.call_id.map(|c| c.to_string()),
            media_room: u.room.as_str().to_owned(),
        }
    }

    /// The `room` a host joins for the destination: the DM call's id when it is a call, else
    /// the voice channel's id. The engine's re-joins after the move ask for this room.
    pub fn room(&self) -> &str {
        self.call_id.as_deref().unwrap_or(&self.channel_id)
    }
}

/// Counters of one call.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct CallCounters {
    /// Opus frames encrypted and sent.
    pub frames_sent: u64,
    /// Frames dropped before sending (no media path, no send key, muted).
    pub frames_not_sent: u64,
    /// Frames the send queue was too full for.
    pub send_queue_drops: u64,
    /// Remote frames decrypted and handed over.
    pub frames_received: u64,
    /// Remote frames that did not decrypt (never played).
    pub decrypt_failures: u64,
    /// Remote frames of tracks the keys do not allow (never played).
    pub refused_frames: u64,
    /// Frames the receive queue was too full for.
    pub recv_queue_drops: u64,
    /// Packets reported lost.
    pub lost_marks: u64,
    /// Resumes completed.
    pub resumes: u64,
    /// Full reconnects completed.
    pub full_reconnects: u64,
    /// Re-joins through the provider.
    pub rejoins: u64,
    /// TURN allocations made.
    pub turn_allocations: u64,
    /// Call-credential renewals.
    pub credential_renewals: u64,
}

/// A point-in-time view of a call (`state` command).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[non_exhaustive]
pub struct CallSnapshot {
    /// The state.
    pub state: CallState,
    /// The participants (this device first).
    pub participants: Vec<Participant>,
    /// The relay's transport in use (`udp`, `tcp`, `tls`), if any.
    pub relay: Option<String>,
    /// Whether this join is relay-only.
    pub relay_only: bool,
    /// Counters.
    pub counters: CallCounters,
    /// The last moderator move, if any ([`CallEvent::Moved`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moved: Option<CallMoved>,
}

impl Default for CallSnapshot {
    fn default() -> CallSnapshot {
        CallSnapshot {
            state: CallState::Connecting,
            participants: Vec::new(),
            relay: None,
            relay_only: false,
            counters: CallCounters::default(),
            moved: None,
        }
    }
}

/// One encoded 20 ms Opus frame to send (kessel-audio's `EncodedFrame`).
#[derive(Clone, Debug, PartialEq)]
pub struct OpusFrame {
    /// The plaintext Opus packet (the engine encrypts it).
    pub payload: Bytes,
    /// Capture timestamp in 48 kHz units (the RTP timestamp; wraps).
    pub ts48k: u32,
    /// Voice activity after processing.
    pub vad: bool,
    /// Processed level, dBov (0 full scale … -127).
    pub level_dbov: f32,
}

/// One decrypted, verified remote frame (kessel-audio's `RecvFrame`).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct RecvOpus {
    /// The engine's stable id for the remote track.
    pub participant: u64,
    /// The publisher's engine identity.
    pub identity: String,
    /// The Opus packet (plaintext).
    pub payload: Bytes,
    /// RTP sequence number.
    pub seq: u16,
    /// RTP timestamp, 48 kHz units.
    pub ts48k: u32,
    /// When it arrived.
    pub arrival: Instant,
}

/// What the receive side hands the audio pipeline, in order.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum RecvItem {
    /// A remote track starts (call `add_participant` before its frames).
    Added {
        /// Its id.
        participant: u64,
        /// The publisher's engine identity.
        identity: String,
    },
    /// A remote track ends.
    Removed {
        /// Its id.
        participant: u64,
    },
    /// A frame.
    Frame(RecvOpus),
    /// A packet is known lost (a gap that did not fill, or a frame that failed decryption).
    Lost {
        /// The track.
        participant: u64,
        /// Its RTP sequence number.
        seq: u16,
    },
    /// Capture on (`true`) or off (muted, by self or a moderator, or SPEAK denied).
    Capture(bool),
    /// Playout off (`true`, deafened) or on.
    Deafened(bool),
}

/// The receive side's queue (one consumer).
#[derive(Debug)]
pub struct FrameStream(pub(crate) mpsc::Receiver<RecvItem>);

impl FrameStream {
    /// The next item; `None` once the call ended.
    pub async fn recv(&mut self) -> Option<RecvItem> {
        self.0.recv().await
    }

    /// The next item if one is waiting.
    pub fn try_recv(&mut self) -> Option<RecvItem> {
        self.0.try_recv().ok()
    }
}

/// Why a call could not start or a command failed.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum CallError {
    /// One call per app.
    #[error("already_in_call")]
    AlreadyInCall,
    /// The provider refused.
    #[error(transparent)]
    Provider(#[from] ProviderError),
    /// The call has ended.
    #[error("the call has ended")]
    Ended,
    /// The engine could not set itself up (TLS roots, HTTP client).
    #[error("setup: {0}")]
    Setup(String),
}

pub(crate) enum Cmd {
    Mute {
        muted: Option<bool>,
        deafened: Option<bool>,
        reply: oneshot::Sender<()>,
    },
    Leave(oneshot::Sender<()>),
}

/// The engine of one app: at most one live call.
pub struct CallClient {
    cfg: ClientConfig,
    e2ee: Arc<dyn E2eeProvider>,
    busy: Arc<AtomicBool>,
}

impl std::fmt::Debug for CallClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallClient")
            .field("cfg", &self.cfg)
            .finish_non_exhaustive()
    }
}

impl CallClient {
    /// An engine with `cfg` whose call keys come from `e2ee` (the `sew-e2ee` key session;
    /// there is no plaintext alternative).
    pub fn new(cfg: ClientConfig, e2ee: Arc<dyn E2eeProvider>) -> CallClient {
        CallClient {
            cfg,
            e2ee,
            busy: Arc::new(AtomicBool::new(false)),
        }
    }

    /// An engine with `cfg` and Sew's MLS call keys ([`crate::e2ee::MlsKeys`]), the device key
    /// where `cfg.device_store` says (by default a 0600 file; the host's own store with
    /// [`ClientConfig::with_device_key_store`]): the production constructor.
    pub fn with_mls(cfg: ClientConfig) -> CallClient {
        let keys = crate::e2ee::MlsKeys::new(cfg.device_store.clone());
        CallClient::new(cfg, Arc::new(keys))
    }

    /// Joins through `provider` and starts the call.
    pub async fn join(
        &self,
        provider: Arc<dyn JoinProvider>,
        req: JoinRequest,
    ) -> Result<Call, CallError> {
        if self.busy.swap(true, Ordering::AcqRel) {
            return Err(CallError::AlreadyInCall);
        }
        let busy = BusyGuard(self.busy.clone());
        let answer = provider.join(req.clone()).await?;
        let (events, _) = broadcast::channel(256);
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (push_tx, push_rx) = mpsc::channel(SEND_QUEUE);
        let (frames_tx, frames_rx) = mpsc::channel(RECV_QUEUE);
        let snapshot = Arc::new(Mutex::new(CallSnapshot::default()));
        let shared = Arc::new(engine::Shared {
            events: events.clone(),
            snapshot: snapshot.clone(),
            send_queue_drops: std::sync::atomic::AtomicU64::new(0),
        });
        engine::spawn(engine::Start {
            cfg: self.cfg.clone(),
            e2ee: self.e2ee.clone(),
            provider,
            req,
            answer,
            shared: shared.clone(),
            cmd_rx,
            push_rx,
            frames_tx,
            busy,
        })
        .map_err(CallError::Setup)?;
        Ok(Call {
            cmd: cmd_tx,
            push: push_tx,
            shared,
            frames: Mutex::new(Some(FrameStream(frames_rx))),
        })
    }
}

/// Clears the app's call slot when the engine task ends.
pub(crate) struct BusyGuard(Arc<AtomicBool>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// The send queue of a call, cloneable ([`Call::frame_sender`]).
#[derive(Clone)]
pub struct FrameSender {
    push: mpsc::Sender<OpusFrame>,
    shared: Arc<engine::Shared>,
}

impl std::fmt::Debug for FrameSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameSender").finish_non_exhaustive()
    }
}

impl FrameSender {
    /// Queues one encoded frame (see [`Call::push_opus`]).
    pub fn push_opus(&self, f: OpusFrame) {
        if self.push.try_send(f).is_err() {
            self.shared.send_queue_drops.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// One live call.
pub struct Call {
    cmd: mpsc::UnboundedSender<Cmd>,
    push: mpsc::Sender<OpusFrame>,
    shared: Arc<engine::Shared>,
    frames: Mutex<Option<FrameStream>>,
}

impl std::fmt::Debug for Call {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Call").finish_non_exhaustive()
    }
}

impl Call {
    /// The call's events (state, participants, speaking, errors).
    pub fn events(&self) -> broadcast::Receiver<CallEvent> {
        self.shared.events.subscribe()
    }

    /// The current state and participants.
    pub fn snapshot(&self) -> CallSnapshot {
        let mut s = self
            .shared
            .snapshot
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default();
        s.counters.send_queue_drops = self.shared.send_queue_drops.load(Ordering::Relaxed);
        s
    }

    /// Mute and deafen (`None` leaves it). Deafened implies muted; un-deafening returns
    /// `muted` to its value before the deafen.
    pub async fn mute(&self, muted: Option<bool>, deafened: Option<bool>) -> Result<(), CallError> {
        let (reply, rx) = oneshot::channel();
        self.cmd
            .send(Cmd::Mute {
                muted,
                deafened,
                reply,
            })
            .map_err(|_| CallError::Ended)?;
        rx.await.map_err(|_| CallError::Ended)
    }

    /// Leaves (Sew's leave with the credential, then the engine's).
    pub async fn leave(self) -> Result<(), CallError> {
        let (reply, rx) = oneshot::channel();
        self.cmd
            .send(Cmd::Leave(reply))
            .map_err(|_| CallError::Ended)?;
        rx.await.map_err(|_| CallError::Ended)
    }

    /// Queues one encoded frame to send. Never blocks: a full queue drops the frame (and
    /// counts it); the engine drops it too unless media is up and a send key is active.
    pub fn push_opus(&self, f: OpusFrame) {
        self.frame_sender().push_opus(f);
    }

    /// A cloneable handle to the send queue (for an audio thread or task).
    pub fn frame_sender(&self) -> FrameSender {
        FrameSender {
            push: self.push.clone(),
            shared: self.shared.clone(),
        }
    }

    /// The receive side (decrypted Opus per remote track, loss marks, mute and deafen), for the
    /// audio pipeline. The first call takes it; later calls get `None`.
    pub fn frames(&self) -> Option<FrameStream> {
        self.frames.lock().ok().and_then(|mut f| f.take())
    }
}
