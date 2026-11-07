//! The native side of the plugin (0.3.0): a [`VoiceListener`] that observes the call whether or
//! not a webview exists or is awake (a CallKit or Telecom shim), and [`VoiceControl`], the handle
//! that shim acts through.
//!
//! **Threading and order.** The listener's methods are called from the plugin's own task on
//! the async runtime (never the main thread or the webview's), in order, one at a time, with
//! the plugin's state lock held: the guarantee the webview's events have, and the same data
//! (each call follows the matching `sew-voice://` event). So a method must not block and must
//! not call [`VoiceControl`] (it would deadlock on the lock): copy what you need and hop to your
//! own queue (CallKit: `DispatchQueue.main.async` or the provider's queue; Telecom: the main
//! looper). A slow listener delays events; it never loses them. On wake, read
//! [`VoiceControl::snapshot`] instead of replaying events.

use sew_call_client::device::DeviceKeyId;

use crate::details::{CallDetails, IncomingCall, RingEndedArgs};
use crate::engine::{AppScene, AudioSessionEvent};
use crate::service::VoiceService;
use crate::wire::{
    CameraArgs, ConsentArgs, DeviceList, ErrorInfo, JoinArgs, JoinOptions, JoinResult, MuteArgs,
    Participant, PermissionState, RecordingInfo, SelfMute, StateEvent, StateSnapshot,
};

/// A native observer of the plugin's call (`Builder::listener`, at most one). The plugin calls
/// it whether or not a webview exists or is awake. Every method has an empty default, so you
/// implement only what you need. The [module docs](crate::listener) give the thread and the
/// order.
pub trait VoiceListener: Send + Sync + 'static {
    /// The call's state changed (idle, connecting, connected, reconnecting{attempt},
    /// consent_required{recording_id, notice_version}, ended{reason}), with the call's target
    /// and room. Also called after a move (same state, new room).
    fn on_state(&self, state: &StateEvent) {
        let _ = state;
    }

    /// The full participant list, after any change: join, leave, mute, deafen, a moderator
    /// mute, SPEAK lost or regained. Your own row has `is_self = true`; its `muted`/`deafened`
    /// are your self-mute, which is what CallKit's mute button mirrors.
    fn on_participants(&self, participants: &[Participant]) {
        let _ = participants;
    }

    /// Your own mute or deafen changed, from any source: your command, the webview, a
    /// moderator (`server_muted`), or SPEAK being lost. A convenience over
    /// [`VoiceListener::on_participants`] (called right after it, or after a mute command that
    /// changes only your own choice), so CallKit doesn't diff the list. It is not called while
    /// the state is idle: the first call comes with your row in the call, and none comes when
    /// the call ends (the list empties).
    fn on_self_mute(&self, me: &SelfMute) {
        let _ = me;
    }

    /// The recording, as the webview's `recording` event: the recording, `my_consent`
    /// (pending, granted, declined or withdrawn), Sew's `consent_prompt` while you're pending,
    /// the participants' answers and `recorder_present`. `None` once no recording is active. A
    /// held join is `on_state(consent_required)` followed by this event with the prompt.
    /// `ConsentPrompt::title_en` and `text_en` (sew-proto) give the kit's words for it.
    fn on_recording(&self, recording: Option<&RecordingInfo>) {
        let _ = recording;
    }

    /// An error the webview would see on `errors` (code, message, detail). Ending errors also
    /// arrive as `on_state(ended{reason})`.
    fn on_error(&self, error: &ErrorInfo) {
        let _ = error;
    }

    /// Audio devices or the route changed (desktop enumeration; on phones the OS route).
    fn on_devices(&self, devices: &DeviceList) {
        let _ = devices;
    }

    /// A DM call rings (`Some`: CallKit's `reportNewIncomingCall`, Telecom's
    /// `addNewIncomingCall`), or stopped ringing (`None`: accepted by a `join` with its target
    /// and room, ended, or declined), as the webview's `incoming` event (0.4.1). A newer ring
    /// replaces the one before.
    fn on_incoming(&self, incoming: Option<&IncomingCall>) {
        let _ = incoming;
    }

    /// The call's video started or stopped: `true` from the moment this device sends its
    /// camera or screen (on or paused) or a remote camera or screen is unmuted, `false` when
    /// none is left (and when the call ends). Called right after the `video` or `local_video`
    /// event that changed it, only on a change. Route the call's audio to the speaker while it
    /// is `true` (iOS: `.defaultToSpeaker` or `overrideOutputAudioPort(.speaker)`; Android:
    /// `setCommunicationDevice` with the built-in speaker, or `setSpeakerphoneOn`), unless the
    /// person chose a route (a headset, Bluetooth); report `hasVideo` to CallKit's
    /// `CXCallUpdate` (Telecom: `setVideoState`). The plugin never sets the route itself.
    fn video_changed(&self, has_video: bool) {
        let _ = has_video;
    }

    /// This device's screen share started (`true`: its track is published, on or paused) or
    /// stopped (`false`, also when the call ends). Called right after the `local_video` event
    /// that changed it, only on a change. On iOS it follows the broadcast extension
    /// ([`VoiceControl::screen_share_ring`]): the share starts with its first frame and stops
    /// when the extension finishes or goes away, both outside the app, so the app's UI follows
    /// this instead of its own button.
    fn screen_share_changed(&self, active: bool) {
        let _ = active;
    }

    // There is no on_speaking: levels arrive up to 10 a second and CallKit doesn't need them.
}

/// What a CallKit or Telecom shim calls: the same [`VoiceService`] the webview's commands use,
/// managed as Tauri state (`app.state::<VoiceControl>().inner().clone()`). The async methods
/// run on `tauri::async_runtime` (or any tokio runtime); none may be called from inside a
/// [`VoiceListener`] method.
#[derive(Clone, Debug)]
pub struct VoiceControl {
    service: VoiceService,
}

impl VoiceControl {
    /// The control over `service` (the plugin makes one; tests make their own).
    pub fn new(service: VoiceService) -> VoiceControl {
        VoiceControl { service }
    }

    /// `CXStartCallAction` / answer: the webview's `join`. On a phone the engine connects and
    /// gets keys, and opens the microphone at [`VoiceControl::session_activated`]. A held
    /// consent join answers `call_id: ""` with `on_state(consent_required)` and then
    /// `on_recording(Some(prompt))`: answer with [`VoiceControl::consent`], or give up with
    /// [`VoiceControl::leave`].
    pub async fn join(&self, args: JoinArgs) -> Result<JoinResult, ErrorInfo> {
        self.service.join(args).await
    }

    /// A video call's start or answer (`CXStartCallAction` with `isVideo`, a Telecom video
    /// state): [`VoiceControl::join`] with [`JoinOptions`]. With `JoinOptions::with_camera()`
    /// the camera turns on as soon as the call is connected and the app is in front (never
    /// from the lock screen: answered there, it starts at the app's next
    /// `app_scene(Foreground)`). The OS permission is never asked for here: without it, and
    /// on any other refusal (`video_slots_full`, `video_not_allowed`, …), `local_video` says
    /// `{camera: "off", reason: <code>}` and the call goes on without the camera.
    pub async fn join_with(
        &self,
        args: JoinArgs,
        options: JoinOptions,
    ) -> Result<JoinResult, ErrorInfo> {
        self.service.join_with(args, options).await
    }

    /// CallKit's or Telecom's video button (the call's video turned on or off from the
    /// system UI; iOS brings the app forward first): the webview's `camera {on, device?}`.
    /// `device` is a `video_devices` id or `"default"` (`None`: the chosen camera). Answers
    /// once publishing (or off), with the same refusals, among them
    /// `camera_permission_denied` and `camera_permission_not_determined`: the plugin never
    /// prompts (call [`VoiceControl::request_camera_permission`] first). Then
    /// [`VoiceListener::video_changed`] follows. An off here says `local_video`
    /// `{camera: "off", reason: "system_ui"}` (the webview's own button says `unpublished`).
    pub async fn set_camera(&self, on: bool, device: Option<&str>) -> Result<(), ErrorInfo> {
        self.service
            .camera_from(
                CameraArgs {
                    on,
                    device: device.map(str::to_owned),
                },
                Some(crate::video::SYSTEM_UI),
            )
            .await
    }

    /// `CXEndCallAction`. Safe to repeat: idle or ended returns `Ok`.
    pub async fn leave(&self) -> Result<(), ErrorInfo> {
        self.service.leave().await
    }

    /// `CXSetMutedCallAction`. `set_muted(false)` while you're server-muted or SPEAK-denied
    /// returns `Ok` and records `self_muted = false`, but `muted` stays true: show
    /// [`SelfMute::muted`]. With no call it is refused with `not_in_call`, as the webview's
    /// `mute`.
    pub async fn set_muted(&self, muted: bool) -> Result<(), ErrorInfo> {
        self.service
            .mute(MuteArgs {
                muted: Some(muted),
                deafened: None,
            })
            .await
    }

    /// Self-deafen on or off (deafened implies muted). With no call: `not_in_call`.
    pub async fn set_deafened(&self, deafened: bool) -> Result<(), ErrorInfo> {
        self.service
            .mute(MuteArgs {
                muted: None,
                deafened: Some(deafened),
            })
            .await
    }

    /// The lock-screen answer to the recording notice: echo the prompt's `notice_version`. A
    /// stale version is refused with `recording_consent_required` and the new notice in
    /// `detail`: show it again.
    pub async fn consent(&self, granted: bool, notice_version: u32) -> Result<(), ErrorInfo> {
        self.service
            .consent(ConsentArgs {
                granted,
                notice_version,
            })
            .await
    }

    /// "Stop recording me".
    pub async fn withdraw_consent(&self) -> Result<(), ErrorInfo> {
        self.service.withdraw_consent().await
    }

    /// The state now: state, target, room, participants, call_id and recording. Sync and
    /// cheap; safe from any thread except inside a listener method.
    pub fn snapshot(&self) -> StateSnapshot {
        self.service.snapshot()
    }

    /// `NWPathMonitor` / `ConnectivityManager`: the network changed. The live call resumes now
    /// on a fresh TURN-TLS socket. With no call it does nothing.
    pub fn network_changed(&self) {
        self.service.network_changed();
    }

    /// `CXProviderDelegate` `didActivate` (on Android: the Telecom connection is active).
    /// Forwarded to kessel-audio unchanged.
    pub fn session_activated(&self) {
        self.service.audio_session(AudioSessionEvent::Activated);
    }

    /// `didDeactivate`. Forwarded to kessel-audio unchanged.
    pub fn session_deactivated(&self) {
        self.service.audio_session(AudioSessionEvent::Deactivated);
    }

    /// An interruption began. Forwarded to kessel-audio unchanged.
    pub fn suspend(&self) {
        self.service.audio_session(AudioSessionEvent::Suspend);
    }

    /// The interruption ended. Forwarded to kessel-audio unchanged.
    pub fn resume(&self) {
        self.service.audio_session(AudioSessionEvent::Resume);
    }

    /// The OS route changed. Forwarded to kessel-audio unchanged.
    pub fn route_changed(&self) {
        self.service.audio_session(AudioSessionEvent::RouteChanged);
    }

    /// The app went to the background (iOS `sceneDidEnterBackground` /
    /// `applicationDidEnterBackground`, Android `onPause`/`onStop`) or came back
    /// (`sceneWillEnterForeground`, `onResume`). Not an audio interruption: `suspend` and
    /// `resume` stay for those and never touch video. In the background every webview sink is
    /// hidden (its track turned off at the media server) and only native sinks
    /// (`VoiceControl::open_native_sink`, the picture in picture) stay enabled; back in front
    /// the sinks start again at a keyframe. The video backend's own signal (kessel-video's
    /// `BackendEvent::Background`) counts too: either one pauses, and both must say foreground
    /// to resume. On Android it also tells kessel-video (`AndroidContext::set_background`).
    /// Audio is untouched. Sync and cheap; any thread except inside a listener method.
    pub fn app_scene(&self, scene: AppScene) {
        self.service.app_scene(scene);
    }

    /// Android: the device turned, in degrees clockwise from its natural orientation
    /// (`OrientationEventListener`; from `Display.getRotation()`, `(360 - 90 * rotation) %
    /// 360`). Camera frames are turned upright from the next one. iOS and desktop ignore it.
    pub fn device_orientation(&self, degrees: u32) {
        self.service.device_orientation(degrees);
    }

    /// The camera permission now. Never prompts.
    pub async fn camera_permission(&self) -> PermissionState {
        self.service
            .camera_permission()
            .await
            .unwrap_or(PermissionState::Unsupported)
    }

    /// Asks for the camera: iOS shows the OS prompt when the permission is `prompt`; Android
    /// reports the state (ask for CAMERA yourself first: it needs an Activity callback).
    pub async fn request_camera_permission(&self) -> PermissionState {
        self.service
            .request_camera_permission()
            .await
            .unwrap_or(PermissionState::Unsupported)
    }

    /// iPhone screen share: arm the broadcast upload extension's ring for the live call. The
    /// app passes its App Group container (Swift: `FileManager.default.containerURL(
    /// forSecurityApplicationGroupIdentifier: <the app's App Group id>)`, shared by the app and
    /// the extension); the plugin makes the ring at `<container>/sew-screen.ring`, which the
    /// extension attaches to. From then on the plugin reads the newest frame about every 10 ms
    /// and publishes it as the call's screen (the same encoder, codec and layers as a desktop
    /// screen; at most 720p). The share starts with the extension's first frame and ends when
    /// it finishes, dies, stays quiet 2 s without having said it runs, or the ring is lost;
    /// [`VoiceListener::screen_share_changed`] follows. `screen {on: false}` stops a share and
    /// asks the extension to finish. The ring lasts until the call ends (it is then removed and
    /// the extension asked to finish); arming again replaces it. Refused with `not_in_call`, or
    /// `screen_unsupported` outside iOS or in a build without `ios-screen-ring` and
    /// `video-core`; a container the ring can't be made in is `screen_unsupported` with the
    /// reason in `detail`.
    pub async fn screen_share_ring(
        &self,
        app_group_container: impl Into<std::path::PathBuf>,
    ) -> Result<(), ErrorInfo> {
        self.service
            .screen_share_ring(app_group_container.into())
            .await
    }

    /// Whether the call has video now (what [`VoiceListener::video_changed`] last said).
    pub fn has_video(&self) -> bool {
        self.service.has_video()
    }

    /// A native sink (feature `video-core`; [`crate::native`]): `target`'s frames in Rust for
    /// `consumer`, at most `max_size` (device pixels; `(640, 360)` asks the media server for
    /// its 360p layer). [`NativeSinkTarget::ActiveSpeaker`](crate::native::NativeSinkTarget)
    /// follows whoever speaks and asks for a keyframe on each switch. It is the one sink that
    /// stays enabled in the background. H.264 tracks come as access units, VP8 tracks decoded.
    /// Refused with `not_in_call`, `unknown_track` or `decoded_unsupported` (a VP8 track
    /// without a video backend). Drop the [`crate::native::NativeSink`] to close it.
    #[cfg(feature = "video-core")]
    pub fn open_native_sink(
        &self,
        target: crate::native::NativeSinkTarget,
        max_size: (u32, u32),
        consumer: std::sync::Arc<dyn crate::native::NativeVideoConsumer>,
    ) -> Result<crate::native::NativeSink, ErrorInfo> {
        self.service.open_native_sink(target, max_size, consumer)
    }

    /// Sign-out from one Sew install: deletes every device of `install_id` in the plugin's
    /// device store, calling the host's `HardwareDeviceKey::delete` for each hardware key, and
    /// the install's pins; answers the removed keys. Refused with `in_call` while a call is
    /// live: leave first. A key that can't be deleted fails with `device_store` and stays
    /// listed, so a retry finds it.
    pub async fn forget_server(&self, install_id: u64) -> Result<Vec<DeviceKeyId>, ErrorInfo> {
        self.service.forget_server(install_id).await
    }

    /// Every device key the plugin's device store holds, sorted (with a hardware key, one
    /// Keychain item or Keystore alias each: `DeviceKeyId::account`).
    pub async fn known_device_keys(&self) -> Result<Vec<DeviceKeyId>, ErrorInfo> {
        self.service.known_device_keys().await
    }

    /// A push said a DM call rings (0.4.1): every window gets `incoming` and the listener
    /// `on_incoming`. Answer it with [`VoiceControl::join`] on its `target` and `room` (that
    /// clears it); a decline is your own request to Sew, then [`VoiceControl::ring_ended`].
    pub fn ring(&self, ring: IncomingCall) -> Result<(), ErrorInfo> {
        self.service.ring(ring)
    }

    /// The ring of `call_id` stopped (0.4.1). Another call's id changes nothing.
    pub fn ring_ended(&self, call_id: &str) {
        let _ = self.service.ring_ended(RingEndedArgs {
            call_id: call_id.to_owned(),
        });
    }

    /// The ring now, if a DM call rings (0.4.1).
    pub fn incoming(&self) -> Option<IncomingCall> {
        self.service.incoming()
    }

    /// The call's key state and security code, quality, media roster and ring now (0.4.1;
    /// the `call_details` command).
    pub fn details(&self) -> CallDetails {
        self.service.details()
    }
}
