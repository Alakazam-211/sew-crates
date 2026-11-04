//! The plugin's core logic, without Tauri: the six commands, the state machine and the event
//! pumps. `plugin.rs` wires it to Tauri; tests drive it directly with the fake engine.
//!
//! States: `idle` → `connecting` (join) → `connected` ⇄ `reconnecting {attempt}` → `ended
//! {reason}`; `ended` → `connecting` on the next join. One call per app.
//!
//! Devices are the engine's from the first use on: the first `devices`, `set_device` or
//! `join` subscribes to the engine's device events (and so starts its device listing), not
//! [`VoiceService::start`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::{Notify, broadcast};
use tokio::time::Instant;

use crate::engine::{
    BoxFuture, CallEvent, CallHandle, CallSnapshot, JoinProvider, JoinRequest, VoiceEngine,
};
use crate::wire::{
    CallState, DeviceList, ErrorInfo, JoinArgs, JoinResult, MuteArgs, Participant, PluginEvent,
    SetDeviceArgs, Speaking, StateEvent, StateSnapshot, code, reason,
};

/// The `speaking` event's floor: at most 10 a second.
pub const SPEAKING_MIN_INTERVAL: Duration = Duration::from_millis(100);

/// Where the plugin's events go (Tauri's emitter in the app, a recorder in tests).
///
/// `emit` is called with the plugin's state lock held, so events keep their order; it must not
/// block or call back into the [`VoiceService`].
pub trait EventSink: Send + Sync + 'static {
    /// Deliver one event to every window.
    fn emit(&self, event: &PluginEvent);
}

/// Runs the plugin's background tasks (Tauri's async runtime in the app, tokio in tests).
pub type Spawn = Arc<dyn Fn(BoxFuture<'static, ()>) + Send + Sync>;

/// The plugin's state: one per app (Tauri manages it).
#[derive(Clone)]
pub struct VoiceService {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for VoiceService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceService")
            .field("state", &self.lock().state)
            .finish_non_exhaustive()
    }
}

struct Inner {
    engine: Arc<dyn VoiceEngine>,
    provider: Arc<dyn JoinProvider>,
    sink: Arc<dyn EventSink>,
    spawn: Spawn,
    slot: Mutex<Slot>,
    /// [`VoiceService::start`] was called: forward device events from the first use on.
    devices_wanted: AtomicBool,
    /// The device events are forwarded (the first use came).
    devices_on: AtomicBool,
}

struct Slot {
    state: CallState,
    /// The call's `target` and `room` (`join`'s, the room a move's), from `connecting` until
    /// the next join.
    target: Option<String>,
    room: Option<String>,
    participants: Vec<Participant>,
    call: Option<Arc<dyn CallHandle>>,
    call_id: Option<String>,
    /// Bumped by every join and leave; a task or answer from an older generation is ignored.
    generation: u64,
    /// Stops the current call's pump.
    stop: Option<Arc<Notify>>,
}

impl VoiceService {
    /// A service over `engine`, joining through `provider`, emitting to `sink`.
    pub fn new(
        engine: Arc<dyn VoiceEngine>,
        provider: Arc<dyn JoinProvider>,
        sink: Arc<dyn EventSink>,
        spawn: Spawn,
    ) -> VoiceService {
        VoiceService {
            inner: Arc::new(Inner {
                engine,
                provider,
                sink,
                spawn,
                slot: Mutex::new(Slot {
                    state: CallState::Idle,
                    target: None,
                    room: None,
                    participants: Vec::new(),
                    call: None,
                    call_id: None,
                    generation: 0,
                    stop: None,
                }),
                devices_wanted: AtomicBool::new(false),
                devices_on: AtomicBool::new(false),
            }),
        }
    }

    /// Forward the engine's device changes as `devices` events, from the first `devices`,
    /// `set_device` or `join` on (until then the engine lists no devices and polls nothing).
    /// Call once.
    pub fn start(&self) {
        self.inner.devices_wanted.store(true, Ordering::Release);
    }

    /// The first use of the devices: subscribe to the engine's device events (once, after
    /// [`VoiceService::start`]).
    fn use_devices(&self) {
        if !self.inner.devices_wanted.load(Ordering::Acquire)
            || self.inner.devices_on.swap(true, Ordering::AcqRel)
        {
            return;
        }
        let mut rx = self.inner.engine.device_events();
        let inner = Arc::downgrade(&self.inner);
        (self.inner.spawn)(Box::pin(async move {
            loop {
                let list = match rx.recv().await {
                    Ok(list) => list,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return,
                };
                let Some(inner) = inner.upgrade() else { return };
                let _slot = lock(&inner.slot);
                inner.sink.emit(&PluginEvent::Devices(list));
            }
        }));
    }

    fn lock(&self) -> MutexGuard<'_, Slot> {
        lock(&self.inner.slot)
    }

    /// `join {target, room, muted?, deafened?}` → `{call_id}`.
    pub async fn join(&self, args: JoinArgs) -> Result<JoinResult, ErrorInfo> {
        self.use_devices();
        let deafened = args.deafened.unwrap_or(false);
        let req = JoinRequest {
            target: args.target,
            room: args.room,
            muted: args.muted.unwrap_or(false) || deafened,
            deafened,
            rejoin: None,
        };
        let generation = {
            let mut slot = self.lock();
            if slot.state.is_live() {
                return Err(ErrorInfo::new(code::ALREADY_IN_CALL));
            }
            slot.generation += 1;
            slot.call = None;
            slot.call_id = None;
            slot.target = Some(req.target.clone());
            slot.room = Some(req.room.clone());
            let had_participants = !slot.participants.is_empty();
            slot.participants.clear();
            if had_participants {
                self.inner.sink.emit(&PluginEvent::Participants(Vec::new()));
            }
            self.set_state(&mut slot, CallState::Connecting);
            slot.generation
        };

        let joined = self
            .inner
            .engine
            .join(Arc::clone(&self.inner.provider), req)
            .await;

        let mut slot = self.lock();
        if slot.generation != generation {
            // A leave came while we were joining: hang the new call up.
            drop(slot);
            if let Ok(call) = joined {
                let sink = Arc::clone(&self.inner.sink);
                (self.inner.spawn)(Box::pin(async move {
                    if let Err(e) = call.leave().await {
                        sink.emit(&PluginEvent::Errors(e.to_info()));
                    }
                }));
            }
            return Err(ErrorInfo::new(code::LEFT));
        }
        let call = match joined {
            Ok(call) => call,
            Err(e) => {
                let info = e.to_info();
                self.inner.sink.emit(&PluginEvent::Errors(info.clone()));
                self.set_state(
                    &mut slot,
                    CallState::Ended {
                        reason: e.end_reason(),
                    },
                );
                return Err(info);
            }
        };
        let call_id = call.call_id();
        // Subscribe before the snapshot, so nothing between them is lost.
        let rx = call.events();
        let stop = Arc::new(Notify::new());
        slot.call = Some(Arc::clone(&call));
        slot.call_id = Some(call_id.clone());
        slot.stop = Some(Arc::clone(&stop));
        let ended = self.apply_snapshot(&mut slot, call.snapshot());
        drop(slot);
        if !ended {
            let pump = Pump {
                inner: Arc::clone(&self.inner),
                generation,
                rx,
                stop,
                call,
            };
            (self.inner.spawn)(Box::pin(pump.run()));
        }
        Ok(JoinResult { call_id })
    }

    /// `leave`. With no call it does nothing. The state is `ended {reason: "left"}` at once;
    /// if Sew's leave then fails, an `errors` event says so (the call is gone either way).
    pub async fn leave(&self) -> Result<(), ErrorInfo> {
        let call = {
            let mut slot = self.lock();
            if !slot.state.is_live() {
                return Ok(());
            }
            slot.generation += 1;
            if let Some(stop) = slot.stop.take() {
                stop.notify_one();
            }
            let call = slot.call.take();
            slot.call_id = None;
            if !slot.participants.is_empty() {
                slot.participants.clear();
                self.inner.sink.emit(&PluginEvent::Participants(Vec::new()));
            }
            self.set_state(
                &mut slot,
                CallState::Ended {
                    reason: reason::LEFT.into(),
                },
            );
            call
        };
        if let Some(call) = call
            && let Err(e) = call.leave().await
        {
            let _slot = self.lock();
            self.inner.sink.emit(&PluginEvent::Errors(e.to_info()));
        }
        Ok(())
    }

    /// `mute {muted?, deafened?}` on the live call.
    pub async fn mute(&self, args: MuteArgs) -> Result<(), ErrorInfo> {
        if args.muted.is_none() && args.deafened.is_none() {
            return Err(ErrorInfo::with_detail(
                code::INVALID_ARGS,
                "mute needs muted or deafened",
            ));
        }
        let call = self
            .lock()
            .call
            .clone()
            .ok_or_else(|| ErrorInfo::new(code::NOT_IN_CALL))?;
        call.mute(args.muted, args.deafened)
            .await
            .map_err(|e| e.to_info())
    }

    /// `devices` → the device list.
    pub async fn devices(&self) -> Result<DeviceList, ErrorInfo> {
        self.use_devices();
        self.inner.engine.devices().map_err(|e| e.to_info())
    }

    /// `set_device {input?, output?}`: a device id, or `"default"` for the OS default.
    pub async fn set_device(&self, args: SetDeviceArgs) -> Result<(), ErrorInfo> {
        if args.input.is_none() && args.output.is_none() {
            return Err(ErrorInfo::with_detail(
                code::INVALID_ARGS,
                "set_device needs input or output",
            ));
        }
        self.use_devices();
        self.inner
            .engine
            .set_device(args.input_choice(), args.output_choice())
            .map_err(|e| e.to_info())
    }

    /// `state` → the state, the participants and the call id (for a late window).
    pub async fn state(&self) -> Result<StateSnapshot, ErrorInfo> {
        Ok(self.snapshot())
    }

    /// The state now (what the `state` command answers).
    pub fn snapshot(&self) -> StateSnapshot {
        let slot = self.lock();
        StateSnapshot {
            state: slot.state.clone(),
            target: slot.target.clone(),
            room: slot.room.clone(),
            participants: slot.participants.clone(),
            call_id: slot.call_id.clone(),
        }
    }

    fn set_state(&self, slot: &mut Slot, state: CallState) {
        set_state(&self.inner, slot, state);
    }

    /// Apply an engine snapshot; true when it says the call ended.
    fn apply_snapshot(&self, slot: &mut Slot, snap: CallSnapshot) -> bool {
        apply_snapshot(&self.inner, slot, snap)
    }
}

fn lock(m: &Mutex<Slot>) -> MutexGuard<'_, Slot> {
    // A panic while holding the lock leaves plain data behind; keep going with it.
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The `state` event for the slot now.
fn state_event(slot: &Slot) -> PluginEvent {
    PluginEvent::State(StateEvent {
        state: slot.state.clone(),
        target: slot.target.clone(),
        room: slot.room.clone(),
    })
}

fn set_state(inner: &Inner, slot: &mut Slot, state: CallState) {
    if slot.state != state {
        slot.state = state;
        inner.sink.emit(&state_event(slot));
    }
}

/// A moderator move: the call is on `room` now (a `state` event says so).
fn set_room(inner: &Inner, slot: &mut Slot, room: String) {
    if slot.room.as_deref() != Some(room.as_str()) {
        slot.room = Some(room);
        inner.sink.emit(&state_event(slot));
    }
}

fn set_participants(inner: &Inner, slot: &mut Slot, participants: Vec<Participant>) {
    if slot.participants != participants {
        slot.participants = participants.clone();
        inner.sink.emit(&PluginEvent::Participants(participants));
    }
}

/// The call ended (engine side): forget it and say so.
fn end(inner: &Inner, slot: &mut Slot, reason: String) {
    slot.call = None;
    slot.call_id = None;
    slot.stop = None;
    set_participants(inner, slot, Vec::new());
    set_state(inner, slot, CallState::Ended { reason });
}

fn apply_snapshot(inner: &Inner, slot: &mut Slot, snap: CallSnapshot) -> bool {
    match snap.state {
        CallState::Ended { reason } => {
            end(inner, slot, reason);
            true
        }
        // An engine never reports idle; a fresh call is connecting.
        CallState::Idle => {
            set_participants(inner, slot, snap.participants);
            false
        }
        state => {
            set_participants(inner, slot, snap.participants);
            set_state(inner, slot, state);
            false
        }
    }
}

/// Forwards one call's events while its generation is current.
struct Pump {
    inner: Arc<Inner>,
    generation: u64,
    rx: broadcast::Receiver<CallEvent>,
    stop: Arc<Notify>,
    call: Arc<dyn CallHandle>,
}

impl Pump {
    async fn run(mut self) {
        let mut last_speaking: Option<Instant> = None;
        let mut pending: Option<Vec<Speaking>> = None;
        loop {
            let flush_at = match (&pending, last_speaking) {
                (Some(_), Some(t)) => Some(t + SPEAKING_MIN_INTERVAL),
                _ => None,
            };
            let ev = tokio::select! {
                () = self.stop.notified() => return,
                () = sleep_until(flush_at) => {
                    if let Some(levels) = pending.take() {
                        if !self.emit_current(PluginEvent::Speaking(levels)) {
                            return;
                        }
                        last_speaking = Some(Instant::now());
                    }
                    continue;
                }
                ev = self.rx.recv() => ev,
            };
            match ev {
                Ok(CallEvent::Speaking(levels)) => {
                    let now = Instant::now();
                    let due = last_speaking.is_none_or(|t| now >= t + SPEAKING_MIN_INTERVAL);
                    if due {
                        pending = None;
                        if !self.emit_current(PluginEvent::Speaking(levels)) {
                            return;
                        }
                        last_speaking = Some(now);
                    } else {
                        // Keep only the newest levels; the timer sends them.
                        pending = Some(levels);
                    }
                }
                Ok(CallEvent::Error(info)) => {
                    if !self.emit_current(PluginEvent::Errors(info)) {
                        return;
                    }
                }
                Ok(CallEvent::Participants(p)) => {
                    let mut slot = lock(&self.inner.slot);
                    if slot.generation != self.generation {
                        return;
                    }
                    set_participants(&self.inner, &mut slot, p);
                }
                Ok(CallEvent::Moved { room }) => {
                    let mut slot = lock(&self.inner.slot);
                    if slot.generation != self.generation {
                        return;
                    }
                    set_room(&self.inner, &mut slot, room);
                }
                Ok(CallEvent::State(state)) => {
                    let mut slot = lock(&self.inner.slot);
                    if slot.generation != self.generation {
                        return;
                    }
                    match state {
                        CallState::Ended { reason } => {
                            end(&self.inner, &mut slot, reason);
                            return;
                        }
                        CallState::Idle => {}
                        state => set_state(&self.inner, &mut slot, state),
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // Missed some: skip what is still queued (older than the snapshot), then
                    // the snapshot is the truth.
                    self.rx = self.rx.resubscribe();
                    let mut slot = lock(&self.inner.slot);
                    if slot.generation != self.generation {
                        return;
                    }
                    if let Some(room) = self.call.moved_room() {
                        set_room(&self.inner, &mut slot, room);
                    }
                    if apply_snapshot(&self.inner, &mut slot, self.call.snapshot()) {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => {
                    let mut slot = lock(&self.inner.slot);
                    if slot.generation == self.generation && slot.state.is_live() {
                        end(&self.inner, &mut slot, reason::ENGINE_STOPPED.into());
                    }
                    return;
                }
            }
        }
    }

    /// Emit if this call is still current; false when it is not (the pump stops).
    fn emit_current(&self, event: PluginEvent) -> bool {
        let slot = lock(&self.inner.slot);
        if slot.generation != self.generation {
            return false;
        }
        self.inner.sink.emit(&event);
        true
    }
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}
