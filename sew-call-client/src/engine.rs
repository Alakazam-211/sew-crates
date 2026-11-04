//! The tokio driver of one call: signalling (through `sew-lk-compat`'s Sans-IO session), the
//! transport, the peer connection, the frame path through the keys, reconnect
//! and the call credential's renewal.
//!
//! Reconnect, as Sew's media server expects it (rewritten from Sew's call recorder's
//! participant task):
//! - the WebSocket drops while the PC is up → **resume**: a new WebSocket with
//!   `JoinRequest.reconnect` and the participant sid, the PC kept, ICE restarted on
//!   `ReconnectResponse`;
//! - the PC fails (ICE down 2 s), the TURN allocation is lost, a resume is refused, or the
//!   engine says `Leave{RECONNECT}` → **full reconnect**: a new WebSocket and a new
//!   JoinResponse (so fresh TURN credentials), a new transport and PC, the microphone
//!   re-published;
//! - both with the engine's refresh token, backing off 250 ms → 4 s, inside the reconnect window
//!   (20 s) from the first loss;
//! - after the window, or when the engine refuses the token: a **re-join** through the
//!   [`JoinProvider`] (a new join answer, credential and key session).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use sew_lk_compat::session::LeaveAction;
use sew_lk_compat::{ClientInfoOpts, JoinParams, Session, SessionEvent};
use sew_lkproto::livekit::{SignalRequest, UpdateSubscription, signal_request};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;

use crate::api;
use crate::call::{
    BusyGuard, CallCounters, CallErrorEvent, CallEvent, CallMoved, CallSnapshot, CallState, Cmd,
    EndReason, OpusFrame, Participant, RecvItem, RecvOpus, Speaking,
};
use crate::config::ClientConfig;
use crate::e2ee::{
    CredentialHandle, E2eeContext, E2eeError, E2eeEvent, E2eeEvents, E2eeProvider, KeySession,
    RemoteTrack, Subscribe,
};
use crate::pc::{IceState, Pc, PcEvent};
use crate::provider::{
    CallCredential, JoinAnswer, JoinProvider, JoinRequest, ProviderError, RejoinReason,
};
use crate::signal::{self, ConnectError, SignalIn, SignalLink};
use crate::transport::{Gather, Transport, TransportError, TransportEvent};

/// First retry delay.
const BACKOFF_MIN: Duration = Duration::from_millis(250);
/// Longest retry delay.
const BACKOFF_MAX: Duration = Duration::from_secs(4);
/// ICE `disconnected` this long → full reconnect.
const PC_FAIL_GRACE: Duration = Duration::from_secs(2);
/// A sequence gap is reported lost after this long.
const LOSS_GRACE: Duration = Duration::from_millis(60);
/// Gaps longer than this are not marked packet by packet (a restart, not loss).
const LOSS_MAX_GAP: u16 = 50;
/// Speaking events at most this often.
const SPEAKING_EVERY: Duration = Duration::from_millis(100);
/// A credential renewal that failed on the network is retried after this.
const RENEW_RETRY: Duration = Duration::from_secs(10);
/// How long a leave waits for Sew's answer.
const LEAVE_TIMEOUT: Duration = Duration::from_secs(3);
/// The track name of the microphone.
const MIC_NAME: &str = "microphone";
/// How long an engine `Disconnect` waits for Sew's own reason (the call socket's self copy or
/// `4011`) before the call ends with the engine's (`kicked` or `room_closed`): media and the
/// call socket may arrive in either order (Sew 0.7.2).
pub(crate) const END_REASON_GRACE: Duration = Duration::from_millis(1_500);

/// What the [`crate::Call`] handle and the engine task share.
pub(crate) struct Shared {
    pub(crate) events: broadcast::Sender<CallEvent>,
    pub(crate) snapshot: Arc<Mutex<CallSnapshot>>,
    pub(crate) send_queue_drops: AtomicU64,
}

/// Everything the engine task starts with.
pub(crate) struct Start {
    pub(crate) cfg: ClientConfig,
    pub(crate) e2ee: Arc<dyn E2eeProvider>,
    pub(crate) provider: Arc<dyn JoinProvider>,
    pub(crate) req: JoinRequest,
    pub(crate) answer: JoinAnswer,
    pub(crate) shared: Arc<Shared>,
    pub(crate) cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    pub(crate) push_rx: mpsc::Receiver<OpusFrame>,
    pub(crate) frames_tx: mpsc::Sender<RecvItem>,
    pub(crate) busy: BusyGuard,
}

/// Starts the engine task.
pub(crate) fn spawn(s: Start) -> Result<(), String> {
    let tls = crate::config::tls_config(&s.cfg).map_err(|e| format!("TLS roots: {e}"))?;
    let http = api::http_client(&tls)?;
    let now = Instant::now();
    let (sig_tx, sig_rx) = mpsc::unbounded_channel();
    let (e2ee_tx, e2ee_rx) = mpsc::unbounded_channel();
    let (cred_tx, _) = watch::channel(s.answer.call_credential().cloned());
    let relay_only = s.answer.join.media.as_ref().is_some_and(|m| m.relay_only);
    let self_member = s.answer.join.state.member_id.to_string();
    let speak_denied = s.answer.join.state.speak_denied;
    let mut e = Engine {
        cfg: s.cfg,
        tls,
        http,
        e2ee: s.e2ee,
        provider: s.provider,
        req: s.req.clone(),
        answer: s.answer,
        shared: s.shared,
        cmd_rx: s.cmd_rx,
        push_rx: s.push_rx,
        frames_tx: s.frames_tx,
        _busy: s.busy,
        cred_tx,
        renew_at: None,
        renewing: None,
        keys: None,
        keys_active: false,
        keys_start: None,
        e2ee_tx,
        e2ee_rx,
        session: Session::new(now),
        link: None,
        link_id: 0,
        sig_tx,
        sig_rx,
        attempt: None,
        rejoining: None,
        opening: None,
        transport: None,
        pc: None,
        pc_down_since: None,
        relay_only,
        identity: None,
        sid: None,
        cid: format!("mic-{:08x}", rand::random::<u32>()),
        track_sid: None,
        published: None,
        send: None,
        send_seq: u64::from(rand::random::<u16>()),
        media_up: false,
        reconnect: None,
        connect_deadline: now + Duration::from_secs(0),
        moving: false,
        tracks: HashMap::new(),
        next_track_id: 1,
        self_muted: s.req.muted,
        deafened: s.req.deafened,
        muted_before_deafen: s.req.muted,
        server_muted: false,
        self_member,
        joined_speak_denied: speak_denied,
        mic_denied: false,
        speak_was_denied: speak_denied,
        publishing: false,
        cid_on_pc: false,
        pending_end: None,
        members: HashMap::new(),
        voice: HashMap::new(),
        engine_levels: HashMap::new(),
        local_level: 0.0,
        next_speaking: now,
        next_snapshot: now,
        last_speaking: Vec::new(),
        state: CallState::Connecting,
        counters: CallCounters::default(),
        ended: None,
    };
    e.connect_deadline = now + e.cfg.connect_timeout;
    tokio::spawn(async move { e.run().await });
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Resume,
    Full,
}

#[derive(Debug)]
struct Reconnect {
    /// The first connect's retries (state stays `connecting`; nothing is counted).
    initial: bool,
    kind: Kind,
    deadline: Instant,
    next_try: Instant,
    backoff: Duration,
    attempt: u32,
}

struct SendPath {
    mid: String,
    pt: u8,
    ssrc: u32,
}

struct RemoteSlot {
    id: u64,
    identity: String,
    allowed: Subscribe,
    announced: bool,
    unsubscribed: bool,
    highest: Option<u64>,
    gaps: VecDeque<(u64, Instant)>,
}

type Attempt = JoinHandle<Result<SignalLink, ConnectError>>;

struct Engine {
    cfg: ClientConfig,
    tls: Arc<rustls::ClientConfig>,
    http: reqwest::Client,
    e2ee: Arc<dyn E2eeProvider>,
    provider: Arc<dyn JoinProvider>,
    req: JoinRequest,
    answer: JoinAnswer,
    shared: Arc<Shared>,
    cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    push_rx: mpsc::Receiver<OpusFrame>,
    frames_tx: mpsc::Sender<RecvItem>,
    _busy: BusyGuard,
    cred_tx: watch::Sender<Option<CallCredential>>,
    renew_at: Option<Instant>,
    renewing: Option<JoinHandle<Result<api::Renewed, api::ApiError>>>,
    keys: Option<Arc<dyn KeySession>>,
    keys_active: bool,
    keys_start: Option<JoinHandle<Result<Arc<dyn KeySession>, E2eeError>>>,
    e2ee_tx: mpsc::UnboundedSender<E2eeEvent>,
    e2ee_rx: mpsc::UnboundedReceiver<E2eeEvent>,
    session: Session,
    link: Option<SignalLink>,
    link_id: u64,
    sig_tx: mpsc::UnboundedSender<(u64, SignalIn)>,
    sig_rx: mpsc::UnboundedReceiver<(u64, SignalIn)>,
    attempt: Option<(Attempt, Kind)>,
    rejoining: Option<JoinHandle<Result<JoinAnswer, ProviderError>>>,
    opening: Option<JoinHandle<Result<Transport, TransportError>>>,
    transport: Option<Transport>,
    pc: Option<Pc>,
    pc_down_since: Option<Instant>,
    relay_only: bool,
    identity: Option<String>,
    sid: Option<String>,
    cid: String,
    track_sid: Option<String>,
    /// The server's answer to our publication (the keys confirm it encrypted, the publish check).
    published: Option<sew_lkproto::livekit::TrackInfo>,
    send: Option<SendPath>,
    send_seq: u64,
    media_up: bool,
    reconnect: Option<Reconnect>,
    connect_deadline: Instant,
    moving: bool,
    /// Remote tracks by track sid.
    tracks: HashMap<String, RemoteSlot>,
    next_track_id: u64,
    self_muted: bool,
    deafened: bool,
    muted_before_deafen: bool,
    server_muted: bool,
    /// This device's member (the join answer's state).
    self_member: String,
    /// The join answer's `speak_denied` (until the call socket sends this member's state).
    joined_speak_denied: bool,
    /// The engine's permission for this participant refuses the microphone (SPEAK lost): the
    /// publication is gone and nothing is published until it allows it again.
    mic_denied: bool,
    /// [`Engine::speak_denied`] as last told to capture and the participants.
    speak_was_denied: bool,
    /// The microphone's AddTrack is out (its TrackPublished not seen yet).
    publishing: bool,
    /// The current PC already carries a send section for `cid` (a re-publication on the same
    /// PC takes a new cid, so a new m-line).
    cid_on_pc: bool,
    /// The engine's `Disconnect` came first: the call ends at this instant with this reason
    /// unless Sew's own reason arrives before.
    pending_end: Option<(Instant, EndReason)>,
    /// Engine identity → member id (from the keys).
    members: HashMap<String, String>,
    /// Member id → voice state (from the call socket).
    voice: HashMap<String, sew_proto::voice::VoiceState>,
    /// Participant sid → the engine's speaker level.
    engine_levels: HashMap<String, f32>,
    local_level: f32,
    next_speaking: Instant,
    next_snapshot: Instant,
    last_speaking: Vec<Speaking>,
    state: CallState,
    counters: CallCounters,
    ended: Option<EndReason>,
}

async fn opt_join<T>(h: &mut Option<JoinHandle<T>>) -> Result<T, tokio::task::JoinError> {
    match h {
        Some(j) => {
            let r = j.await;
            *h = None;
            r
        }
        None => std::future::pending().await,
    }
}

async fn opt_attempt(
    h: &mut Option<(Attempt, Kind)>,
) -> (
    Result<Result<SignalLink, ConnectError>, tokio::task::JoinError>,
    Kind,
) {
    match h {
        Some((j, k)) => {
            let k = *k;
            let r = j.await;
            *h = None;
            (r, k)
        }
        None => std::future::pending().await,
    }
}

async fn opt_recv(t: &mut Option<Transport>) -> Option<TransportEvent> {
    match t {
        Some(t) => t.recv().await,
        None => std::future::pending().await,
    }
}

#[allow(clippy::large_enum_variant)]
enum Step {
    Cmd(Option<Cmd>),
    Push(Option<OpusFrame>),
    Signal(Option<(u64, SignalIn)>),
    Transport(Option<TransportEvent>),
    E2ee(Option<E2eeEvent>),
    KeysStarted(Result<Result<Arc<dyn KeySession>, E2eeError>, tokio::task::JoinError>),
    Attempt(
        Result<Result<SignalLink, ConnectError>, tokio::task::JoinError>,
        Kind,
    ),
    Rejoined(Result<Result<JoinAnswer, ProviderError>, tokio::task::JoinError>),
    Opened(Result<Result<Transport, TransportError>, tokio::task::JoinError>),
    Renewed(Result<Result<api::Renewed, api::ApiError>, tokio::task::JoinError>),
    Timeout,
}

fn rfc6464(level_dbov: f32) -> u8 {
    if level_dbov.is_nan() {
        return 127;
    }
    (-level_dbov).clamp(0.0, 127.0) as u8
}

/// One member's flags from Sew's voice state.
struct Flags {
    self_mute: bool,
    self_deaf: bool,
    server_mute: bool,
    server_deaf: bool,
    speak_denied: bool,
}

/// Sew's reason for ending this join (a voice leave reason: the call socket's self copy, its
/// `4011` close, or the key group's removal) as the call's end: passed on unchanged,
/// but `replaced` (another device took the call over) is `credential_lost` and `left` (a leave
/// from elsewhere) is `left`.
pub(crate) fn end_for_sew_reason(reason: &str) -> EndReason {
    match reason {
        "replaced" => EndReason::CredentialLost,
        "left" => EndReason::Left,
        r => EndReason::Removed(r.to_owned()),
    }
}

fn dbov_to_unit(level_dbov: f32) -> f32 {
    // -60 dBov and below reads as silence; 0 dBov as 1.
    ((level_dbov + 60.0) / 60.0).clamp(0.0, 1.0)
}

impl Engine {
    async fn run(&mut self) {
        self.publish_state(CallState::Connecting);
        if self.speak_was_denied {
            // Joined without SPEAK: capture stays off until it is granted.
            self.frames_item(RecvItem::Capture(false));
        }
        self.start_keys(None);
        self.start_attempt(Kind::Full);
        self.schedule_renew(Instant::now());
        while self.ended.is_none() {
            self.pump();
            let deadline = self.poll_timeout();
            let sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline));
            let step = tokio::select! {
                biased;
                c = self.cmd_rx.recv() => Step::Cmd(c),
                s = self.sig_rx.recv() => Step::Signal(s),
                t = opt_recv(&mut self.transport) => Step::Transport(t),
                e = self.e2ee_rx.recv() => Step::E2ee(e),
                r = opt_join(&mut self.keys_start) => Step::KeysStarted(r),
                (r, k) = opt_attempt(&mut self.attempt) => Step::Attempt(r, k),
                r = opt_join(&mut self.rejoining) => Step::Rejoined(r),
                r = opt_join(&mut self.opening) => Step::Opened(r),
                r = opt_join(&mut self.renewing) => Step::Renewed(r),
                p = self.push_rx.recv() => Step::Push(p),
                () = sleep => Step::Timeout,
            };
            let now = Instant::now();
            match step {
                Step::Cmd(None) => {
                    // The Call handle is gone without a leave: leave.
                    self.leave().await;
                }
                Step::Cmd(Some(Cmd::Mute {
                    muted,
                    deafened,
                    reply,
                })) => {
                    self.set_mute(muted, deafened);
                    let _ = reply.send(());
                }
                Step::Cmd(Some(Cmd::Leave(reply))) => {
                    self.leave().await;
                    let _ = reply.send(());
                }
                Step::Push(Some(f)) => self.send_frame(now, f),
                Step::Push(None) => {}
                Step::Signal(Some((id, msg))) => {
                    if id == self.link_id {
                        match msg {
                            SignalIn::Response(r) => {
                                self.session.handle_response(now, r);
                                self.drain_session(now);
                            }
                            SignalIn::Closed(why) => {
                                tracing::info!(target: "sew_call_client", %why, "signalling lost");
                                self.signal_lost(false);
                            }
                        }
                    }
                }
                Step::Signal(None) => {}
                Step::Transport(Some(TransportEvent::Datagram(d))) => {
                    if let Some(pc) = self.pc.as_mut() {
                        pc.handle_datagram(now, &d);
                    }
                }
                Step::Transport(Some(TransportEvent::TurnLost(why))) => {
                    tracing::info!(target: "sew_call_client", %why, "TURN allocation lost; full reconnect");
                    self.signal_lost(true);
                }
                Step::Transport(None) => {
                    self.signal_lost(true);
                }
                Step::E2ee(Some(ev)) => self.on_e2ee(ev),
                Step::E2ee(None) => {}
                Step::KeysStarted(r) => match r {
                    Ok(Ok(k)) => {
                        if let Some(t) = &self.published
                            && !k.published(t)
                        {
                            self.error(
                                "e2ee_refused",
                                "the engine did not confirm the publication encrypted",
                            );
                        }
                        self.keys = Some(k);
                        self.send_roster();
                    }
                    Ok(Err(e)) => {
                        self.error(&e.code, &e.detail);
                        self.end(EndReason::E2eeRefused).await;
                    }
                    Err(e) => {
                        self.error("e2ee_refused", &e.to_string());
                        self.end(EndReason::E2eeRefused).await;
                    }
                },
                Step::Attempt(r, kind) => self.on_attempt(now, r, kind),
                Step::Rejoined(r) => self.on_rejoined(now, r).await,
                Step::Opened(r) => self.on_opened(now, r).await,
                Step::Renewed(r) => self.on_renewed(now, r).await,
                Step::Timeout => self.on_timeout(now).await,
            }
            if now >= self.next_snapshot {
                // Counters reach `Call::snapshot` at most 100 ms late.
                self.next_snapshot = now + SPEAKING_EVERY;
                self.snapshot_update();
            }
        }
    }

    // ------------------------------------------------------------------ the loop's helpers

    fn poll_timeout(&self) -> Instant {
        let now = Instant::now();
        let mut t = now + Duration::from_secs(1);
        if let Some(p) = &self.pc {
            t = t.min(p.timeout());
        }
        if let Some(s) = self.session.poll_timeout() {
            t = t.min(s);
        }
        if let Some(r) = &self.reconnect {
            t = t.min(r.deadline);
            if self.attempt.is_none() && self.link.is_none() && self.rejoining.is_none() {
                t = t.min(r.next_try);
            }
        }
        if let Some(r) = self.renew_at {
            t = t.min(r);
        }
        if let Some((at, _)) = &self.pending_end {
            t = t.min(*at);
        }
        if let Some(d) = self.pc_down_since {
            t = t.min(d + PC_FAIL_GRACE);
        }
        for s in self.tracks.values() {
            if let Some((_, at)) = s.gaps.front() {
                t = t.min(*at);
            }
        }
        if self.media_up {
            t = t.min(self.next_speaking);
        }
        if self.ended.is_none() {
            t = t.min(self.next_snapshot);
        }
        t.max(now)
    }

    /// Moves the PC's datagrams out and handles its events; sends the session's requests.
    fn pump(&mut self) {
        let now = Instant::now();
        for _ in 0..4 {
            let Some(pc) = self.pc.as_mut() else { break };
            let (datagrams, events) = pc.poll(now);
            if let Some(t) = self.transport.as_mut() {
                for d in datagrams {
                    t.send(d);
                }
            }
            if events.is_empty() {
                break;
            }
            for ev in events {
                self.on_pc(now, ev);
            }
        }
        while let Some(r) = self.session.poll_request() {
            if let Some(l) = &self.link {
                l.send(r);
            }
        }
    }

    fn snapshot_update(&self) {
        let participants = self.participants();
        if let Ok(mut s) = self.shared.snapshot.lock() {
            s.state = self.state.clone();
            s.participants = participants;
            s.relay = self
                .transport
                .as_ref()
                .and_then(Transport::relay_via)
                .map(|v| v.name().to_owned());
            s.relay_only = self.relay_only;
            s.counters = self.counters.clone();
        }
    }

    fn publish_state(&mut self, st: CallState) {
        if self.state == st && st != CallState::Connecting {
            return;
        }
        self.state = st.clone();
        self.snapshot_update();
        let _ = self.shared.events.send(CallEvent::State(st));
    }

    fn update_state(&mut self) {
        if self.ended.is_some() {
            return;
        }
        let st = match &self.reconnect {
            Some(r) if !self.moving && !r.initial => CallState::Reconnecting { attempt: r.attempt },
            _ if self.media_up && self.keys_active => CallState::Connected,
            _ => match &self.state {
                // A move (or a brief gap) keeps `connected`.
                CallState::Connected if self.moving => CallState::Connected,
                CallState::Reconnecting { attempt } if !self.media_up => {
                    CallState::Reconnecting { attempt: *attempt }
                }
                _ => CallState::Connecting,
            },
        };
        if st != self.state {
            self.publish_state(st);
        } else {
            self.snapshot_update();
        }
    }

    fn error(&self, code: &str, detail: &str) {
        let _ = self.shared.events.send(CallEvent::Error(CallErrorEvent {
            code: code.to_owned(),
            detail: detail.to_owned(),
        }));
    }

    fn emit_participants(&self) {
        let _ = self
            .shared
            .events
            .send(CallEvent::Participants(self.participants()));
        self.snapshot_update();
    }

    fn participants(&self) -> Vec<Participant> {
        let mut out = Vec::new();
        let flags = |identity: &str| -> (Option<String>, Flags) {
            let member = self.members.get(identity).cloned();
            let v = member.as_ref().and_then(|m| self.voice.get(m));
            let f = Flags {
                self_mute: v.is_some_and(|v| v.self_mute),
                self_deaf: v.is_some_and(|v| v.self_deaf),
                server_mute: v.is_some_and(|v| v.server_mute),
                server_deaf: v.is_some_and(|v| v.server_deaf),
                speak_denied: v.is_some_and(|v| v.speak_denied),
            };
            (member, f)
        };
        if let Some(me) = &self.identity {
            let (member, f) = flags(me);
            out.push(Participant {
                identity: me.clone(),
                member_id: member,
                is_self: true,
                muted: self.send_blocked() || f.server_mute,
                deafened: self.deafened,
                server_muted: self.server_muted || f.server_mute,
                server_deafened: f.server_deaf,
                speak_denied: self.speak_denied(),
            });
        }
        for p in self.session.remotes().values() {
            let (member, f) = flags(&p.identity);
            let track_muted =
                !p.tracks.is_empty() && p.tracks.iter().filter(|t| t.r#type == 0).all(|t| t.muted);
            out.push(Participant {
                identity: p.identity.clone(),
                member_id: member,
                is_self: false,
                muted: f.self_mute
                    || f.server_mute
                    || f.speak_denied
                    || track_muted
                    || p.tracks.is_empty(),
                deafened: f.self_deaf,
                server_muted: f.server_mute,
                server_deafened: f.server_deaf,
                speak_denied: f.speak_denied,
            });
        }
        out
    }

    /// Muted by self, deafened, or muted by a moderator (what Sew's self state and the
    /// track's mute carry).
    fn effective_muted(&self) -> bool {
        self.self_muted || self.deafened || self.server_muted
    }

    /// This member lacks SPEAK: Sew's voice state says so (the call socket's, else the
    /// join answer's), or the engine's permission refuses the microphone.
    fn speak_denied(&self) -> bool {
        let sew = self
            .voice
            .get(&self.self_member)
            .map_or(self.joined_speak_denied, |v| v.speak_denied);
        sew || self.mic_denied
    }

    /// Nothing is captured or sent: muted, or SPEAK denied.
    fn send_blocked(&self) -> bool {
        self.effective_muted() || self.speak_denied()
    }

    /// After a change of [`Engine::speak_denied`]: capture follows and the participants show
    /// it. Self-mute is untouched, so it comes back as it was.
    fn speak_changed(&mut self) {
        let denied = self.speak_denied();
        if denied == self.speak_was_denied {
            return;
        }
        self.speak_was_denied = denied;
        tracing::info!(target: "sew_call_client", denied, "SPEAK changed");
        self.frames_item(RecvItem::Capture(!self.send_blocked()));
        self.emit_participants();
    }

    /// Whether the engine's permission for this participant allows the microphone (no
    /// permission seen yet: the token's grants, which Sew made).
    fn engine_allows_mic(&self) -> bool {
        let Some(p) = self.session.local().and_then(|l| l.permission.as_ref()) else {
            return true;
        };
        let mic = sew_lkproto::livekit::TrackSource::Microphone as i32;
        p.can_publish && (p.can_publish_sources.is_empty() || p.can_publish_sources.contains(&mic))
    }

    /// Publishes the microphone when the PC is up, nothing is published or on its way, and the
    /// engine allows it (the first publication, a full reconnect's, or SPEAK given back).
    fn maybe_publish(&mut self) {
        if self.pc.is_none() || self.track_sid.is_some() || self.publishing {
            return;
        }
        if !self.engine_allows_mic() {
            self.mic_denied = true;
            return;
        }
        if self.cid_on_pc {
            // The old send section stays (the engine marked its mid revoked): a new one.
            self.cid = format!("mic-{:08x}", rand::random::<u32>());
            self.cid_on_pc = false;
        }
        self.publishing = true;
        self.session
            .publish_audio(&self.cid, MIC_NAME, self.effective_muted());
        self.negotiate();
    }

    /// The local participant changed (the engine's ParticipantUpdate): a SPEAK change shows as
    /// its permission. Losing the microphone drops the publication (the engine unpublished it);
    /// getting it back publishes again on the same PC, with no re-join.
    fn local_permission_changed(&mut self) {
        let allowed = self.engine_allows_mic();
        if !allowed && !self.mic_denied {
            tracing::info!(target: "sew_call_client", "the engine revoked the microphone");
            self.mic_denied = true;
            self.send = None;
            self.track_sid = None;
            self.published = None;
            self.publishing = false;
        } else if allowed && self.mic_denied {
            tracing::info!(target: "sew_call_client", "the engine allows the microphone again");
            self.mic_denied = false;
        }
        if allowed {
            self.maybe_publish();
        }
        self.speak_changed();
    }

    fn frames_item(&mut self, item: RecvItem) {
        if self.frames_tx.try_send(item).is_err() {
            self.counters.recv_queue_drops += 1;
        }
    }

    // ------------------------------------------------------------------ keys

    fn start_keys(&mut self, rejoin: Option<RejoinReason>) {
        let ctx = E2eeContext {
            join: self.answer.join.clone(),
            api_base: self.answer.api_base.clone(),
            credential: CredentialHandle(self.cred_tx.subscribe()),
            http: self.http.clone(),
            tls: self.tls.clone(),
            app_id: self.cfg.app_id.clone(),
            data_dir: self.cfg.data_dir.clone(),
            rejoin,
        };
        let e2ee = self.e2ee.clone();
        let events = E2eeEvents(self.e2ee_tx.clone());
        self.keys_active = false;
        self.keys_start = Some(tokio::spawn(async move { e2ee.start(ctx, events).await }));
    }

    fn send_roster(&self) {
        if let Some(k) = &self.keys {
            let ids: Vec<String> = self
                .session
                .remotes()
                .values()
                .map(|p| p.identity.clone())
                .collect();
            k.roster(&ids);
        }
    }

    fn on_e2ee(&mut self, ev: E2eeEvent) {
        match ev {
            E2eeEvent::Active { epoch } => {
                tracing::debug!(target: "sew_call_client", epoch, "key epoch active");
                self.keys_active = true;
                self.update_state();
            }
            E2eeEvent::Refused { code, detail } if self.pending_end.is_some() => {
                // The engine removed us already; Sew's reason (or the fallback) ends the call.
                tracing::debug!(target: "sew_call_client", %code, %detail, "keys ended while the end waits for Sew's reason");
            }
            E2eeEvent::Refused { code, detail } => {
                self.error(&code, &detail);
                self.ended_soon(EndReason::E2eeRefused);
            }
            E2eeEvent::Warning { code, detail } => self.error(&code, &detail),
            E2eeEvent::Members(m) => {
                for (identity, member) in m {
                    self.members.insert(identity, member);
                }
                self.emit_participants();
            }
            E2eeEvent::VoiceStates(states) => {
                for v in states {
                    self.voice.insert(v.member_id.to_string(), v);
                }
                self.emit_participants();
                self.speak_changed();
            }
            E2eeEvent::ServerUpdated(u) => {
                tracing::info!(target: "sew_call_client", "voice server updated (a move); reconnecting signalling");
                let moved = CallMoved::from_update(&u);
                // A re-join after the move asks the host for the destination, not the source.
                self.req.room = moved.room().to_owned();
                if let Ok(mut s) = self.shared.snapshot.lock() {
                    s.moved = Some(moved.clone());
                }
                let _ = self.shared.events.send(CallEvent::Moved(moved));
                self.answer.join.url = u.url;
                self.answer.join.token = u.token;
                self.answer.join.room = u.room;
                self.answer.join.expires_at = u.expires_at;
                if let Some(m) = u.media {
                    self.relay_only = m.relay_only;
                    self.answer.join.media = Some(m);
                }
                self.moving = true;
                self.session.reset(Instant::now());
                // The move's admit takes the new token, not the old refresh token.
                self.session = Session::new(Instant::now());
                self.signal_lost(true);
            }
            E2eeEvent::CredentialLost => {
                // An older Sew's 4011 names no reason: after the engine's Disconnect its reason
                // stands (as before Sew 0.7.2), else the credential was lost.
                let r = self
                    .pending_end
                    .take()
                    .map_or(EndReason::CredentialLost, |(_, r)| r);
                self.ended_soon(r);
            }
            E2eeEvent::Ended { reason } => {
                tracing::info!(target: "sew_call_client", %reason, "Sew ended the join");
                self.ended_soon(end_for_sew_reason(&reason));
            }
        }
    }

    /// The engine said `Disconnect` (removed, or the room closed): media goes at once, and the
    /// call ends with Sew's reason when the call socket brings it within
    /// [`END_REASON_GRACE`], else with `fallback`.
    fn engine_disconnected(&mut self, now: Instant, fallback: EndReason) {
        if self.ended.is_some() || self.pending_end.is_some() {
            return;
        }
        if self.keys.is_none() && self.keys_start.is_none() {
            // No call socket to wait for.
            self.ended_soon(fallback);
            return;
        }
        self.teardown_media();
        self.link = None;
        if let Some((a, _)) = self.attempt.take() {
            a.abort();
        }
        if let Some(r) = self.rejoining.take() {
            r.abort();
        }
        self.reconnect = None;
        self.pending_end = Some((now + END_REASON_GRACE, fallback));
        self.snapshot_update();
    }

    /// Ends at the next loop turn (from sync code).
    fn ended_soon(&mut self, reason: EndReason) {
        if self.ended.is_none() {
            self.teardown_media();
            self.link = None;
            self.ended = Some(reason.clone());
            self.publish_state(CallState::Ended { reason });
            if let Some(k) = self.keys.take() {
                tokio::spawn(async move { k.close().await });
            }
        }
    }

    async fn end(&mut self, reason: EndReason) {
        if self.ended.is_some() {
            return;
        }
        self.teardown_media();
        if let Some(l) = self.link.take() {
            l.close().await;
        }
        if let Some(k) = self.keys.take() {
            k.close().await;
        }
        self.ended = Some(reason.clone());
        self.publish_state(CallState::Ended { reason });
    }

    async fn leave(&mut self) {
        if self.ended.is_some() {
            return;
        }
        let cred = self.cred_tx.borrow().clone();
        if let Some(c) = cred {
            let http = self.http.clone();
            let base = self.answer.api_base.clone();
            let r = tokio::time::timeout(LEAVE_TIMEOUT, api::leave(&http, &base, &c.token)).await;
            if let Ok(Err(e)) = r {
                tracing::info!(target: "sew_call_client", ?e, "Sew's leave failed");
            }
        }
        self.session.leave();
        while let Some(r) = self.session.poll_request() {
            if let Some(l) = &self.link {
                l.send(r);
            }
        }
        self.end(EndReason::Left).await;
    }

    // ------------------------------------------------------------------ credential

    fn schedule_renew(&mut self, now: Instant) {
        let cred = self.cred_tx.borrow().clone();
        let Some(c) = cred else {
            self.renew_at = None;
            return;
        };
        // Wall time only through sew-proto's clock (the workspace's time lint).
        let now_ms = {
            use sew_proto::clock::Clock as _;
            sew_proto::clock::SystemClock.now_unix_ms()
        };
        let left = Duration::from_millis(
            u64::try_from(c.expires_at.unix_ms())
                .unwrap_or(0)
                .saturating_sub(now_ms),
        );
        self.renew_at = Some(now + left / 2);
    }

    async fn on_renewed(
        &mut self,
        now: Instant,
        r: Result<Result<api::Renewed, api::ApiError>, tokio::task::JoinError>,
    ) {
        match r {
            Ok(Ok(n)) => {
                self.counters.credential_renewals += 1;
                let next = self.cred_tx.borrow().clone().map(|mut c| {
                    c.token = n.token;
                    c.expires_at = n.expires_at;
                    c
                });
                self.cred_tx.send_replace(next);
                self.schedule_renew(now);
                self.snapshot_update();
            }
            Ok(Err(api::ApiError::Status {
                status: 401,
                reason,
            })) => {
                // The server's `401` reasons: `call_ended`, `signed_out` and `expired`
                // all end the call; the reason goes to the host as the `errors` code.
                let why = reason.unwrap_or_else(|| "call_ended".to_owned());
                self.error(&why, "the call credential was refused");
                match why.as_str() {
                    "signed_out" => self.end(EndReason::SigninRequired).await,
                    _ => self.end(EndReason::CredentialLost).await,
                }
            }
            Ok(Err(api::ApiError::Status { status: 409, .. })) => {
                self.end(EndReason::CredentialLost).await
            }
            Ok(Err(e)) => {
                tracing::info!(target: "sew_call_client", ?e, "credential renewal failed; retrying");
                self.renew_at = Some(now + RENEW_RETRY);
            }
            Err(_) => self.renew_at = Some(now + RENEW_RETRY),
        }
    }

    // ------------------------------------------------------------------ signalling

    fn join_params(&self, kind: Kind) -> JoinParams {
        let token = self
            .session
            .refresh_token()
            .map(str::to_owned)
            .unwrap_or_else(|| self.answer.join.token.clone());
        let mut p = JoinParams::new(
            &token,
            ClientInfoOpts {
                version: concat!("sew-call-client/", env!("CARGO_PKG_VERSION")).to_owned(),
                ..ClientInfoOpts::default()
            },
        );
        if kind == Kind::Resume {
            p.reconnect = true;
            p.participant_sid = self.sid.clone();
        }
        p
    }

    fn start_attempt(&mut self, kind: Kind) {
        if kind == Kind::Full {
            let now = Instant::now();
            self.session.reset(now);
        }
        let params = self.join_params(kind);
        let cfg = self.cfg.clone();
        let tls = self.tls.clone();
        let base = self.answer.join.url.clone();
        self.link_id += 1;
        let id = self.link_id;
        let tx = self.sig_tx.clone();
        self.attempt = Some((
            tokio::spawn(async move { signal::connect(&cfg, &tls, &base, &params, id, tx).await }),
            kind,
        ));
    }

    fn on_attempt(
        &mut self,
        now: Instant,
        r: Result<Result<SignalLink, ConnectError>, tokio::task::JoinError>,
        kind: Kind,
    ) {
        match r {
            Ok(Ok(link)) => {
                self.link = Some(link);
                tracing::debug!(target: "sew_call_client", ?kind, "signalling connected");
            }
            Ok(Err(e)) => {
                tracing::info!(target: "sew_call_client", ?kind, ?e, "signalling connect failed");
                let refused = matches!(e, ConnectError::Refused { status: 401 | 403 });
                if let ConnectError::BadUrl(why) = &e {
                    self.error("network", why);
                    self.ended_soon(EndReason::Network);
                    return;
                }
                match &mut self.reconnect {
                    Some(r) => {
                        if refused && r.kind == Kind::Full {
                            // The engine no longer knows the token: the admit is gone.
                            self.start_rejoin(RejoinReason::AdmitLost);
                            return;
                        }
                        if refused {
                            r.kind = Kind::Full;
                        }
                        r.next_try = now + r.backoff;
                        r.backoff = (r.backoff * 2).min(BACKOFF_MAX);
                    }
                    None => {
                        // The first connect: retry inside the connect window.
                        if now >= self.connect_deadline || refused {
                            self.error("network", &format!("{e:?}"));
                            self.ended_soon(EndReason::Network);
                            return;
                        }
                        self.reconnect = Some(Reconnect {
                            initial: true,
                            kind: Kind::Full,
                            deadline: self.connect_deadline,
                            next_try: now + BACKOFF_MIN,
                            backoff: BACKOFF_MIN * 2,
                            attempt: 1,
                        });
                    }
                }
            }
            Err(_) => self.signal_lost(true),
        }
    }

    /// The WebSocket is gone (or must go). Resume when the session and PC are up, else
    /// reconnect fully; within a reconnect already running only an upgrade to full changes
    /// anything.
    fn signal_lost(&mut self, force_full: bool) {
        if self.ended.is_some() || self.pending_end.is_some() {
            return;
        }
        let now = Instant::now();
        self.link = None;
        self.session.signal_lost();
        let pc_up = self
            .pc
            .as_ref()
            .is_some_and(|p| p.connected && p.ice != IceState::Disconnected);
        let kind = if force_full || self.sid.is_none() || !pc_up {
            Kind::Full
        } else {
            Kind::Resume
        };
        match self.reconnect.as_mut() {
            Some(r) => {
                if kind == Kind::Full {
                    r.kind = Kind::Full;
                }
                r.next_try = r.next_try.min(now);
            }
            None => {
                self.reconnect = Some(Reconnect {
                    initial: self.state == CallState::Connecting && !self.media_up,
                    kind,
                    deadline: now + self.cfg.reconnect_window,
                    next_try: now,
                    backoff: BACKOFF_MIN,
                    attempt: 1,
                });
            }
        }
        if self
            .reconnect
            .as_ref()
            .is_some_and(|r| r.kind == Kind::Full)
        {
            if let Some((a, _)) = self.attempt.take() {
                a.abort();
            }
            self.teardown_media();
        }
        self.update_state();
    }

    /// Drops the PC, the transport and the remote tracks (a full reconnect builds new ones).
    fn teardown_media(&mut self) {
        if let Some(mut pc) = self.pc.take() {
            pc.disconnect();
        }
        if let Some(o) = self.opening.take() {
            o.abort();
        }
        self.transport = None;
        self.media_up = false;
        self.send = None;
        self.track_sid = None;
        self.published = None;
        self.publishing = false;
        self.cid_on_pc = false;
        self.pc_down_since = None;
        let ids: Vec<u64> = self
            .tracks
            .drain()
            .filter(|(_, s)| s.announced)
            .map(|(_, s)| s.id)
            .collect();
        for id in ids {
            self.frames_item(RecvItem::Removed { participant: id });
        }
    }

    fn reconnected(&mut self, kind: Kind) {
        let Some(r) = self.reconnect.take() else {
            return;
        };
        if r.initial {
            self.update_state();
            return;
        }
        match kind {
            Kind::Resume => self.counters.resumes += 1,
            Kind::Full => {
                self.counters.full_reconnects += 1;
                if let Some(k) = &self.keys {
                    k.media_reconnected(true);
                }
            }
        }
        self.moving = false;
        tracing::info!(target: "sew_call_client", ?kind, "reconnected");
        self.update_state();
    }

    fn start_rejoin(&mut self, why: RejoinReason) {
        if self.rejoining.is_some() || self.ended.is_some() || self.pending_end.is_some() {
            return;
        }
        tracing::info!(target: "sew_call_client", ?why, "re-joining through the provider");
        self.teardown_media();
        self.link = None;
        if let Some((a, _)) = self.attempt.take() {
            a.abort();
        }
        let mut req = self.req.clone();
        req.rejoin = Some(why);
        req.muted = self.effective_muted();
        req.deafened = self.deafened;
        let p = self.provider.clone();
        self.rejoining = Some(tokio::spawn(async move { p.join(req).await }));
        let now = Instant::now();
        let r = self.reconnect.get_or_insert(Reconnect {
            initial: false,
            kind: Kind::Full,
            deadline: now + self.cfg.reconnect_window,
            next_try: now,
            backoff: BACKOFF_MIN,
            attempt: 1,
        });
        r.kind = Kind::Full;
        self.update_state();
    }

    async fn on_rejoined(
        &mut self,
        now: Instant,
        r: Result<Result<JoinAnswer, ProviderError>, tokio::task::JoinError>,
    ) {
        match r {
            Ok(Ok(answer)) => {
                self.counters.rejoins += 1;
                self.relay_only = answer.join.media.as_ref().is_some_and(|m| m.relay_only);
                self.cred_tx.send_replace(answer.call_credential().cloned());
                self.self_member = answer.join.state.member_id.to_string();
                self.joined_speak_denied = answer.join.state.speak_denied;
                self.answer = answer;
                self.schedule_renew(now);
                if let Some(k) = self.keys.take() {
                    k.close().await;
                }
                self.start_keys(Some(RejoinReason::AdmitLost));
                self.session = Session::new(now);
                self.sid = None;
                if let Some(r) = self.reconnect.as_mut() {
                    r.kind = Kind::Full;
                    r.deadline = now + self.cfg.reconnect_window;
                    r.next_try = now;
                    r.backoff = BACKOFF_MIN;
                    r.attempt += 1;
                }
                self.start_attempt(Kind::Full);
                self.update_state();
            }
            Ok(Err(ProviderError::SigninRequired)) => self.end(EndReason::SigninRequired).await,
            Ok(Err(ProviderError::Refused { code, detail })) => {
                self.error(&code, &detail.to_string());
                self.end(EndReason::Refused(code)).await;
            }
            Ok(Err(e)) => {
                self.error("network", &e.to_string());
                self.end(EndReason::Network).await;
            }
            Err(e) => {
                self.error("network", &e.to_string());
                self.end(EndReason::Network).await;
            }
        }
    }

    async fn on_timeout(&mut self, now: Instant) {
        if let Some((at, r)) = &self.pending_end
            && now >= *at
        {
            let r = r.clone();
            tracing::info!(target: "sew_call_client", ?r, "no reason from Sew after the engine's Disconnect");
            self.end(r).await;
            return;
        }
        if let Some(pc) = self.pc.as_mut()
            && pc.timeout() <= now
        {
            pc.handle_timeout(now);
        }
        self.session.handle_timeout(now);
        if self.renew_at.is_some_and(|t| t <= now) && self.renewing.is_none() {
            self.renew_at = None;
            let cred = self.cred_tx.borrow().clone();
            if let Some(c) = cred {
                let http = self.http.clone();
                let base = self.answer.api_base.clone();
                self.renewing = Some(tokio::spawn(async move {
                    api::renew(&http, &base, &c.token).await
                }));
            }
        }
        if let Some(d) = self.pc_down_since
            && now >= d + PC_FAIL_GRACE
        {
            self.pc_down_since = None;
            tracing::info!(target: "sew_call_client", "ICE down; full reconnect");
            self.signal_lost(true);
        }
        if let Some(r) = &self.reconnect
            && self.rejoining.is_none()
        {
            if now >= r.deadline {
                if r.initial {
                    // Never connected inside the connect window.
                    self.error("network", "the first connect timed out");
                    self.end(EndReason::Network).await;
                    return;
                }
                self.start_rejoin(RejoinReason::AdmitLost);
            } else if self.attempt.is_none() && self.link.is_none() && now >= r.next_try {
                let kind = r.kind;
                self.start_attempt(kind);
            }
        }
        self.expire_gaps(now);
        if self.media_up && now >= self.next_speaking {
            self.next_speaking = now + SPEAKING_EVERY;
            self.emit_speaking();
        }
    }

    // ------------------------------------------------------------------ session events

    fn drain_session(&mut self, now: Instant) {
        while let Some(ev) = self.session.poll_event() {
            self.on_session(now, ev);
        }
    }

    // One arm per event: the arms' conditions stay in their bodies rather than becoming guards
    // that fall through to the arm for events sew-lk-compat adds later.
    #[allow(clippy::collapsible_match)]
    fn on_session(&mut self, now: Instant, ev: SessionEvent) {
        match ev {
            SessionEvent::Joined {
                identity,
                sid,
                ice_servers,
                force_relay,
            } => {
                self.identity = Some(identity);
                self.sid = Some(sid);
                if force_relay {
                    self.relay_only = true;
                }
                let gather = if self.relay_only {
                    Gather::RelayOnly
                } else {
                    Gather::All
                };
                let cfg = self.cfg.clone();
                let tls = self.tls.clone();
                self.teardown_media();
                self.opening = Some(tokio::spawn(async move {
                    Transport::open(&cfg, &tls, &ice_servers, gather).await
                }));
                self.send_roster();
                self.emit_participants();
            }
            SessionEvent::Answer {
                sdp,
                mid_to_track_id,
                ..
            } => self.on_answer(&sdp, &mid_to_track_id),
            SessionEvent::Offer { .. } => {
                tracing::debug!(target: "sew_call_client", "an engine offer on the single PC (ignored)");
            }
            SessionEvent::RemoteCandidate { candidate } => {
                if let Some(pc) = self.pc.as_mut() {
                    pc.add_remote_candidate(&candidate);
                }
            }
            SessionEvent::MediaSections { audios, videos } => {
                if let Some(pc) = self.pc.as_mut() {
                    pc.add_recv_audio(audios);
                    if audios == 0 {
                        pc.renegotiate();
                    }
                    let _ = videos;
                }
                self.negotiate();
            }
            SessionEvent::TrackPublished { cid, track } => {
                if cid == self.cid && self.publishing {
                    self.publishing = false;
                    self.cid_on_pc = true;
                    let stream = self.sid.clone().unwrap_or_default();
                    if let Some(k) = &self.keys
                        && !k.published(&track)
                    {
                        self.error(
                            "e2ee_refused",
                            "the engine did not confirm the publication encrypted",
                        );
                    }
                    self.track_sid = Some(track.sid.clone());
                    self.published = Some(track);
                    if let Some(pc) = self.pc.as_mut() {
                        pc.add_send_audio(&stream, &cid);
                    }
                    if self.effective_muted() {
                        self.send_mute_signal();
                    }
                    self.negotiate();
                }
            }
            SessionEvent::Participants => {
                self.roster_changed();
                self.send_roster();
                self.emit_participants();
                self.local_permission_changed();
            }
            SessionEvent::Speakers(s) => {
                for sp in s {
                    if sp.active {
                        self.engine_levels.insert(sp.sid, sp.level);
                    } else {
                        self.engine_levels.remove(&sp.sid);
                    }
                }
            }
            SessionEvent::Muted { sid, muted } => {
                if self.track_sid.as_deref() == Some(sid.as_str()) {
                    self.server_muted = muted;
                    self.frames_item(RecvItem::Capture(!self.send_blocked()));
                    self.emit_participants();
                }
            }
            SessionEvent::Resumed { .. } => {
                if let Some(pc) = self.pc.as_mut() {
                    pc.forget_pending_offer();
                    pc.ice_restart();
                }
                self.negotiate();
                self.reconnected(Kind::Resume);
            }
            SessionEvent::Leave { reason, action } => {
                tracing::info!(target: "sew_call_client", reason, ?action, "engine leave");
                match action {
                    LeaveAction::Resume => self.signal_lost(false),
                    LeaveAction::Reconnect => self.signal_lost(true),
                    LeaveAction::Disconnect => {
                        // DisconnectReason: 4 PARTICIPANT_REMOVED, 2 DUPLICATE_IDENTITY, 5
                        // ROOM_DELETED, 10 ROOM_CLOSED; a server shutdown or a migration may
                        // come back, so it reconnects within the window.
                        let reason = match reason {
                            2 | 4 => Some(EndReason::Kicked),
                            5 | 10 => Some(EndReason::RoomClosed),
                            _ => None,
                        };
                        match reason {
                            Some(r) => self.engine_disconnected(now, r),
                            None => self.signal_lost(true),
                        }
                    }
                    // An action sew-lk-compat adds later: reconnect within the window.
                    _ => self.signal_lost(true),
                }
            }
            SessionEvent::Refused {
                request_id,
                reason,
                message,
            } => {
                tracing::info!(target: "sew_call_client", request_id, reason, %message, "request refused");
            }
            // An event sew-lk-compat adds later: nothing to do here yet.
            _ => {}
        }
        let _ = now;
    }

    async fn on_opened(
        &mut self,
        now: Instant,
        r: Result<Result<Transport, TransportError>, tokio::task::JoinError>,
    ) {
        match r {
            Ok(Ok(t)) => {
                if t.relay_via().is_some() {
                    self.counters.turn_allocations += 1;
                }
                let mut pc = Pc::new(t.local_candidates(), now);
                pc.handle_timeout(now);
                self.transport = Some(t);
                self.pc = Some(pc);
                self.publishing = false;
                self.cid_on_pc = false;
                // Without SPEAK nothing is published until the engine allows the microphone.
                self.maybe_publish();
                self.negotiate();
                self.speak_changed();
                self.snapshot_update();
            }
            Ok(Err(TransportError::RelayOnlyNoTurns)) => {
                self.error(
                    "relay_only_no_turns",
                    "relay-only, but the engine offered no turns: server",
                );
                self.end(EndReason::Network).await;
            }
            Ok(Err(e)) => {
                self.error("turn_unreachable", &e.to_string());
                self.signal_lost(true);
            }
            Err(_) => self.signal_lost(true),
        }
    }

    fn negotiate(&mut self) {
        let Some(pc) = self.pc.as_mut() else { return };
        if let Some((sdp, id)) = pc.negotiate() {
            self.session.offer(sdp, id);
        }
    }

    fn on_answer(&mut self, sdp: &str, mid_to_track_id: &BTreeMap<String, String>) {
        let Some(pc) = self.pc.as_mut() else { return };
        if let Err(e) = pc.handle_answer(sdp) {
            tracing::info!(target: "sew_call_client", error = %e, "answer refused; full reconnect");
            self.signal_lost(true);
            return;
        }
        for (mid, track) in mid_to_track_id {
            pc.set_msid(mid, "", track);
        }
        if self.send.is_none()
            && self.track_sid.is_some()
            && let Some(mid) = pc.mid_for_track(&self.cid)
            && let (Some(pt), Some(ssrc)) = (pc.opus_pt(&mid), pc.tx_ssrc(&mid))
        {
            self.send = Some(SendPath { mid, pt, ssrc });
        }
        if pc.needs_negotiation() {
            self.negotiate();
        }
    }

    // ------------------------------------------------------------------ PC events

    fn on_pc(&mut self, now: Instant, ev: PcEvent) {
        match ev {
            PcEvent::Connected => {
                self.media_up = true;
                self.pc_down_since = None;
                if self
                    .reconnect
                    .as_ref()
                    .is_some_and(|r| r.kind == Kind::Full)
                    && self.link.is_some()
                {
                    self.reconnected(Kind::Full);
                }
                self.update_state();
            }
            PcEvent::Ice(IceState::Disconnected) => {
                self.pc_down_since.get_or_insert(now);
            }
            PcEvent::Ice(IceState::Connected) => self.pc_down_since = None,
            PcEvent::Ice(IceState::Checking) => {}
            PcEvent::Rtp(r) => self.on_rtp(now, r),
        }
    }

    // ------------------------------------------------------------------ media

    fn send_frame(&mut self, now: Instant, f: OpusFrame) {
        self.local_level = if f.vad {
            dbov_to_unit(f.level_dbov)
        } else {
            0.0
        };
        if self.send_blocked() || !self.media_up || f.payload.is_empty() {
            self.counters.frames_not_sent += 1;
            return;
        }
        let (Some(send), Some(keys), Some(pc)) = (&self.send, &self.keys, self.pc.as_mut()) else {
            self.counters.frames_not_sent += 1;
            return;
        };
        let mut out = Vec::with_capacity(f.payload.len() + 32);
        if keys
            .encrypt(&f.payload, send.ssrc, f.ts48k, &mut out)
            .is_err()
            || out.is_empty()
        {
            // No send key (or the frame did not encrypt): dropped, never sent in the clear.
            self.counters.frames_not_sent += 1;
            return;
        }
        self.send_seq += 1;
        match pc.write_audio(
            &send.mid,
            send.pt,
            self.send_seq,
            f.ts48k,
            now,
            out,
            rfc6464(f.level_dbov),
            f.vad,
        ) {
            Ok(()) => self.counters.frames_sent += 1,
            Err(_) => self.counters.frames_not_sent += 1,
        }
    }

    /// The track sid and publisher identity of a receive section.
    fn sender_of(&self, mid: &str) -> Option<(String, String)> {
        let (stream, track) = self.pc.as_ref()?.msid(mid)?;
        let track_sid = stream
            .split_once('|')
            .map(|(_, t)| t.to_owned())
            .filter(|t| t.starts_with("TR_"))
            .unwrap_or_else(|| track.clone());
        let psid = stream.split_once('|').map(|(p, _)| p.to_owned());
        let remotes = self.session.remotes();
        let identity = psid
            .and_then(|s| remotes.get(&s).map(|p| p.identity.clone()))
            .or_else(|| {
                self.session
                    .publisher_of(&track_sid)
                    .map(|p| p.identity.clone())
            })?;
        Some((track_sid, identity))
    }

    fn remote_track(&self, track_sid: &str) -> Option<RemoteTrack> {
        let p = self.session.publisher_of(track_sid)?;
        let t = p.tracks.iter().find(|t| t.sid == track_sid)?;
        Some(RemoteTrack {
            sid: t.sid.clone(),
            encryption: t.encryption,
            mime: t.mime_type.clone(),
            audio: t.r#type == sew_lkproto::livekit::TrackType::Audio as i32,
        })
    }

    fn on_rtp(&mut self, now: Instant, r: crate::pc::RtpRx) {
        let Some((track_sid, identity)) = self.sender_of(&r.mid) else {
            self.counters.refused_frames += 1;
            return;
        };
        let Some(keys) = self.keys.clone() else {
            self.counters.refused_frames += 1;
            return;
        };
        if !self.tracks.contains_key(&track_sid) {
            let id = self.next_track_id;
            self.next_track_id += 1;
            self.tracks.insert(
                track_sid.clone(),
                RemoteSlot {
                    id,
                    identity: identity.clone(),
                    allowed: Subscribe::NotYet,
                    announced: false,
                    unsubscribed: false,
                    highest: None,
                    gaps: VecDeque::new(),
                },
            );
        }
        let verdict = match self.remote_track(&track_sid) {
            Some(t) => keys.may_subscribe(&identity, &t),
            None => Subscribe::NotYet,
        };
        let mut unsubscribe = false;
        let mut items = Vec::new();
        {
            let Some(slot) = self.tracks.get_mut(&track_sid) else {
                return;
            };
            slot.allowed = verdict.clone();
            match verdict {
                Subscribe::Yes => {}
                Subscribe::NotYet => {
                    self.counters.refused_frames += 1;
                    return;
                }
                Subscribe::No { reason } => {
                    self.counters.refused_frames += 1;
                    if !slot.unsubscribed {
                        slot.unsubscribed = true;
                        unsubscribe = true;
                        tracing::warn!(target: "sew_call_client", track = %track_sid, %reason, "refusing a remote track");
                    }
                }
            }
            if !unsubscribe && slot.allowed == Subscribe::Yes {
                // Loss marks: gaps that do not fill within LOSS_GRACE.
                match slot.highest {
                    None => slot.highest = Some(r.seq),
                    Some(h) if r.seq > h => {
                        let gap = r.seq - h - 1;
                        if gap > 0 && gap <= u64::from(LOSS_MAX_GAP) {
                            for s in (h + 1)..r.seq {
                                slot.gaps.push_back((s, now + LOSS_GRACE));
                            }
                        }
                        slot.highest = Some(r.seq);
                    }
                    Some(_) => slot.gaps.retain(|(s, _)| *s != r.seq),
                }
                let mut out = Vec::with_capacity(r.payload.len());
                match keys.decrypt(&identity, &r.payload, &mut out) {
                    Ok(()) if !out.is_empty() => {
                        if !slot.announced {
                            slot.announced = true;
                            items.push(RecvItem::Added {
                                participant: slot.id,
                                identity: slot.identity.clone(),
                            });
                        }
                        self.counters.frames_received += 1;
                        items.push(RecvItem::Frame(RecvOpus {
                            participant: slot.id,
                            identity: identity.clone(),
                            payload: Bytes::from(out),
                            seq: r.seq as u16,
                            ts48k: r.timestamp,
                            arrival: now,
                        }));
                    }
                    Ok(()) => {}
                    Err(_) => {
                        self.counters.decrypt_failures += 1;
                        if slot.announced {
                            self.counters.lost_marks += 1;
                            items.push(RecvItem::Lost {
                                participant: slot.id,
                                seq: r.seq as u16,
                            });
                        }
                    }
                }
            }
        }
        for it in items {
            self.frames_item(it);
        }
        if unsubscribe {
            let req = SignalRequest {
                message: Some(signal_request::Message::Subscription(UpdateSubscription {
                    track_sids: vec![track_sid],
                    subscribe: false,
                    ..UpdateSubscription::default()
                })),
            };
            if let Some(l) = &self.link {
                l.send(req);
            }
        }
    }

    fn expire_gaps(&mut self, now: Instant) {
        let mut items = Vec::new();
        for s in self.tracks.values_mut() {
            while let Some((seq, at)) = s.gaps.front().copied() {
                if at > now {
                    break;
                }
                s.gaps.pop_front();
                if s.announced {
                    items.push(RecvItem::Lost {
                        participant: s.id,
                        seq: seq as u16,
                    });
                }
            }
        }
        self.counters.lost_marks += items.len() as u64;
        for it in items {
            self.frames_item(it);
        }
    }

    /// Removes the tracks whose publisher or track left the roster.
    fn roster_changed(&mut self) {
        let live: std::collections::HashSet<String> = self
            .session
            .remotes()
            .values()
            .flat_map(|p| p.tracks.iter().map(|t| t.sid.clone()))
            .collect();
        let gone: Vec<String> = self
            .tracks
            .keys()
            .filter(|k| !live.contains(*k))
            .cloned()
            .collect();
        for k in gone {
            if let Some(s) = self.tracks.remove(&k)
                && s.announced
            {
                self.frames_item(RecvItem::Removed { participant: s.id });
            }
        }
    }

    fn emit_speaking(&mut self) {
        let mut v = Vec::new();
        if let Some(me) = &self.identity
            && !self.send_blocked()
            && self.local_level > 0.0
        {
            v.push(Speaking {
                identity: me.clone(),
                member_id: self.members.get(me).cloned(),
                level: self.local_level,
            });
        }
        let by_sid: BTreeMap<&String, &String> = self
            .session
            .remotes()
            .iter()
            .map(|(sid, p)| (sid, &p.identity))
            .collect();
        for (sid, level) in &self.engine_levels {
            if let Some(identity) = by_sid.get(sid) {
                v.push(Speaking {
                    identity: (*identity).clone(),
                    member_id: self.members.get(*identity).cloned(),
                    level: *level,
                });
            }
        }
        if v != self.last_speaking {
            self.last_speaking = v.clone();
            let _ = self.shared.events.send(CallEvent::Speaking(v));
        }
    }

    // ------------------------------------------------------------------ mute

    fn send_mute_signal(&mut self) {
        if let Some(sid) = self.track_sid.clone() {
            let m = self.effective_muted();
            self.session.mute(&sid, m);
        }
    }

    fn set_mute(&mut self, muted: Option<bool>, deafened: Option<bool>) {
        let before = (self.self_muted, self.deafened);
        if let Some(d) = deafened {
            if d && !self.deafened {
                self.muted_before_deafen = self.self_muted;
                self.self_muted = true;
            } else if !d && self.deafened {
                self.self_muted = self.muted_before_deafen;
            }
            self.deafened = d;
        }
        if let Some(m) = muted {
            self.self_muted = m || self.deafened;
            if !self.deafened {
                self.muted_before_deafen = self.self_muted;
            }
        }
        if before == (self.self_muted, self.deafened) {
            return;
        }
        // Without SPEAK there is no publication to unmute: capture stays off, and the self-mute
        // is kept for when SPEAK comes back.
        self.send_mute_signal();
        self.frames_item(RecvItem::Capture(!self.send_blocked()));
        self.frames_item(RecvItem::Deafened(self.deafened));
        let cred = self.cred_tx.borrow().clone();
        if let Some(c) = cred {
            let http = self.http.clone();
            let base = self.answer.api_base.clone();
            let (m, d) = (self.self_muted, self.deafened);
            tokio::spawn(async move {
                if let Err(e) = api::self_state(&http, &base, &c.token, m, d).await {
                    tracing::info!(target: "sew_call_client", ?e, "self state not saved");
                }
            });
        }
        self.emit_participants();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    #[test]
    fn levels() {
        assert_eq!(rfc6464(0.0), 0);
        assert_eq!(rfc6464(-30.5), 30);
        assert_eq!(rfc6464(-200.0), 127);
        assert_eq!(rfc6464(f32::NAN), 127);
        assert!((dbov_to_unit(0.0) - 1.0).abs() < 1e-6);
        assert!(dbov_to_unit(-80.0).abs() < 1e-6);
    }

    #[test]
    fn sew_reasons_end_the_call_unchanged() {
        for r in [
            "perms_lost",
            "removed_from_space",
            "timed_out",
            "channel_archived",
            "account_removed",
            "removed_by_moderator",
            "session_revoked",
            "device_revoked",
            "media_path_down",
            "some_reason_from_a_later_sew",
        ] {
            assert_eq!(
                end_for_sew_reason(r),
                EndReason::Removed(r.to_owned()),
                "{r}"
            );
        }
        // A swap is the device takeover; a leave from elsewhere is a leave.
        assert_eq!(end_for_sew_reason("replaced"), EndReason::CredentialLost);
        assert_eq!(end_for_sew_reason("left"), EndReason::Left);
    }

    #[test]
    fn a_move_names_the_destination_room() {
        let mut v = serde_json::json!({
            "channel_id": "48831765147549710",
            "room": "r_bbbbbbbbbbbbbbbbbbbbbbbbbb",
            "url": "wss://voice.sew.invalid",
            "token": "t",
            "expires_at": "2026-11-02T12:02:00.000Z",
            "e2ee": {
                "call_id": "r_bbbbbbbbbbbbbbbbbbbbbbbbbb",
                "join_id": "00112233445566778899aabbccddeeff",
                "key_join": "required"
            },
            "reason": "moved"
        });
        let u: sew_proto::voice::VoiceServerUpdated =
            serde_json::from_value(v.clone()).expect("voice_server_updated");
        let m = CallMoved::from_update(&u);
        assert_eq!(m.channel_id, "48831765147549710");
        assert_eq!(m.call_id, None);
        assert_eq!(m.media_room, "r_bbbbbbbbbbbbbbbbbbbbbbbbbb");
        assert_eq!(m.room(), "48831765147549710");
        // Into a DM call: the call's id is the room.
        v["call_id"] = serde_json::json!("48831765147549711");
        let u: sew_proto::voice::VoiceServerUpdated =
            serde_json::from_value(v).expect("voice_server_updated");
        assert_eq!(CallMoved::from_update(&u).room(), "48831765147549711");
    }

    #[test]
    fn the_end_waits_one_and_a_half_seconds_for_sew() {
        assert_eq!(END_REASON_GRACE, Duration::from_millis(1_500));
    }
}
