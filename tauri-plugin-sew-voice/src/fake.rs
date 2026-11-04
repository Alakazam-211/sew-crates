//! An in-memory engine, provider and event recorder, for tests and for running the example
//! without Sew. **Not a voice engine:** it moves no audio and talks to no server.
//!
//! It stays beside the real engine (`crate::real`), for hosts' tests and the example's offline
//! mode.
//!
//! [`FakeEngine`] behaves like the engine as far as the plugin can see: it calls the provider
//! (so a provider's refusal reaches the plugin as it would), keeps a roster with this device's
//! member in it, applies Sew's mute rules (deafened implies muted; un-deafening restores the
//! earlier mute; SPEAK denied shows muted and keeps the self-mute for when SPEAK comes back,
//! [`FakeCall::set_speak_denied`]), ends a call with any reason ([`FakeCall::end`], Sew's
//! reasons in [`crate::wire::reason::SEW`]), and lets a test push any [`CallEvent`] through
//! [`FakeCall::emit`].

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::json;
use tokio::sync::{Semaphore, broadcast};

use crate::engine::{
    BoxFuture, CallEvent, CallHandle, CallSnapshot, EngineError, JoinAnswer, JoinProvider,
    JoinRequest, ProviderError, VoiceEngine,
};
use crate::service::EventSink;
use crate::wire::{
    CallState, Device, DeviceChoice, DeviceList, ErrorInfo, Participant, PluginEvent,
};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The in-memory engine.
#[derive(Debug)]
pub struct FakeEngine {
    state: Mutex<EngineState>,
    devices_tx: broadcast::Sender<DeviceList>,
}

#[derive(Debug)]
struct EngineState {
    devices: DeviceList,
    calls: Vec<Arc<FakeCall>>,
    joins: Vec<JoinRequest>,
    next_join_error: Option<EngineError>,
    auto_connect: bool,
}

impl Default for FakeEngine {
    fn default() -> Self {
        FakeEngine::new()
    }
}

impl FakeEngine {
    /// No devices; calls connect as soon as they join.
    pub fn new() -> FakeEngine {
        let (devices_tx, _) = broadcast::channel(16);
        FakeEngine {
            state: Mutex::new(EngineState {
                devices: DeviceList::default(),
                calls: Vec::new(),
                joins: Vec::new(),
                next_join_error: None,
                auto_connect: true,
            }),
            devices_tx,
        }
    }

    /// Two inputs and two outputs, for the example.
    pub fn with_demo_devices() -> FakeEngine {
        let e = FakeEngine::new();
        lock(&e.state).devices = demo_devices();
        e
    }

    /// When false, a joined call stays `connecting` until [`FakeCall::connect`].
    pub fn set_auto_connect(&self, on: bool) {
        lock(&self.state).auto_connect = on;
    }

    /// The next `join` fails with this, before the provider is called.
    pub fn fail_next_join(&self, e: EngineError) {
        lock(&self.state).next_join_error = Some(e);
    }

    /// Replace the device list (a hot-plug): sends a `devices` change.
    pub fn set_devices(&self, devices: DeviceList) {
        lock(&self.state).devices = devices.clone();
        let _ = self.devices_tx.send(devices);
    }

    /// Every join request the engine was given, in order.
    pub fn joins(&self) -> Vec<JoinRequest> {
        lock(&self.state).joins.clone()
    }

    /// The most recent call.
    pub fn last_call(&self) -> Option<Arc<FakeCall>> {
        lock(&self.state).calls.last().cloned()
    }

    /// How many receivers of [`VoiceEngine::device_events`] are alive (the plugin subscribes
    /// on the first use of the devices, not at start).
    pub fn device_subscribers(&self) -> usize {
        self.devices_tx.receiver_count()
    }
}

/// The example's devices.
pub fn demo_devices() -> DeviceList {
    let dev = |id: &str, name: &str, default: bool| Device {
        id: id.into(),
        name: name.into(),
        default,
    };
    DeviceList {
        inputs: vec![
            dev("in:builtin", "Built-in microphone", true),
            dev("in:headset", "Headset microphone", false),
        ],
        outputs: vec![
            dev("out:builtin", "Built-in speakers", true),
            dev("out:headset", "Headset", false),
        ],
        input: None,
        output: None,
    }
}

impl VoiceEngine for FakeEngine {
    fn join(
        &self,
        provider: Arc<dyn JoinProvider>,
        req: JoinRequest,
    ) -> BoxFuture<'_, Result<Arc<dyn CallHandle>, EngineError>> {
        Box::pin(async move {
            let (fail, auto_connect) = {
                let mut s = lock(&self.state);
                s.joins.push(req.clone());
                (s.next_join_error.take(), s.auto_connect)
            };
            if let Some(e) = fail {
                return Err(e);
            }
            let answer = provider
                .join(req.clone())
                .await
                .map_err(EngineError::Provider)?;
            let call = Arc::new(FakeCall::new(&answer, &req, auto_connect));
            lock(&self.state).calls.push(Arc::clone(&call));
            let handle: Arc<dyn CallHandle> = call;
            Ok(handle)
        })
    }

    fn devices(&self) -> Result<DeviceList, EngineError> {
        Ok(lock(&self.state).devices.clone())
    }

    fn set_device(
        &self,
        input: Option<DeviceChoice>,
        output: Option<DeviceChoice>,
    ) -> Result<(), EngineError> {
        let list = {
            let mut s = lock(&self.state);
            if let Some(DeviceChoice::Id(id)) = &input
                && !s.devices.inputs.iter().any(|d| &d.id == id)
            {
                return Err(EngineError::Failed(ErrorInfo::with_detail(
                    "device_not_found",
                    json!({ "input": id }),
                )));
            }
            if let Some(DeviceChoice::Id(id)) = &output
                && !s.devices.outputs.iter().any(|d| &d.id == id)
            {
                return Err(EngineError::Failed(ErrorInfo::with_detail(
                    "device_not_found",
                    json!({ "output": id }),
                )));
            }
            // The OS default is `null` in the list.
            let kept = |c: DeviceChoice| match c {
                DeviceChoice::Default => None,
                DeviceChoice::Id(id) => Some(id),
            };
            if let Some(c) = input {
                s.devices.input = kept(c);
            }
            if let Some(c) = output {
                s.devices.output = kept(c);
            }
            s.devices.clone()
        };
        let _ = self.devices_tx.send(list);
        Ok(())
    }

    fn device_events(&self) -> broadcast::Receiver<DeviceList> {
        self.devices_tx.subscribe()
    }
}

/// One fake call. Tests drive it with [`FakeCall::emit`] and its shortcuts.
#[derive(Debug)]
pub struct FakeCall {
    call_id: String,
    self_member: String,
    tx: broadcast::Sender<CallEvent>,
    state: Mutex<CallFake>,
}

#[derive(Debug)]
struct CallFake {
    snapshot: CallSnapshot,
    /// `muted` before the current deafen.
    muted_before_deafen: Option<bool>,
    mutes: Vec<(Option<bool>, Option<bool>)>,
    /// Members without SPEAK, and the `muted` each had before (it comes back with SPEAK).
    speak_saved: BTreeMap<String, bool>,
    left: bool,
    /// The last move's destination room ([`FakeCall::move_to`]).
    moved_room: Option<String>,
    next_mute_error: Option<EngineError>,
    next_leave_error: Option<EngineError>,
}

impl FakeCall {
    fn new(answer: &JoinAnswer, req: &JoinRequest, auto_connect: bool) -> FakeCall {
        let (tx, _) = broadcast::channel(64);
        let self_member = answer.join.state.member_id.to_string();
        let speak_denied = answer.join.state.speak_denied;
        let me = Participant {
            member_id: self_member.clone(),
            is_self: true,
            muted: req.muted || req.deafened || speak_denied,
            deafened: req.deafened,
            server_muted: answer.join.state.server_mute,
            server_deafened: answer.join.state.server_deaf,
            speak_denied,
        };
        let mut speak_saved = BTreeMap::new();
        if speak_denied {
            speak_saved.insert(self_member.clone(), req.muted || req.deafened);
        }
        FakeCall {
            call_id: answer.join.room.as_str().to_owned(),
            self_member,
            tx,
            state: Mutex::new(CallFake {
                snapshot: CallSnapshot {
                    state: if auto_connect {
                        CallState::Connected
                    } else {
                        CallState::Connecting
                    },
                    participants: vec![me],
                },
                muted_before_deafen: req.deafened.then_some(req.muted),
                mutes: Vec::new(),
                speak_saved,
                left: false,
                moved_room: None,
                next_mute_error: None,
                next_leave_error: None,
            }),
        }
    }

    /// This device's member id.
    pub fn self_member(&self) -> &str {
        &self.self_member
    }

    /// Send an event as the engine would (state and roster events also update the snapshot).
    pub fn emit(&self, ev: CallEvent) {
        {
            let mut s = lock(&self.state);
            match &ev {
                CallEvent::State(st) => s.snapshot.state = st.clone(),
                CallEvent::Participants(p) => s.snapshot.participants = p.clone(),
                _ => {}
            }
        }
        let _ = self.tx.send(ev);
    }

    /// `connected`.
    pub fn connect(&self) {
        self.emit(CallEvent::State(CallState::Connected));
    }

    /// `reconnecting {attempt}`.
    pub fn reconnecting(&self, attempt: u32) {
        self.emit(CallEvent::State(CallState::Reconnecting { attempt }));
    }

    /// `ended {reason}` from the engine's side: Sew's reason as the real engine passes it on
    /// (`perms_lost`, `removed_from_space`, `session_revoked`, ...; [`crate::wire::reason::SEW`]),
    /// the engine's fallback when Sew gave none within 1.5 s of its Disconnect (`kicked`,
    /// `room_closed`), `credential_lost` for a takeover, or a network loss.
    pub fn end(&self, reason: &str) {
        self.emit(CallEvent::State(CallState::Ended {
            reason: reason.into(),
        }));
    }

    /// Add a remote member (unmuted).
    pub fn add_remote(&self, member_id: &str) {
        let mut p = self.snapshot().participants;
        p.push(Participant {
            member_id: member_id.into(),
            is_self: false,
            muted: false,
            deafened: false,
            server_muted: false,
            server_deafened: false,
            speak_denied: false,
        });
        self.emit(CallEvent::Participants(p));
    }

    /// SPEAK lost (`true`) or given back (`false`) for `member_id` (this device's member or a
    /// remote one), as Sew's voice state and the real engine show it: `speak_denied` and
    /// `muted` while denied; when it comes back, `muted` is the member's own mute again (a
    /// `mute` while denied is kept for then). Sends `participants`.
    pub fn set_speak_denied(&self, member_id: &str, denied: bool) {
        let roster = {
            let mut s = lock(&self.state);
            let saved = s.speak_saved.get(member_id).copied();
            let Some(p) = s
                .snapshot
                .participants
                .iter_mut()
                .find(|p| p.member_id == member_id)
            else {
                return;
            };
            let keep = match (denied, saved) {
                (true, None) => {
                    let before = p.muted;
                    p.muted = true;
                    p.speak_denied = true;
                    Some(Some(before))
                }
                (false, Some(m)) => {
                    p.muted = m;
                    p.speak_denied = false;
                    Some(None)
                }
                _ => None,
            };
            match keep {
                Some(Some(m)) => {
                    s.speak_saved.insert(member_id.to_owned(), m);
                }
                Some(None) => {
                    s.speak_saved.remove(member_id);
                }
                None => {}
            }
            s.snapshot.participants.clone()
        };
        let _ = self.tx.send(CallEvent::Participants(roster));
    }

    /// A moderator move to `room` (Sew's `voice_server_updated`, as the real engine passes it
    /// on: the DM call's id or the voice channel's id). Sends [`CallEvent::Moved`].
    pub fn move_to(&self, room: &str) {
        lock(&self.state).moved_room = Some(room.to_owned());
        let _ = self.tx.send(CallEvent::Moved { room: room.into() });
    }

    /// A move the plugin is not told of (to test a missed event; [`CallHandle::moved_room`]
    /// still says it).
    pub fn move_silently(&self, room: &str) {
        lock(&self.state).moved_room = Some(room.to_owned());
    }

    /// Change the snapshot without sending anything (to test a missed event).
    pub fn set_snapshot_silently(&self, snap: CallSnapshot) {
        lock(&self.state).snapshot = snap;
    }

    /// The next `mute` fails with this.
    pub fn fail_next_mute(&self, e: EngineError) {
        lock(&self.state).next_mute_error = Some(e);
    }

    /// The next `leave` fails with this (after leaving locally).
    pub fn fail_next_leave(&self, e: EngineError) {
        lock(&self.state).next_leave_error = Some(e);
    }

    /// Every `mute` call's arguments, in order.
    pub fn mutes(&self) -> Vec<(Option<bool>, Option<bool>)> {
        lock(&self.state).mutes.clone()
    }

    /// Whether `leave` was called.
    pub fn left(&self) -> bool {
        lock(&self.state).left
    }
}

impl CallHandle for FakeCall {
    fn call_id(&self) -> String {
        self.call_id.clone()
    }

    fn events(&self) -> broadcast::Receiver<CallEvent> {
        self.tx.subscribe()
    }

    fn snapshot(&self) -> CallSnapshot {
        lock(&self.state).snapshot.clone()
    }

    fn moved_room(&self) -> Option<String> {
        lock(&self.state).moved_room.clone()
    }

    fn mute(
        &self,
        muted: Option<bool>,
        deafened: Option<bool>,
    ) -> BoxFuture<'_, Result<(), EngineError>> {
        Box::pin(async move {
            let roster = {
                let mut s = lock(&self.state);
                s.mutes.push((muted, deafened));
                if let Some(e) = s.next_mute_error.take() {
                    return Err(e);
                }
                let mut saved = s.muted_before_deafen;
                // Without SPEAK, the mute rules work on the member's own mute (kept for when
                // SPEAK comes back) and the roster still shows `muted`.
                let held = s.speak_saved.get(&self.self_member).copied();
                let me = s
                    .snapshot
                    .participants
                    .iter_mut()
                    .find(|p| p.is_self)
                    .ok_or_else(|| EngineError::failed("not_in_roster"))?;
                if let Some(m) = held {
                    me.muted = m;
                }
                match (deafened, me.deafened) {
                    // Deafen: remember the mute to come back to; deafened implies muted.
                    (Some(true), false) => {
                        saved = Some(muted.unwrap_or(me.muted));
                        me.deafened = true;
                        me.muted = true;
                    }
                    // Un-deafen: back to the mute before the deafen (or the one given now).
                    (Some(false), true) => {
                        me.deafened = false;
                        me.muted = muted.or(saved).unwrap_or(false);
                        saved = None;
                    }
                    // Still deafened: a mute change is kept for the un-deafen.
                    (_, true) => {
                        if muted.is_some() {
                            saved = muted;
                        }
                    }
                    // Not deafened.
                    (_, false) => {
                        if let Some(m) = muted {
                            me.muted = m;
                        }
                    }
                }
                let own = me.muted;
                if held.is_some() {
                    me.muted = true;
                    s.speak_saved.insert(self.self_member.clone(), own);
                }
                s.muted_before_deafen = saved;
                s.snapshot.participants.clone()
            };
            let _ = self.tx.send(CallEvent::Participants(roster));
            Ok(())
        })
    }

    fn leave(&self) -> BoxFuture<'_, Result<(), EngineError>> {
        Box::pin(async move {
            let mut s = lock(&self.state);
            s.left = true;
            match s.next_leave_error.take() {
                Some(e) => Err(e),
                None => Ok(()),
            }
        })
    }
}

/// A provider that answers from memory (no network), optionally after a release.
#[derive(Debug)]
pub struct FakeProvider {
    member_id: String,
    result: Mutex<Option<ProviderError>>,
    gate: Option<Arc<Semaphore>>,
    calls: Mutex<Vec<JoinRequest>>,
}

impl FakeProvider {
    /// Answers every join as `member_id` (a Sew id: a decimal string).
    pub fn answering(member_id: &str) -> FakeProvider {
        FakeProvider {
            member_id: member_id.into(),
            result: Mutex::new(None),
            gate: None,
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Fails every join with `e`.
    pub fn failing(e: ProviderError) -> FakeProvider {
        let p = FakeProvider::answering("1");
        *lock(&p.result) = Some(e);
        p
    }

    /// Answers each join only after the returned semaphore gets a permit (to test a leave mid-join).
    pub fn gated(member_id: &str) -> (FakeProvider, Arc<Semaphore>) {
        let gate = Arc::new(Semaphore::new(0));
        let mut p = FakeProvider::answering(member_id);
        p.gate = Some(Arc::clone(&gate));
        (p, gate)
    }

    /// Every request the provider got, in order.
    pub fn calls(&self) -> Vec<JoinRequest> {
        lock(&self.calls).clone()
    }
}

/// A join answer as Sew 0.7 writes it, for `member_id` in room `r_fakeroom...`.
pub fn fake_answer(member_id: &str) -> Result<JoinAnswer, ProviderError> {
    let v = json!({
        "join": {
            "url": "wss://voice.sew.invalid",
            "token": "fake-media-token",
            "expires_at": "2026-11-02T12:02:00.000Z",
            "room": "r_fakeroomfakeroomfakeroomfa",
            "state": {
                "member_id": member_id,
                "channel_id": "48831765147549698",
                "connection": "connecting",
                "secure": false,
                "self_mute": false,
                "self_deaf": false,
                "self_stream": false,
                "self_video": false,
                "server_deaf": false,
                "server_mute": false,
                "speak_denied": false,
                "joined_at": "2026-11-02T12:00:00.000Z"
            }
        },
        "api_base": "https://sew.invalid/"
    });
    let body =
        serde_json::to_vec(&v).map_err(|e| ProviderError::Other(format!("fake answer: {e}")))?;
    crate::engine::app_server_answer(&body)
}

impl JoinProvider for FakeProvider {
    fn join(&self, req: JoinRequest) -> BoxFuture<'_, Result<JoinAnswer, ProviderError>> {
        Box::pin(async move {
            lock(&self.calls).push(req);
            if let Some(gate) = &self.gate {
                let permit = gate
                    .acquire()
                    .await
                    .map_err(|_| ProviderError::Other("gate closed".into()))?;
                permit.forget();
            }
            if let Some(e) = lock(&self.result).clone() {
                return Err(e);
            }
            fake_answer(&self.member_id)
        })
    }
}

/// Records every event (name and JSON payload), for tests.
#[derive(Debug, Default)]
pub struct RecordingSink {
    events: Mutex<Vec<(String, serde_json::Value)>>,
}

impl RecordingSink {
    /// An empty recorder.
    pub fn new() -> RecordingSink {
        RecordingSink::default()
    }

    /// Every event so far as `(name, payload)`, names without the `sew-voice://` prefix.
    pub fn events(&self) -> Vec<(String, serde_json::Value)> {
        lock(&self.events).clone()
    }

    /// The payloads of one event name.
    pub fn of(&self, name: &str) -> Vec<serde_json::Value> {
        lock(&self.events)
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
            .collect()
    }

    /// Forget what was recorded.
    pub fn clear(&self) {
        lock(&self.events).clear();
    }
}

impl EventSink for RecordingSink {
    fn emit(&self, event: &PluginEvent) {
        lock(&self.events).push((event.name().to_owned(), event.payload()));
    }
}
