//! The six Tauri commands. Each one is a thin call into [`VoiceService`]; the argument
//! names are the JSON keys the webview sends (`invoke('plugin:sew-voice|join', {target, room,
//! muted, deafened})`).

use tauri::State;

use crate::service::VoiceService;
use crate::wire::{
    DeviceList, ErrorInfo, JoinArgs, JoinResult, MuteArgs, SetDeviceArgs, StateSnapshot,
};

#[tauri::command]
pub(crate) async fn join(
    service: State<'_, VoiceService>,
    target: String,
    room: String,
    muted: Option<bool>,
    deafened: Option<bool>,
) -> Result<JoinResult, ErrorInfo> {
    service
        .join(JoinArgs {
            target,
            room,
            muted,
            deafened,
        })
        .await
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
