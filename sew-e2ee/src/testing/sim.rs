//! `Sim(n)`: n `CallSession`s, their `VirtualSdk`s and `MiniDs` on a fake clock, a seeded RNG
//! and per-link delays.
//!
//! Every client → DS request and DS → client dispatch travels with a per-link delay (FIFO per
//! link). Device signatures, fences and sender self-checks are answered by the client's own
//! software key and virtual SDK. A frame generator lets tests assert at the key level that everyone
//! decrypts everyone: every active member sends a frame every `frame_every_ms`, every other member
//! tries to decrypt it.
//!
//! The clock starts at the real wall-clock time (openmls checks key-package lifetimes against
//! wall time), optionally shifted (`SimConfig::t0_offset_ms`).

use std::collections::{BTreeMap, BTreeSet};

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use sew_proto::e2ee::{CallKeysInit, CallKeysMessage, RemoveReason};

use super::fixtures::{Install, TestDevice};
use super::mini_ds::{ClientId, DsConfig, MiniDs};
use crate::bridge::virtual_sdk::{Flavour, VirtualFrame, VirtualSdk};
use crate::bridge::{KeyEvent, KeySink, SinkAck};
use crate::config::{E2eeConfig, Params};
use crate::ids::{FenceId, JoinId, LkIdentity, Millis, RequestId, SignId, Slot};
use crate::keypackage::PreparedKp;
use crate::session::{
    CallSession, E2eeStatus, Input, Output, SessionState, Transition, UnknownCallBuffer,
};
use crate::wire::{ApiFailure, KeyReply, KeyRequest};

/// The fixture member names, in order.
pub const NAMES: [&str; 12] = [
    "alice", "bob", "carol", "dave", "eve", "frank", "grace", "heidi", "ivan", "judy", "mallory",
    "oscar",
];

/// Wall-clock unix ms (the harness may read a clock; the core never does): sew_proto's
/// `SystemClock` natively, `Date.now()` on wasm32.
pub fn wall_now_ms() -> Millis {
    #[cfg(not(target_arch = "wasm32"))]
    {
        use sew_proto::clock::Clock as _;
        sew_proto::clock::SystemClock.now_unix_ms()
    }
    #[cfg(target_arch = "wasm32")]
    {
        let ms = js_sys::Date::now();
        if ms.is_finite() && ms > 0.0 {
            ms as u64
        } else {
            0
        }
    }
}

/// Sim configuration.
#[derive(Debug, Clone)]
pub struct SimConfig {
    /// Clients (all created, none joined).
    pub n: usize,
    /// Virtual SDK flavour.
    pub flavour: Flavour,
    /// RNG seed.
    pub seed: u64,
    /// Per-link delay range (ms).
    pub link_delay: (u64, u64),
    /// Shift of the start clock against wall time (negative = the past).
    pub t0_offset_ms: i64,
    /// MiniDs timings.
    pub ds: DsConfig,
    /// Local client config.
    pub client: E2eeConfig,
}

impl SimConfig {
    /// `n` clients, `Js` flavour, zero link delay.
    pub fn new(n: usize) -> SimConfig {
        SimConfig {
            n,
            flavour: Flavour::Js,
            seed: 7,
            link_delay: (0, 0),
            t0_offset_ms: 0,
            ds: DsConfig::default(),
            client: E2eeConfig::default(),
        }
    }
}

/// One step in the global trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trace {
    /// A key event a client's session emitted.
    Key {
        /// Kind (`install`, `activate`, …).
        kind: &'static str,
        /// Identity it names.
        identity: Option<LkIdentity>,
        /// Slot.
        slot: Option<Slot>,
    },
    /// The epoch the session was at when it emitted the previous key event (MLS epoch for
    /// install/fence, active epoch for activate) and its slot base.
    KeyEpoch {
        /// Epoch.
        epoch: Option<u64>,
        /// Slot base.
        slot_base: u8,
    },
    /// A request a client sent.
    Send(&'static str, Option<u64>),
    /// A dispatch a client received.
    Recv(&'static str),
    /// A status.
    Status(E2eeStatus),
}

#[derive(Debug)]
enum Event {
    Deliver(ClientId, Box<CallKeysMessage>),
    ToDs(ClientId, RequestId, Box<KeyRequest>),
    Reply(ClientId, RequestId, Box<Result<KeyReply, ApiFailure>>),
    Fence(ClientId, FenceId),
    Sign(ClientId, SignId, Vec<u8>),
    Slot(ClientId),
    JoinAnswer(ClientId, Box<Result<CallKeysInit, ApiFailure>>),
    Frame(ClientId, Box<VirtualFrame>),
}

/// One simulated client.
#[derive(Debug)]
pub struct SimClient {
    /// The fixture name.
    pub name: String,
    /// The device.
    pub device: TestDevice,
    /// The session, once joined.
    pub session: Option<CallSession>,
    /// The virtual SDK, once joined.
    pub sdk: Option<VirtualSdk>,
    /// READY's `session_id`.
    pub gateway_session: String,
    /// The join id, once joined.
    pub join_id: Option<JoinId>,
    /// What the session emitted, with the time.
    pub trace: Vec<(Millis, Trace)>,
    /// K-route requests sent (time, request).
    pub sent: Vec<(Millis, KeyRequest)>,
    /// Never sends its `commit`s (a silent committer).
    pub silent_committer: bool,
    /// Every dispatch to it is dropped.
    pub blackout: bool,
    /// Dispatch kinds dropped for it.
    pub drop_kinds: BTreeSet<&'static str>,
    /// Corrupt the next `commit` dispatch to it.
    pub corrupt_next_commit: bool,
    /// Do not answer its device-signature requests (the signer is busy).
    pub hold_signatures: bool,
    /// Errors `handle` returned.
    pub errors: Vec<String>,
    /// Every dispatch delivered to the session, in order.
    pub delivered: Vec<CallKeysMessage>,
    /// The join's final error, if it failed.
    pub join_error: Option<crate::error::JoinError>,
    join_retried: bool,
    /// The last key package this client joined with (MLS bytes).
    pub last_kp: Option<Vec<u8>>,
    /// Every key event with the session's epoch tag and slot base.
    pub key_log: Vec<(KeyEvent, u64, u8)>,
    pending_kp: Option<PreparedKp>,
    buffer: UnknownCallBuffer,
    last_delivery: Millis,
    last_to_ds: Millis,
}

/// Decrypt outcomes per (sender, receiver).
#[derive(Debug, Default, Clone)]
pub struct FrameStats {
    /// Frames decrypted.
    pub ok: BTreeMap<(ClientId, ClientId), u64>,
    /// Frames not decrypted.
    pub failed: BTreeMap<(ClientId, ClientId), u64>,
}

impl FrameStats {
    /// Failures at receivers not in `exclude` from senders not in `exclude`.
    pub fn failures_excluding(&self, exclude: &BTreeSet<ClientId>) -> u64 {
        self.failed
            .iter()
            .filter(|((s, r), _)| !exclude.contains(s) && !exclude.contains(r))
            .map(|(_, n)| *n)
            .sum()
    }

    /// Successes in all.
    pub fn total_ok(&self) -> u64 {
        self.ok.values().sum()
    }
}

/// The harness.
#[derive(Debug)]
pub struct Sim {
    /// The clock (unix ms).
    pub now: Millis,
    /// The delivery service.
    pub ds: MiniDs,
    /// The clients.
    pub clients: Vec<SimClient>,
    /// The install.
    pub install: Install,
    /// The global trace (time, client, step).
    pub trace: Vec<(Millis, ClientId, Trace)>,
    /// Frame decrypt outcomes.
    pub frames: FrameStats,
    /// Frames every this many ms from every active member (0 = off).
    pub frame_every_ms: u64,
    next_frame: Millis,
    cfg: SimConfig,
    events: BTreeMap<(Millis, u64), Event>,
    seq: u64,
    rng: StdRng,
    k2_log: Vec<(ClientId, sew_proto::e2ee::KeyJoinRequest)>,
    first_active: BTreeMap<ClientId, Millis>,
    /// Forget the DS's refs this many times between an upload and its join.
    pub forget_refs_before_join: u32,
}

impl Sim {
    /// A Sim with `cfg.n` clients enrolled and none joined.
    pub fn new(cfg: SimConfig) -> Sim {
        let now = (wall_now_ms() as i64 + cfg.t0_offset_ms).max(0) as u64;
        let mut install = Install::acme();
        let mut clients = Vec::new();
        for i in 0..cfg.n {
            let name = NAMES.get(i).map_or(format!("m{i}"), |n| (*n).to_owned());
            let member = 48_831_765_147_600_000 + i as u64;
            let device = 48_831_765_147_700_000 + i as u64;
            let mut dev = install.enroll(member, device, cfg.seed * 1000 + i as u64, now);
            dev.client.set_config(cfg.client.clone());
            clients.push(SimClient {
                name,
                device: dev,
                session: None,
                sdk: None,
                gateway_session: format!("gw-{i}-1"),
                join_id: None,
                trace: Vec::new(),
                sent: Vec::new(),
                silent_committer: false,
                blackout: false,
                drop_kinds: BTreeSet::new(),
                corrupt_next_commit: false,
                hold_signatures: false,
                errors: Vec::new(),
                delivered: Vec::new(),
                join_error: None,
                join_retried: false,
                last_kp: None,
                key_log: Vec::new(),
                pending_kp: None,
                buffer: UnknownCallBuffer::new(&Params::default(), &cfg.client),
                last_delivery: 0,
                last_to_ds: 0,
            });
        }
        let mut ds = MiniDs::new(install.clone(), Install::lounge(), cfg.seed);
        ds.cfg = cfg.ds.clone();
        Sim {
            now,
            ds,
            clients,
            install,
            trace: Vec::new(),
            frames: FrameStats::default(),
            frame_every_ms: 0,
            next_frame: 0,
            rng: StdRng::seed_from_u64(cfg.seed),
            cfg,
            events: BTreeMap::new(),
            seq: 0,
            k2_log: Vec::new(),
            first_active: BTreeMap::new(),
            forget_refs_before_join: 0,
        }
    }

    /// `Sim(n)`: `n` members all joined and active at a common epoch `e0`.
    pub fn joined(n: usize, flavour: Flavour, seed: u64) -> Sim {
        let mut cfg = SimConfig::new(n);
        cfg.flavour = flavour;
        cfg.seed = seed;
        let mut sim = Sim::new(cfg);
        for i in 0..n {
            sim.join(i);
            sim.settle();
        }
        sim
    }

    /// The configuration.
    pub fn config(&self) -> &SimConfig {
        &self.cfg
    }

    fn delay(&mut self) -> u64 {
        let (lo, hi) = self.cfg.link_delay;
        if hi <= lo {
            lo
        } else {
            self.rng.gen_range(lo..=hi)
        }
    }

    fn push(&mut self, at: Millis, ev: Event) {
        self.seq += 1;
        self.events.insert((at, self.seq), ev);
    }

    // ------------------------------------------------------------------ joining

    /// Prepares and uploads a package for client `i`, then joins (the voice join).
    pub fn join(&mut self, i: ClientId) {
        self.join_with_answer_delay(i, 0);
    }

    /// As [`Sim::join`], with the voice join's answer delayed.
    pub fn join_with_answer_delay(&mut self, i: ClientId, answer_delay: u64) {
        let now = self.now;
        let scope = self.ds.scope();
        let kp = self.clients[i].device.prepare(scope, now);
        let member = self.clients[i].device.member_id();
        if let Err(e) = self.ds.upload(now, member, &kp.upload_body()) {
            self.clients[i].errors.push(format!("upload: {e:?}"));
            return;
        }
        if self.forget_refs_before_join > 0 {
            self.forget_refs_before_join -= 1;
            self.ds.forget_refs();
        }
        self.clients[i].last_kp = Some(kp.key_package_bytes().to_vec());
        let caps = VirtualSdk::new(self.cfg.flavour, placeholder_identity()).caps();
        let gs = self.clients[i].gateway_session.clone();
        let field = match kp.join_field(&caps, &gs) {
            Ok(f) => f,
            Err(e) => {
                self.clients[i].errors.push(format!("join_field: {e}"));
                return;
            }
        };
        let answer = self.ds.join(now, i, member, &field);
        self.clients[i].pending_kp = Some(kp);
        self.route_ds();
        self.push(now + answer_delay, Event::JoinAnswer(i, Box::new(answer)));
        self.step_due();
    }

    /// Joins with a pre-made answer path for a move: the key join with the join id Sew minted (test
    /// 112).
    pub fn move_join(&mut self, i: ClientId, join_id: JoinId) {
        let now = self.now;
        let scope = self.ds.scope();
        let kp = self.clients[i].device.prepare(scope, now);
        let member = self.clients[i].device.member_id();
        if let Err(e) = self.ds.upload(now, member, &kp.upload_body()) {
            self.clients[i].errors.push(format!("upload: {e:?}"));
            return;
        }
        let caps = VirtualSdk::new(self.cfg.flavour, placeholder_identity()).caps();
        let gs = self.clients[i].gateway_session.clone();
        let req = match kp.key_join_request(join_id, &caps, &gs) {
            Ok(r) => r,
            Err(e) => {
                self.clients[i]
                    .errors
                    .push(format!("key_join_request: {e}"));
                return;
            }
        };
        self.k2_log.push((i, req.clone()));
        let answer = self.ds.key_join(now, i, member, &req);
        self.clients[i].pending_kp = Some(kp);
        self.route_ds();
        self.push(now, Event::JoinAnswer(i, Box::new(answer)));
        self.step_due();
    }

    fn on_join_answer(&mut self, i: ClientId, answer: Result<CallKeysInit, ApiFailure>) {
        let now = self.now;
        let Some(kp) = self.clients[i].pending_kp.take() else {
            return;
        };
        let init = match answer {
            Ok(init) => init,
            Err(e) => {
                let retried = self.clients[i].join_retried;
                match crate::keypackage::on_join_failure(&e, retried) {
                    Ok(()) => {
                        // Prepare and upload a fresh package, retry once.
                        self.clients[i].join_retried = true;
                        self.join(i);
                    }
                    Err(je) => {
                        self.clients[i].errors.push(format!("join: {e:?}"));
                        self.clients[i].join_error = Some(je);
                    }
                }
                return;
            }
        };
        let gs = self.clients[i].gateway_session.clone();
        let identity = init.lk_identity.clone();
        let join_id = init.join_id.clone();
        let call_id = init.call_id.clone();
        let session = match kp.into_session(&self.clients[i].device.client, init, &gs, now) {
            Ok(s) => s,
            Err(e) => {
                self.clients[i].errors.push(format!("into_session: {e}"));
                return;
            }
        };
        let mut sdk = VirtualSdk::new(self.cfg.flavour, identity);
        sdk.set_now(now);
        self.clients[i].session = Some(session);
        self.clients[i].sdk = Some(sdk);
        self.clients[i].join_id = Some(join_id);
        self.pump(i);
        let buffered = self.clients[i].buffer.take(now, &call_id);
        for m in buffered {
            self.deliver_now(i, m);
        }
    }

    // ------------------------------------------------------------------ the loop

    fn pump(&mut self, i: ClientId) {
        let now = self.now;
        loop {
            let Some(o) = self.clients[i]
                .session
                .as_mut()
                .and_then(CallSession::poll_output)
            else {
                break;
            };
            match o {
                Output::Send(id, req) => {
                    let epoch = match &req {
                        KeyRequest::Ready(_, b) => Some(b.epoch),
                        KeyRequest::Commit(_, b) => Some(b.epoch),
                        KeyRequest::Release(_, b) => Some(b.epoch),
                        _ => None,
                    };
                    self.record(i, Trace::Send(req.kind(), epoch));
                    self.clients[i].sent.push((now, req.clone()));
                    if self.clients[i].silent_committer && matches!(req, KeyRequest::Commit(..)) {
                        continue;
                    }
                    let d = self.delay();
                    let at = (now + d).max(self.clients[i].last_to_ds);
                    self.clients[i].last_to_ds = at;
                    self.push(at, Event::ToDs(i, id, Box::new(req)));
                }
                Output::Key(ev) => {
                    self.record(
                        i,
                        Trace::Key {
                            kind: ev.kind(),
                            identity: ev.identity().cloned(),
                            slot: ev.slot(),
                        },
                    );
                    let (epoch, slot_base) = self.clients[i]
                        .session
                        .as_ref()
                        .map(|s| {
                            let e = if matches!(ev, KeyEvent::Activate { .. }) {
                                s.active_epoch()
                            } else {
                                s.mls_epoch()
                            };
                            (e, s.slot_base())
                        })
                        .unwrap_or((None, 0));
                    self.record(i, Trace::KeyEpoch { epoch, slot_base });
                    self.clients[i]
                        .key_log
                        .push((ev.duplicate(), epoch.unwrap_or(0), slot_base));
                    if matches!(ev, KeyEvent::Activate { .. })
                        && !self.first_active.contains_key(&i)
                    {
                        self.first_active.insert(i, now);
                    }
                    let fence = match &ev {
                        KeyEvent::Fence { id } => Some(*id),
                        _ => None,
                    };
                    let ack = self.clients[i].sdk.as_mut().map(|s| {
                        s.set_now(now);
                        s.apply(ev)
                    });
                    if let (Some(f), Some(Ok(SinkAck::Now))) = (fence, ack) {
                        self.push(now, Event::Fence(i, f));
                    }
                }
                Output::NeedDeviceSignature { request, message } => {
                    if !self.clients[i].hold_signatures {
                        self.push(now, Event::Sign(i, request, message));
                    }
                }
                Output::CheckSenderSlot { .. } => {
                    self.push(now, Event::Slot(i));
                }
                Output::Status(s) => self.record(i, Trace::Status(s)),
                Output::Roster => {}
            }
        }
    }

    fn record(&mut self, i: ClientId, t: Trace) {
        self.clients[i].trace.push((self.now, t.clone()));
        self.trace.push((self.now, i, t));
    }

    fn route_ds(&mut self) {
        let now = self.now;
        for (to, msg) in self.ds.take_deliveries() {
            let Some(c) = self.clients.get(to) else {
                continue;
            };
            if c.blackout || c.drop_kinds.contains(msg.kind()) {
                continue;
            }
            let d = self.delay();
            let at = (now + d).max(self.clients[to].last_delivery);
            self.clients[to].last_delivery = at;
            self.push(at, Event::Deliver(to, Box::new(msg)));
        }
    }

    fn handle(&mut self, i: ClientId, input: Input) {
        let now = self.now;
        let r = match self.clients[i].session.as_mut() {
            Some(s) if matches!(s.state(), SessionState::Left { .. }) => return,
            Some(s) => s.handle(now, input),
            None => return,
        };
        if let Err(e) = r {
            self.clients[i].errors.push(e.to_string());
        }
        self.pump(i);
    }

    fn deliver_now(&mut self, i: ClientId, mut msg: CallKeysMessage) {
        if self.clients[i].corrupt_next_commit
            && let CallKeysMessage::Commit { commit, .. } = &mut msg
        {
            self.clients[i].corrupt_next_commit = false;
            let mut b = commit.as_bytes().to_vec();
            let n = b.len();
            if n > 40 {
                b[n - 20] ^= 0x55;
            }
            *commit = sew_proto::e2ee::B64::new(b);
        }
        self.record(i, Trace::Recv(msg.kind()));
        self.clients[i].delivered.push(msg.clone());
        if self.clients[i].session.is_none() {
            let now = self.now;
            if let Some(call) = crate::session::call_id_of(&msg).cloned() {
                self.clients[i].buffer.push(now, call, msg, 1024);
            }
            return;
        }
        self.handle(i, Input::Server(msg));
    }

    fn process(&mut self, ev: Event) {
        let now = self.now;
        match ev {
            Event::Deliver(i, msg) => self.deliver_now(i, *msg),
            Event::ToDs(i, id, req) => {
                let Some(join) = self.clients[i].join_id.clone() else {
                    return;
                };
                let r = self.ds.handle(now, &join, &req);
                self.route_ds();
                let d = self.delay();
                self.push(now + d, Event::Reply(i, id, Box::new(r)));
            }
            Event::Reply(i, id, r) => self.handle(i, Input::Reply(id, *r)),
            Event::Fence(i, f) => self.handle(i, Input::Installed { fence: f }),
            Event::Sign(i, request, message) => {
                let sig = self.clients[i].device.signer.sign(&message);
                self.handle(i, Input::DeviceSignature { request, sig });
            }
            Event::Slot(i) => {
                let observed = self.clients[i].sdk.as_ref().and_then(VirtualSdk::send_slot);
                if let Some(observed) = observed {
                    self.handle(i, Input::SenderSlotObserved { observed });
                }
            }
            Event::JoinAnswer(i, a) => self.on_join_answer(i, *a),
            Event::Frame(to, f) => {
                let live = self.clients[to]
                    .session
                    .as_ref()
                    .is_some_and(|s| !matches!(s.state(), SessionState::Left { .. }));
                let joined_long_enough =
                    self.first_active.get(&to).is_some_and(|t| f.at >= t + 200);
                if !live || !joined_long_enough {
                    return;
                }
                let ok = self.clients[to]
                    .sdk
                    .as_ref()
                    .is_some_and(|s| s.can_decrypt(&f));
                let from = self
                    .clients
                    .iter()
                    .position(|c| c.sdk.as_ref().is_some_and(|s| s.local() == &f.identity))
                    .unwrap_or(usize::MAX);
                let e = if ok {
                    self.frames.ok.entry((from, to)).or_insert(0)
                } else {
                    self.frames.failed.entry((from, to)).or_insert(0)
                };
                *e += 1;
            }
        }
    }

    fn next_due(&self) -> Option<Millis> {
        let mut t: Option<Millis> = self.events.keys().next().map(|(t, _)| *t);
        let mut take = |v: Option<Millis>| {
            if let Some(v) = v {
                t = Some(t.map_or(v, |c: Millis| c.min(v)));
            }
        };
        take(self.ds.poll_timeout());
        for c in &self.clients {
            take(c.session.as_ref().and_then(CallSession::poll_timeout));
            take(c.sdk.as_ref().and_then(VirtualSdk::next_fence_at));
        }
        if self.frame_every_ms > 0 {
            take(Some(self.next_frame.max(self.now)));
        }
        t
    }

    /// Processes everything due at the current time.
    fn step_due(&mut self) {
        let now = self.now;
        self.route_ds();
        if self.ds.poll_timeout().is_some_and(|t| t <= now) {
            self.ds.handle_timeout(now);
            self.route_ds();
        }
        for i in 0..self.clients.len() {
            if self.clients[i]
                .session
                .as_ref()
                .and_then(CallSession::poll_timeout)
                .is_some_and(|t| t <= now)
            {
                if let Some(s) = self.clients[i].session.as_mut() {
                    s.handle_timeout(now);
                }
                self.pump(i);
            }
            let fences = self.clients[i]
                .sdk
                .as_mut()
                .map(|s| s.due_fences(now))
                .unwrap_or_default();
            for f in fences {
                self.handle(i, Input::Installed { fence: f });
            }
        }
        if self.frame_every_ms > 0 && self.next_frame <= now {
            self.next_frame = now + self.frame_every_ms;
            self.emit_frames();
        }
        while let Some((&(t, s), _)) = self.events.iter().next() {
            if t > now {
                break;
            }
            let Some(ev) = self.events.remove(&(t, s)) else {
                break;
            };
            self.process(ev);
        }
    }

    fn emit_frames(&mut self) {
        let now = self.now;
        let mut sends = Vec::new();
        for (i, c) in self.clients.iter().enumerate() {
            let Some(sdk) = &c.sdk else {
                continue;
            };
            if c.session.as_ref().is_none_or(|s| {
                matches!(s.state(), SessionState::Left { .. }) || s.active_epoch().is_none()
            }) {
                continue;
            }
            if let Some(f) = sdk.send_frame(now) {
                sends.push((i, f));
            }
        }
        for (from, f) in sends {
            for to in 0..self.clients.len() {
                if to == from || !self.is_receiver(to) {
                    continue;
                }
                let d = self.delay();
                let dup = VirtualFrame {
                    identity: f.identity.clone(),
                    slot: f.slot,
                    key: f.key.duplicate(),
                    at: f.at,
                };
                self.push(now + d, Event::Frame(to, Box::new(dup)));
            }
        }
    }

    fn is_receiver(&self, i: ClientId) -> bool {
        self.clients[i].session.as_ref().is_some_and(|s| {
            !matches!(s.state(), SessionState::Left { .. }) && s.active_epoch().is_some()
        })
    }

    /// Runs until `t` (inclusive).
    pub fn run_until(&mut self, t: Millis) {
        let mut guard = 0u64;
        loop {
            guard += 1;
            if guard > 5_000_000 {
                panic!("sim: no progress before t={t} (now={})", self.now);
            }
            self.step_due();
            match self.next_due() {
                Some(n) if n <= t => {
                    self.now = n.max(self.now);
                }
                _ => {
                    self.now = t.max(self.now);
                    self.step_due();
                    return;
                }
            }
        }
    }

    /// Runs for `ms`.
    pub fn run_for(&mut self, ms: u64) {
        let t = self.now + ms;
        self.run_until(t);
    }

    /// Runs until the DS is idle, no message is in flight and every joined session is idle
    /// (or `limit_ms` passed).
    pub fn settle_within(&mut self, limit_ms: u64) -> bool {
        let end = self.now + limit_ms;
        while self.now < end {
            let quiet = self.events.values().all(|e| matches!(e, Event::Frame(..)))
                && !self.ds.busy()
                && self.clients.iter().all(|c| {
                    c.session.as_ref().is_none_or(|s| {
                        matches!(
                            s.state(),
                            SessionState::Active {
                                transition: Transition::Idle,
                                ..
                            } | SessionState::Left { .. }
                        )
                    })
                });
            if quiet {
                return true;
            }
            let next = self.next_due().map_or(end, |n| n.min(end));
            self.run_until(next.max(self.now + 1));
        }
        false
    }

    /// [`Sim::settle_within`] 30 s, panicking if it does not settle.
    pub fn settle(&mut self) {
        if !self.settle_within(30_000) {
            panic!(
                "sim: did not settle (now {})\n{}",
                self.now,
                self.debug_states()
            );
        }
    }

    /// One line per client (state, epochs) and the DS log tail.
    pub fn debug_states(&self) -> String {
        let mut s = String::new();
        for (i, c) in self.clients.iter().enumerate() {
            if let Some(sess) = &c.session {
                s.push_str(&format!(
                    "{i} {}: {:?} active={:?} mls={:?} errors={:?}\n",
                    c.name,
                    sess.state(),
                    sess.active_epoch(),
                    sess.mls_epoch(),
                    c.errors
                ));
            }
        }
        for l in self.ds.log.iter().rev().take(10) {
            s.push_str(&format!("ds: {l}\n"));
        }
        s
    }

    // ------------------------------------------------------------------ helpers

    /// Client `i`'s session.
    pub fn session(&self, i: ClientId) -> &CallSession {
        match self.clients[i].session.as_ref() {
            Some(s) => s,
            None => panic!("sim: {} has no session", self.clients[i].name),
        }
    }

    /// Client `i`'s session, mutably.
    pub fn session_mut(&mut self, i: ClientId) -> &mut CallSession {
        let name = self.clients[i].name.clone();
        match self.clients[i].session.as_mut() {
            Some(s) => s,
            None => panic!("sim: {name} has no session"),
        }
    }

    /// Feeds an input to client `i` now and pumps its outputs.
    pub fn input(&mut self, i: ClientId, input: Input) {
        self.handle(i, input);
        self.route_ds();
        self.step_due();
    }

    /// Delivers a crafted dispatch to client `i` now.
    pub fn deliver(&mut self, i: ClientId, msg: CallKeysMessage) {
        self.deliver_now(i, msg);
        self.route_ds();
        self.step_due();
    }

    /// Client `i`'s virtual SDK.
    pub fn sdk(&self, i: ClientId) -> &VirtualSdk {
        match self.clients[i].sdk.as_ref() {
            Some(s) => s,
            None => panic!("sim: {} has no sdk", self.clients[i].name),
        }
    }

    /// Client `i`'s virtual SDK, mutably.
    pub fn sdk_mut(&mut self, i: ClientId) -> &mut VirtualSdk {
        let name = self.clients[i].name.clone();
        match self.clients[i].sdk.as_mut() {
            Some(s) => s,
            None => panic!("sim: {name} has no sdk"),
        }
    }

    /// Client `i`'s join id.
    pub fn join_id(&self, i: ClientId) -> JoinId {
        match &self.clients[i].join_id {
            Some(j) => j.clone(),
            None => panic!("sim: {} has not joined", self.clients[i].name),
        }
    }

    /// Client `i`'s engine identity.
    pub fn identity(&self, i: ClientId) -> LkIdentity {
        self.join_id(i).lk_identity()
    }

    /// Client `i` leaves (session teardown + the DS removes the leaf).
    pub fn leave(&mut self, i: ClientId) {
        let now = self.now;
        if let Some(s) = self.clients[i].session.as_mut() {
            s.leave(now);
        }
        self.pump(i);
        let join = self.join_id(i);
        self.ds.remove(now, &join, RemoveReason::Left);
        self.route_ds();
    }

    /// The DS removes client `i` (a kick).
    pub fn kick(&mut self, i: ClientId, reason: RemoveReason) {
        let now = self.now;
        let join = self.join_id(i);
        self.ds.remove(now, &join, reason);
        self.route_ds();
    }

    /// A periodic rekey.
    pub fn rekey(&mut self) {
        let now = self.now;
        self.ds.rekey(now);
        self.route_ds();
    }

    /// Requests client `i` sent of `kind` (`state`, `commit`, …).
    pub fn sent_kinds(&self, i: ClientId, kind: &str) -> Vec<&KeyRequest> {
        self.clients[i]
            .sent
            .iter()
            .filter(|(_, r)| r.kind() == kind)
            .map(|(_, r)| r)
            .collect()
    }

    /// Enrolls another device of client `i`'s member as a new client (not joined).
    pub fn add_device_of(&mut self, i: ClientId) -> ClientId {
        let k = self.clients.len();
        let member = self.clients[i].device.member_id().get();
        let device = 48_831_765_147_800_000 + k as u64;
        let now = self.now;
        let mut dev =
            self.install
                .enroll(member, device, self.cfg.seed * 1000 + 500 + k as u64, now);
        dev.client.set_config(self.cfg.client.clone());
        self.clients.push(SimClient {
            name: format!("{}#{k}", self.clients[i].name),
            device: dev,
            session: None,
            sdk: None,
            gateway_session: format!("gw-{k}-1"),
            join_id: None,
            trace: Vec::new(),
            sent: Vec::new(),
            silent_committer: false,
            blackout: false,
            drop_kinds: BTreeSet::new(),
            corrupt_next_commit: false,
            hold_signatures: false,
            errors: Vec::new(),
            delivered: Vec::new(),
            join_error: None,
            join_retried: false,
            last_kp: None,
            key_log: Vec::new(),
            pending_kp: None,
            buffer: UnknownCallBuffer::new(&Params::default(), &self.cfg.client),
            last_delivery: 0,
            last_to_ds: 0,
        });
        k
    }

    /// Clients with a live session.
    pub fn live(&self) -> Vec<ClientId> {
        (0..self.clients.len())
            .filter(|i| {
                self.clients[*i]
                    .session
                    .as_ref()
                    .is_some_and(|s| !matches!(s.state(), SessionState::Left { .. }))
            })
            .collect()
    }
}

/// The key join requests a Sim sent.
impl Sim {
    /// Every key join body sent, with the client.
    pub fn k2_requests(&self) -> &[(ClientId, sew_proto::e2ee::KeyJoinRequest)] {
        &self.k2_log
    }
}

fn placeholder_identity() -> LkIdentity {
    JoinId::from_bytes([0; 16]).lk_identity()
}
