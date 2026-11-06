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

use crate::engine::AudioSessionEvent;
use crate::service::VoiceService;
use crate::wire::{
    ConsentArgs, DeviceList, ErrorInfo, JoinArgs, JoinResult, MuteArgs, Participant, RecordingInfo,
    SelfMute, StateEvent, StateSnapshot,
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
}
