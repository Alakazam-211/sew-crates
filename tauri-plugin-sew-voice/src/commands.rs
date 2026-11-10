//! The Tauri commands: the eight of `sew-voice:default`, the two device-key commands, the
//! three call-detail commands and the fourteen video commands, each set behind its own
//! permissions. Each one is a thin call into [`VoiceService`]; the argument names are the JSON
//! keys the webview sends (`invoke('plugin:sew-voice|join', {target, room, muted, deafened})`,
//! `invoke('plugin:sew-voice|consent', {granted, notice_version})`: snake_case, as every other
//! key on the wire).

use std::sync::Arc;

use tauri::ipc::{Channel, InvokeResponseBody};
use tauri::{Runtime, State, Window};

use crate::details::{CallDetails, IncomingCall, RingEndedArgs};
use crate::service::VoiceService;
use crate::sink::FrameChannel;
use crate::video::ParentWindow;
use crate::wire::{
    CameraArgs, CodecPreferenceInfo, ConsentArgs, DeviceKeyInfo, DeviceList, ErrorInfo,
    ForgetServerArgs, JoinArgs, JoinOptions, JoinResult, MuteArgs, PermissionState, PreviewAnswer,
    PreviewArgs, ScreenArgs, ScreenSource, SetDeviceArgs, SetVideoCodecPreferenceArgs,
    SetVideoDeviceArgs, SinkIdArgs, SinkStats, StateSnapshot, Tile, VideoCapsAnswer, VideoCodec,
    VideoCodecPreference, VideoDevices, VideoSinkAnswer, VideoSinkArgs, VideoSinkUpdateArgs,
};

#[tauri::command]
pub(crate) async fn join(
    service: State<'_, VoiceService>,
    target: String,
    room: String,
    muted: Option<bool>,
    deafened: Option<bool>,
    decode: Option<Vec<VideoCodec>>,
) -> Result<JoinResult, ErrorInfo> {
    let args = JoinArgs {
        target,
        room,
        muted,
        deafened,
    };
    let opts = JoinOptions {
        decode,
        ..JoinOptions::default()
    };
    service.join_with(args, opts).await
}

#[tauri::command]
pub(crate) async fn leave(service: State<'_, VoiceService>) -> Result<(), ErrorInfo> {
    service.leave().await
}

#[tauri::command]
pub(crate) async fn mute(
    service: State<'_, VoiceService>,
    muted: Option<bool>,
    deafened: Option<bool>,
) -> Result<(), ErrorInfo> {
    service.mute(MuteArgs { muted, deafened }).await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn consent(
    service: State<'_, VoiceService>,
    granted: bool,
    notice_version: u32,
) -> Result<(), ErrorInfo> {
    service
        .consent(ConsentArgs {
            granted,
            notice_version,
        })
        .await
}

#[tauri::command]
pub(crate) async fn withdraw_consent(service: State<'_, VoiceService>) -> Result<(), ErrorInfo> {
    service.withdraw_consent().await
}

#[tauri::command]
pub(crate) async fn devices(service: State<'_, VoiceService>) -> Result<DeviceList, ErrorInfo> {
    service.devices().await
}

#[tauri::command]
pub(crate) async fn set_device(
    service: State<'_, VoiceService>,
    input: Option<String>,
    output: Option<String>,
) -> Result<(), ErrorInfo> {
    service.set_device(SetDeviceArgs { input, output }).await
}

#[tauri::command]
pub(crate) async fn state(service: State<'_, VoiceService>) -> Result<StateSnapshot, ErrorInfo> {
    service.state().await
}

/// Behind `sew-voice:allow-forget-server` (not in `sew-voice:default`).
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn forget_server(
    service: State<'_, VoiceService>,
    install_id: String,
) -> Result<Vec<DeviceKeyInfo>, ErrorInfo> {
    let install = ForgetServerArgs { install_id }.install()?;
    let removed = service.forget_server(install).await?;
    Ok(removed.iter().map(DeviceKeyInfo::from).collect())
}

/// Behind `sew-voice:allow-known-device-keys` (not in `sew-voice:default`).
#[tauri::command]
pub(crate) async fn known_device_keys(
    service: State<'_, VoiceService>,
) -> Result<Vec<DeviceKeyInfo>, ErrorInfo> {
    let keys = service.known_device_keys().await?;
    Ok(keys.iter().map(DeviceKeyInfo::from).collect())
}

/// Behind `sew-voice:allow-call-details` (or the set `sew-voice:call-details`).
#[tauri::command]
pub(crate) async fn call_details(
    service: State<'_, VoiceService>,
) -> Result<CallDetails, ErrorInfo> {
    Ok(service.details())
}

/// Behind `sew-voice:allow-ring` (or the set `sew-voice:call-details`).
#[tauri::command(rename_all = "snake_case")]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn ring(
    service: State<'_, VoiceService>,
    call_id: String,
    target: String,
    room: String,
    started_by: Option<String>,
    silent: Option<bool>,
    reason: Option<String>,
) -> Result<(), ErrorInfo> {
    service.ring(IncomingCall {
        call_id,
        target,
        room,
        started_by,
        silent: silent.unwrap_or(false),
        reason,
    })
}

/// Behind `sew-voice:allow-ring-ended` (or the set `sew-voice:call-details`).
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn ring_ended(
    service: State<'_, VoiceService>,
    call_id: String,
) -> Result<(), ErrorInfo> {
    service.ring_ended(RingEndedArgs { call_id })
}

// ───────────────────────────── video (behind `sew-voice:video`) ─────────────────────────────

/// A sink's Tauri raw channel: one `InvokeResponseBody::Raw` message per frame.
struct TauriFrameChannel(Channel<InvokeResponseBody>);

impl FrameChannel for TauriFrameChannel {
    fn send(&self, message: Vec<u8>) -> bool {
        self.0.send(InvokeResponseBody::Raw(message)).is_ok()
    }
}

#[tauri::command]
pub(crate) async fn video_caps(
    service: State<'_, VoiceService>,
) -> Result<VideoCapsAnswer, ErrorInfo> {
    Ok(VideoCapsAnswer {
        caps: service.video_caps().await?,
        codec_preference: service.video_codec_preference().await?,
    })
}

#[tauri::command]
pub(crate) async fn set_video_codec_preference(
    service: State<'_, VoiceService>,
    preference: VideoCodecPreference,
) -> Result<CodecPreferenceInfo, ErrorInfo> {
    service
        .set_video_codec_preference(SetVideoCodecPreferenceArgs::new(preference))
        .await
}

#[tauri::command]
pub(crate) async fn video_devices(
    service: State<'_, VoiceService>,
) -> Result<VideoDevices, ErrorInfo> {
    service.video_devices().await
}

#[tauri::command]
pub(crate) async fn set_video_device(
    service: State<'_, VoiceService>,
    camera: String,
) -> Result<(), ErrorInfo> {
    service
        .set_video_device(SetVideoDeviceArgs { camera })
        .await
}

#[tauri::command]
pub(crate) async fn camera(
    service: State<'_, VoiceService>,
    on: bool,
    device: Option<String>,
) -> Result<(), ErrorInfo> {
    service.camera(CameraArgs { on, device }).await
}

#[tauri::command]
pub(crate) async fn screen_sources(
    service: State<'_, VoiceService>,
) -> Result<Vec<ScreenSource>, ErrorInfo> {
    service.screen_sources().await
}

#[tauri::command]
pub(crate) async fn screen<R: Runtime>(
    window: Window<R>,
    service: State<'_, VoiceService>,
    on: bool,
    source: Option<String>,
) -> Result<(), ErrorInfo> {
    service
        .screen(ScreenArgs { on, source }, parent_window(&window))
        .await
}

/// The OS picker's parent: the calling window's `HWND` on Windows (the picker refuses to open
/// without one), nothing elsewhere.
#[cfg(windows)]
fn parent_window<R: Runtime>(window: &Window<R>) -> Option<ParentWindow> {
    window.hwnd().ok().map(|h| ParentWindow(h.0 as isize))
}

#[cfg(not(windows))]
fn parent_window<R: Runtime>(_window: &Window<R>) -> Option<ParentWindow> {
    None
}

#[tauri::command]
pub(crate) async fn preview(
    service: State<'_, VoiceService>,
    on: bool,
    device: Option<String>,
) -> Result<PreviewAnswer, ErrorInfo> {
    service.preview(PreviewArgs { on, device }).await
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn video_sink(
    service: State<'_, VoiceService>,
    track: String,
    channel: Channel<InvokeResponseBody>,
    width: u32,
    height: u32,
    visible: bool,
    tile: Tile,
    decode: Vec<String>,
) -> Result<VideoSinkAnswer, ErrorInfo> {
    let args = VideoSinkArgs {
        track,
        width,
        height,
        visible,
        tile,
        decode,
    };
    service
        .video_sink(args, Arc::new(TauriFrameChannel(channel)))
        .await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn video_sink_update(
    service: State<'_, VoiceService>,
    sink_id: u32,
    width: Option<u32>,
    height: Option<u32>,
    visible: Option<bool>,
    tile: Option<Tile>,
    stats: Option<SinkStats>,
) -> Result<(), ErrorInfo> {
    service
        .video_sink_update(VideoSinkUpdateArgs {
            sink_id,
            width,
            height,
            visible,
            tile,
            stats,
        })
        .await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn video_sink_close(
    service: State<'_, VoiceService>,
    sink_id: u32,
) -> Result<(), ErrorInfo> {
    service.video_sink_close(SinkIdArgs { sink_id }).await
}

#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn request_keyframe(
    service: State<'_, VoiceService>,
    sink_id: u32,
) -> Result<(), ErrorInfo> {
    service.request_keyframe(SinkIdArgs { sink_id }).await
}

#[tauri::command]
pub(crate) async fn camera_permission(
    service: State<'_, VoiceService>,
) -> Result<PermissionState, ErrorInfo> {
    service.camera_permission().await
}

#[tauri::command]
pub(crate) async fn request_camera_permission(
    service: State<'_, VoiceService>,
) -> Result<PermissionState, ErrorInfo> {
    service.request_camera_permission().await
}
