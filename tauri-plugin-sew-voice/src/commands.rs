//! The ten Tauri commands (the eight of `sew-voice:default`, and the two device-key commands
//! behind their own permissions). Each one is a thin call into [`VoiceService`]; the argument
//! names are the JSON keys the webview sends (`invoke('plugin:sew-voice|join', {target, room,
//! muted, deafened})`, `invoke('plugin:sew-voice|consent', {granted, notice_version})`: snake_case,
//! as every other key on the wire).

use tauri::State;

use crate::service::VoiceService;
use crate::wire::{
    ConsentArgs, DeviceKeyInfo, DeviceList, ErrorInfo, ForgetServerArgs, JoinArgs, JoinResult,
    MuteArgs, SetDeviceArgs, StateSnapshot,
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
