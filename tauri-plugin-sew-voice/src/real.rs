//! The real engine (feature `audio-core`; `audio` adds the system devices):
//! [`CallClientEngine`], the plugin's [`VoiceEngine`] over `sew-call-client`'s `CallClient`, and
//! its calls, a [`CallHandle`] over `Call` with kessel-audio through `AudioBridge`.
//!
//! - **Events.** The engine's state, participants and errors become the plugin's (the
//!   [`wire`](crate::wire) shapes). `speaking` comes from kessel-audio's levels (decoded audio for
//!   each remote track, the processed capture for this device), sampled every 100 ms and sent when
//!   they change; the plugin's own throttle keeps it at 10 a second at most.
//! - **Devices.** During a call they are kessel-audio's (`AudioBridge::engine()`: its list, its
//!   hot-plug events, its device switch). Between calls no audio engine runs (it would open the
//!   microphone), so the list comes from a [`DeviceLister`] (the system's through cpal with the
//!   `audio` feature, the same ids kessel-audio uses), and a `set_device` is kept for the next
//!   call. Nothing is listed before the first use (the first `devices`, `set_device`, `join` or
//!   `device_events`); after it the lister is polled every 2 s for hot-plug, between calls and
//!   only while something is subscribed to the device events.
//! - **One call per app**, as the engine: `Call` sits in an `RwLock<Option<Call>>` because
//!   `Call::leave(self)` consumes it.
//! - **Recording** (Sew 0.7.3). A join held for the recording notice (`consent_required`) opens
//!   no microphone: kessel-audio starts when the call first leaves that state (after
//!   `consent`). The `recording` event is the engine's recording with `recorder_present` from
//!   its roster (the recorder is never a `participants` entry: it is no member). `consent` and
//!   `withdraw_consent` are the engine's, through the host's provider.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use serde_json::{Value, json};
use sew_call_client::kessel_audio::{
    self, AudioError, DeviceChoice, DeviceSelection, EngineConfig, ErrorCode, Event as AudioEvent,
};
use sew_call_client::{
    AudioBridge, BridgeLevels, Call, CallClient, CallError, CallRecording, EndReason,
};
use tokio::sync::{RwLock, broadcast};
use tokio::task::JoinHandle;

use crate::engine::{
    BoxFuture, CallEvent, CallHandle, CallSnapshot, EngineError, JoinAnswer, JoinProvider,
    JoinRequest, ProviderError, VoiceEngine,
};
use crate::wire::{
    CallState, Device, DeviceChoice as Choice, DeviceList, ErrorInfo, Participant, RecordingInfo,
    Speaking, reason,
};

/// How often the `speaking` levels are sampled (at most 10 a second).
pub const SPEAKING_EVERY: Duration = Duration::from_millis(100);
/// How often the device list is polled for hot-plug between calls (after the first use, while
/// something is subscribed to the device events).
pub const IDLE_DEVICE_POLL: Duration = Duration::from_secs(2);

/// The poll's pace (the unit tests' is short).
fn idle_device_poll() -> Duration {
    if cfg!(test) {
        Duration::from_millis(20)
    } else {
        IDLE_DEVICE_POLL
    }
}

/// Builds kessel-audio's configuration for a call from the chosen input and output.
pub type AudioConfig = Arc<dyn Fn(DeviceChoice, DeviceChoice) -> EngineConfig + Send + Sync>;

/// Lists the audio devices while no call (and so no kessel-audio engine) runs.
pub type DeviceLister = Arc<dyn Fn() -> kessel_audio::DeviceList + Send + Sync>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// `sew-call-client` as the plugin's engine.
pub struct CallClientEngine {
    inner: Arc<EngineInner>,
}

impl std::fmt::Debug for CallClientEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallClientEngine")
            .field("client", &self.inner.client)
            .finish_non_exhaustive()
    }
}

struct EngineInner {
    client: CallClient,
    audio: AudioConfig,
    lister: Mutex<Option<DeviceLister>>,
    /// The devices were used (listed once; the poll thread runs).
    listing: AtomicBool,
    devices_tx: broadcast::Sender<DeviceList>,
    state: Mutex<DevState>,
}

#[derive(Default)]
struct DevState {
    /// The last list seen (kessel-audio's during a call, the lister's between calls).
    list: kessel_audio::DeviceList,
    /// The chosen input and output (`None`: the OS default).
    input: Option<String>,
    output: Option<String>,
    /// The live call.
    live: Option<Weak<CallShared>>,
}

impl DevState {
    fn wire(&self) -> DeviceList {
        let dev = |d: &kessel_audio::DeviceInfo| Device {
            id: d.id.clone(),
            name: d.name.clone(),
            default: d.is_default,
        };
        DeviceList {
            inputs: self.list.inputs.iter().map(dev).collect(),
            outputs: self.list.outputs.iter().map(dev).collect(),
            input: self.input.clone(),
            output: self.output.clone(),
        }
    }

    fn live(&self) -> Option<Arc<CallShared>> {
        self.live.as_ref().and_then(Weak::upgrade)
    }
}

impl CallClientEngine {
    /// The engine over `client`, with kessel-audio configured by `audio` for each call. Until
    /// [`CallClientEngine::devices_from`], the device list between calls is the last call's.
    pub fn new(client: CallClient, audio: AudioConfig) -> CallClientEngine {
        let (devices_tx, _) = broadcast::channel(16);
        CallClientEngine {
            inner: Arc::new(EngineInner {
                client,
                audio,
                lister: Mutex::new(None),
                listing: AtomicBool::new(false),
                devices_tx,
                state: Mutex::new(DevState::default()),
            }),
        }
    }

    /// The production engine: Sew's MLS call keys ([`CallClient::with_mls`]: the device key
    /// where `cfg.device_store` says, by default a 0600 file under `cfg.data_dir`), the
    /// system's audio devices through kessel-audio, and the system device list between calls.
    #[cfg(feature = "audio")]
    pub fn with_mls(cfg: sew_call_client::ClientConfig) -> CallClientEngine {
        CallClientEngine::new(CallClient::with_mls(cfg), system_audio())
            .devices_from(system_devices())
    }

    /// Lists the devices between calls with `lister`: first on the first use of the devices
    /// (`devices`, `set_device`, `join` or `device_events`), then every [`IDLE_DEVICE_POLL`]
    /// between calls while something is subscribed to the device events (a change is a
    /// `devices` event). Nothing is listed at launch. The polling thread stops with the engine.
    pub fn devices_from(self, lister: DeviceLister) -> CallClientEngine {
        *lock(&self.inner.lister) = Some(lister);
        self
    }

    /// The first use of the devices: list them once and start the poll (with a lister).
    fn use_devices(&self) {
        let Some(lister) = lock(&self.inner.lister).clone() else {
            return;
        };
        if self.inner.listing.swap(true, Ordering::AcqRel) {
            return;
        }
        let list = lister();
        {
            let mut st = lock(&self.inner.state);
            if st.live().is_none() {
                st.list = list;
            }
        }
        let weak = Arc::downgrade(&self.inner);
        let spawned = std::thread::Builder::new()
            .name("sew-voice-devices".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(idle_device_poll());
                    let Some(inner) = weak.upgrade() else { return };
                    inner.poll_idle_devices();
                }
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "sew-voice: no device polling thread");
        }
    }
}

impl EngineInner {
    fn emit_devices(&self, st: &DevState) {
        let _ = self.devices_tx.send(st.wire());
    }

    fn poll_idle_devices(&self) {
        let Some(lister) = lock(&self.lister).clone() else {
            return;
        };
        // In a call kessel-audio has the list; with nobody subscribed a change tells no one.
        if self.devices_tx.receiver_count() == 0 || lock(&self.state).live().is_some() {
            return;
        }
        let list = lister();
        let mut st = lock(&self.state);
        if st.live().is_none() && st.list != list {
            st.list = list;
            self.emit_devices(&st);
        }
    }

    /// kessel-audio's list changed during a call.
    fn call_devices(&self, list: kessel_audio::DeviceList) {
        let mut st = lock(&self.state);
        if st.list != list {
            st.list = list;
            self.emit_devices(&st);
        }
    }

    /// kessel-audio switched devices on its own (a chosen device went away: it falls back to
    /// the default). The choice is then the OS default again.
    fn call_devices_in_use(&self, in_use: &kessel_audio::DevicesInUse) {
        let mut st = lock(&self.state);
        let mut changed = false;
        if let (Some(want), Some(got)) = (&st.input, &in_use.input)
            && *want != got.id
        {
            st.input = None;
            changed = true;
        }
        if let (Some(want), Some(got)) = (&st.output, &in_use.output)
            && *want != got.id
        {
            st.output = None;
            changed = true;
        }
        if changed {
            self.emit_devices(&st);
        }
    }

    /// kessel-audio for `call` on the chosen devices.
    fn start_audio(&self, call: &Call) -> Result<AudioBridge, ErrorInfo> {
        let (input, output) = {
            let st = lock(&self.state);
            (st.input.clone(), st.output.clone())
        };
        let cfg = (self.audio)(choice(input), choice(output));
        AudioBridge::start(call, cfg)
            .map_err(|e| ErrorInfo::with_detail("mic_unavailable", e.to_string()))
    }

    fn call_over(&self, call: &Arc<CallShared>) {
        let mut st = lock(&self.state);
        if st.live().is_some_and(|l| Arc::ptr_eq(&l, call)) {
            st.live = None;
        }
    }
}

/// The audio for a call on the system's devices (kessel-audio's defaults otherwise).
#[cfg(feature = "audio")]
pub fn system_audio() -> AudioConfig {
    Arc::new(|input, output| EngineConfig {
        backend: kessel_audio::Backend::System { input, output },
        ..EngineConfig::default()
    })
}

/// The system's device list through cpal, with kessel-audio's ids and names.
#[cfg(feature = "audio")]
pub fn system_devices() -> DeviceLister {
    Arc::new(|| {
        use cpal::traits::{DeviceTrait, HostTrait};
        let host = cpal::default_host();
        let id_of = |d: &cpal::Device| d.id().ok().map(|i| i.to_string());
        let info = |d: &cpal::Device, default: Option<&str>| {
            let id = id_of(d)?;
            let name = d
                .description()
                .map(|x| x.name().to_string())
                .unwrap_or_else(|_| id.clone());
            Some(kessel_audio::DeviceInfo {
                is_default: default == Some(id.as_str()),
                id,
                name,
            })
        };
        let def_in = host.default_input_device().and_then(|d| id_of(&d));
        let def_out = host.default_output_device().and_then(|d| id_of(&d));
        kessel_audio::DeviceList {
            inputs: host
                .input_devices()
                .map(|it| it.filter_map(|d| info(&d, def_in.as_deref())).collect())
                .unwrap_or_default(),
            outputs: host
                .output_devices()
                .map(|it| it.filter_map(|d| info(&d, def_out.as_deref())).collect())
                .unwrap_or_default(),
        }
    })
}

fn choice(id: Option<String>) -> DeviceChoice {
    id.map_or(DeviceChoice::Default, DeviceChoice::Id)
}

/// The plugin's choice as kessel-audio's.
fn audio_choice(c: &Choice) -> DeviceChoice {
    match c {
        Choice::Default => DeviceChoice::Default,
        Choice::Id(id) => DeviceChoice::Id(id.clone()),
    }
}

/// The choice as the `devices` list keeps it (`None`: the OS default).
fn kept(c: Choice) -> Option<String> {
    match c {
        Choice::Default => None,
        Choice::Id(id) => Some(id),
    }
}

impl VoiceEngine for CallClientEngine {
    fn join(
        &self,
        provider: Arc<dyn JoinProvider>,
        req: JoinRequest,
    ) -> BoxFuture<'_, Result<Arc<dyn CallHandle>, EngineError>> {
        Box::pin(async move {
            self.use_devices();
            let room = Arc::new(Mutex::new(None));
            let recorder: Arc<dyn JoinProvider> = Arc::new(RoomRecorder {
                inner: provider,
                room: Arc::clone(&room),
            });
            let call = self
                .inner
                .client
                .join(recorder, req)
                .await
                .map_err(join_error)?;
            // Held for the recording notice: no microphone until the person answered.
            let held = matches!(
                call.snapshot().state,
                sew_call_client::CallState::ConsentRequired { .. }
            );
            let bridge = if held {
                None
            } else {
                match self.inner.start_audio(&call) {
                    Ok(b) => Some(b),
                    Err(e) => {
                        let _ = call.leave().await;
                        return Err(EngineError::Failed(e));
                    }
                }
            };
            let real = RealCall::start(call, bridge, room, Arc::downgrade(&self.inner));
            {
                let mut st = lock(&self.inner.state);
                st.live = Some(Arc::downgrade(&real.shared));
                // A held join has no kessel-audio yet: the list stays until it starts.
                if let Some(list) = real.shared.with_bridge(|b| b.engine().devices()) {
                    st.list = list;
                }
                self.inner.emit_devices(&st);
            }
            let handle: Arc<dyn CallHandle> = Arc::new(real);
            Ok(handle)
        })
    }

    fn devices(&self) -> Result<DeviceList, EngineError> {
        self.use_devices();
        let live = lock(&self.inner.state).live();
        let list = match live {
            Some(call) => call.with_bridge(|b| b.engine().devices()),
            None => lock(&self.inner.lister).clone().map(|l| l()),
        };
        let mut st = lock(&self.inner.state);
        if let Some(list) = list {
            st.list = list;
        }
        Ok(st.wire())
    }

    fn set_device(&self, input: Option<Choice>, output: Option<Choice>) -> Result<(), EngineError> {
        self.use_devices();
        let live = {
            let st = lock(&self.inner.state);
            if let Some(Choice::Id(id)) = &input
                && !st.list.inputs.iter().any(|d| &d.id == id)
            {
                return Err(EngineError::Failed(ErrorInfo::with_detail(
                    "device_not_found",
                    json!({ "input": id }),
                )));
            }
            if let Some(Choice::Id(id)) = &output
                && !st.list.outputs.iter().any(|d| &d.id == id)
            {
                return Err(EngineError::Failed(ErrorInfo::with_detail(
                    "device_not_found",
                    json!({ "output": id }),
                )));
            }
            st.live()
        };
        if let Some(call) = live {
            // kessel-audio's DeviceChoice::Default: the OS default, followed when it changes.
            let sel = DeviceSelection {
                input: input.as_ref().map(audio_choice),
                output: output.as_ref().map(audio_choice),
            };
            if let Some(Err(e)) = call.with_bridge(|b| b.engine().set_device(sel)) {
                return Err(EngineError::Failed(audio_error(&e)));
            }
        }
        let mut st = lock(&self.inner.state);
        if let Some(c) = input {
            st.input = kept(c);
        }
        if let Some(c) = output {
            st.output = kept(c);
        }
        self.inner.emit_devices(&st);
        Ok(())
    }

    fn device_events(&self) -> broadcast::Receiver<DeviceList> {
        let rx = self.inner.devices_tx.subscribe();
        self.use_devices();
        rx
    }
}

/// Passes the host's provider through and keeps the answer's call room (the `call_id`).
struct RoomRecorder {
    inner: Arc<dyn JoinProvider>,
    room: Arc<Mutex<Option<String>>>,
}

impl JoinProvider for RoomRecorder {
    fn join(&self, req: JoinRequest) -> BoxFuture<'_, Result<JoinAnswer, ProviderError>> {
        Box::pin(async move {
            let answer = self.inner.join(req).await?;
            *lock(&self.room) = Some(answer.join.room.as_str().to_owned());
            Ok(answer)
        })
    }
}

/// One live call of the real engine.
struct RealCall {
    shared: Arc<CallShared>,
}

impl std::fmt::Debug for RealCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealCall").finish_non_exhaustive()
    }
}

struct CallShared {
    call: RwLock<Option<Call>>,
    bridge: Mutex<Option<AudioBridge>>,
    /// kessel-audio was started (a held join starts it after the answer; once only).
    audio_started: AtomicBool,
    /// The engine's recording, and the `recording` event last sent.
    recording: Mutex<(Option<CallRecording>, Option<RecordingInfo>)>,
    /// The plugin's events; dropped when the engine's stream closes (the plugin then ends the
    /// call `engine_stopped`).
    tx: Mutex<Option<broadcast::Sender<CallEvent>>>,
    room: Arc<Mutex<Option<String>>>,
    /// The last snapshot, for when the call is being left.
    last: Mutex<CallSnapshot>,
    engine: Weak<EngineInner>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl RealCall {
    fn start(
        call: Call,
        bridge: Option<AudioBridge>,
        room: Arc<Mutex<Option<String>>>,
        engine: Weak<EngineInner>,
    ) -> RealCall {
        let (tx, _) = broadcast::channel(256);
        let rx = call.events();
        let snap = call.snapshot();
        let last = map_snapshot(&snap);
        let shared = Arc::new(CallShared {
            call: RwLock::new(Some(call)),
            audio_started: AtomicBool::new(bridge.is_some()),
            bridge: Mutex::new(bridge),
            recording: Mutex::new((snap.recording.clone(), last.recording.clone())),
            tx: Mutex::new(Some(tx)),
            room,
            last: Mutex::new(last),
            engine,
            tasks: Mutex::new(Vec::new()),
        });
        let forward = tokio::spawn(forward(Arc::downgrade(&shared), rx));
        let audio = tokio::spawn(audio(Arc::downgrade(&shared)));
        lock(&shared.tasks).extend([forward, audio]);
        RealCall { shared }
    }
}

impl Drop for RealCall {
    fn drop(&mut self) {
        for t in lock(&self.shared.tasks).drain(..) {
            t.abort();
        }
        self.shared.finish();
    }
}

impl CallShared {
    fn send(&self, ev: CallEvent) {
        if let Some(tx) = lock(&self.tx).as_ref() {
            let _ = tx.send(ev);
        }
    }

    fn with_bridge<T>(&self, f: impl FnOnce(&AudioBridge) -> T) -> Option<T> {
        lock(&self.bridge).as_ref().map(f)
    }

    /// A held join was let in: kessel-audio starts now (once). If it cannot, the call is left
    /// (`mic_unavailable`), as a join that could not open the microphone: `false`.
    async fn start_audio_late(self: &Arc<Self>) -> bool {
        if self.audio_started.swap(true, Ordering::AcqRel) {
            return true;
        }
        let Some(engine) = self.engine.upgrade() else {
            return true;
        };
        let started = {
            let g = self.call.read().await;
            match g.as_ref() {
                Some(call) => engine.start_audio(call),
                None => return true,
            }
        };
        match started {
            Ok(bridge) => {
                let list = bridge.engine().devices();
                *lock(&self.bridge) = Some(bridge);
                let mut st = lock(&engine.state);
                st.list = list;
                engine.emit_devices(&st);
                true
            }
            Err(e) => {
                self.send(CallEvent::Error(e.clone()));
                let call = self.call.write().await.take();
                if let Some(call) = call {
                    let _ = call.leave().await;
                }
                *lock(&self.last) = CallSnapshot {
                    state: CallState::Ended {
                        reason: e.code.clone(),
                    },
                    participants: Vec::new(),
                    recording: None,
                };
                self.send(CallEvent::State(CallState::Ended { reason: e.code }));
                self.finish();
                false
            }
        }
    }

    /// The `recording` event for the engine's recording (`rec`, when it changed) and roster
    /// (`participants`, for `recorder_present`): sent when it changed.
    fn update_recording(
        &self,
        rec: Option<Option<CallRecording>>,
        participants: &[sew_call_client::Participant],
    ) {
        let info = {
            let mut r = lock(&self.recording);
            if let Some(rec) = rec {
                r.0 = rec;
            }
            let info = map_recording(r.0.as_ref(), participants);
            if info == r.1 {
                return;
            }
            r.1.clone_from(&info);
            info
        };
        self.send(CallEvent::Recording(info));
    }

    /// The engine's roster now (empty while the call is being left).
    fn engine_participants(&self) -> Vec<sew_call_client::Participant> {
        self.engine_snapshot()
            .map(|s| s.participants)
            .unwrap_or_default()
    }

    /// The engine's snapshot now, when the call is not being left.
    fn engine_snapshot(&self) -> Option<sew_call_client::CallSnapshot> {
        let g = self.call.try_read().ok()?;
        g.as_ref().map(Call::snapshot)
    }

    fn snapshot(&self) -> CallSnapshot {
        match self.engine_snapshot() {
            Some(s) => {
                let s = map_snapshot(&s);
                *lock(&self.last) = s.clone();
                s
            }
            None => lock(&self.last).clone(),
        }
    }

    /// The call is over: close the audio (the microphone) and free the app's call slot.
    fn finish(self: &Arc<Self>) {
        drop(lock(&self.bridge).take());
        if let Some(engine) = self.engine.upgrade() {
            engine.call_over(self);
        }
    }
}

impl CallHandle for RealCall {
    fn call_id(&self) -> String {
        lock(&self.shared.room).clone().unwrap_or_default()
    }

    fn events(&self) -> broadcast::Receiver<CallEvent> {
        match lock(&self.shared.tx).as_ref() {
            Some(tx) => tx.subscribe(),
            // The engine's stream is gone: a receiver that is closed at once.
            None => broadcast::channel(1).1,
        }
    }

    fn snapshot(&self) -> CallSnapshot {
        self.shared.snapshot()
    }

    fn moved_room(&self) -> Option<String> {
        let moved = self.shared.engine_snapshot()?.moved?;
        Some(moved.room().to_owned())
    }

    fn consent(
        &self,
        granted: bool,
        notice_version: u32,
    ) -> BoxFuture<'_, Result<(), EngineError>> {
        Box::pin(async move {
            let g = self.shared.call.read().await;
            let call = g
                .as_ref()
                .ok_or_else(|| EngineError::failed(crate::wire::code::NOT_IN_CALL))?;
            call.consent(granted, notice_version)
                .await
                .map_err(consent_error)
        })
    }

    fn withdraw_consent(&self) -> BoxFuture<'_, Result<(), EngineError>> {
        Box::pin(async move {
            let g = self.shared.call.read().await;
            let call = g
                .as_ref()
                .ok_or_else(|| EngineError::failed(crate::wire::code::NOT_IN_CALL))?;
            call.withdraw_consent().await.map_err(consent_error)
        })
    }

    fn mute(
        &self,
        muted: Option<bool>,
        deafened: Option<bool>,
    ) -> BoxFuture<'_, Result<(), EngineError>> {
        Box::pin(async move {
            let g = self.shared.call.read().await;
            let call = g
                .as_ref()
                .ok_or_else(|| EngineError::failed(crate::wire::code::NOT_IN_CALL))?;
            call.mute(muted, deafened).await.map_err(|e| match e {
                CallError::Ended => EngineError::failed(crate::wire::code::NOT_IN_CALL),
                e => join_error(e),
            })
        })
    }

    fn leave(&self) -> BoxFuture<'_, Result<(), EngineError>> {
        Box::pin(async move {
            let call = self.shared.call.write().await.take();
            if let Some(call) = call {
                // `Ended` only: the call was already over, which is what a leave wants.
                let _ = call.leave().await;
            }
            *lock(&self.shared.last) = CallSnapshot {
                state: CallState::Ended {
                    reason: reason::LEFT.into(),
                },
                participants: Vec::new(),
                recording: None,
            };
            self.shared.finish();
            Ok(())
        })
    }
}

/// The engine's events in the plugin's shapes, while the call lives.
async fn forward(
    shared: Weak<CallShared>,
    mut rx: broadcast::Receiver<sew_call_client::CallEvent>,
) {
    use sew_call_client::CallEvent as E;
    loop {
        let ev = rx.recv().await;
        let Some(sh) = shared.upgrade() else { return };
        match ev {
            Ok(E::State(s)) => {
                let state = map_state(&s);
                let ended = matches!(state, CallState::Ended { .. });
                let held = matches!(state, CallState::ConsentRequired { .. });
                sh.send(CallEvent::State(state));
                if ended {
                    sh.finish();
                    return;
                }
                if !held && !sh.start_audio_late().await {
                    return;
                }
            }
            Ok(E::Participants(p)) => {
                sh.send(CallEvent::Participants(map_participants(&p)));
                sh.update_recording(None, &p);
            }
            Ok(E::Recording(r)) => {
                let p = sh.engine_participants();
                sh.update_recording(Some(r.map(|b| *b)), &p);
            }
            // The engine's levels are the media server's; kessel-audio's are the plugin's.
            Ok(E::Speaking(_)) => {}
            Ok(E::Error(e)) => sh.send(CallEvent::Error(map_error(&e))),
            Ok(E::Moved(m)) => sh.send(CallEvent::Moved {
                room: m.room().to_owned(),
            }),
            // An event sew-call-client adds later.
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(_)) => {
                let snap = sh.snapshot();
                let ended = matches!(snap.state, CallState::Ended { .. });
                if let Some(engine) = sh.engine_snapshot() {
                    sh.update_recording(Some(engine.recording.clone()), &engine.participants);
                }
                sh.send(CallEvent::Participants(snap.participants));
                sh.send(CallEvent::State(snap.state));
                if ended {
                    sh.finish();
                    return;
                }
            }
            Err(broadcast::error::RecvError::Closed) => {
                drop(lock(&sh.tx).take());
                sh.finish();
                return;
            }
        }
    }
}

/// kessel-audio's side of the call: its device events and errors, and the `speaking` levels.
async fn audio(shared: Weak<CallShared>) {
    let mut tick = tokio::time::interval(SPEAKING_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last: Vec<Speaking> = Vec::new();
    loop {
        tick.tick().await;
        let Some(sh) = shared.upgrade() else { return };
        let Some((levels, events)) = sh.with_bridge(|b| {
            let rx = b.engine().events();
            let mut events = Vec::new();
            while let Some(e) = rx.try_recv() {
                events.push(e);
            }
            (b.levels(), events)
        }) else {
            // A held join has no audio yet; once started, no bridge means the call is over.
            if sh.audio_started.load(Ordering::Acquire) {
                return;
            }
            continue;
        };
        let engine = sh.engine.upgrade();
        for e in events {
            match e {
                AudioEvent::Devices(list) => {
                    if let Some(engine) = &engine {
                        engine.call_devices(list);
                    }
                }
                AudioEvent::DevicesInUse(in_use) => {
                    if let Some(engine) = &engine {
                        engine.call_devices_in_use(&in_use);
                    }
                }
                AudioEvent::Error(err) => sh.send(CallEvent::Error(audio_error(&err))),
                _ => {}
            }
        }
        let Some(snap) = sh.engine_snapshot() else {
            continue;
        };
        let now = speaking(&levels, &snap.participants);
        if now != last {
            last = now.clone();
            sh.send(CallEvent::Speaking(now));
        }
    }
}

/// -60 dBov and below reads as silence, 0 dBov as 1 (the engine's scale).
fn unit(level_dbov: f32) -> f64 {
    f64::from(((level_dbov + 60.0) / 60.0).clamp(0.0, 1.0))
}

/// Who is speaking now, by member: this device (unless muted) and each remote track that
/// kessel-audio hears above its speaking threshold.
pub(crate) fn speaking(
    levels: &BridgeLevels,
    participants: &[sew_call_client::Participant],
) -> Vec<Speaking> {
    let mut out = Vec::new();
    if levels.local.speaking
        && let Some(me) = participants.iter().find(|p| p.is_self)
        && !me.muted
        && let Some(member) = &me.member_id
    {
        out.push(Speaking {
            member_id: member.clone(),
            level: unit(levels.local.level_dbov),
        });
    }
    for (identity, level) in &levels.remotes {
        if !level.speaking {
            continue;
        }
        if let Some(member) = participants
            .iter()
            .find(|p| !p.is_self && &p.identity == identity)
            .and_then(|p| p.member_id.clone())
        {
            out.push(Speaking {
                member_id: member,
                level: unit(level.level_dbov),
            });
        }
    }
    out
}

pub(crate) fn map_state(s: &sew_call_client::CallState) -> CallState {
    use sew_call_client::CallState as S;
    match s {
        S::Connecting => CallState::Connecting,
        S::Connected => CallState::Connected,
        S::Reconnecting { attempt } => CallState::Reconnecting { attempt: *attempt },
        S::Ended { reason } => CallState::Ended {
            reason: end_reason(reason),
        },
        S::ConsentRequired {
            recording_id,
            notice_version,
        } => CallState::ConsentRequired {
            recording_id: recording_id.clone(),
            notice_version: *notice_version,
        },
        // A state sew-call-client adds later: the call is not up.
        _ => CallState::Connecting,
    }
}

/// The engine's end reasons as `ended.reason` (a refusal's code as given).
pub(crate) fn end_reason(r: &EndReason) -> String {
    match r {
        EndReason::Left => reason::LEFT.into(),
        EndReason::SigninRequired => reason::SIGNIN_REQUIRED.into(),
        EndReason::Network => "network".into(),
        EndReason::E2eeRefused => "e2ee_refused".into(),
        EndReason::CredentialLost => reason::CREDENTIAL_LOST.into(),
        EndReason::Kicked => reason::KICKED.into(),
        EndReason::RoomClosed => reason::ROOM_CLOSED.into(),
        EndReason::Refused(code) => code.clone(),
        // Sew's voice leave reason, unchanged (Sew 0.7.2).
        EndReason::Removed(why) => why.clone(),
        // A reason sew-call-client adds later.
        _ => reason::ENGINE_STOPPED.into(),
    }
}

/// The engine's participants as the plugin's: those the keys' roster or the call socket has
/// named a member for (the plugin's participants are members; a participant appears once its
/// member is known).
pub(crate) fn map_participants(p: &[sew_call_client::Participant]) -> Vec<Participant> {
    p.iter()
        .filter_map(|p| {
            Some(Participant {
                member_id: p.member_id.clone()?,
                is_self: p.is_self,
                muted: p.muted || p.deafened,
                deafened: p.deafened,
                server_muted: p.server_muted,
                server_deafened: p.server_deafened,
                speak_denied: p.speak_denied,
            })
        })
        .collect()
}

pub(crate) fn map_snapshot(s: &sew_call_client::CallSnapshot) -> CallSnapshot {
    CallSnapshot {
        state: map_state(&s.state),
        participants: map_participants(&s.participants),
        recording: map_recording(s.recording.as_ref(), &s.participants),
    }
}

/// The engine's recording as the `recording` event, with `recorder_present` from its roster.
pub(crate) fn map_recording(
    r: Option<&CallRecording>,
    participants: &[sew_call_client::Participant],
) -> Option<RecordingInfo> {
    r.map(|r| RecordingInfo::from_engine(r, participants.iter().any(|p| p.recorder)))
}

/// A failed `consent` or `withdraw_consent`.
fn consent_error(e: CallError) -> EngineError {
    match e {
        CallError::NoRecording => EngineError::failed(crate::wire::code::NO_RECORDING),
        CallError::Ended => EngineError::failed(crate::wire::code::NOT_IN_CALL),
        e => join_error(e),
    }
}

/// The engine's `{code, detail}`: an empty detail is `null`, a JSON object (a refusal's detail
/// as the provider gave it) is that object, anything else is the text.
pub(crate) fn map_error(e: &sew_call_client::CallErrorEvent) -> ErrorInfo {
    let detail = if e.detail.is_empty() {
        Value::Null
    } else {
        match serde_json::from_str::<Value>(&e.detail) {
            Ok(v @ Value::Object(_)) => v,
            _ => Value::String(e.detail.clone()),
        }
    };
    ErrorInfo {
        code: e.code.clone(),
        detail,
    }
}

/// A kessel-audio device error as the plugin's: `mic_denied` (the OS refused), `device_lost`
/// (unplugged; kessel-audio falls back to the default), `mic_unavailable` otherwise, with
/// kessel-audio's own code in the detail.
pub(crate) fn audio_error(e: &AudioError) -> ErrorInfo {
    let code = match e.code {
        ErrorCode::NoPermission => "mic_denied",
        ErrorCode::DeviceLost => "device_lost",
        _ => "mic_unavailable",
    };
    ErrorInfo::with_detail(
        code,
        json!({ "audio": e.code.as_str(), "detail": e.detail }),
    )
}

/// A failed join or command.
pub(crate) fn join_error(e: CallError) -> EngineError {
    match e {
        CallError::AlreadyInCall => EngineError::failed(crate::wire::code::ALREADY_IN_CALL),
        CallError::Provider(p) => EngineError::Provider(p),
        CallError::Ended => EngineError::failed(reason::ENGINE_STOPPED),
        CallError::Setup(s) => {
            EngineError::Failed(ErrorInfo::with_detail(reason::ENGINE_STOPPED, s))
        }
        // An error sew-call-client adds later.
        e => EngineError::Failed(ErrorInfo::with_detail(
            reason::ENGINE_STOPPED,
            e.to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    // sew-call-client's Participant and BridgeLevels are non_exhaustive: built from Default.
    #![allow(clippy::field_reassign_with_default)]
    use super::*;
    use sew_call_client::kessel_audio::Level;

    fn p(identity: &str, member: Option<&str>, is_self: bool) -> sew_call_client::Participant {
        let mut p = sew_call_client::Participant::default();
        p.identity = identity.into();
        p.member_id = member.map(str::to_owned);
        p.is_self = is_self;
        p
    }

    fn lvl(db: f32, speaking: bool) -> Level {
        Level {
            level_dbov: db,
            speaking,
        }
    }

    #[test]
    fn states_and_end_reasons_map_to_j4() {
        use sew_call_client::CallState as S;
        assert_eq!(map_state(&S::Connecting), CallState::Connecting);
        assert_eq!(map_state(&S::Connected), CallState::Connected);
        assert_eq!(
            map_state(&S::Reconnecting { attempt: 3 }),
            CallState::Reconnecting { attempt: 3 }
        );
        for (r, want) in [
            (EndReason::Left, "left"),
            (EndReason::SigninRequired, "signin_required"),
            (EndReason::Network, "network"),
            (EndReason::E2eeRefused, "e2ee_refused"),
            (EndReason::CredentialLost, "credential_lost"),
            (EndReason::Kicked, "kicked"),
            (EndReason::RoomClosed, "room_closed"),
            (EndReason::Refused("channel_full".into()), "channel_full"),
            (EndReason::Removed("perms_lost".into()), "perms_lost"),
            (
                EndReason::Removed("removed_from_space".into()),
                "removed_from_space",
            ),
            (EndReason::Removed("timed_out".into()), "timed_out"),
            (
                EndReason::Removed("channel_archived".into()),
                "channel_archived",
            ),
            (
                EndReason::Removed("account_removed".into()),
                "account_removed",
            ),
            (
                EndReason::Removed("removed_by_moderator".into()),
                "removed_by_moderator",
            ),
            (
                EndReason::Removed("session_revoked".into()),
                "session_revoked",
            ),
            (
                EndReason::Removed("device_revoked".into()),
                "device_revoked",
            ),
        ] {
            assert_eq!(
                map_state(&S::Ended { reason: r }),
                CallState::Ended {
                    reason: want.into()
                }
            );
        }
    }

    #[test]
    fn participants_are_members_and_deafened_is_muted() {
        let mut me = p("p_me", Some("1001"), true);
        me.deafened = true;
        let unnamed = p("p_new", None, false);
        let mut bob = p("p_bob", Some("1002"), false);
        bob.server_muted = true;
        bob.muted = true;
        let mut eve = p("p_eve", Some("1003"), false);
        eve.speak_denied = true;
        eve.muted = true;
        let out = map_participants(&[me, unnamed, bob, eve]);
        assert_eq!(
            out,
            vec![
                Participant {
                    member_id: "1001".into(),
                    is_self: true,
                    muted: true,
                    deafened: true,
                    server_muted: false,
                    server_deafened: false,
                    speak_denied: false,
                },
                Participant {
                    member_id: "1002".into(),
                    is_self: false,
                    muted: true,
                    deafened: false,
                    server_muted: true,
                    server_deafened: false,
                    speak_denied: false,
                },
                // SPEAK denied is its own flag, not a moderator mute.
                Participant {
                    member_id: "1003".into(),
                    is_self: false,
                    muted: true,
                    deafened: false,
                    server_muted: false,
                    server_deafened: false,
                    speak_denied: true,
                },
            ]
        );
    }

    #[test]
    fn speaking_comes_from_kessel_audio_levels_by_member() {
        let roster = vec![
            p("p_me", Some("1001"), true),
            p("p_bob", Some("1002"), false),
            p("p_eve", Some("1003"), false),
            p("p_new", None, false),
        ];
        let mut levels = BridgeLevels::default();
        levels.local = lvl(-20.0, true);
        levels.remotes = vec![
            ("p_bob".into(), lvl(-30.0, true)),
            ("p_eve".into(), lvl(-80.0, false)),
            ("p_new".into(), lvl(-10.0, true)),
        ];
        let out = speaking(&levels, &roster);
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(out[0].member_id, "1001");
        assert!((out[0].level - 40.0 / 60.0).abs() < 1e-6);
        assert_eq!(out[1].member_id, "1002");
        assert!((out[1].level - 0.5).abs() < 1e-6);
        // Muted, this device is never speaking.
        let mut muted = roster.clone();
        muted[0].muted = true;
        assert!(
            speaking(&levels, &muted)
                .iter()
                .all(|s| s.member_id != "1001")
        );
    }

    #[test]
    fn errors_keep_their_codes_and_a_refusal_keeps_its_object() {
        let e = map_error(&sew_call_client::CallErrorEvent::new(
            "turn_unreachable",
            "tls: timed out",
        ));
        assert_eq!(e.code, "turn_unreachable");
        assert_eq!(e.detail, json!("tls: timed out"));
        let e = map_error(&sew_call_client::CallErrorEvent::new(
            "channel_full",
            r#"{"limit":25}"#,
        ));
        assert_eq!(e.detail, json!({"limit": 25}));
        let e = map_error(&sew_call_client::CallErrorEvent::new(
            "key_storage_file",
            String::new(),
        ));
        assert_eq!(e.detail, Value::Null);
    }

    /// An engine whose lister counts its calls and lists `in:1` (the null audio otherwise).
    fn counted() -> (CallClientEngine, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::AtomicUsize;
        let n = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&n);
        let lister: DeviceLister = Arc::new(move || {
            seen.fetch_add(1, Ordering::SeqCst);
            let mut d = kessel_audio::DeviceList::default();
            d.inputs.push(kessel_audio::DeviceInfo {
                id: "in:1".into(),
                name: "Mic".into(),
                is_default: true,
            });
            d
        });
        let dir = std::env::temp_dir().join("sew-voice-real-unit");
        let client = CallClient::with_mls(sew_call_client::ClientConfig::new("sew.test", dir));
        let audio: AudioConfig = Arc::new(|_, _| EngineConfig::default());
        (CallClientEngine::new(client, audio).devices_from(lister), n)
    }

    fn wait_for(what: &str, mut pred: impl FnMut() -> bool) {
        for _ in 0..500 {
            if pred() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for: {what}");
    }

    /// Nothing is listed at launch; the first device query lists once; the poll runs only
    /// while something is subscribed.
    #[test]
    fn devices_are_listed_from_the_first_use_and_polled_while_subscribed() {
        let (engine, n) = counted();
        std::thread::sleep(idle_device_poll() * 5);
        assert_eq!(
            n.load(Ordering::SeqCst),
            0,
            "no listing before the first use"
        );
        let list = engine.devices().unwrap();
        assert_eq!(list.inputs.len(), 1);
        let after_first = n.load(Ordering::SeqCst);
        assert!(after_first >= 1);
        // Nobody subscribed: no poll.
        std::thread::sleep(idle_device_poll() * 5);
        assert_eq!(
            n.load(Ordering::SeqCst),
            after_first,
            "no poll without a subscriber"
        );
        // Subscribed: polled.
        let rx = engine.device_events();
        wait_for("a poll", || n.load(Ordering::SeqCst) >= after_first + 2);
        drop(rx);
        std::thread::sleep(idle_device_poll() * 3);
        let stopped = n.load(Ordering::SeqCst);
        std::thread::sleep(idle_device_poll() * 5);
        assert_eq!(
            n.load(Ordering::SeqCst),
            stopped,
            "the poll stops with the subscriber"
        );
    }

    /// Subscribing to the device events is a first use too, and so is `set_device`.
    #[test]
    fn a_subscription_or_set_device_is_a_first_use() {
        let (engine, n) = counted();
        let _rx = engine.device_events();
        wait_for("listed", || n.load(Ordering::SeqCst) >= 1);

        let (engine, n) = counted();
        // `in:1` is known at once (listed on this first use), not refused.
        engine
            .set_device(Some(Choice::Id("in:1".into())), None)
            .unwrap();
        assert!(n.load(Ordering::SeqCst) >= 1);
        assert_eq!(engine.devices().unwrap().input.as_deref(), Some("in:1"));
    }

    /// `set_device` with the OS default (no call): the choice is `None` again, and no
    /// device check applies to it.
    #[test]
    fn set_device_default_between_calls() {
        let (engine, _) = counted();
        engine
            .set_device(Some(Choice::Id("in:1".into())), None)
            .unwrap();
        engine
            .set_device(Some(Choice::Default), Some(Choice::Default))
            .unwrap();
        let list = engine.devices().unwrap();
        assert_eq!((list.input, list.output), (None, None));
        assert_eq!(audio_choice(&Choice::Default), DeviceChoice::Default);
        assert_eq!(
            audio_choice(&Choice::Id("x".into())),
            DeviceChoice::Id("x".into())
        );
        // An unknown id is still refused.
        let e = engine
            .set_device(Some(Choice::Id("in:nope".into())), None)
            .unwrap_err();
        assert_eq!(e.to_info().code, "device_not_found");
    }

    #[test]
    fn join_errors_map_to_plugin_errors() {
        assert_eq!(
            join_error(CallError::AlreadyInCall).to_info().code,
            "already_in_call"
        );
        assert_eq!(
            join_error(CallError::Provider(ProviderError::SigninRequired)).end_reason(),
            "signin_required"
        );
        assert_eq!(
            join_error(CallError::Setup("no roots".into())).to_info(),
            ErrorInfo::with_detail("engine_stopped", "no roots")
        );
    }
}
