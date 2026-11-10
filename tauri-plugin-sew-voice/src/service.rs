//! The plugin's core logic, without Tauri: the eight commands, the state machine and the event
//! pumps. `plugin.rs` wires it to Tauri; tests drive it directly with the fake engine.
//!
//! States: `idle` → `connecting` (join) → `connected` ⇄ `reconnecting {attempt}` → `ended
//! {reason}`; `ended` → `connecting` on the next join. One call per app. A join into a recorded
//! room (Sew 0.7.3) goes `connecting` → `consent_required` (held: no media) until `consent`
//! (→ `connecting` → `connected`) or `leave`; a re-join that Sew refuses for the notice holds
//! the same way. The `recording` event follows the call's recording.
//!
//! Devices are the engine's from the first use on: the first `devices`, `set_device` or
//! `join` subscribes to the engine's device events (and so starts its device listing), not
//! [`VoiceService::start`]. Cameras likewise: the first `video_devices`, `set_video_device`,
//! `camera` or `preview` subscribes to the engine's camera events.
//!
//! **Video.** The call's video tracks (`video`), this device's camera and screen
//! (`local_video`) and the room's media policy (`policy`) follow the engine's events, under the
//! same lock and in the same order as the voice events. Frames take another path: one task per
//! call (and one for the pre-join preview) reads the engine's frame channel and routes each
//! frame to the open sinks (`crate::sink`), under the sinks' own lock, never the state lock.
//! The sinks' union per track goes back to the engine as [`TrackSettings`].
//!
//! **Phones.** The app's scene ([`VoiceService::app_scene`]) and the video backend's own
//! background signal ([`VoiceEngine::video_background_events`]) decide together: either one
//! in the background hides every webview sink (their tracks go disabled at the media server)
//! while native sinks (`crate::native`, the picture in picture) stay enabled; back in the
//! foreground (both say so) the visible sinks wait for, and ask for, a keyframe. The listener's
//! `video_changed` follows whether the call has any video (local or remote), for the app's
//! speaker route.
//!
//! **Decoded fallback.** A sink whose webview can't decode the track's codec (its `decode`
//! probe) gets the engine's decoder instead ([`VoiceEngine::video_decoder`], for a codec the
//! engine lists in `decode_codecs`): at most [`MAX_DECODED_SINKS`] at once, each with its own
//! drain task that sends the decoder's outputs as they come (the decoder's wake).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use sew_call_client::device::DeviceKeyId;
use tokio::sync::{Notify, broadcast};
use tokio::time::Instant;

use crate::details::{
    CallDetails, IncomingCall, QualityInfo, RingEndedArgs, RosterEntry, SecurityInfo,
};
use serde_json::Value;

use crate::engine::{
    AppScene, AudioSessionEvent, BoxFuture, CallEvent, CallHandle, CallSnapshot, JoinProvider,
    JoinRequest, VideoSnapshot, VoiceEngine,
};
use crate::listener::VoiceListener;
use crate::sink::{Decoded, DecoderOpener, FrameChannel, OpenSink, Routed, SinkChange, Sinks};
use crate::video::{
    CameraRequest, DecodedFormat, DecoderSpec, DecoderWake, FrameDecoder, LOCAL_CAMERA,
    MAX_DECODED_SINKS, ParentWindow, PreviewRequest, ScreenRequest, TrackSettings, VideoCodec,
    VideoFrames, decoded_fit, is_local_track,
};
use crate::wire::{
    CallState, CameraArgs, CodecPreferenceInfo, ConsentArgs, DeviceChoice, DeviceList, ErrorInfo,
    JoinArgs, JoinOptions, JoinResult, LocalVideo, LocalVideoState, MuteArgs, Participant,
    PermissionState, PluginEvent, PreviewAnswer, PreviewArgs, RecordingInfo, ScreenArgs,
    ScreenSource, SelfMute, SetDeviceArgs, SetVideoCodecPreferenceArgs, SetVideoDeviceArgs,
    SinkIdArgs, SinkMode, Speaking, StateEvent, StateSnapshot, VideoCaps, VideoDevices,
    VideoSinkAnswer, VideoSinkArgs, VideoSinkUpdateArgs, VideoTrack, code, reason, video_code,
};

/// The `speaking` event's floor: at most 10 a second.
pub const SPEAKING_MIN_INTERVAL: Duration = Duration::from_millis(100);

/// The level (0 to 1) a participant must reach to become the active speaker.
pub const ACTIVE_SPEAKER_LEVEL: f64 = 0.1;
/// The active speaker holds at least this long before another one takes over.
pub const ACTIVE_SPEAKER_HOLD: Duration = Duration::from_millis(1500);

/// Where the plugin's events go (Tauri's emitter in the app, a recorder in tests).
///
/// `emit` is called with the plugin's state lock held, so events keep their order; it must not
/// block or call back into the [`VoiceService`]. The native [`VoiceListener`], when there is
/// one, is called right after each `emit`, under the same lock.
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
    /// The native listener (`Builder::listener`), called after the sink under the lock.
    listener: Option<Arc<dyn VoiceListener>>,
    spawn: Spawn,
    slot: Mutex<Slot>,
    /// [`VoiceService::start`] was called: forward device events from the first use on.
    devices_wanted: AtomicBool,
    /// The device events are forwarded (the first use came).
    devices_on: AtomicBool,
    /// The camera events are forwarded (the first use of video came).
    video_devices_on: AtomicBool,
    /// The frame sinks and the preview. Never locked while `slot` is held.
    video: Mutex<VideoSlot>,
    /// The app's and the video backend's background flags (`scene` before `video`, never
    /// while `slot` is held).
    scene: Mutex<Scene>,
    /// The video backend's background events are forwarded (the first sink or scene came).
    scene_events_on: AtomicBool,
}

/// Background as the app says it and as the video backend says it.
#[derive(Default)]
struct Scene {
    app: bool,
    backend: bool,
}

impl Scene {
    fn background(&self) -> bool {
        self.app || self.backend
    }
}

/// The frame side: the sinks and the preview's frame task.
#[derive(Default)]
struct VideoSlot {
    sinks: Sinks,
    /// Stops the preview's frame task.
    preview_stop: Option<Arc<Notify>>,
}

struct Slot {
    state: CallState,
    /// The call's `target` and `room` (`join`'s, the room a move's), from `connecting` until
    /// the next join.
    target: Option<String>,
    room: Option<String>,
    participants: Vec<Participant>,
    /// The call's active recording (Sew 0.7.3).
    recording: Option<RecordingInfo>,
    call: Option<Arc<dyn CallHandle>>,
    call_id: Option<String>,
    /// Bumped by every join and leave; a task or answer from an older generation is ignored.
    generation: u64,
    /// Stops the current call's pump.
    stop: Option<Arc<Notify>>,
    /// This device's own mute choice (the join's `muted`, then each mute command's): the
    /// listener's `self_muted` while a deafen, a moderator mute or SPEAK lost hides it in the
    /// row; resynced from the row whenever nothing does.
    own_muted: bool,
    /// The last `on_self_mute` the listener got (`None`: no own row, so none yet).
    self_mute: Option<SelfMute>,
    /// Call details (0.4.1): the key state and code, the media roster, the engine's last
    /// quality and the quality shown (`lost` while reconnecting), and the ring.
    details: Details,
    /// The call's video tracks (the `video` event).
    video: Vec<VideoTrack>,
    /// This device's camera and screen (the `local_video` event).
    local_video: LocalVideo,
    /// The room's media policy (the `policy` event).
    policy: Option<Value>,
    /// Stops the current call's frame task.
    video_stop: Option<Arc<Notify>>,
    /// The call has video, local or remote (the listener's `video_changed`).
    has_video: bool,
    /// The active speaker (a remote member id) and since when.
    speaker: Option<(String, Instant)>,
    /// What the active-speaker sinks show: a track (`""`: none) and its member.
    follow: (String, Option<String>),
    /// A join's camera ([`JoinOptions`]), until the call is connected and the app in front.
    pending_camera: Option<CameraRequest>,
}

#[derive(Default)]
struct Details {
    security: Option<SecurityInfo>,
    roster: Vec<RosterEntry>,
    engine_quality: QualityInfo,
    quality: QualityInfo,
    incoming: Option<IncomingCall>,
}

impl VoiceService {
    /// A service over `engine`, joining through `provider`, emitting to `sink`.
    pub fn new(
        engine: Arc<dyn VoiceEngine>,
        provider: Arc<dyn JoinProvider>,
        sink: Arc<dyn EventSink>,
        spawn: Spawn,
    ) -> VoiceService {
        VoiceService::build(engine, provider, sink, None, spawn)
    }

    /// As [`VoiceService::new`], with a native [`VoiceListener`] called after each event,
    /// under the same lock and in the same order.
    pub fn with_listener(
        engine: Arc<dyn VoiceEngine>,
        provider: Arc<dyn JoinProvider>,
        sink: Arc<dyn EventSink>,
        listener: Arc<dyn VoiceListener>,
        spawn: Spawn,
    ) -> VoiceService {
        VoiceService::build(engine, provider, sink, Some(listener), spawn)
    }

    fn build(
        engine: Arc<dyn VoiceEngine>,
        provider: Arc<dyn JoinProvider>,
        sink: Arc<dyn EventSink>,
        listener: Option<Arc<dyn VoiceListener>>,
        spawn: Spawn,
    ) -> VoiceService {
        VoiceService {
            inner: Arc::new(Inner {
                engine,
                provider,
                sink,
                listener,
                spawn,
                slot: Mutex::new(Slot {
                    state: CallState::Idle,
                    target: None,
                    room: None,
                    participants: Vec::new(),
                    recording: None,
                    call: None,
                    call_id: None,
                    generation: 0,
                    stop: None,
                    own_muted: false,
                    self_mute: None,
                    details: Details::default(),
                    video: Vec::new(),
                    local_video: LocalVideo::default(),
                    policy: None,
                    video_stop: None,
                    has_video: false,
                    speaker: None,
                    follow: (String::new(), None),
                    pending_camera: None,
                }),
                devices_wanted: AtomicBool::new(false),
                devices_on: AtomicBool::new(false),
                video_devices_on: AtomicBool::new(false),
                video: Mutex::new(VideoSlot::default()),
                scene: Mutex::new(Scene::default()),
                scene_events_on: AtomicBool::new(false),
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
                let mut slot = lock(&inner.slot);
                notify(&inner, &mut slot, &PluginEvent::Devices(list));
            }
        }));
    }

    fn lock(&self) -> MutexGuard<'_, Slot> {
        lock(&self.inner.slot)
    }

    /// `join {target, room, muted?, deafened?, decode?}` → `{call_id}`. `decode` (the
    /// `join` command's, here [`JoinOptions::decode`] through [`VoiceService::join_with`]):
    /// the video codecs the webview decodes, from its WebCodecs probe.
    pub async fn join(&self, args: JoinArgs) -> Result<JoinResult, ErrorInfo> {
        self.join_with(args, JoinOptions::default()).await
    }

    /// `join` with the native options ([`JoinOptions`]): with `camera`, the camera turns on as
    /// soon as the call is connected and the app is in front (never from the lock screen), as
    /// `camera {on, device}` would, under the same rules (the OS permission is never asked
    /// for). Its refusals come back as `local_video {camera: "off", reason: <code>}`; the
    /// call itself still joins.
    pub async fn join_with(
        &self,
        args: JoinArgs,
        opts: JoinOptions,
    ) -> Result<JoinResult, ErrorInfo> {
        self.use_devices();
        let deafened = args.deafened.unwrap_or(false);
        let own_muted = args.muted.unwrap_or(false);
        let req = JoinRequest {
            target: args.target,
            room: args.room,
            muted: args.muted.unwrap_or(false) || deafened,
            deafened,
            rejoin: None,
            // Answered later, with `consent`, when Sew holds the join for its notice.
            recording_notice: None,
        };
        let generation = {
            let mut slot = self.lock();
            if slot.state.is_live() {
                return Err(ErrorInfo::new(code::ALREADY_IN_CALL));
            }
            clear_video(&self.inner, &mut slot);
            slot.generation += 1;
            slot.call = None;
            slot.call_id = None;
            slot.target = Some(req.target.clone());
            slot.room = Some(req.room.clone());
            slot.own_muted = own_muted;
            set_participants(&self.inner, &mut slot, Vec::new());
            set_recording(&self.inner, &mut slot, None);
            clear_details(&self.inner, &mut slot);
            // A join with a ring's target and room accepts it.
            if slot
                .details
                .incoming
                .as_ref()
                .is_some_and(|i| i.target == req.target && i.room == req.room)
            {
                set_incoming(&self.inner, &mut slot, None);
            }
            self.set_state(&mut slot, CallState::Connecting);
            slot.generation
        };

        let mut joined = self
            .inner
            .engine
            .join_decoding(
                Arc::clone(&self.inner.provider),
                req.clone(),
                opts.decode.clone(),
            )
            .await;
        if joined
            .as_ref()
            .is_err_and(|e| e.to_info().code == code::ALREADY_IN_CALL)
        {
            // The service has no live call, so the engine's is one the plugin lost hold of (a
            // leave that could not take it): released, then the join once more.
            tracing::warn!("sew-voice: the engine still had a call; released, joining again");
            self.inner.engine.release_calls().await;
            joined = self
                .inner
                .engine
                .join_decoding(Arc::clone(&self.inner.provider), req, opts.decode.clone())
                .await;
        }

        let mut slot = self.lock();
        if slot.generation != generation {
            // A leave came while we were joining: hang the new call up.
            drop(slot);
            if let Ok(call) = joined {
                let inner = Arc::clone(&self.inner);
                (self.inner.spawn)(Box::pin(async move {
                    if let Err(e) = call.leave().await {
                        let mut slot = lock(&inner.slot);
                        notify(&inner, &mut slot, &PluginEvent::Errors(e.to_info()));
                    }
                }));
            }
            return Err(ErrorInfo::new(code::LEFT));
        }
        let call = match joined {
            Ok(call) => call,
            Err(e) => {
                let info = e.to_info();
                notify(&self.inner, &mut slot, &PluginEvent::Errors(info.clone()));
                self.set_state(
                    &mut slot,
                    CallState::Ended {
                        reason: e.end_reason(),
                    },
                );
                return Err(info);
            }
        };
        // Empty while the join is held for the recording notice (the room comes with Sew's
        // answer; the `state` command's `call_id` follows).
        let call_id = call.call_id();
        // Subscribe before the snapshot, so nothing between them is lost.
        let rx = call.events();
        let stop = Arc::new(Notify::new());
        slot.call = Some(Arc::clone(&call));
        slot.call_id = (!call_id.is_empty()).then(|| call_id.clone());
        slot.stop = Some(Arc::clone(&stop));
        let ended = self.apply_snapshot(&mut slot, call.snapshot());
        if !ended {
            apply_details(&self.inner, &mut slot, call.as_ref());
            if opts.camera {
                slot.pending_camera = Some(CameraRequest {
                    on: true,
                    device: opts.camera_device.as_deref().map(DeviceChoice::from_wire),
                });
            }
        }
        let frames = if ended {
            None
        } else {
            apply_video(&self.inner, &mut slot, call.video_snapshot());
            call.video_frames().map(|rx| {
                let stop = Arc::new(Notify::new());
                slot.video_stop = Some(Arc::clone(&stop));
                (rx, stop)
            })
        };
        drop(slot);
        if let Some((rx, stop)) = frames {
            // The new call's engine starts with every track disabled.
            lock(&self.inner.video).sinks.forget_pushed();
            let pump = FramePump {
                inner: Arc::clone(&self.inner),
                rx,
                stop,
                call: Some(Arc::clone(&call)),
            };
            (self.inner.spawn)(Box::pin(pump.run()));
        }
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
        if opts.camera {
            kick_pending_camera(&self.inner);
        }
        Ok(JoinResult { call_id })
    }

    /// `leave`. With no call it releases any call the engine still holds that the plugin lost
    /// hold of ([`VoiceEngine::release_calls`]), and nothing more. The state is
    /// `ended {reason: "left"}` at once; if Sew's leave then fails, an `errors` event says so
    /// (the call is gone either way).
    pub async fn leave(&self) -> Result<(), ErrorInfo> {
        if !self.lock().state.is_live() {
            // No call here, but the engine may still hold one the plugin lost hold of: a leave
            // releases it too, so the next join is never `already_in_call`.
            self.inner.engine.release_calls().await;
            return Ok(());
        }
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
            set_participants(&self.inner, &mut slot, Vec::new());
            set_recording(&self.inner, &mut slot, None);
            clear_details(&self.inner, &mut slot);
            clear_video(&self.inner, &mut slot);
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
            let mut slot = self.lock();
            notify(&self.inner, &mut slot, &PluginEvent::Errors(e.to_info()));
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
        let (call, generation, before) = {
            let mut slot = self.lock();
            let call = slot
                .call
                .clone()
                .ok_or_else(|| ErrorInfo::new(code::NOT_IN_CALL))?;
            let before = slot.own_muted;
            // The choice first, so a listener's `self_muted` follows it even when the row
            // cannot show it (deafened, a moderator mute, SPEAK lost).
            if let Some(m) = args.muted {
                slot.own_muted = m;
                self_mute_changed(&self.inner, &mut slot, false);
            }
            (call, slot.generation, before)
        };
        let r = call.mute(args.muted, args.deafened).await;
        if r.is_err() && args.muted.is_some() {
            let mut slot = self.lock();
            if slot.generation == generation {
                slot.own_muted = before;
                self_mute_changed(&self.inner, &mut slot, false);
            }
        }
        r.map_err(|e| e.to_info())
    }

    /// `consent {granted, notice_version}`: the person's answer to the recording notice (Sew
    /// 0.7.3). Held at `consent_required`, the call joins with it; in a call, the host's
    /// provider sends it with the person's session. This device is recorded only once Sew says
    /// `granted` (the `recording` event). Errors: `not_in_call`, `no_recording`,
    /// `recording_consent_required` (the notice changed: `detail` has the new one), the
    /// provider's refusals.
    pub async fn consent(&self, args: ConsentArgs) -> Result<(), ErrorInfo> {
        let call = self.live_call()?;
        call.consent(args.granted, args.notice_version)
            .await
            .map_err(|e| e.to_info())
    }

    /// `withdraw_consent`: "Stop recording me" (Sew stops capturing this person and the keys
    /// rekey, so the recorder gets nothing later). Before an answer it is a decline.
    pub async fn withdraw_consent(&self) -> Result<(), ErrorInfo> {
        let call = self.live_call()?;
        call.withdraw_consent().await.map_err(|e| e.to_info())
    }

    fn live_call(&self) -> Result<Arc<dyn CallHandle>, ErrorInfo> {
        self.lock()
            .call
            .clone()
            .ok_or_else(|| ErrorInfo::new(code::NOT_IN_CALL))
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

    /// The device's network changed: the live call's engine resumes now (a phone's
    /// `NWPathMonitor` or `ConnectivityManager`, through [`crate::VoiceControl`]). With no
    /// call it does nothing.
    pub fn network_changed(&self) {
        let call = self.lock().call.clone();
        if let Some(call) = call {
            call.network_changed();
        }
    }

    /// The phone's audio session changed (CallKit or Telecom, through
    /// [`crate::VoiceControl`]): passed to the engine (and so kessel-audio) unchanged, with
    /// or without a call.
    pub fn audio_session(&self, event: AudioSessionEvent) {
        self.inner.engine.audio_session(event);
    }

    /// The host's sign-out from one Sew install (also the `forget_server {install_id}`
    /// command, behind its own permission): the engine deletes every device of `install_id`
    /// its device store holds (a hardware key through `HardwareDeviceKey::delete`) and the
    /// install's pins, and answers the removed keys. The next join there enrols a new device.
    /// Refused with `in_call` while a call is live (connecting, held, connected or
    /// reconnecting); a store failure is `device_store`.
    pub async fn forget_server(&self, install_id: u64) -> Result<Vec<DeviceKeyId>, ErrorInfo> {
        if self.lock().state.is_live() {
            return Err(ErrorInfo::new(code::IN_CALL));
        }
        self.inner
            .engine
            .forget_server(install_id)
            .await
            .map_err(|e| e.to_info())
    }

    /// Every device key the engine's device store holds, sorted (also the
    /// `known_device_keys` command, behind its own permission).
    pub async fn known_device_keys(&self) -> Result<Vec<DeviceKeyId>, ErrorInfo> {
        self.inner
            .engine
            .known_device_keys()
            .await
            .map_err(|e| e.to_info())
    }

    /// `call_details` (0.4.1): the key state and security code, the quality, the media
    /// roster and the ring now, for a window opened mid-call.
    pub fn details(&self) -> CallDetails {
        let slot = self.lock();
        CallDetails {
            security: slot.details.security.clone(),
            quality: slot.details.quality.clone(),
            roster: slot.details.roster.clone(),
            incoming: slot.details.incoming.clone(),
        }
    }

    /// `ring {call_id, target, room, started_by?, silent?, reason?}` (0.4.1): a DM call rings
    /// (the page saw Sew's `call_ring`, or the host a push). Every window gets `incoming`, and
    /// the native listener `on_incoming`. A newer ring replaces the one before; a `join` with
    /// its `target` and `room` accepts it. `invalid_args` without a `call_id`, `target` or
    /// `room`.
    pub fn ring(&self, ring: IncomingCall) -> Result<(), ErrorInfo> {
        if ring.call_id.is_empty() || ring.target.is_empty() || ring.room.is_empty() {
            return Err(ErrorInfo::with_detail(
                code::INVALID_ARGS,
                "ring needs call_id, target and room",
            ));
        }
        let mut slot = self.lock();
        set_incoming(&self.inner, &mut slot, Some(ring));
        Ok(())
    }

    /// `ring_ended {call_id}` (0.4.1): the ring stopped (Sew's ring ended, the call ended, or
    /// the person declined: the decline itself is the caller's request to Sew). Another
    /// call's id, or no ring, changes nothing.
    pub fn ring_ended(&self, args: RingEndedArgs) -> Result<(), ErrorInfo> {
        let mut slot = self.lock();
        if slot
            .details
            .incoming
            .as_ref()
            .is_some_and(|i| i.call_id == args.call_id)
        {
            set_incoming(&self.inner, &mut slot, None);
        }
        Ok(())
    }

    /// The ring now (0.4.1), if a DM call is ringing.
    pub fn incoming(&self) -> Option<IncomingCall> {
        self.lock().details.incoming.clone()
    }

    // ───────────────────────────── video ─────────────────────────────

    /// The first use of video: subscribe to the engine's camera events (once, after
    /// [`VoiceService::start`]).
    fn use_video_devices(&self) {
        if !self.inner.devices_wanted.load(Ordering::Acquire)
            || self.inner.video_devices_on.swap(true, Ordering::AcqRel)
        {
            return;
        }
        let Some(mut rx) = self.inner.engine.video_device_events() else {
            return;
        };
        let inner = Arc::downgrade(&self.inner);
        (self.inner.spawn)(Box::pin(async move {
            loop {
                let list = match rx.recv().await {
                    Ok(list) => list,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return,
                };
                let Some(inner) = inner.upgrade() else { return };
                let mut slot = lock(&inner.slot);
                notify(&inner, &mut slot, &PluginEvent::VideoDevices(list));
            }
        }));
    }

    /// `video_caps` → what this device's video can do.
    pub async fn video_caps(&self) -> Result<VideoCaps, ErrorInfo> {
        Ok(self.inner.engine.video_caps())
    }

    /// `video_caps.codec_preference` → the video codec setting and what it gives here now;
    /// `None` for an engine without video publish.
    pub async fn video_codec_preference(&self) -> Result<Option<CodecPreferenceInfo>, ErrorInfo> {
        Ok(self.inner.engine.video_codec_preference())
    }

    /// `set_video_codec_preference {preference}`: this device's video codec setting, for a
    /// camera or screen turned on after it (the live call's too) and every later call;
    /// `prefer_vp8` also stops this device receiving H.264 from its next join. It answers the
    /// readback (`video_caps.codec_preference`). The engine keeps it until the process ends;
    /// persisting it is the host's job. An engine without video: `video_unsupported`.
    pub async fn set_video_codec_preference(
        &self,
        args: SetVideoCodecPreferenceArgs,
    ) -> Result<CodecPreferenceInfo, ErrorInfo> {
        tracing::info!(
            preference = args.preference.as_str(),
            "sew-voice: video codec preference"
        );
        self.inner
            .engine
            .set_video_codec_preference(args.preference)
            .map_err(|e| e.to_info())?;
        self.inner
            .engine
            .video_codec_preference()
            .ok_or_else(|| ErrorInfo::new(video_code::VIDEO_UNSUPPORTED))
    }

    /// `video_devices` → the cameras, the chosen one and the OS permissions.
    pub async fn video_devices(&self) -> Result<VideoDevices, ErrorInfo> {
        self.use_video_devices();
        self.inner.engine.video_devices().map_err(|e| e.to_info())
    }

    /// `set_video_device {camera}`: a camera id or `"default"`. Switches a live camera without
    /// a republish; kept for the next call when none is live.
    pub async fn set_video_device(&self, args: SetVideoDeviceArgs) -> Result<(), ErrorInfo> {
        if args.camera.is_empty() {
            return Err(ErrorInfo::with_detail(
                code::INVALID_ARGS,
                "set_video_device needs camera, an id or \"default\"",
            ));
        }
        self.use_video_devices();
        self.inner
            .engine
            .set_video_device(DeviceChoice::from_wire(&args.camera))
            .map_err(|e| e.to_info())
    }

    /// `camera {on, device?}` on the live call: answers once publishing (or off). The plugin
    /// never prompts on its own: without the OS permission `camera {on}` is refused with
    /// `camera_permission_denied` or `camera_permission_not_determined` (the app's camera
    /// button calls `request_camera_permission` first). `camera {off}` also drops a join's
    /// camera that hasn't started yet.
    pub async fn camera(&self, args: CameraArgs) -> Result<(), ErrorInfo> {
        self.camera_from(args, None).await
    }

    /// [`VoiceService::camera`] with, for an off, `local_video`'s reason (`None`:
    /// `unpublished`, the webview's own button).
    pub(crate) async fn camera_from(
        &self,
        args: CameraArgs,
        off_reason: Option<&'static str>,
    ) -> Result<(), ErrorInfo> {
        tracing::info!(
            on = args.on,
            by = off_reason.unwrap_or("webview"),
            "sew-voice: camera"
        );
        self.use_video_devices();
        if !args.on {
            self.lock().pending_camera = None;
        } else {
            camera_gate(self.inner.engine.as_ref())?;
        }
        let call = self.live_call()?;
        if let (false, Some(reason)) = (args.on, off_reason) {
            return call
                .camera_off_because(reason)
                .await
                .map_err(|e| e.to_info());
        }
        call.camera(CameraRequest {
            on: args.on,
            device: args.device.as_deref().map(DeviceChoice::from_wire),
        })
        .await
        .map_err(|e| e.to_info())
    }

    /// `screen {on, source?}` on the live call. `parent` is the app window the OS picker
    /// belongs to (Windows). Without screen capture here, `screen_unsupported`.
    pub async fn screen(
        &self,
        args: ScreenArgs,
        parent: Option<ParentWindow>,
    ) -> Result<(), ErrorInfo> {
        if args.on && !self.inner.engine.video_caps().screen.supported {
            return Err(ErrorInfo::new(video_code::SCREEN_UNSUPPORTED));
        }
        let call = self.live_call()?;
        call.screen(ScreenRequest {
            on: args.on,
            source: args.source,
            parent_window: parent,
        })
        .await
        .map_err(|e| e.to_info())
    }

    /// The iPhone screen share's ring in `container` (the App Group container), armed on the
    /// live call ([`crate::VoiceControl::screen_share_ring`]).
    pub async fn screen_share_ring(&self, container: std::path::PathBuf) -> Result<(), ErrorInfo> {
        let call = self.live_call()?;
        call.screen_share_ring(container)
            .await
            .map_err(|e| e.to_info())
    }

    /// `screen_sources` → the displays and windows (`[]` where only the OS picker exists).
    pub async fn screen_sources(&self) -> Result<Vec<ScreenSource>, ErrorInfo> {
        self.inner
            .engine
            .screen_sources()
            .await
            .map_err(|e| e.to_info())
    }

    /// `preview {on, device?}` → `{sink_track: "local:camera"}`: the camera outside a call
    /// (opened only by this command). A second `preview {on: true}` replaces the first.
    pub async fn preview(&self, args: PreviewArgs) -> Result<PreviewAnswer, ErrorInfo> {
        self.use_video_devices();
        if args.on {
            camera_gate(self.inner.engine.as_ref())?;
        }
        let frames = self
            .inner
            .engine
            .preview(PreviewRequest {
                on: args.on,
                device: args.device.as_deref().map(DeviceChoice::from_wire),
            })
            .await
            .map_err(|e| e.to_info())?;
        let stop = {
            let mut v = lock(&self.inner.video);
            if let Some(old) = v.preview_stop.take() {
                old.notify_one();
            }
            frames.map(|rx| {
                let stop = Arc::new(Notify::new());
                v.preview_stop = Some(Arc::clone(&stop));
                (rx, stop)
            })
        };
        if let Some((rx, stop)) = stop {
            let pump = FramePump {
                inner: Arc::clone(&self.inner),
                rx,
                stop,
                call: None,
            };
            (self.inner.spawn)(Box::pin(pump.run()));
        }
        Ok(PreviewAnswer {
            sink_track: LOCAL_CAMERA.into(),
        })
    }

    /// `video_sink {track, channel, width, height, visible, tile, decode}` →
    /// `{sink_id, codec, mode}`. The sink sends nothing until the track's next keyframe (it
    /// asks for one when visible). A remote track needs a live call and must be in the `video`
    /// list (`not_in_call`, `unknown_track`); `local:camera` and `local:screen` need neither.
    /// When the webview can't decode the track's codec, the sink is in `decoded` mode: the
    /// engine decodes it (a codec its `decode_codecs` lists) and the sink gets I420 frames, or
    /// RGBA when the webview has no WebCodecs at all (an empty `decode`). A self-view sink
    /// with an empty `decode` is decoded too. Refusals: `decoded_unsupported` (the engine
    /// can't decode it either), `decoded_tiles_full` (already [`MAX_DECODED_SINKS`] decoded
    /// sinks: the tile shows "video paused (this device)").
    pub async fn video_sink(
        &self,
        args: VideoSinkArgs,
        channel: Arc<dyn FrameChannel>,
    ) -> Result<VideoSinkAnswer, ErrorInfo> {
        self.use_scene_events();
        let decode: Vec<VideoCodec> = args
            .decode
            .iter()
            .filter_map(|c| VideoCodec::from_wire(c))
            .collect();
        let (call, codec) = {
            let slot = self.lock();
            if is_local_track(&args.track) {
                (slot.call.clone(), None)
            } else {
                let call = slot
                    .call
                    .clone()
                    .ok_or_else(|| ErrorInfo::new(code::NOT_IN_CALL))?;
                let t = slot
                    .video
                    .iter()
                    .find(|t| t.track == args.track)
                    .ok_or_else(|| {
                        ErrorInfo::with_detail(
                            video_code::UNKNOWN_TRACK,
                            serde_json::json!({ "track": args.track }),
                        )
                    })?;
                (Some(call), Some(t.codec))
            }
        };
        // Decoded mode: a remote track whose codec the webview can't decode, or a self-view
        // sink of a webview without WebCodecs.
        let decoded_codec = match codec {
            Some(c) if !decode.contains(&c) => Some(Some(c)),
            None if decode.is_empty()
                && !self.inner.engine.video_caps().decode_codecs.is_empty() =>
            {
                Some(None)
            }
            _ => None,
        };
        let decoded = match decoded_codec {
            Some(c) => Some(self.open_decoded(c, &decode, args.width, args.height)?),
            None => None,
        };
        let mode = if decoded.is_some() {
            SinkMode::Decoded
        } else {
            SinkMode::Encoded
        };
        let (decoded, drain) = match decoded {
            Some((d, wake, stop)) => (Some(d), Some((wake, stop))),
            None => (None, None),
        };
        let (sink_id, settings, background) = {
            let mut v = lock(&self.inner.video);
            if decoded.is_some() && v.sinks.decoded_count() >= MAX_DECODED_SINKS {
                return Err(decoded_tiles_full());
            }
            let id = v.sinks.open(OpenSink {
                track: args.track.clone(),
                channel,
                width: args.width,
                height: args.height,
                visible: args.visible,
                tile: args.tile,
                decode,
                decoded,
            });
            (id, v.sinks.changed(&args.track), v.sinks.is_background())
        };
        if let Some(drain) = drain {
            self.spawn_drain(sink_id, drain);
        }
        // In the background the keyframe waits for the return.
        self.apply_sink_change(
            call.as_ref(),
            &args.track,
            settings,
            args.visible && !background,
        );
        Ok(VideoSinkAnswer {
            sink_id,
            codec: if mode == SinkMode::Decoded {
                None
            } else {
                codec
            },
            mode,
        })
    }

    /// A decoded sink's decoder state: the engine's decoder for `codec` (`None`: a self-view
    /// sink, opened on its first keyframe), its wake and its drain task's stop.
    fn open_decoded(
        &self,
        codec: Option<VideoCodec>,
        decode: &[VideoCodec],
        width: u32,
        height: u32,
    ) -> Result<(Decoded, Arc<Notify>, Arc<Notify>), ErrorInfo> {
        let unsupported = |c: Option<VideoCodec>| {
            ErrorInfo::with_detail(
                video_code::DECODED_UNSUPPORTED,
                serde_json::json!({ "codec": c }),
            )
        };
        let caps = self.inner.engine.video_caps();
        if let Some(c) = codec
            && !caps.decode_codecs.contains(&c)
        {
            return Err(unsupported(Some(c)));
        }
        // Checked before a decoder is opened; checked again when the sink is added.
        if lock(&self.inner.video).sinks.decoded_count() >= MAX_DECODED_SINKS {
            return Err(decoded_tiles_full());
        }
        let format = if decode.is_empty() {
            DecodedFormat::Rgba
        } else {
            DecodedFormat::I420
        };
        let wake = Arc::new(Notify::new());
        let stop = Arc::new(Notify::new());
        let engine = Arc::clone(&self.inner.engine);
        let notify = Arc::clone(&wake);
        let decoder_wake: DecoderWake = Arc::new(move || notify.notify_one());
        let open: DecoderOpener = Arc::new(move |spec: DecoderSpec| {
            engine.video_decoder(spec, Arc::clone(&decoder_wake))
        });
        let dec: Option<(VideoCodec, Box<dyn FrameDecoder>)> = match codec {
            Some(c) => {
                let d = open(DecoderSpec {
                    codec: c,
                    format,
                    fit: decoded_fit(width, height),
                })
                .map_err(|e| {
                    tracing::debug!(codec = c.as_str(), error = %e, "the engine has no decoder");
                    unsupported(Some(c))
                })?;
                Some((c, d))
            }
            None => None,
        };
        Ok((
            Decoded::new(format, open, dec, Arc::clone(&stop)),
            wake,
            stop,
        ))
    }

    /// A decoded sink's drain task: on every decoder wake, sends what the decoder has ready;
    /// ends when the sink closes.
    fn spawn_drain(&self, sink_id: u32, (wake, stop): (Arc<Notify>, Arc<Notify>)) {
        let inner = Arc::downgrade(&self.inner);
        (self.inner.spawn)(Box::pin(async move {
            loop {
                tokio::select! {
                    () = stop.notified() => return,
                    () = wake.notified() => {}
                }
                let Some(inner) = inner.upgrade() else { return };
                let (routed, settings) = {
                    let mut v = lock(&inner.video);
                    let Some(routed) = v.sinks.drain_decoded(sink_id) else {
                        return;
                    };
                    let settings = closed_settings(&mut v.sinks, &routed);
                    (routed, settings)
                };
                if routed.closed.is_empty() && routed.decoder_keyframes.is_empty() {
                    continue;
                }
                let call = lock(&inner.slot).call.clone();
                if let Some(call) = &call {
                    for (track, s) in settings {
                        call.set_video_settings(&track, s);
                    }
                }
                for track in &routed.decoder_keyframes {
                    ask_keyframe(&inner, call.as_ref(), track);
                }
            }
        }));
    }

    /// `video_sink_update {sink_id, width?, height?, visible?, tile?, stats?}`: feeds the
    /// track's settings (and, with `stats`, the stats beacon). A sink that becomes visible
    /// waits for (and asks for) a keyframe.
    pub async fn video_sink_update(&self, args: VideoSinkUpdateArgs) -> Result<(), ErrorInfo> {
        let call = self.lock().call.clone();
        let (track, shown, settings, stats) = {
            let mut v = lock(&self.inner.video);
            let change = SinkChange {
                width: args.width,
                height: args.height,
                visible: args.visible,
                tile: args.tile,
            };
            let (track, shown) = v
                .sinks
                .update(args.sink_id, change, args.stats)
                .ok_or_else(|| unknown_sink(args.sink_id))?;
            let settings = v.sinks.changed(&track);
            let stats = args.stats.map(|_| v.sinks.stats(&track));
            (track, shown, settings, stats)
        };
        self.apply_sink_change(call.as_ref(), &track, settings, shown);
        if let (Some(call), Some(stats)) = (call, stats) {
            call.video_sink_stats(&track, stats);
        }
        Ok(())
    }

    /// `video_sink_close {sink_id}`: with no sink left, the track is disabled again. Closing a
    /// sink that is already closed does nothing.
    pub async fn video_sink_close(&self, args: SinkIdArgs) -> Result<(), ErrorInfo> {
        let call = self.lock().call.clone();
        let (track, settings) = {
            let mut v = lock(&self.inner.video);
            let Some(track) = v.sinks.close(args.sink_id, false) else {
                return Ok(());
            };
            let settings = v.sinks.changed(&track);
            (track, settings)
        };
        self.apply_sink_change(call.as_ref(), &track, settings, false);
        Ok(())
    }

    /// `request_keyframe {sink_id}`: the webview's decoder failed or fell behind. The sink
    /// waits for the next keyframe; the engine asks the sender (rate-limited).
    pub async fn request_keyframe(&self, args: SinkIdArgs) -> Result<(), ErrorInfo> {
        let call = self.lock().call.clone();
        let track = lock(&self.inner.video)
            .sinks
            .want_keyframe(args.sink_id, false)
            .ok_or_else(|| unknown_sink(args.sink_id))?;
        ask_keyframe(&self.inner, call.as_ref(), &track);
        Ok(())
    }

    /// Hands a sink change to the engine: the track's new settings, and a keyframe request
    /// when a sink of it is newly waiting and visible.
    fn apply_sink_change(
        &self,
        call: Option<&Arc<dyn CallHandle>>,
        track: &str,
        settings: Option<TrackSettings>,
        keyframe: bool,
    ) {
        if let (Some(call), Some(settings)) = (call, settings) {
            call.set_video_settings(track, settings);
        }
        if keyframe {
            ask_keyframe(&self.inner, call, track);
        }
    }

    // ───────────────────────────── phones ─────────────────────────────

    /// A native sink ([`crate::native`]): `target`'s frames in Rust for `consumer` (the
    /// picture in picture), asking the media server for the layer that fits `max_size`. It
    /// stays enabled in the background. Needs a live call (`not_in_call`); a track must be in
    /// the `video` list (`unknown_track`), and a VP8 track needs the engine's video backend
    /// (`decoded_unsupported`).
    #[cfg(feature = "video-core")]
    pub fn open_native_sink(
        &self,
        target: crate::native::NativeSinkTarget,
        max_size: (u32, u32),
        consumer: Arc<dyn crate::native::NativeVideoConsumer>,
    ) -> Result<crate::native::NativeSink, ErrorInfo> {
        use crate::native::{NativeSink, NativeSinkTarget, NativeWorker};
        use crate::sink::{NativeTarget, OpenNative};
        self.use_scene_events();
        let backend = self.inner.engine.native_video_backend();
        let mut decode = vec![VideoCodec::H264];
        if backend.is_some() {
            decode.push(VideoCodec::Vp8);
        }
        let (call, track, member, follow) = {
            let mut slot = self.lock();
            let call = slot
                .call
                .clone()
                .ok_or_else(|| ErrorInfo::new(code::NOT_IN_CALL))?;
            match &target {
                NativeSinkTarget::Track(t) => {
                    // Self-view is a local sink or the preview, never a native sink.
                    let self_view = || {
                        ErrorInfo::with_detail(
                            video_code::UNKNOWN_TRACK,
                            serde_json::json!({ "track": t, "self_view": true }),
                        )
                    };
                    if is_local_track(t) {
                        return Err(self_view());
                    }
                    let v = slot.video.iter().find(|v| v.track == *t).ok_or_else(|| {
                        ErrorInfo::with_detail(
                            video_code::UNKNOWN_TRACK,
                            serde_json::json!({ "track": t }),
                        )
                    })?;
                    if v.is_self {
                        return Err(self_view());
                    }
                    if !decode.contains(&v.codec) {
                        return Err(ErrorInfo::with_detail(
                            video_code::DECODED_UNSUPPORTED,
                            serde_json::json!({ "codec": v.codec }),
                        ));
                    }
                    (call, t.clone(), Some(v.member_id.clone()), false)
                }
                NativeSinkTarget::ActiveSpeaker => {
                    let (t, m) = follow_target(&slot);
                    slot.follow = (t.clone(), m.clone());
                    (call, t, m, true)
                }
            }
        };
        let id_cell = Arc::new(std::sync::OnceLock::<u32>::new());
        let ask = {
            let inner = Arc::downgrade(&self.inner);
            let id_cell = Arc::clone(&id_cell);
            Box::new(move || {
                if let (Some(inner), Some(id)) = (inner.upgrade(), id_cell.get()) {
                    native_keyframe(&inner, *id);
                }
            })
        };
        let low_layer = {
            let inner = Arc::downgrade(&self.inner);
            let id_cell = Arc::clone(&id_cell);
            Box::new(move |low: bool| {
                if let (Some(inner), Some(id)) = (inner.upgrade(), id_cell.get()) {
                    native_low_layer(&inner, *id, low);
                }
            })
        };
        let hooks = crate::native::Hooks {
            ask_keyframe: ask,
            low_layer,
        };
        let worker = NativeWorker::start(consumer, backend, max_size, hooks).map_err(|e| {
            ErrorInfo::with_detail(video_code::VIDEO_UNSUPPORTED, format!("native sink: {e}"))
        })?;
        let stats = worker.stats();
        if follow {
            let to = (!track.is_empty()).then_some(track.as_str());
            let _ = worker.switched(to, member.as_deref());
        }
        let (id, settings) = {
            let mut v = lock(&self.inner.video);
            let id = v.sinks.open_native(OpenNative {
                track: track.clone(),
                target: worker,
                width: max_size.0,
                height: max_size.1,
                decode,
                follow,
            });
            (id, v.sinks.changed(&track))
        };
        let _ = id_cell.set(id);
        self.apply_sink_change(Some(&call), &track, settings, !track.is_empty());
        Ok(NativeSink {
            id,
            control: Arc::new(NativeControl {
                inner: Arc::downgrade(&self.inner),
            }),
            stats,
        })
    }

    /// `camera_permission` → the camera permission now (`granted`, `denied`, `prompt`,
    /// `unsupported`). Never prompts.
    pub async fn camera_permission(&self) -> Result<PermissionState, ErrorInfo> {
        Ok(self.inner.engine.camera_permission())
    }

    /// `request_camera_permission` → the OS prompt when the answer is `prompt` (iOS), else the
    /// state at once (on Android the app asks for CAMERA itself first).
    pub async fn request_camera_permission(&self) -> Result<PermissionState, ErrorInfo> {
        Ok(self.inner.engine.request_camera_permission().await)
    }

    /// The app went to the background or came back ([`crate::VoiceControl::app_scene`]). The
    /// engine hears it first (kessel-video's Android backend), then the sinks follow the
    /// combined signal: background while the app **or** the video backend says so.
    pub fn app_scene(&self, scene: AppScene) {
        self.inner.engine.app_scene(scene);
        self.use_scene_events();
        let changed = {
            let mut sc = lock(&self.inner.scene);
            let was = sc.background();
            sc.app = scene.is_background();
            was != sc.background()
        };
        if changed {
            apply_scene(&self.inner);
            kick_pending_camera(&self.inner);
        }
    }

    /// Whether the plugin treats the app as in the background now (the app's or the video
    /// backend's word).
    pub fn is_background(&self) -> bool {
        lock(&self.inner.scene).background()
    }

    /// The device turned ([`crate::VoiceControl::device_orientation`]): to the engine.
    pub fn device_orientation(&self, degrees: u32) {
        self.inner.engine.device_orientation(degrees);
    }

    /// The first sink or scene: forward the video backend's background events.
    fn use_scene_events(&self) {
        if self.inner.scene_events_on.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(mut rx) = self.inner.engine.video_background_events() else {
            return;
        };
        let inner = Arc::downgrade(&self.inner);
        (self.inner.spawn)(Box::pin(async move {
            loop {
                let bg = match rx.recv().await {
                    Ok(bg) => bg,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return,
                };
                let Some(inner) = inner.upgrade() else { return };
                let changed = {
                    let mut sc = lock(&inner.scene);
                    let was = sc.background();
                    sc.backend = bg;
                    was != sc.background()
                };
                if changed {
                    apply_scene(&inner);
                    kick_pending_camera(&inner);
                }
            }
        }));
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
            recording: slot.recording.clone(),
            video: slot.video.clone(),
            local_video: slot.local_video.clone(),
            policy: slot.policy.clone(),
            has_video: slot.has_video,
        }
    }

    /// Whether the call has video now, local or remote (what the listener's `video_changed`
    /// last said).
    pub fn has_video(&self) -> bool {
        self.lock().has_video
    }

    fn set_state(&self, slot: &mut Slot, state: CallState) {
        set_state(&self.inner, slot, state);
    }

    /// Apply an engine snapshot; true when it says the call ended.
    fn apply_snapshot(&self, slot: &mut Slot, snap: CallSnapshot) -> bool {
        apply_snapshot(&self.inner, slot, snap)
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic while holding the lock leaves plain data behind; keep going with it.
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// One event: to the webview's sink, then to the native listener, under the slot's lock (the
/// caller holds it), so both see every event in the same order.
fn notify(inner: &Inner, slot: &mut Slot, event: &PluginEvent) {
    inner.sink.emit(event);
    let Some(l) = &inner.listener else { return };
    match event {
        PluginEvent::State(s) => l.on_state(s),
        PluginEvent::Participants(p) => {
            l.on_participants(p);
            self_mute_changed(inner, slot, true);
        }
        // No on_speaking: CallKit doesn't need levels.
        PluginEvent::Speaking(_) => {}
        PluginEvent::Errors(e) => l.on_error(e),
        PluginEvent::Devices(d) => l.on_devices(d),
        PluginEvent::Recording(r) => l.on_recording(r.as_ref()),
        PluginEvent::Incoming(i) => l.on_incoming(i.as_ref()),
        // The webview's: CallKit and Telecom show none of these.
        PluginEvent::Security(_) | PluginEvent::Roster(_) | PluginEvent::Quality(_) => {}
        // Video: the listener hears `video_changed` (set_video, set_local_video), not these.
        PluginEvent::Video(_)
        | PluginEvent::LocalVideo(_)
        | PluginEvent::VideoDevices(_)
        | PluginEvent::Policy(_) => {}
    }
}

/// The listener's `on_self_mute`, when this device's flags changed (from the own row in the
/// slot's participants and the own mute choice). No own row (idle, ended, held): nothing.
/// `resync` (a new row): where nothing forces the mute, the row's `muted` is the choice.
fn self_mute_changed(inner: &Inner, slot: &mut Slot, resync: bool) {
    let Some(l) = &inner.listener else { return };
    let Some(me) = slot.participants.iter().find(|p| p.is_self) else {
        slot.self_mute = None;
        return;
    };
    if resync && SelfMute::is_unforced(me) {
        slot.own_muted = me.muted;
    }
    let now = SelfMute::of(me, slot.own_muted);
    if slot.self_mute != Some(now) {
        slot.self_mute = Some(now);
        l.on_self_mute(&now);
    }
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
        let ev = state_event(slot);
        notify(inner, slot, &ev);
        show_quality(inner, slot);
    }
}

// ---------------------------------------------------------------- call details (0.4.1)

fn set_security(inner: &Inner, slot: &mut Slot, security: Option<SecurityInfo>) {
    if slot.details.security != security {
        slot.details.security.clone_from(&security);
        notify(inner, slot, &PluginEvent::Security(security));
    }
}

fn set_roster(inner: &Inner, slot: &mut Slot, roster: Vec<RosterEntry>) {
    if slot.details.roster != roster {
        slot.details.roster.clone_from(&roster);
        notify(inner, slot, &PluginEvent::Roster(roster));
    }
}

fn set_engine_quality(inner: &Inner, slot: &mut Slot, q: QualityInfo) {
    slot.details.engine_quality = q;
    show_quality(inner, slot);
}

/// The quality shown: `lost` while reconnecting, the engine's while connected, else `unknown`.
fn show_quality(inner: &Inner, slot: &mut Slot) {
    let now = match slot.state {
        CallState::Reconnecting { .. } => QualityInfo::lost(),
        CallState::Connected => slot.details.engine_quality.clone(),
        _ => QualityInfo::unknown(),
    };
    if slot.details.quality != now {
        slot.details.quality = now.clone();
        notify(inner, slot, &PluginEvent::Quality(now));
    }
}

fn set_incoming(inner: &Inner, slot: &mut Slot, incoming: Option<IncomingCall>) {
    if slot.details.incoming != incoming {
        slot.details.incoming.clone_from(&incoming);
        notify(inner, slot, &PluginEvent::Incoming(incoming));
    }
}

/// No call: no key state, no roster, quality `unknown` (the ring stays: it is not the call's).
fn clear_details(inner: &Inner, slot: &mut Slot) {
    set_security(inner, slot, None);
    set_roster(inner, slot, Vec::new());
    set_engine_quality(inner, slot, QualityInfo::unknown());
}

/// A new call's details as its engine has them now.
fn apply_details(inner: &Inner, slot: &mut Slot, call: &dyn CallHandle) {
    set_roster(inner, slot, call.roster());
    set_security(inner, slot, call.security());
    set_engine_quality(inner, slot, call.quality());
}

/// A moderator move: the call is on `room` now (a `state` event says so).
fn set_room(inner: &Inner, slot: &mut Slot, room: String) {
    if slot.room.as_deref() != Some(room.as_str()) {
        slot.room = Some(room);
        let ev = state_event(slot);
        notify(inner, slot, &ev);
    }
}

fn set_recording(inner: &Inner, slot: &mut Slot, recording: Option<RecordingInfo>) {
    if slot.recording != recording {
        slot.recording.clone_from(&recording);
        notify(inner, slot, &PluginEvent::Recording(recording));
    }
}

/// A call held for the notice has its room once Sew let it in.
fn refresh_call_id(slot: &mut Slot, call: &dyn CallHandle) {
    if slot.call_id.is_none() {
        let id = call.call_id();
        if !id.is_empty() {
            slot.call_id = Some(id);
        }
    }
}

fn set_participants(inner: &Inner, slot: &mut Slot, participants: Vec<Participant>) {
    if slot.participants != participants {
        slot.participants = participants.clone();
        notify(inner, slot, &PluginEvent::Participants(participants));
    }
}

/// The call ended (engine side): forget it and say so.
fn end(inner: &Inner, slot: &mut Slot, reason: String) {
    slot.call = None;
    slot.call_id = None;
    slot.stop = None;
    set_participants(inner, slot, Vec::new());
    set_recording(inner, slot, None);
    clear_video(inner, slot);
    set_state(inner, slot, CallState::Ended { reason });
    clear_details(inner, slot);
}

/// The state comes before the recording: a held join is `consent_required`, then its notice.
fn apply_snapshot(inner: &Inner, slot: &mut Slot, snap: CallSnapshot) -> bool {
    match snap.state {
        CallState::Ended { reason } => {
            end(inner, slot, reason);
            true
        }
        // An engine never reports idle; a fresh call is connecting.
        CallState::Idle => {
            set_participants(inner, slot, snap.participants);
            set_recording(inner, slot, snap.recording);
            false
        }
        state => {
            set_participants(inner, slot, snap.participants);
            set_state(inner, slot, state);
            set_recording(inner, slot, snap.recording);
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
                    if !self.follow(Some(&levels)) {
                        return;
                    }
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
                Ok(CallEvent::Recording(r)) => {
                    let mut slot = lock(&self.inner.slot);
                    if slot.generation != self.generation {
                        return;
                    }
                    // A re-join held for the notice: the engine sends the notice, then the
                    // state; its snapshot already says `consent_required`, so the state goes
                    // first (its own event then changes nothing), as for a held join.
                    if r.as_ref().is_some_and(|r| r.consent_prompt.is_some())
                        && let held @ CallState::ConsentRequired { .. } = self.call.snapshot().state
                    {
                        set_state(&self.inner, &mut slot, held);
                    }
                    set_recording(&self.inner, &mut slot, r);
                }
                Ok(CallEvent::Security(sec)) => {
                    let mut slot = lock(&self.inner.slot);
                    if slot.generation != self.generation {
                        return;
                    }
                    set_security(&self.inner, &mut slot, sec);
                }
                Ok(CallEvent::Roster(r)) => {
                    let mut slot = lock(&self.inner.slot);
                    if slot.generation != self.generation {
                        return;
                    }
                    set_roster(&self.inner, &mut slot, r);
                }
                Ok(CallEvent::Quality(q)) => {
                    let mut slot = lock(&self.inner.slot);
                    if slot.generation != self.generation {
                        return;
                    }
                    set_engine_quality(&self.inner, &mut slot, q);
                }
                Ok(CallEvent::Video(tracks)) => {
                    let unmuted: Vec<String> = {
                        let mut slot = lock(&self.inner.slot);
                        if slot.generation != self.generation {
                            return;
                        }
                        let unmuted = tracks
                            .iter()
                            .filter(|t| {
                                !t.muted && slot.video.iter().any(|o| o.track == t.track && o.muted)
                            })
                            .map(|t| t.track.clone())
                            .collect();
                        set_video(&self.inner, &mut slot, tracks);
                        unmuted
                    };
                    // A remote unmute restarts the frames: the sinks wait for its keyframe
                    // (the engine asks for it too; an extra request is rate-limited there).
                    for track in unmuted {
                        // A keyframe request sends nothing and closes no sink.
                        let _ = lock(&self.inner.video).sinks.route(
                            crate::video::VideoRecvItem::KeyframeNeeded {
                                track: track.clone(),
                            },
                        );
                        self.call.request_keyframe(&track);
                    }
                    if !self.follow(None) {
                        return;
                    }
                }
                Ok(CallEvent::LocalVideo(local)) => {
                    let mut slot = lock(&self.inner.slot);
                    if slot.generation != self.generation {
                        return;
                    }
                    set_local_video(&self.inner, &mut slot, local);
                }
                Ok(CallEvent::Policy(policy)) => {
                    let mut slot = lock(&self.inner.slot);
                    if slot.generation != self.generation {
                        return;
                    }
                    set_policy(&self.inner, &mut slot, Some(policy));
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
                        state => {
                            refresh_call_id(&mut slot, self.call.as_ref());
                            let up = state == CallState::Connected;
                            set_state(&self.inner, &mut slot, state);
                            if up && slot.pending_camera.is_some() {
                                drop(slot);
                                kick_pending_camera(&self.inner);
                            }
                        }
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
                    apply_details(&self.inner, &mut slot, self.call.as_ref());
                    apply_video(&self.inner, &mut slot, self.call.video_snapshot());
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

    /// The active speaker after `levels` (and the video list now): the active-speaker sinks
    /// follow. False when this call is no longer current (the pump stops).
    fn follow(&self, levels: Option<&[Speaking]>) -> bool {
        let (track, member) = {
            let mut slot = lock(&self.inner.slot);
            if slot.generation != self.generation {
                return false;
            }
            if let Some(levels) = levels {
                update_speaker(&mut slot, levels, Instant::now());
            }
            let target = follow_target(&slot);
            slot.follow = target.clone();
            target
        };
        retarget_followers(&self.inner, Some(&self.call), &track, member.as_deref());
        true
    }

    /// Emit if this call is still current; false when it is not (the pump stops).
    fn emit_current(&self, event: PluginEvent) -> bool {
        let mut slot = lock(&self.inner.slot);
        if slot.generation != self.generation {
            return false;
        }
        notify(&self.inner, &mut slot, &event);
        true
    }
}

// ───────────────────────────── video ─────────────────────────────

/// `decoded_tiles_full`: every decoded slot is taken.
fn decoded_tiles_full() -> ErrorInfo {
    ErrorInfo::with_detail(
        video_code::DECODED_TILES_FULL,
        serde_json::json!({ "max": MAX_DECODED_SINKS }),
    )
}

/// The settings of the tracks whose sinks a route closed (their channel was gone).
fn closed_settings(sinks: &mut Sinks, routed: &Routed) -> Vec<(String, TrackSettings)> {
    let mut settings = Vec::new();
    for (_, track) in &routed.closed {
        if let Some(s) = sinks.changed(track) {
            settings.push((track.clone(), s));
        }
    }
    settings
}

fn unknown_sink(sink_id: u32) -> ErrorInfo {
    ErrorInfo::with_detail(
        video_code::UNKNOWN_SINK,
        serde_json::json!({ "sink_id": sink_id }),
    )
}

/// A keyframe for `track`'s sinks: from the call's engine, or for `local:camera` outside a
/// call, from the preview's encoder.
fn ask_keyframe(inner: &Inner, call: Option<&Arc<dyn CallHandle>>, track: &str) {
    match call {
        Some(call) => call.request_keyframe(track),
        None if track == LOCAL_CAMERA => inner.engine.preview_keyframe(),
        None => {}
    }
}

/// A native sink needs a keyframe (its consumer's decoder failed, or it asked).
#[cfg(feature = "video-core")]
fn native_keyframe(inner: &Inner, id: u32) {
    let call = lock(&inner.slot).call.clone();
    let track = lock(&inner.video).sinks.want_keyframe(id, true);
    if let Some(track) = track.filter(|t| !t.is_empty()) {
        ask_keyframe(inner, call.as_ref(), &track);
    }
}

/// A native sink steps down to the low layer (`low`) or back up: the track's settings follow.
#[cfg(feature = "video-core")]
fn native_low_layer(inner: &Inner, id: u32, low: bool) {
    let call = lock(&inner.slot).call.clone();
    let (track, settings) = {
        let mut v = lock(&inner.video);
        let Some(track) = v.sinks.set_native_low(id, low) else {
            return;
        };
        let settings = v.sinks.changed(&track);
        (track, settings)
    };
    if let (Some(call), Some(s)) = (call, settings) {
        call.set_video_settings(&track, s);
    }
}

/// The service side of a [`crate::native::NativeSink`].
#[cfg(feature = "video-core")]
struct NativeControl {
    inner: std::sync::Weak<Inner>,
}

#[cfg(feature = "video-core")]
impl crate::native::NativeSinkControl for NativeControl {
    fn track(&self, id: u32) -> Option<String> {
        let inner = self.inner.upgrade()?;
        lock(&inner.video).sinks.track_of(id)
    }

    fn resize(&self, id: u32, width: u32, height: u32) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let call = lock(&inner.slot).call.clone();
        let (track, settings) = {
            let mut v = lock(&inner.video);
            let Some(track) = v.sinks.resize_native(id, width, height) else {
                return;
            };
            let settings = v.sinks.changed(&track);
            (track, settings)
        };
        if let (Some(call), Some(s)) = (call, settings) {
            call.set_video_settings(&track, s);
        }
    }

    fn keyframe(&self, id: u32) {
        if let Some(inner) = self.inner.upgrade() {
            native_keyframe(&inner, id);
        }
    }

    fn close(&self, id: u32) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let call = lock(&inner.slot).call.clone();
        let (track, settings) = {
            let mut v = lock(&inner.video);
            let Some(track) = v.sinks.close(id, true) else {
                return;
            };
            let settings = v.sinks.changed(&track);
            (track, settings)
        };
        if let (Some(call), Some(s)) = (call, settings) {
            call.set_video_settings(&track, s);
        }
    }
}

/// The sinks follow the scene now: in the background every webview sink is hidden (only
/// native sinks stay enabled); back in front, the visible sinks wait for a keyframe and one is
/// asked of each of their tracks. Then the call's own hook (its camera's pause). The caller
/// holds no lock.
fn apply_scene(inner: &Inner) {
    let call = lock(&inner.slot).call.clone();
    let (background, settings, keyframes) = {
        // Read under the lock, so two changes racing here end on the latest.
        let scene = lock(&inner.scene);
        let background = scene.background();
        let mut v = lock(&inner.video);
        let Some((tracks, keyframes)) = v.sinks.set_background(background) else {
            return;
        };
        let settings: Vec<(String, TrackSettings)> = tracks
            .iter()
            .filter_map(|t| v.sinks.changed(t).map(|s| (t.clone(), s)))
            .collect();
        (background, settings, keyframes)
    };
    if let Some(call) = &call {
        for (t, s) in settings {
            call.set_video_settings(&t, s);
        }
    }
    for t in keyframes {
        ask_keyframe(inner, call.as_ref(), &t);
    }
    if let Some(call) = &call {
        call.app_scene(AppScene::background(background));
    }
}

fn set_video(inner: &Inner, slot: &mut Slot, tracks: Vec<VideoTrack>) {
    if slot.video != tracks {
        slot.video.clone_from(&tracks);
        notify(inner, slot, &PluginEvent::Video(tracks));
        video_changed(inner, slot);
    }
}

fn set_local_video(inner: &Inner, slot: &mut Slot, local: LocalVideo) {
    if slot.local_video != local {
        let sharing =
            |s: LocalVideoState| matches!(s, LocalVideoState::On | LocalVideoState::Paused);
        let was = sharing(slot.local_video.screen);
        let now = sharing(local.screen);
        slot.local_video = local.clone();
        notify(inner, slot, &PluginEvent::LocalVideo(local));
        video_changed(inner, slot);
        if was != now
            && let Some(l) = &inner.listener
        {
            l.screen_share_changed(now);
        }
    }
}

/// The call has video when this device sends its camera or screen (on or paused) or a remote
/// camera or screen is unmuted. The listener hears every change, right after the `video` or
/// `local_video` event that made it.
fn video_changed(inner: &Inner, slot: &mut Slot) {
    let sending = |s: LocalVideoState| matches!(s, LocalVideoState::On | LocalVideoState::Paused);
    let now = sending(slot.local_video.camera)
        || sending(slot.local_video.screen)
        || slot.video.iter().any(|t| !t.is_self && !t.muted);
    if now != slot.has_video {
        slot.has_video = now;
        if let Some(l) = &inner.listener {
            l.video_changed(now);
        }
    }
}

/// The active speaker after `levels` (the loudest remote member at or above
/// [`ACTIVE_SPEAKER_LEVEL`]; a new one takes over only after [`ACTIVE_SPEAKER_HOLD`]).
fn update_speaker(slot: &mut Slot, levels: &[Speaking], now: Instant) {
    let me = slot
        .participants
        .iter()
        .find(|p| p.is_self)
        .map(|p| p.member_id.clone());
    let loudest = levels
        .iter()
        .filter(|l| Some(&l.member_id) != me.as_ref() && l.level >= ACTIVE_SPEAKER_LEVEL)
        .max_by(|a, b| a.level.total_cmp(&b.level));
    let Some(loudest) = loudest else { return };
    match &slot.speaker {
        Some((m, _)) if *m == loudest.member_id => {}
        Some((_, since)) if now.duration_since(*since) < ACTIVE_SPEAKER_HOLD => {}
        _ => slot.speaker = Some((loudest.member_id.clone(), now)),
    }
}

/// What the active-speaker sinks show now: the speaker's camera, else their screen; else the
/// track they show already while it is still unmuted; else the first remote camera, else
/// screen; else nothing (`""`). Never this device's own video: no `is_self` track and no
/// track of this member (self-view is a local sink or the preview, never the picture in
/// picture).
fn follow_target(slot: &Slot) -> (String, Option<String>) {
    let me = slot
        .participants
        .iter()
        .find(|p| p.is_self)
        .map(|p| p.member_id.as_str());
    let live = |t: &&VideoTrack| !t.is_self && !t.muted && Some(t.member_id.as_str()) != me;
    let pick = |t: &VideoTrack| (t.track.clone(), Some(t.member_id.clone()));
    if let Some((member, _)) = &slot.speaker {
        let theirs = || {
            slot.video
                .iter()
                .filter(live)
                .filter(|t| t.member_id == *member)
        };
        if let Some(t) = theirs()
            .find(|t| t.source == crate::wire::VideoSource::Camera)
            .or_else(|| theirs().next())
        {
            return pick(t);
        }
    }
    if let Some(t) = slot
        .video
        .iter()
        .filter(live)
        .find(|t| t.track == slot.follow.0)
    {
        return pick(t);
    }
    slot.video
        .iter()
        .filter(live)
        .find(|t| t.source == crate::wire::VideoSource::Camera)
        .or_else(|| slot.video.iter().find(live))
        .map_or((String::new(), None), pick)
}

/// Points every active-speaker sink at `track` (`""`: nothing): the old and new tracks'
/// settings go to the engine, and a keyframe is asked of the new one (the switch). Takes the
/// sinks' lock: the caller holds no lock.
fn retarget_followers(
    inner: &Inner,
    call: Option<&Arc<dyn CallHandle>>,
    track: &str,
    member: Option<&str>,
) {
    let (settings, moved) = {
        let mut v = lock(&inner.video);
        let ids = v.sinks.followers_off(track);
        if ids.is_empty() {
            return;
        }
        let mut tracks = vec![track.to_owned()];
        let mut closed = Vec::new();
        for id in ids {
            if let Some(old) = v.sinks.retarget(id, track, member, &mut closed) {
                tracks.push(old);
            }
        }
        tracks.extend(closed.into_iter().map(|(_, t)| t));
        tracks.sort();
        tracks.dedup();
        let settings: Vec<(String, TrackSettings)> = tracks
            .iter()
            .filter_map(|t| v.sinks.changed(t).map(|s| (t.clone(), s)))
            .collect();
        (settings, !track.is_empty())
    };
    if let Some(call) = call {
        for (t, s) in settings {
            call.set_video_settings(&t, s);
        }
        if moved {
            call.request_keyframe(track);
        }
    }
}

/// A new policy is announced; none (the call is over) is dropped silently.
fn set_policy(inner: &Inner, slot: &mut Slot, policy: Option<Value>) {
    if slot.policy == policy {
        return;
    }
    slot.policy.clone_from(&policy);
    if let Some(p) = policy {
        notify(inner, slot, &PluginEvent::Policy(p));
    }
}

fn apply_video(inner: &Inner, slot: &mut Slot, v: VideoSnapshot) {
    set_video(inner, slot, v.tracks);
    set_local_video(inner, slot, v.local);
    set_policy(inner, slot, v.policy);
}

/// No call: no tracks, camera and screen off, no policy, the frame task stopped. (Sinks stay
/// open until the webview closes them; a self-view sink keeps the preview.)
fn clear_video(inner: &Inner, slot: &mut Slot) {
    if let Some(stop) = slot.video_stop.take() {
        stop.notify_one();
    }
    set_video(inner, slot, Vec::new());
    set_local_video(inner, slot, LocalVideo::default());
    set_policy(inner, slot, None);
    slot.speaker = None;
    slot.follow = (String::new(), None);
    slot.pending_camera = None;
}

/// `camera {on}`, `preview {on}`, `VoiceControl::set_camera` and a join's camera: the
/// capability first (no camera here, `video_caps.camera` false: `video_unsupported`, since a
/// permission doesn't apply), then the OS permission, which must be granted: the plugin never
/// prompts on its own (`request_camera_permission` does, on the app's call). An engine without
/// a camera permission (`unsupported`) decides itself.
fn camera_gate(engine: &dyn VoiceEngine) -> Result<(), ErrorInfo> {
    if !engine.video_caps().camera {
        return Err(ErrorInfo::new(video_code::VIDEO_UNSUPPORTED));
    }
    match engine.camera_permission() {
        PermissionState::Granted | PermissionState::Unsupported => Ok(()),
        PermissionState::Denied => Err(ErrorInfo::new(video_code::CAMERA_PERMISSION_DENIED)),
        PermissionState::Prompt => {
            Err(ErrorInfo::new(video_code::CAMERA_PERMISSION_NOT_DETERMINED))
        }
    }
}

/// Starts a join's camera ([`JoinOptions`]) once the call is connected and the app is in
/// front; called on each of those changes. The caller holds no lock.
fn kick_pending_camera(inner: &Arc<Inner>) {
    let inner2 = Arc::clone(inner);
    (inner.spawn)(Box::pin(async move {
        let (call, req, generation) = {
            // The scene first, then the slot: a foreground racing this one finds the request
            // either taken here or still pending.
            let scene = lock(&inner2.scene);
            if scene.background() {
                return;
            }
            let mut slot = lock(&inner2.slot);
            if slot.state != CallState::Connected || slot.pending_camera.is_none() {
                return;
            }
            let Some(call) = slot.call.clone() else {
                return;
            };
            let Some(req) = slot.pending_camera.take() else {
                return;
            };
            (call, req, slot.generation)
        };
        let started = match camera_gate(inner2.engine.as_ref()) {
            Ok(()) => call.camera(req).await.map_err(|e| e.to_info()),
            Err(e) => Err(e),
        };
        if let Err(e) = started {
            let mut slot = lock(&inner2.slot);
            if slot.generation == generation {
                let mut local = slot.local_video.clone();
                local.camera = LocalVideoState::Off;
                local.reason = Some(e.code);
                set_local_video(&inner2, &mut slot, local);
            }
        }
    }));
}

/// Reads one frame channel (a call's or the preview's) into the sinks.
struct FramePump {
    inner: Arc<Inner>,
    rx: VideoFrames,
    stop: Arc<Notify>,
    /// The call whose channel it is (`None`: the preview).
    call: Option<Arc<dyn CallHandle>>,
}

impl FramePump {
    async fn run(mut self) {
        loop {
            let item = tokio::select! {
                () = self.stop.notified() => return,
                item = self.rx.recv() => item,
            };
            let Some(item) = item else { return };
            let (routed, settings) = {
                let mut v = lock(&self.inner.video);
                let routed: Routed = v.sinks.route(item);
                let mut settings = closed_settings(&mut v.sinks, &routed);
                for track in &routed.keyframes {
                    // A track that came back: the engine has it disabled again.
                    if let Some(s) = v.sinks.force(track) {
                        settings.push((track.clone(), s));
                    }
                }
                (routed, settings)
            };
            if let Some(call) = &self.call {
                for (track, s) in settings {
                    call.set_video_settings(&track, s);
                }
            }
            for track in routed
                .keyframes
                .iter()
                .chain(&routed.pli)
                .chain(&routed.decoder_keyframes)
            {
                ask_keyframe(&self.inner, self.call.as_ref(), track);
            }
        }
    }
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}
