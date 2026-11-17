//! The commands (`plugin:sew-app|<name>`): thin wrappers over [`AppService`]. The argument
//! names are the JSON keys the webview sends (snake_case):
//! `invoke('plugin:sew-app|sign_in', {server, username, password})`.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;
use tauri::ipc::{Channel, InvokeBody};
use tauri::{AppHandle, Manager, Runtime, State, WebviewWindow};

use crate::api::{ApiCall, ApiReply, Upload, UploadData};
use crate::error::{AppError, code};
use crate::gateway::{GatewayEvent, GatewayOpened, GatewaySink};
use crate::lane::{LaneInfo, Secret, SessionInfo};
use crate::origin::Origin;
use crate::plugin::Shell;
use crate::servers::{AccountKey, AccountView, ServersView};
use crate::service::{AppService, HandoffView, PendingServer, SignOutResult};
use crate::wire::{Badge, NotifyResult, check_notification};

fn origin(svc: &AppService, server: &str) -> Result<Origin, AppError> {
    Origin::parse(server, svc.config().allow_loopback_http)
}

#[tauri::command]
pub(crate) async fn servers(svc: State<'_, AppService>) -> Result<ServersView, AppError> {
    Ok(svc.servers())
}

#[tauri::command]
pub(crate) async fn add_server(
    svc: State<'_, AppService>,
    url: String,
) -> Result<PendingServer, AppError> {
    svc.add_server(&url).await
}

#[tauri::command]
pub(crate) async fn confirm_server<R: Runtime>(
    app: AppHandle<R>,
    svc: State<'_, AppService>,
    server: String,
) -> Result<ServersView, AppError> {
    let o = origin(&svc, &server)?;
    let view = svc.confirm_server(&o)?;
    if let Some(shell) = app.try_state::<Shell>() {
        shell.ensure_remote(&app, &o);
    }
    Ok(view)
}

#[tauri::command]
pub(crate) async fn remove_server(
    svc: State<'_, AppService>,
    server: String,
) -> Result<ServersView, AppError> {
    let o = origin(&svc, &server)?;
    svc.remove_server(&o).await
}

#[tauri::command]
pub(crate) async fn server_info(
    svc: State<'_, AppService>,
    server: String,
) -> Result<LaneInfo, AppError> {
    let o = origin(&svc, &server)?;
    svc.refresh_info(&o).await
}

#[tauri::command]
pub(crate) async fn set_active(
    svc: State<'_, AppService>,
    key: AccountKey,
) -> Result<ServersView, AppError> {
    svc.set_active(&key)
}

#[tauri::command]
pub(crate) async fn sign_in(
    svc: State<'_, AppService>,
    server: String,
    username: String,
    password: String,
    account: Option<String>,
) -> Result<AccountView, AppError> {
    let o = origin(&svc, &server)?;
    svc.sign_in_password(&o, &username, Secret::new(password), account.as_deref())
        .await
}

#[tauri::command]
pub(crate) async fn pair(
    svc: State<'_, AppService>,
    server: String,
    code: String,
    account: Option<String>,
) -> Result<AccountView, AppError> {
    let o = origin(&svc, &server)?;
    svc.pair(&o, Secret::new(code), account.as_deref()).await
}

/// The host's sign-in door, all in Rust: the password goes from here to the confirmed server's
/// own `/.k2/auth/sign-in`, and nothing secret comes back.
#[tauri::command]
pub(crate) async fn sign_in_door(
    svc: State<'_, AppService>,
    server: String,
    username: String,
    password: String,
    account: Option<String>,
) -> Result<AccountView, AppError> {
    let o = origin(&svc, &server)?;
    svc.sign_in_door(&o, &username, Secret::new(password), account.as_deref())
        .await
}

/// After `password_change_required`: the person's new password, then the sign-in.
#[tauri::command]
pub(crate) async fn sign_in_door_set_password(
    svc: State<'_, AppService>,
    server: String,
    password: String,
    account: Option<String>,
) -> Result<AccountView, AppError> {
    let o = origin(&svc, &server)?;
    svc.sign_in_door_set_password(&o, Secret::new(password), account.as_deref())
        .await
}

/// The person confirmed a `pair_request`: redeem the held code.
#[tauri::command]
pub(crate) async fn pair_confirm(
    svc: State<'_, AppService>,
    server: String,
    account: Option<String>,
) -> Result<AccountView, AppError> {
    let o = origin(&svc, &server)?;
    svc.pair_confirm(&o, account.as_deref()).await
}

/// The person declined a `pair_request`: forget the held code.
#[tauri::command]
pub(crate) async fn pair_decline(
    svc: State<'_, AppService>,
    server: String,
) -> Result<bool, AppError> {
    let o = origin(&svc, &server)?;
    Ok(svc.pair_decline(&o))
}

#[tauri::command]
pub(crate) async fn sign_out(
    svc: State<'_, AppService>,
    key: Option<AccountKey>,
) -> Result<SignOutResult, AppError> {
    let key = svc.account_or_active(key)?;
    svc.sign_out(&key).await
}

#[tauri::command]
pub(crate) async fn devices(
    svc: State<'_, AppService>,
    key: Option<AccountKey>,
) -> Result<Vec<SessionInfo>, AppError> {
    let key = svc.account_or_active(key)?;
    svc.devices(&key).await
}

#[tauri::command]
pub(crate) async fn revoke_device(
    svc: State<'_, AppService>,
    key: Option<AccountKey>,
    session: String,
) -> Result<(), AppError> {
    let key = svc.account_or_active(key)?;
    svc.revoke_device(&key, &session).await
}

/// Loads a page of the account's server in the calling window, signed in through the handoff
/// page when one is configured. Answers the URL without its fragment (never the code).
#[tauri::command]
pub(crate) async fn open<R: Runtime>(
    window: WebviewWindow<R>,
    svc: State<'_, AppService>,
    key: Option<AccountKey>,
    path: Option<String>,
) -> Result<String, AppError> {
    let key = svc.account_or_active(key)?;
    let url = svc.open_url(&key, path.as_deref().unwrap_or("/")).await?;
    window
        .navigate(url.clone())
        .map_err(|e| AppError::with_detail(code::WINDOW, e.to_string()))?;
    let mut shown = url;
    shown.set_fragment(None);
    Ok(shown.to_string())
}

/// A handoff code for the calling page: only a page on the account's own server gets one.
#[tauri::command]
pub(crate) async fn handoff<R: Runtime>(
    window: WebviewWindow<R>,
    svc: State<'_, AppService>,
    key: Option<AccountKey>,
) -> Result<HandoffView, AppError> {
    let key = svc.account_or_active(key)?;
    let caller = window
        .url()
        .map_err(|e| AppError::with_detail(code::WINDOW, e.to_string()))?;
    svc.handoff(&key, Some(&caller)).await
}

/// The notifications bridge: a notification (tauri-plugin-notification) and the app badge.
#[tauri::command]
pub(crate) async fn notify<R: Runtime>(
    app: AppHandle<R>,
    window: WebviewWindow<R>,
    title: Option<String>,
    body: Option<String>,
    badge: Option<i64>,
) -> Result<NotifyResult, AppError> {
    check_notification(title.as_deref(), body.as_deref())?;
    let badge = Badge::from_arg(badge)?;
    let mut shown = false;
    if let Some(title) = title {
        use tauri_plugin_notification::NotificationExt;
        if app
            .try_state::<tauri_plugin_notification::Notification<R>>()
            .is_none()
        {
            return Err(AppError::with_detail(
                code::NOTIFY_FAILED,
                "tauri-plugin-notification is not registered (use Builder::install)",
            ));
        }
        let mut n = app.notification().builder().title(title);
        if let Some(body) = body {
            n = n.body(body);
        }
        n.show()
            .map_err(|e| AppError::with_detail(code::NOTIFY_FAILED, e.to_string()))?;
        shown = true;
    }
    let badge = match badge {
        Badge::Unchanged => "unchanged",
        _ if cfg!(target_os = "windows") => "unsupported",
        Badge::Clear => match window.set_badge_count(None) {
            Ok(()) => "cleared",
            Err(_) => "unsupported",
        },
        Badge::Count(n) => match window.set_badge_count(Some(n)) {
            Ok(()) => "set",
            Err(_) => "unsupported",
        },
    };
    Ok(NotifyResult {
        shown,
        badge: badge.to_owned(),
    })
}

// ── the API transport (`sew-app:transport`, `sew-app:upload-file`) ──

/// One call of the App's client API with the account's bearer: `{status, headers, body}`.
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn api(
    svc: State<'_, AppService>,
    key: Option<AccountKey>,
    method: String,
    path: String,
    body: Option<Value>,
    idempotency_key: Option<String>,
) -> Result<ApiReply, AppError> {
    svc.api(
        key,
        ApiCall {
            method,
            path,
            body,
            idempotency_key,
        },
    )
    .await
}

/// Opens a gateway socket and relays it on `on_event`: `{event: "open", id}`, then
/// `{event: "message", data}` per frame, then `{event: "close", code, reason, clean}`.
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn gateway_open(
    svc: State<'_, AppService>,
    key: Option<AccountKey>,
    on_event: Channel<GatewayEvent>,
) -> Result<GatewayOpened, AppError> {
    let sink: GatewaySink = Arc::new(move |e| {
        if on_event.send(e).is_err() {
            tracing::debug!("sew-app: a gateway event found no page");
        }
    });
    svc.gateway_open(key, sink).await
}

/// Sends a frame on an open socket (the ticket is put in here).
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn gateway_send(
    svc: State<'_, AppService>,
    id: String,
    data: String,
) -> Result<(), AppError> {
    svc.gateway_send(&id, &data).await
}

/// Closes an open socket; its `close` event follows.
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn gateway_close(
    svc: State<'_, AppService>,
    id: String,
    code: Option<u16>,
    reason: Option<String>,
) -> Result<bool, AppError> {
    svc.gateway_close(&id, code, reason)
}

/// Raw bytes as the invoke's body: `invoke('plugin:sew-app|upload_bytes', bytes, {headers:
/// {'sew-path': '/uploads/9/data', 'content-type': 'image/png'}})`, with `content-range`, and
/// `sew-server` with `sew-account` for another account than the active one.
#[tauri::command]
pub(crate) async fn upload_bytes(
    svc: State<'_, AppService>,
    request: tauri::ipc::Request<'_>,
) -> Result<ApiReply, AppError> {
    let InvokeBody::Raw(bytes) = request.body() else {
        return Err(AppError::invalid_args(
            "upload_bytes takes the bytes as the invoke's raw body",
        ));
    };
    let h = request.headers();
    let get = |n: &str| h.get(n).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let path = get("sew-path").ok_or_else(|| AppError::invalid_args("the sew-path header"))?;
    let key = match (get("sew-server"), get("sew-account")) {
        (None, None) => None,
        (Some(s), Some(account)) => Some(AccountKey {
            server: origin(&svc, &s)?,
            account,
        }),
        _ => {
            return Err(AppError::invalid_args(
                "sew-server and sew-account go together",
            ));
        }
    };
    let up = Upload {
        path,
        content_type: get("content-type"),
        content_range: get("content-range"),
        data: UploadData::Bytes(bytes.clone()),
    };
    svc.upload(key, up).await
}

/// A file read in Rust and posted as an upload's bytes (its own permission,
/// `sew-app:upload-file`: the page names a path on this machine).
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn upload_file(
    svc: State<'_, AppService>,
    key: Option<AccountKey>,
    path: String,
    file: PathBuf,
    content_type: Option<String>,
    content_range: Option<String>,
) -> Result<ApiReply, AppError> {
    let up = Upload {
        path,
        content_type,
        content_range,
        data: UploadData::File(file),
    };
    svc.upload(key, up).await
}
