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
//!
//! Recording (Sew 0.7.3): a provider refusal that is [`ProviderError::ConsentRequired`] holds the
//! call at `consent_required` (no roster, no room) as the real engine does; `consent` then joins
//! again through the provider with the answer as `recording_notice`. In a call, `consent` and
//! `withdraw_consent` are recorded ([`FakeCall::consents`]) and the test says what Sew answered
//! with [`FakeCall::set_recording`].
//!
//! Video: off until [`FakeEngine::enable_video`] (then the demo cameras and capabilities). A
//! call's frame channel is fed with [`FakeCall::push_video`]; its tracks, camera and policy are
//! set with [`FakeCall::set_video_tracks`], [`FakeCall::set_local_video`] and
//! [`FakeCall::set_policy`]; what the plugin asked of the engine is in
//! [`FakeCall::video_settings`], [`FakeCall::keyframe_requests`] and
//! [`FakeCall::camera_requests`]. [`RecordingFrameChannel`] stands in for a webview's sink
//! channel.
//!
//! The decoded fallback: [`FakeEngine::enable_fake_decoder`] gives the engine
//! [`FakeDecoders`] (VP8 and H.264; each output is the input's first byte at the fitted size,
//! with failures and held outputs on demand), and [`FakeEngine::set_video_decoder`] any other
//! decoder (kessel-video's, in the `video-core` tests).

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::json;
use sew_call_client::device::DeviceKeyId;
use tokio::sync::{Semaphore, broadcast, mpsc};

use crate::details::{IncomingCall, QualityInfo, RosterEntry, SecurityInfo};
use crate::engine::{
    AppScene, AudioSessionEvent, BoxFuture, CallEvent, CallHandle, CallSnapshot, ConsentRequired,
    EngineError, JoinAnswer, JoinProvider, JoinRequest, ProviderError, VideoSnapshot, VoiceEngine,
};
use crate::listener::VoiceListener;
use crate::service::EventSink;
use crate::sink::FrameChannel;
use crate::video::{
    CameraRequest, DecodeFailure, DecodedFormat, DecodedFrame, DecoderSpec, DecoderWake,
    EncodedVideoFrame, FrameDecoder, FrameHeader, HEADER_LEN, PreviewRequest, ScreenRequest,
    TrackSettings, VideoCodec, VideoFrames, VideoRecvItem,
};
use crate::wire::{
    CallState, Camera, CodecPreferenceInfo, Device, DeviceChoice, DeviceList, ErrorInfo, Facing,
    LocalVideo, LocalVideoState, Participant, PermissionState, PluginEvent, RecordingInfo,
    ScreenCaps, ScreenSource, SelfMute, SinkStats, StateEvent, VideoCaps, VideoCodecPreference,
    VideoDevices, VideoPermissions, VideoTrack, video_code,
};

/// How many items a fake frame channel holds before `push_video` drops.
const FAKE_FRAME_QUEUE: usize = 256;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The in-memory engine.
#[derive(Debug)]
pub struct FakeEngine {
    state: Mutex<EngineState>,
    devices_tx: broadcast::Sender<DeviceList>,
    video: Mutex<FakeVideoEngine>,
    video_devices_tx: broadcast::Sender<VideoDevices>,
    /// The video backend's background signal ([`FakeEngine::backend_background`]).
    background_tx: broadcast::Sender<bool>,
    /// The video backend the native sinks use ([`FakeEngine::set_native_video_backend`]).
    #[cfg(feature = "video-core")]
    native_backend: Mutex<Option<NativeBackend>>,
}

#[cfg(feature = "video-core")]
struct NativeBackend(Arc<dyn sew_call_client::kessel_video::VideoBackend>);

#[cfg(feature = "video-core")]
impl std::fmt::Debug for NativeBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("NativeBackend")
    }
}

/// The fake's engine-wide video.
#[derive(Debug, Default)]
struct FakeVideoEngine {
    enabled: bool,
    caps: VideoCaps,
    devices: VideoDevices,
    screen_sources: Vec<ScreenSource>,
    choices: Vec<DeviceChoice>,
    previews: Vec<PreviewRequest>,
    preview_tx: Option<mpsc::Sender<VideoRecvItem>>,
    preview_keyframes: usize,
    /// The camera permission (`unsupported` until [`FakeEngine::set_camera_permission`]).
    camera_permission: PermissionState,
    /// What `request_camera_permission` turns a `prompt` into.
    prompt_answer: PermissionState,
    /// How many times the camera permission was requested.
    permission_requests: usize,
    /// Every `app_scene`, in order.
    scenes: Vec<AppScene>,
    /// Every `device_orientation`, in order.
    orientations: Vec<u32>,
    /// The decoded fallback's decoders ([`FakeEngine::set_video_decoder`]).
    decoder: Option<Opener>,
    /// Each join's `decode` (the webview's decoders), in order.
    join_decodes: Vec<Option<Vec<VideoCodec>>>,
    /// The video codec preference (`set_video_codec_preference`).
    codec_preference: VideoCodecPreference,
}

/// Opens a decoder for the fake engine's `video_decoder` (the decoded fallback).
pub type FakeDecoderOpener = Arc<
    dyn Fn(DecoderSpec, DecoderWake) -> Result<Box<dyn FrameDecoder>, EngineError> + Send + Sync,
>;

struct Opener(FakeDecoderOpener);

impl std::fmt::Debug for Opener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Opener")
    }
}

#[derive(Debug)]
struct EngineState {
    devices: DeviceList,
    calls: Vec<Arc<FakeCall>>,
    joins: Vec<JoinRequest>,
    next_join_error: Option<EngineError>,
    auto_connect: bool,
    /// Every audio-session event, in order ([`VoiceEngine::audio_session`]).
    audio_sessions: Vec<AudioSessionEvent>,
    /// The device store: the keys it holds ([`FakeEngine::set_device_keys`]).
    device_keys: Vec<DeviceKeyId>,
    /// Every `forget_server` install id, in order.
    forgets: Vec<u64>,
    /// The next `forget_server` fails with this.
    next_forget_error: Option<EngineError>,
    /// A call the plugin lost hold of: joins answer `already_in_call` until
    /// [`VoiceEngine::release_calls`] ([`FakeEngine::strand_call`]).
    stranded: bool,
    /// How many times [`VoiceEngine::release_calls`] ran.
    releases: usize,
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
                audio_sessions: Vec::new(),
                device_keys: Vec::new(),
                forgets: Vec::new(),
                next_forget_error: None,
                stranded: false,
                releases: 0,
            }),
            devices_tx,
            video: Mutex::new(FakeVideoEngine::default()),
            video_devices_tx: broadcast::channel(16).0,
            background_tx: broadcast::channel(16).0,
            #[cfg(feature = "video-core")]
            native_backend: Mutex::new(None),
        }
    }

    /// The camera permission `camera_permission` answers, and what a `request_camera_permission`
    /// turns `prompt` into (the person's answer to the OS prompt).
    pub fn set_camera_permission(&self, now: PermissionState, prompt_answer: PermissionState) {
        let mut v = lock(&self.video);
        v.camera_permission = now;
        v.prompt_answer = prompt_answer;
    }

    /// How many times the camera permission was requested.
    pub fn permission_requests(&self) -> usize {
        lock(&self.video).permission_requests
    }

    /// Every [`VoiceEngine::app_scene`], in order.
    pub fn app_scenes(&self) -> Vec<AppScene> {
        lock(&self.video).scenes.clone()
    }

    /// Every [`VoiceEngine::device_orientation`], in order.
    pub fn device_orientations(&self) -> Vec<u32> {
        lock(&self.video).orientations.clone()
    }

    /// The video backend says the app went to the background (`true`) or came back
    /// (kessel-video's `BackendEvent::Background`).
    pub fn backend_background(&self, background: bool) {
        let _ = self.background_tx.send(background);
    }

    /// How many receivers of [`VoiceEngine::video_background_events`] are alive.
    pub fn background_subscribers(&self) -> usize {
        self.background_tx.receiver_count()
    }

    /// The video backend native sinks decode VP8 with (kessel-video's fake backend in tests).
    #[cfg(feature = "video-core")]
    pub fn set_native_video_backend(
        &self,
        backend: Arc<dyn sew_call_client::kessel_video::VideoBackend>,
    ) {
        *lock(&self.native_backend) = Some(NativeBackend(backend));
    }

    /// Turns video on: the demo capabilities ([`demo_video_caps`]) and cameras
    /// ([`demo_cameras`]); calls joined from now on have a frame channel.
    pub fn enable_video(&self) {
        let mut v = lock(&self.video);
        v.enabled = true;
        v.caps = demo_video_caps();
        v.devices = demo_cameras();
    }

    /// The decoded fallback decodes `codecs` (`video_caps.decode_codecs`) with `open`.
    pub fn set_video_decoder(&self, codecs: Vec<VideoCodec>, open: FakeDecoderOpener) {
        let mut v = lock(&self.video);
        v.caps.decode_codecs = codecs;
        v.decoder = Some(Opener(open));
    }

    /// The decoded fallback with the fake's own decoders (VP8 and H.264); the answer drives
    /// and records them.
    pub fn enable_fake_decoder(&self) -> Arc<FakeDecoders> {
        let decoders = Arc::new(FakeDecoders::default());
        let d = Arc::clone(&decoders);
        self.set_video_decoder(
            vec![VideoCodec::Vp8, VideoCodec::H264],
            Arc::new(move |spec, wake| d.open(spec, wake)),
        );
        decoders
    }

    /// Each join's `decode` (the webview's decoders it passed), in order.
    pub fn join_decodes(&self) -> Vec<Option<Vec<VideoCodec>>> {
        lock(&self.video).join_decodes.clone()
    }

    /// Replace the video capabilities.
    pub fn set_video_caps(&self, caps: VideoCaps) {
        lock(&self.video).caps = caps;
    }

    /// Replace the camera list (a hot-plug): sends a `video_devices` change.
    pub fn set_video_devices(&self, devices: VideoDevices) {
        lock(&self.video).devices = devices.clone();
        let _ = self.video_devices_tx.send(devices);
    }

    /// The displays and windows `screen_sources` answers.
    pub fn set_screen_sources(&self, sources: Vec<ScreenSource>) {
        lock(&self.video).screen_sources = sources;
    }

    /// Every `set_video_device`, in order.
    pub fn video_device_choices(&self) -> Vec<DeviceChoice> {
        lock(&self.video).choices.clone()
    }

    /// How many receivers of [`VoiceEngine::video_device_events`] are alive.
    pub fn video_device_subscribers(&self) -> usize {
        self.video_devices_tx.receiver_count()
    }

    /// Every `preview` request, in order.
    pub fn previews(&self) -> Vec<PreviewRequest> {
        lock(&self.video).previews.clone()
    }

    /// The running preview's frame channel (`None` when it is off).
    pub fn preview_sender(&self) -> Option<mpsc::Sender<VideoRecvItem>> {
        lock(&self.video).preview_tx.clone()
    }

    /// How many keyframes the plugin asked of the preview.
    pub fn preview_keyframes(&self) -> usize {
        lock(&self.video).preview_keyframes
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

    /// The engine holds a call the plugin lost hold of (a leave that could not take it): every
    /// `join` answers `already_in_call` until [`VoiceEngine::release_calls`] releases it.
    pub fn strand_call(&self) {
        lock(&self.state).stranded = true;
    }

    /// How many times [`VoiceEngine::release_calls`] ran.
    pub fn releases(&self) -> usize {
        lock(&self.state).releases
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

    /// Every audio-session event the plugin passed on (CallKit's or Telecom's), in order.
    pub fn audio_sessions(&self) -> Vec<AudioSessionEvent> {
        lock(&self.state).audio_sessions.clone()
    }

    /// The keys the fake device store holds (`known_device_keys`; `forget_server` removes an
    /// install's).
    pub fn set_device_keys(&self, mut keys: Vec<DeviceKeyId>) {
        keys.sort();
        lock(&self.state).device_keys = keys;
    }

    /// The next `forget_server` fails with this (a key that can't be deleted); nothing is
    /// removed.
    pub fn fail_next_forget(&self, e: EngineError) {
        lock(&self.state).next_forget_error = Some(e);
    }

    /// Every `forget_server` the engine was given, in order.
    pub fn forgets(&self) -> Vec<u64> {
        lock(&self.state).forgets.clone()
    }
}

/// The fake's video capabilities: a camera, screen capture with a picker and a list, VP8 and
/// H.264 publish, VP8 decode, receive, backend `fake`.
pub fn demo_video_caps() -> VideoCaps {
    VideoCaps {
        camera: true,
        screen: ScreenCaps {
            supported: true,
            system_picker: true,
            list: true,
            windows: true,
        },
        publish_codecs: vec![VideoCodec::Vp8, VideoCodec::H264],
        decode_codecs: vec![VideoCodec::Vp8],
        receive: true,
        // The fake engine feeds `local:camera` only from its preview channel.
        self_view: false,
        live_switch: false,
        backend: "fake".into(),
    }
}

/// The fake's cameras: a built-in front camera (the default) and a USB one.
pub fn demo_cameras() -> VideoDevices {
    VideoDevices {
        cameras: vec![
            Camera {
                id: "cam:builtin".into(),
                name: "Built-in camera".into(),
                facing: Facing::Front,
                default: true,
            },
            Camera {
                id: "cam:usb".into(),
                name: "USB camera".into(),
                facing: Facing::External,
                default: false,
            },
        ],
        camera: None,
        permission: VideoPermissions {
            camera: PermissionState::Granted,
            screen: PermissionState::Prompt,
        },
    }
}

fn no_video() -> EngineError {
    EngineError::failed(video_code::VIDEO_UNSUPPORTED)
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
    fn release_calls(&self) -> BoxFuture<'_, ()> {
        let mut s = lock(&self.state);
        s.stranded = false;
        s.releases += 1;
        Box::pin(std::future::ready(()))
    }

    fn join(
        &self,
        provider: Arc<dyn JoinProvider>,
        req: JoinRequest,
    ) -> BoxFuture<'_, Result<Arc<dyn CallHandle>, EngineError>> {
        Box::pin(async move {
            let (fail, auto_connect) = {
                let mut s = lock(&self.state);
                s.joins.push(req.clone());
                let fail = if s.stranded {
                    Some(EngineError::failed(crate::wire::code::ALREADY_IN_CALL))
                } else {
                    s.next_join_error.take()
                };
                (fail, s.auto_connect)
            };
            if let Some(e) = fail {
                return Err(e);
            }
            let video = lock(&self.video).enabled;
            let call = match provider.join(req.clone()).await {
                Ok(answer) => Arc::new(FakeCall::new(&answer, &req, auto_connect, video)),
                // Sew holds the join for the recording notice: the call waits for `consent`.
                Err(ProviderError::ConsentRequired(c)) => {
                    Arc::new(FakeCall::held(&c, provider, req, auto_connect, video))
                }
                Err(e) => return Err(EngineError::Provider(e)),
            };
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

    fn audio_session(&self, event: AudioSessionEvent) {
        lock(&self.state).audio_sessions.push(event);
    }

    fn forget_server(
        &self,
        install_id: u64,
    ) -> BoxFuture<'_, Result<Vec<DeviceKeyId>, EngineError>> {
        let res = {
            let mut s = lock(&self.state);
            s.forgets.push(install_id);
            match s.next_forget_error.take() {
                Some(e) => Err(e),
                None => {
                    let (gone, kept): (Vec<DeviceKeyId>, Vec<DeviceKeyId>) =
                        std::mem::take(&mut s.device_keys)
                            .into_iter()
                            .partition(|k| k.install_id == install_id);
                    s.device_keys = kept;
                    Ok(gone)
                }
            }
        };
        Box::pin(std::future::ready(res))
    }

    fn known_device_keys(&self) -> BoxFuture<'_, Result<Vec<DeviceKeyId>, EngineError>> {
        let keys = lock(&self.state).device_keys.clone();
        Box::pin(std::future::ready(Ok(keys)))
    }

    fn join_decoding(
        &self,
        provider: Arc<dyn JoinProvider>,
        req: JoinRequest,
        decode: Option<Vec<VideoCodec>>,
    ) -> BoxFuture<'_, Result<Arc<dyn CallHandle>, EngineError>> {
        lock(&self.video).join_decodes.push(decode);
        self.join(provider, req)
    }

    fn video_caps(&self) -> VideoCaps {
        let v = lock(&self.video);
        if v.enabled {
            v.caps.clone()
        } else {
            VideoCaps::default()
        }
    }

    fn set_video_codec_preference(&self, pref: VideoCodecPreference) -> Result<(), EngineError> {
        let mut v = lock(&self.video);
        if !v.enabled {
            return Err(no_video());
        }
        v.codec_preference = pref;
        Ok(())
    }

    /// The setting over the fake's capabilities: H.264 counts as detected when
    /// `publish_codecs` lists it.
    fn video_codec_preference(&self) -> Option<CodecPreferenceInfo> {
        let v = lock(&self.video);
        if !v.enabled {
            return None;
        }
        let detected = if v.caps.publish_codecs.contains(&VideoCodec::H264) {
            sew_call_client::H264Detection::Usable
        } else {
            sew_call_client::H264Detection::NotDetected
        };
        let (codec, reason) =
            sew_call_client::effective_publish_codec(v.codec_preference, detected);
        let effective = match codec {
            sew_call_client::VideoCodec::H264 => VideoCodec::H264,
            _ => VideoCodec::Vp8,
        };
        Some(CodecPreferenceInfo::new(
            v.codec_preference,
            effective,
            reason,
        ))
    }

    fn video_devices(&self) -> Result<VideoDevices, EngineError> {
        let v = lock(&self.video);
        if v.enabled {
            Ok(v.devices.clone())
        } else {
            Err(no_video())
        }
    }

    fn set_video_device(&self, camera: DeviceChoice) -> Result<(), EngineError> {
        let list = {
            let mut v = lock(&self.video);
            if !v.enabled {
                return Err(no_video());
            }
            v.choices.push(camera.clone());
            match camera {
                DeviceChoice::Default => v.devices.camera = None,
                DeviceChoice::Id(id) => {
                    if !v.devices.cameras.iter().any(|c| c.id == id) {
                        return Err(EngineError::Failed(ErrorInfo::with_detail(
                            video_code::CAMERA_NOT_FOUND,
                            json!({ "camera": id }),
                        )));
                    }
                    v.devices.camera = Some(id);
                }
            }
            v.devices.clone()
        };
        let _ = self.video_devices_tx.send(list);
        Ok(())
    }

    fn video_device_events(&self) -> Option<broadcast::Receiver<VideoDevices>> {
        lock(&self.video)
            .enabled
            .then(|| self.video_devices_tx.subscribe())
    }

    fn screen_sources(&self) -> BoxFuture<'_, Result<Vec<ScreenSource>, EngineError>> {
        let v = lock(&self.video);
        let r = if v.enabled {
            Ok(v.screen_sources.clone())
        } else {
            Err(no_video())
        };
        Box::pin(std::future::ready(r))
    }

    fn preview(
        &self,
        req: PreviewRequest,
    ) -> BoxFuture<'_, Result<Option<VideoFrames>, EngineError>> {
        let r = {
            let mut v = lock(&self.video);
            if v.enabled {
                v.previews.push(req.clone());
                if req.on {
                    let (tx, rx) = mpsc::channel(FAKE_FRAME_QUEUE);
                    v.preview_tx = Some(tx);
                    Ok(Some(rx))
                } else {
                    v.preview_tx = None;
                    Ok(None)
                }
            } else {
                Err(no_video())
            }
        };
        Box::pin(std::future::ready(r))
    }

    fn preview_keyframe(&self) {
        lock(&self.video).preview_keyframes += 1;
    }

    fn camera_permission(&self) -> PermissionState {
        lock(&self.video).camera_permission
    }

    fn request_camera_permission(&self) -> BoxFuture<'_, PermissionState> {
        let mut v = lock(&self.video);
        v.permission_requests += 1;
        if v.camera_permission == PermissionState::Prompt {
            v.camera_permission = v.prompt_answer;
        }
        Box::pin(std::future::ready(v.camera_permission))
    }

    fn app_scene(&self, scene: AppScene) {
        lock(&self.video).scenes.push(scene);
    }

    fn device_orientation(&self, degrees: u32) {
        lock(&self.video).orientations.push(degrees);
    }

    fn video_background_events(&self) -> Option<broadcast::Receiver<bool>> {
        Some(self.background_tx.subscribe())
    }

    #[cfg(feature = "video-core")]
    fn native_video_backend(&self) -> Option<Arc<dyn sew_call_client::kessel_video::VideoBackend>> {
        lock(&self.native_backend)
            .as_ref()
            .map(|b| Arc::clone(&b.0))
    }

    fn video_decoder(
        &self,
        spec: DecoderSpec,
        wake: DecoderWake,
    ) -> Result<Box<dyn FrameDecoder>, EngineError> {
        let open = lock(&self.video).decoder.as_ref().map(|o| Arc::clone(&o.0));
        match open {
            Some(open) => open(spec, wake),
            None => Err(EngineError::Failed(ErrorInfo::with_detail(
                video_code::DECODED_UNSUPPORTED,
                json!({ "codec": spec.codec }),
            ))),
        }
    }
}

// ───────────────────────────── the fake decoders ─────────────────────────────

/// The fake engine's decoders ([`FakeEngine::enable_fake_decoder`]): every decoder it opened
/// shares this. A decoder answers each accepted input with one frame: the input's coded size
/// (a keyframe's `width` x `height`, kept for the deltas) fitted into the decoder's box
/// (keeping the aspect ratio, even sizes, never larger), every byte the input's first byte,
/// the input's `arrival_us` as its timestamp. A delta before the first keyframe (or after a
/// reset) is `NeedKeyframe`.
#[derive(Debug, Default)]
pub struct FakeDecoders {
    state: Arc<Mutex<FakeDecoderState>>,
}

#[derive(Debug, Default)]
struct FakeDecoderState {
    opened: Vec<DecoderSpec>,
    submitted: Vec<(u8, bool)>,
    resets: usize,
    fits: Vec<(u32, u32)>,
    fail_submit: Option<DecodeFailure>,
    fail_output: Option<DecodeFailure>,
    hold: bool,
    /// Outputs held back ([`FakeDecoders::hold`]), with the wakes to call on release.
    held: Vec<(usize, Result<DecodedFrame, DecodeFailure>)>,
    wakes: Vec<WakeEntry>,
    /// Released outputs by decoder.
    ready: Vec<(usize, Result<DecodedFrame, DecodeFailure>)>,
}

struct WakeEntry(DecoderWake);

impl std::fmt::Debug for WakeEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Wake")
    }
}

impl FakeDecoders {
    /// Every decoder opened, in order.
    pub fn opened(&self) -> Vec<DecoderSpec> {
        lock(&self.state).opened.clone()
    }

    /// Every input accepted or refused: its first byte and whether it was a keyframe.
    pub fn submitted(&self) -> Vec<(u8, bool)> {
        lock(&self.state).submitted.clone()
    }

    /// How many resets the decoders had.
    pub fn resets(&self) -> usize {
        lock(&self.state).resets
    }

    /// Every `set_fit`, in order.
    pub fn fits(&self) -> Vec<(u32, u32)> {
        lock(&self.state).fits.clone()
    }

    /// The next input is refused with `e`.
    pub fn fail_next_submit(&self, e: DecodeFailure) {
        lock(&self.state).fail_submit = Some(e);
    }

    /// The next input's output is `e` instead of a frame.
    pub fn fail_next_output(&self, e: DecodeFailure) {
        lock(&self.state).fail_output = Some(e);
    }

    /// Hold outputs back (as a threaded decoder does) until [`FakeDecoders::release`].
    pub fn hold(&self) {
        lock(&self.state).hold = true;
    }

    /// Hand over the held outputs and call their decoders' wakes (from this thread, as a
    /// decoder thread would); outputs are no longer held.
    pub fn release(&self) {
        let wakes: Vec<DecoderWake> = {
            let mut st = lock(&self.state);
            st.hold = false;
            let held = std::mem::take(&mut st.held);
            let mut ids: Vec<usize> = held.iter().map(|(i, _)| *i).collect();
            ids.dedup();
            st.ready.extend(held);
            ids.iter()
                .filter_map(|i| st.wakes.get(*i).map(|w| Arc::clone(&w.0)))
                .collect()
        };
        for w in wakes {
            w();
        }
    }

    fn open(
        &self,
        spec: DecoderSpec,
        wake: DecoderWake,
    ) -> Result<Box<dyn FrameDecoder>, EngineError> {
        let mut st = lock(&self.state);
        st.opened.push(spec);
        st.wakes.push(WakeEntry(wake));
        let id = st.wakes.len() - 1;
        Ok(Box::new(FakeFrameDecoder {
            id,
            fit: spec.fit,
            format: spec.format,
            coded: None,
            keyed: false,
            state: Arc::clone(&self.state),
        }))
    }
}

/// One fake decoder.
struct FakeFrameDecoder {
    id: usize,
    fit: (u32, u32),
    format: DecodedFormat,
    /// The coded size (the last keyframe's).
    coded: Option<(u32, u32)>,
    /// A keyframe since the last reset.
    keyed: bool,
    state: Arc<Mutex<FakeDecoderState>>,
}

/// `width` x `height` inside `bw` x `bh`, keeping the aspect ratio, even, never larger.
fn fit_within(width: u32, height: u32, bw: u32, bh: u32) -> (u32, u32) {
    if width <= bw && height <= bh {
        return (width, height);
    }
    let even = |v: u64| u32::try_from((v / 2) * 2).unwrap_or(u32::MAX).max(2);
    // Scale by the tighter side, in integers.
    let (w, h, bw, bh) = (
        u64::from(width),
        u64::from(height),
        u64::from(bw),
        u64::from(bh),
    );
    if w * bh <= h * bw {
        (even(w * bh / h), even(bh))
    } else {
        (even(bw), even(h * bw / w))
    }
}

impl FrameDecoder for FakeFrameDecoder {
    fn submit(&mut self, frame: &EncodedVideoFrame) -> Result<(), DecodeFailure> {
        let mut st = lock(&self.state);
        let first = frame.data.first().copied().unwrap_or(0);
        st.submitted.push((first, frame.keyframe));
        if let Some(e) = st.fail_submit.take() {
            return Err(e);
        }
        if frame.keyframe {
            self.keyed = true;
            if frame.width > 0 && frame.height > 0 {
                self.coded = Some((frame.width, frame.height));
            }
        } else if !self.keyed {
            return Err(DecodeFailure::NeedKeyframe);
        }
        let out = match st.fail_output.take() {
            Some(e) => Err(e),
            None => {
                let (cw, ch) = self.coded.unwrap_or((320, 180));
                let (w, h) = fit_within(cw, ch, self.fit.0, self.fit.1);
                Ok(DecodedFrame {
                    format: self.format,
                    width: w,
                    height: h,
                    timestamp_us: frame.arrival_us,
                    data: vec![first; self.format.frame_len(w, h)],
                })
            }
        };
        let wake = if st.hold {
            st.held.push((self.id, out));
            None
        } else {
            st.ready.push((self.id, out));
            st.wakes.get(self.id).map(|w| Arc::clone(&w.0))
        };
        drop(st);
        if let Some(w) = wake {
            w();
        }
        Ok(())
    }

    fn poll(&mut self) -> Option<Result<DecodedFrame, DecodeFailure>> {
        let mut st = lock(&self.state);
        let i = st.ready.iter().position(|(id, _)| *id == self.id)?;
        Some(st.ready.remove(i).1)
    }

    fn reset(&mut self) {
        self.keyed = false;
        let id = self.id;
        let mut st = lock(&self.state);
        st.resets += 1;
        st.ready.retain(|(i, _)| *i != id);
        st.held.retain(|(i, _)| *i != id);
    }

    fn set_fit(&mut self, fit: (u32, u32)) {
        self.fit = fit;
        lock(&self.state).fits.push(fit);
    }
}

/// One fake call. Tests drive it with [`FakeCall::emit`] and its shortcuts.
pub struct FakeCall {
    call_id: Mutex<String>,
    self_member: Mutex<String>,
    tx: broadcast::Sender<CallEvent>,
    state: Mutex<CallFake>,
    /// A held join's way back in: the provider and the request (`consent` joins again).
    held: Mutex<Option<Held>>,
    /// How many times [`CallHandle::network_changed`] was called.
    network_changes: AtomicUsize,
    video: Mutex<FakeVideoCall>,
}

/// One fake call's video.
#[derive(Debug, Default)]
struct FakeVideoCall {
    enabled: bool,
    tx: Option<mpsc::Sender<VideoRecvItem>>,
    rx: Option<VideoFrames>,
    snapshot: VideoSnapshot,
    settings: Vec<(String, TrackSettings)>,
    keyframes: Vec<String>,
    stats: Vec<(String, SinkStats)>,
    cameras: Vec<CameraRequest>,
    screens: Vec<ScreenRequest>,
    next_camera_error: Option<EngineError>,
    next_screen_error: Option<EngineError>,
    scenes: Vec<AppScene>,
}

impl FakeVideoCall {
    fn new(enabled: bool) -> FakeVideoCall {
        let mut v = FakeVideoCall {
            enabled,
            ..FakeVideoCall::default()
        };
        if enabled {
            let (tx, rx) = mpsc::channel(FAKE_FRAME_QUEUE);
            v.tx = Some(tx);
            v.rx = Some(rx);
        }
        v
    }
}

struct Held {
    provider: Arc<dyn JoinProvider>,
    req: JoinRequest,
    prompt: ConsentRequired,
    auto_connect: bool,
}

impl std::fmt::Debug for FakeCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeCall")
            .field("call_id", &*lock(&self.call_id))
            .field("held", &lock(&self.held).is_some())
            .finish_non_exhaustive()
    }
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
    /// Every `consent` (`Some(version)`) and `withdraw_consent` (`None`), in order.
    consents: Vec<(bool, Option<u32>)>,
    next_consent_error: Option<EngineError>,
    /// Call details (0.4.1): what [`FakeCall::set_security`], [`FakeCall::set_roster`] and
    /// [`FakeCall::set_quality`] last said.
    security: Option<SecurityInfo>,
    roster: Vec<RosterEntry>,
    quality: QualityInfo,
}

fn me_of(answer: &JoinAnswer, req: &JoinRequest) -> Participant {
    let speak_denied = answer.join.state.speak_denied;
    Participant {
        member_id: answer.join.state.member_id.to_string(),
        is_self: true,
        muted: req.muted || req.deafened || speak_denied,
        deafened: req.deafened,
        server_muted: answer.join.state.server_mute,
        server_deafened: answer.join.state.server_deaf,
        speak_denied,
    }
}

/// A held join's `recording`: Sew's notice as the real engine shows it.
pub fn held_recording(c: &ConsentRequired) -> RecordingInfo {
    RecordingInfo::from_engine(
        &sew_call_client::CallRecording::from_consent_required(c),
        false,
    )
}

/// Sew's `409 recording_consent_required` details for a live recording `recording_id` with
/// notice `notice_version` (for tests).
pub fn consent_required(recording_id: u64, notice_version: u32) -> ConsentRequired {
    let details = json!({
        "recording": null,
        "auto": false,
        "consent_prompt": {
            "recording_id": recording_id.to_string(),
            "notice_version": notice_version,
            "recorder_name": "Sew Notes",
            "feature_label": "Sew Notes",
            "started_by": {"member_id": "1001", "display_name": "Alice"},
            "room": {"channel_id": "48831765147549698", "kind": "voice", "name": "lounge"},
            "audio_keep_days": 30,
            "transcript": {"posted_to_channel": true, "readable_by": ["channel_members"], "e2ee": false},
            "admin_download": true,
            "decline_mode": "join_unrecorded",
            "recorded_now": ["1001"]
        }
    });
    match ConsentRequired::from_details(&details) {
        Some(c) => c,
        None => unreachable!("the fake's consent prompt parses"),
    }
}

impl FakeCall {
    fn held(
        c: &ConsentRequired,
        provider: Arc<dyn JoinProvider>,
        req: JoinRequest,
        auto_connect: bool,
        video: bool,
    ) -> FakeCall {
        let (tx, _) = broadcast::channel(64);
        FakeCall {
            video: Mutex::new(FakeVideoCall::new(video)),
            call_id: Mutex::new(String::new()),
            self_member: Mutex::new(String::new()),
            tx,
            state: Mutex::new(CallFake {
                snapshot: CallSnapshot {
                    state: CallState::ConsentRequired {
                        recording_id: c.consent_prompt.recording_id.to_string(),
                        notice_version: c.notice_version(),
                    },
                    participants: Vec::new(),
                    recording: Some(held_recording(c)),
                },
                muted_before_deafen: req.deafened.then_some(req.muted),
                mutes: Vec::new(),
                speak_saved: BTreeMap::new(),
                left: false,
                moved_room: None,
                next_mute_error: None,
                next_leave_error: None,
                consents: Vec::new(),
                next_consent_error: None,
                security: None,
                roster: Vec::new(),
                quality: QualityInfo::unknown(),
            }),
            held: Mutex::new(Some(Held {
                provider,
                req,
                prompt: c.clone(),
                auto_connect,
            })),
            network_changes: AtomicUsize::new(0),
        }
    }

    fn new(answer: &JoinAnswer, req: &JoinRequest, auto_connect: bool, video: bool) -> FakeCall {
        let (tx, _) = broadcast::channel(64);
        let self_member = answer.join.state.member_id.to_string();
        let speak_denied = answer.join.state.speak_denied;
        let me = me_of(answer, req);
        let mut speak_saved = BTreeMap::new();
        if speak_denied {
            speak_saved.insert(self_member.clone(), req.muted || req.deafened);
        }
        FakeCall {
            video: Mutex::new(FakeVideoCall::new(video)),
            call_id: Mutex::new(answer.join.room.as_str().to_owned()),
            self_member: Mutex::new(self_member),
            tx,
            held: Mutex::new(None),
            network_changes: AtomicUsize::new(0),
            state: Mutex::new(CallFake {
                snapshot: CallSnapshot {
                    state: if auto_connect {
                        CallState::Connected
                    } else {
                        CallState::Connecting
                    },
                    participants: vec![me],
                    recording: None,
                },
                muted_before_deafen: req.deafened.then_some(req.muted),
                mutes: Vec::new(),
                speak_saved,
                left: false,
                moved_room: None,
                next_mute_error: None,
                next_leave_error: None,
                consents: Vec::new(),
                next_consent_error: None,
                security: None,
                roster: Vec::new(),
                quality: QualityInfo::unknown(),
            }),
        }
    }

    /// Puts one item on the call's frame channel (as the engine would); false when the
    /// channel is full, closed or the call has no video.
    pub fn push_video(&self, item: VideoRecvItem) -> bool {
        let tx = lock(&self.video).tx.clone();
        tx.is_some_and(|tx| tx.try_send(item).is_ok())
    }

    /// Closes the call's frame channel (the engine stopped its video).
    pub fn close_video(&self) {
        lock(&self.video).tx = None;
    }

    /// The call's video tracks changed: the snapshot and a [`CallEvent::Video`].
    pub fn set_video_tracks(&self, tracks: Vec<VideoTrack>) {
        lock(&self.video).snapshot.tracks.clone_from(&tracks);
        let _ = self.tx.send(CallEvent::Video(tracks));
    }

    /// This device's camera or screen changed: the snapshot and a [`CallEvent::LocalVideo`].
    pub fn set_local_video(&self, local: LocalVideo) {
        lock(&self.video).snapshot.local = local.clone();
        let _ = self.tx.send(CallEvent::LocalVideo(local));
    }

    /// The room's media policy changed: the snapshot and a [`CallEvent::Policy`].
    pub fn set_policy(&self, policy: serde_json::Value) {
        lock(&self.video).snapshot.policy = Some(policy.clone());
        let _ = self.tx.send(CallEvent::Policy(policy));
    }

    /// Replace the video snapshot without an event (a missed event).
    pub fn set_video_snapshot_silently(&self, snap: VideoSnapshot) {
        lock(&self.video).snapshot = snap;
    }

    /// Every track setting the plugin handed the engine, in order.
    pub fn video_settings(&self) -> Vec<(String, TrackSettings)> {
        lock(&self.video).settings.clone()
    }

    /// Every [`CallHandle::app_scene`] the plugin passed (the combined signal), in order.
    pub fn app_scenes(&self) -> Vec<AppScene> {
        lock(&self.video).scenes.clone()
    }

    /// Every keyframe request, in order (by track).
    pub fn keyframe_requests(&self) -> Vec<String> {
        lock(&self.video).keyframes.clone()
    }

    /// Every sink-stats report, in order.
    pub fn video_stats(&self) -> Vec<(String, SinkStats)> {
        lock(&self.video).stats.clone()
    }

    /// Every `camera` request, in order.
    pub fn camera_requests(&self) -> Vec<CameraRequest> {
        lock(&self.video).cameras.clone()
    }

    /// Every `screen` request, in order.
    pub fn screen_requests(&self) -> Vec<ScreenRequest> {
        lock(&self.video).screens.clone()
    }

    /// The next `camera` fails with this (the camera stays off, `local_video` says why).
    pub fn fail_next_camera(&self, e: EngineError) {
        lock(&self.video).next_camera_error = Some(e);
    }

    /// The next `screen` fails with this.
    pub fn fail_next_screen(&self, e: EngineError) {
        lock(&self.video).next_screen_error = Some(e);
    }

    /// `camera` or `screen`: asking, then on (or off), as `local_video` events.
    fn local_source(&self, camera: bool, on: bool) -> Result<(), EngineError> {
        let (err, enabled) = {
            let mut v = lock(&self.video);
            let err = if camera {
                v.next_camera_error.take()
            } else {
                v.next_screen_error.take()
            };
            (err, v.enabled)
        };
        if !enabled {
            return Err(no_video());
        }
        let set = |state: LocalVideoState, reason: Option<String>| {
            let mut local = lock(&self.video).snapshot.local.clone();
            if camera {
                local.camera = state;
            } else {
                local.screen = state;
            }
            local.reason = reason;
            self.set_local_video(local);
        };
        if on {
            set(LocalVideoState::Asking, None);
        }
        match err {
            Some(e) => {
                set(LocalVideoState::Off, Some(e.to_info().code));
                Err(e)
            }
            None => {
                set(
                    if on {
                        LocalVideoState::On
                    } else {
                        LocalVideoState::Off
                    },
                    None,
                );
                Ok(())
            }
        }
    }

    /// This device's member id (empty while the join is held for the notice).
    pub fn self_member(&self) -> String {
        lock(&self.self_member).clone()
    }

    /// Sew's recording as the real engine passes it on (`None`: none active): the snapshot and
    /// a `recording` event.
    pub fn set_recording(&self, recording: Option<RecordingInfo>) {
        lock(&self.state).snapshot.recording.clone_from(&recording);
        let _ = self.tx.send(CallEvent::Recording(recording));
    }

    /// Every `consent` (`(granted, Some(notice_version))`) and `withdraw_consent`
    /// (`(false, None)`), in order.
    pub fn consents(&self) -> Vec<(bool, Option<u32>)> {
        lock(&self.state).consents.clone()
    }

    /// The next `consent` or `withdraw_consent` in a call fails with this.
    pub fn fail_next_consent(&self, e: EngineError) {
        lock(&self.state).next_consent_error = Some(e);
    }

    /// How many times the plugin passed a network change to this call.
    pub fn network_changes(&self) -> usize {
        self.network_changes.load(Ordering::Acquire)
    }

    /// Whether the join is held for the recording notice.
    pub fn is_held(&self) -> bool {
        lock(&self.held).is_some()
    }

    /// A held join answered: join again with the answer, as the real engine does.
    async fn answer_held(&self, granted: bool, notice_version: u32) -> Result<(), EngineError> {
        let Some((provider, mut req, prompt, auto_connect)) = lock(&self.held).as_ref().map(|h| {
            (
                h.provider.clone(),
                h.req.clone(),
                h.prompt.clone(),
                h.auto_connect,
            )
        }) else {
            return Err(EngineError::failed(crate::wire::code::NO_RECORDING));
        };
        let mut notice = prompt.notice(granted);
        notice.notice_version = Some(notice_version);
        req.recording_notice = Some(notice);
        match provider.join(req.clone()).await {
            Ok(answer) => {
                *lock(&self.held) = None;
                *lock(&self.call_id) = answer.join.room.as_str().to_owned();
                *lock(&self.self_member) = answer.join.state.member_id.to_string();
                let me = me_of(&answer, &req);
                self.set_recording(None);
                self.emit(CallEvent::Participants(vec![me]));
                self.emit(CallEvent::State(if auto_connect {
                    CallState::Connected
                } else {
                    CallState::Connecting
                }));
                Ok(())
            }
            Err(ProviderError::ConsentRequired(next)) => {
                if let Some(h) = lock(&self.held).as_mut() {
                    h.prompt = (*next).clone();
                }
                // As the real engine: the snapshot first, then the notice and the state.
                let held = CallState::ConsentRequired {
                    recording_id: next.consent_prompt.recording_id.to_string(),
                    notice_version: next.notice_version(),
                };
                lock(&self.state).snapshot.state = held.clone();
                self.set_recording(Some(held_recording(&next)));
                self.emit(CallEvent::State(held));
                Err(EngineError::Provider(ProviderError::ConsentRequired(next)))
            }
            Err(e) => {
                let err = EngineError::Provider(e);
                *lock(&self.held) = None;
                self.emit(CallEvent::Error(err.to_info()));
                self.end(&err.end_reason());
                Err(err)
            }
        }
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

    /// The key state and security code, as the real engine reports them (0.4.1): kept for
    /// [`CallHandle::security`] and sent as a `Security` event.
    pub fn set_security(&self, security: Option<SecurityInfo>) {
        lock(&self.state).security.clone_from(&security);
        let _ = self.tx.send(CallEvent::Security(security));
    }

    /// The media roster (engine identities and labels; 0.4.1): kept and sent as a `Roster`
    /// event.
    pub fn set_roster(&self, roster: Vec<RosterEntry>) {
        lock(&self.state).roster.clone_from(&roster);
        let _ = self.tx.send(CallEvent::Roster(roster));
    }

    /// A stats window's quality (0.4.1): kept and sent as a `Quality` event.
    pub fn set_quality(&self, quality: QualityInfo) {
        lock(&self.state).quality = quality.clone();
        let _ = self.tx.send(CallEvent::Quality(quality));
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
        lock(&self.call_id).clone()
    }

    fn consent(
        &self,
        granted: bool,
        notice_version: u32,
    ) -> BoxFuture<'_, Result<(), EngineError>> {
        Box::pin(async move {
            lock(&self.state)
                .consents
                .push((granted, Some(notice_version)));
            if self.is_held() {
                return self.answer_held(granted, notice_version).await;
            }
            match lock(&self.state).next_consent_error.take() {
                Some(e) => Err(e),
                None => Ok(()),
            }
        })
    }

    fn withdraw_consent(&self) -> BoxFuture<'_, Result<(), EngineError>> {
        Box::pin(async move {
            lock(&self.state).consents.push((false, None));
            let version = lock(&self.held).as_ref().map(|h| h.prompt.notice_version());
            if let Some(v) = version {
                return self.answer_held(false, v).await;
            }
            match lock(&self.state).next_consent_error.take() {
                Some(e) => Err(e),
                None => Ok(()),
            }
        })
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

    fn security(&self) -> Option<SecurityInfo> {
        lock(&self.state).security.clone()
    }

    fn roster(&self) -> Vec<RosterEntry> {
        lock(&self.state).roster.clone()
    }

    fn quality(&self) -> QualityInfo {
        lock(&self.state).quality.clone()
    }

    fn network_changed(&self) {
        self.network_changes.fetch_add(1, Ordering::AcqRel);
    }

    fn video_frames(&self) -> Option<VideoFrames> {
        lock(&self.video).rx.take()
    }

    fn video_snapshot(&self) -> VideoSnapshot {
        lock(&self.video).snapshot.clone()
    }

    fn set_video_settings(&self, track: &str, settings: TrackSettings) {
        lock(&self.video)
            .settings
            .push((track.to_owned(), settings));
    }

    fn request_keyframe(&self, track: &str) {
        lock(&self.video).keyframes.push(track.to_owned());
    }

    fn video_sink_stats(&self, track: &str, stats: SinkStats) {
        lock(&self.video).stats.push((track.to_owned(), stats));
    }

    fn app_scene(&self, scene: AppScene) {
        lock(&self.video).scenes.push(scene);
    }

    fn camera(&self, req: CameraRequest) -> BoxFuture<'_, Result<(), EngineError>> {
        let on = req.on;
        lock(&self.video).cameras.push(req);
        Box::pin(std::future::ready(self.local_source(true, on)))
    }

    fn screen(&self, req: ScreenRequest) -> BoxFuture<'_, Result<(), EngineError>> {
        let on = req.on;
        lock(&self.video).screens.push(req);
        Box::pin(std::future::ready(self.local_source(false, on)))
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
                let me_id = lock(&self.self_member).clone();
                let held = s.speak_saved.get(&me_id).copied();
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
                    s.speak_saved.insert(me_id, own);
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
    /// Refusals for the next joins, first (then `result`, then the answer).
    first: Mutex<VecDeque<ProviderError>>,
    gate: Option<Arc<Semaphore>>,
    calls: Mutex<Vec<JoinRequest>>,
}

impl FakeProvider {
    /// Answers every join as `member_id` (a Sew id: a decimal string).
    pub fn answering(member_id: &str) -> FakeProvider {
        FakeProvider {
            member_id: member_id.into(),
            result: Mutex::new(None),
            first: Mutex::new(VecDeque::new()),
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

    /// The next join is refused with `e` (once; queued after earlier ones), e.g. Sew's
    /// [`ProviderError::ConsentRequired`] for a recorded room.
    pub fn refuse_next(&self, e: ProviderError) {
        lock(&self.first).push_back(e);
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
            if let Some(e) = lock(&self.first).pop_front() {
                return Err(e);
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

/// Records every [`VoiceListener`] call as one `{"<method>": <argument as JSON>}` object, in
/// order (the shape of `tests/sew-native-voice-listener.json`), for tests of a native shim.
#[derive(Debug, Default)]
pub struct RecordingListener {
    calls: Mutex<Vec<serde_json::Value>>,
}

impl RecordingListener {
    /// An empty recorder.
    pub fn new() -> RecordingListener {
        RecordingListener::default()
    }

    /// Every call so far.
    pub fn calls(&self) -> Vec<serde_json::Value> {
        lock(&self.calls).clone()
    }

    /// Forget what was recorded.
    pub fn clear(&self) {
        lock(&self.calls).clear();
    }

    fn push(&self, method: &str, arg: serde_json::Value) {
        let mut call = serde_json::Map::new();
        call.insert(method.to_owned(), arg);
        lock(&self.calls).push(serde_json::Value::Object(call));
    }
}

/// A listener argument as JSON (plain structs of strings, bools and numbers: never fails).
fn to_json<T: serde::Serialize + ?Sized>(v: &T) -> serde_json::Value {
    serde_json::to_value(v).unwrap_or(serde_json::Value::Null)
}

impl VoiceListener for RecordingListener {
    fn on_state(&self, state: &StateEvent) {
        self.push("on_state", to_json(state));
    }

    fn on_participants(&self, participants: &[Participant]) {
        self.push("on_participants", to_json(participants));
    }

    fn on_self_mute(&self, me: &SelfMute) {
        self.push("on_self_mute", to_json(me));
    }

    fn on_recording(&self, recording: Option<&RecordingInfo>) {
        self.push("on_recording", to_json(&recording));
    }

    fn on_error(&self, error: &ErrorInfo) {
        self.push("on_error", to_json(error));
    }

    fn on_devices(&self, devices: &DeviceList) {
        self.push("on_devices", to_json(devices));
    }

    fn on_incoming(&self, incoming: Option<&IncomingCall>) {
        self.push("on_incoming", to_json(&incoming));
    }

    fn video_changed(&self, has_video: bool) {
        self.push("video_changed", serde_json::Value::Bool(has_video));
    }

    fn screen_share_changed(&self, active: bool) {
        self.push("screen_share_changed", serde_json::Value::Bool(active));
    }
}

/// A webview sink's channel in memory: records every message; [`RecordingFrameChannel::close`]
/// makes the next send fail (the webview went away).
#[derive(Debug, Default)]
pub struct RecordingFrameChannel {
    messages: Mutex<Vec<Vec<u8>>>,
    closed: AtomicBool,
}

impl RecordingFrameChannel {
    /// An open channel.
    pub fn new() -> RecordingFrameChannel {
        RecordingFrameChannel::default()
    }

    /// Every message sent, in order.
    pub fn messages(&self) -> Vec<Vec<u8>> {
        lock(&self.messages).clone()
    }

    /// Every message's header and payload, in order (a message that doesn't parse panics:
    /// the framing is wrong).
    pub fn frames(&self) -> Vec<(FrameHeader, Vec<u8>)> {
        self.messages()
            .iter()
            .map(|m| match FrameHeader::parse(m) {
                Some(h) => (h, m[HEADER_LEN..].to_vec()),
                None => panic!("a sink message that doesn't parse: {} bytes", m.len()),
            })
            .collect()
    }

    /// Forget the messages so far.
    pub fn clear(&self) {
        lock(&self.messages).clear();
    }

    /// The webview is gone: sends fail from now on.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }
}

impl FrameChannel for RecordingFrameChannel {
    fn send(&self, message: Vec<u8>) -> bool {
        if self.closed.load(Ordering::Acquire) {
            return false;
        }
        lock(&self.messages).push(message);
        true
    }
}
