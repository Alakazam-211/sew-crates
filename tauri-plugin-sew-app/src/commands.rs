//! The commands (`plugin:sew-app|<name>`): thin wrappers over [`AppService`]. The argument
//! names are the JSON keys the webview sends (snake_case):
//! `invoke('plugin:sew-app|sign_in', {server, username, password})`.
//!
//! Each command reads its caller from the calling page ([`Caller`]): the bundled UI acts on
//! every saved server; a saved server's live site acts only on its own server and accounts
//! (another is `wrong_origin`), and lists only those; `add_server` and `confirm_server`
//! answer the bundled UI only (`bundled_only`).

use std::sync::Arc;

use serde_json::Value;
use tauri::ipc::{Channel, InvokeBody};
use tauri::{AppHandle, Manager, Runtime, State, Webview, WebviewWindow};

use crate::api::{ApiCall, ApiReply, PickedFile, Upload, UploadData};
use crate::deeplink::DeepLinkEvent;
use crate::error::{AppError, code};
use crate::gateway::{GatewayEvent, GatewayOpened, GatewaySink};
use crate::lane::{LaneInfo, Secret, SessionInfo};
use crate::origin::Origin;
use crate::plugin::Shell;
use crate::scope::Caller;
use crate::servers::{AccountKey, AccountView, ServersView};
use crate::service::{AppService, HandoffView, PendingPair, PendingServer, SignOutResult};
use crate::wire::{Badge, NotifyResult, check_notification};

fn origin(svc: &AppService, server: &str) -> Result<Origin, AppError> {
    Origin::parse(server, svc.config().allow_loopback_http)
}

/// The calling page: who it is, and the name its sockets and picked files are kept under
/// (its webview and origin).
struct Page {
    caller: Caller,
    owner: String,
}

fn page<R: Runtime>(webview: &Webview<R>, svc: &AppService) -> Result<Page, AppError> {
    let url = webview
        .url()
        .map_err(|e| AppError::with_detail(code::WINDOW, e.to_string()))?;
    let bundled = webview
        .try_state::<Shell>()
        .map(|s| s.bundled.clone())
        .unwrap_or_default();
    Ok(Page {
        caller: Caller::of(&url, &bundled, &svc.book()),
        owner: format!("{} {}", webview.label(), url.origin().ascii_serialization()),
    })
}

/// The server named by `server`, when the caller may act on it.
fn scoped(svc: &AppService, p: &Page, server: &str) -> Result<Origin, AppError> {
    let o = origin(svc, server)?;
    p.caller.server(&o)?;
    Ok(o)
}

/// `key`, or the active account, when the caller may act on it.
fn scoped_key(svc: &AppService, p: &Page, key: Option<AccountKey>) -> Result<AccountKey, AppError> {
    p.caller.key(key, svc.active())
}

#[tauri::command]
pub(crate) async fn servers<R: Runtime>(
    webview: Webview<R>,
    svc: State<'_, AppService>,
) -> Result<ServersView, AppError> {
    let p = page(&webview, &svc)?;
    Ok(p.caller.view(svc.servers()))
}

/// The bundled UI only: the probe of a typed address (`GET /sew/native/info`, no credential).
#[tauri::command]
pub(crate) async fn add_server<R: Runtime>(
    webview: Webview<R>,
    svc: State<'_, AppService>,
    url: String,
) -> Result<PendingServer, AppError> {
    page(&webview, &svc)?.caller.bundled_only()?;
    svc.add_server(&url).await
}

/// The bundled UI only: the person's confirm.
#[tauri::command]
pub(crate) async fn confirm_server<R: Runtime>(
    app: AppHandle<R>,
    webview: Webview<R>,
    svc: State<'_, AppService>,
    server: String,
) -> Result<ServersView, AppError> {
    page(&webview, &svc)?.caller.bundled_only()?;
    let o = origin(&svc, &server)?;
    let view = svc.confirm_server(&o)?;
    if let Some(shell) = app.try_state::<Shell>() {
        shell.ensure_remote(&app, &o);
    }
    Ok(view)
}

#[tauri::command]
pub(crate) async fn remove_server<R: Runtime>(
    webview: Webview<R>,
    svc: State<'_, AppService>,
    server: String,
) -> Result<ServersView, AppError> {
    let o = scoped(&svc, &page(&webview, &svc)?, &server)?;
    svc.remove_server(&o).await
}

#[tauri::command]
pub(crate) async fn server_info<R: Runtime>(
    webview: Webview<R>,
    svc: State<'_, AppService>,
    server: String,
) -> Result<LaneInfo, AppError> {
    let o = scoped(&svc, &page(&webview, &svc)?, &server)?;
    svc.refresh_info(&o).await
}

#[tauri::command]
pub(crate) async fn set_active<R: Runtime>(
    webview: Webview<R>,
    svc: State<'_, AppService>,
    key: AccountKey,
) -> Result<ServersView, AppError> {
    let p = page(&webview, &svc)?;
    p.caller.server(&key.server)?;
    Ok(p.caller.view(svc.set_active(&key)?))
}

#[tauri::command]
pub(crate) async fn sign_in<R: Runtime>(
    webview: Webview<R>,
    svc: State<'_, AppService>,
    server: String,
    username: String,
    password: String,
    account: Option<String>,
) -> Result<AccountView, AppError> {
    let o = scoped(&svc, &page(&webview, &svc)?, &server)?;
    svc.sign_in_password(&o, &username, Secret::new(password), account.as_deref())
        .await
}

#[tauri::command]
pub(crate) async fn pair<R: Runtime>(
    webview: Webview<R>,
    svc: State<'_, AppService>,
    server: String,
    code: String,
    account: Option<String>,
) -> Result<AccountView, AppError> {
    let o = scoped(&svc, &page(&webview, &svc)?, &server)?;
    svc.pair(&o, Secret::new(code), account.as_deref()).await
}

/// The host's sign-in door, all in Rust: the password goes from here to the confirmed server's
/// own `/.k2/auth/sign-in`, and nothing secret comes back.
#[tauri::command]
pub(crate) async fn sign_in_door<R: Runtime>(
    webview: Webview<R>,
    svc: State<'_, AppService>,
    server: String,
    username: String,
    password: String,
    account: Option<String>,
) -> Result<AccountView, AppError> {
    let o = scoped(&svc, &page(&webview, &svc)?, &server)?;
    svc.sign_in_door(&o, &username, Secret::new(password), account.as_deref())
        .await
}

/// After `password_change_required`: the person's new password, then the sign-in.
#[tauri::command]
pub(crate) async fn sign_in_door_set_password<R: Runtime>(
    webview: Webview<R>,
    svc: State<'_, AppService>,
    server: String,
    password: String,
    account: Option<String>,
) -> Result<AccountView, AppError> {
    let o = scoped(&svc, &page(&webview, &svc)?, &server)?;
    svc.sign_in_door_set_password(&o, Secret::new(password), account.as_deref())
        .await
}

/// The person confirmed a `pair_request`: redeem the held code.
#[tauri::command]
pub(crate) async fn pair_confirm<R: Runtime>(
    webview: Webview<R>,
    svc: State<'_, AppService>,
    server: String,
    account: Option<String>,
) -> Result<AccountView, AppError> {
    let o = scoped(&svc, &page(&webview, &svc)?, &server)?;
    svc.pair_confirm(&o, account.as_deref()).await
}

/// The person declined a `pair_request`: forget the held code.
#[tauri::command]
pub(crate) async fn pair_decline<R: Runtime>(
    webview: Webview<R>,
    svc: State<'_, AppService>,
    server: String,
) -> Result<bool, AppError> {
    let o = scoped(&svc, &page(&webview, &svc)?, &server)?;
    Ok(svc.pair_decline(&o))
}

#[tauri::command]
pub(crate) async fn sign_out<R: Runtime>(
    webview: Webview<R>,
    svc: State<'_, AppService>,
    key: Option<AccountKey>,
) -> Result<SignOutResult, AppError> {
    let key = scoped_key(&svc, &page(&webview, &svc)?, key)?;
    svc.sign_out(&key).await
}

#[tauri::command]
pub(crate) async fn devices<R: Runtime>(
    webview: Webview<R>,
    svc: State<'_, AppService>,
    key: Option<AccountKey>,
) -> Result<Vec<SessionInfo>, AppError> {
    let key = scoped_key(&svc, &page(&webview, &svc)?, key)?;
    svc.devices(&key).await
}

#[tauri::command]
pub(crate) async fn revoke_device<R: Runtime>(
    webview: Webview<R>,
    svc: State<'_, AppService>,
    key: Option<AccountKey>,
    session: String,
) -> Result<(), AppError> {
    let key = scoped_key(&svc, &page(&webview, &svc)?, key)?;
    svc.revoke_device(&key, &session).await
}

/// Loads a page of the account's server in the calling window, signed in through the handoff
/// page when one is configured. Answers the URL without its fragment (never the code).
#[tauri::command]
pub(crate) async fn open<R: Runtime>(
    window: WebviewWindow<R>,
    webview: Webview<R>,
    svc: State<'_, AppService>,
    key: Option<AccountKey>,
    path: Option<String>,
) -> Result<String, AppError> {
    let key = scoped_key(&svc, &page(&webview, &svc)?, key)?;
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
    webview: Webview<R>,
    svc: State<'_, AppService>,
    key: Option<AccountKey>,
) -> Result<HandoffView, AppError> {
    let key = scoped_key(&svc, &page(&webview, &svc)?, key)?;
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
pub(crate) async fn api<R: Runtime>(
    webview: Webview<R>,
    svc: State<'_, AppService>,
    key: Option<AccountKey>,
    method: String,
    path: String,
    body: Option<Value>,
    idempotency_key: Option<String>,
) -> Result<ApiReply, AppError> {
    let key = scoped_key(&svc, &page(&webview, &svc)?, key)?;
    svc.api(
        Some(key),
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
/// `{event: "message", data}` per frame, then `{event: "close", code, reason, clean}`. Only
/// the page that opened it may send on or close it.
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn gateway_open<R: Runtime>(
    webview: Webview<R>,
    svc: State<'_, AppService>,
    key: Option<AccountKey>,
    on_event: Channel<GatewayEvent>,
) -> Result<GatewayOpened, AppError> {
    let p = page(&webview, &svc)?;
    let key = scoped_key(&svc, &p, key)?;
    let sink: GatewaySink = Arc::new(move |e| {
        if on_event.send(e).is_err() {
            tracing::debug!("sew-app: a gateway event found no page");
        }
    });
    svc.gateway_open_as(key, Some(p.owner), sink).await
}

/// Sends a frame on an open socket (the ticket is put in here).
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn gateway_send<R: Runtime>(
    webview: Webview<R>,
    svc: State<'_, AppService>,
    id: String,
    data: String,
) -> Result<(), AppError> {
    let p = page(&webview, &svc)?;
    svc.gateway_send_as(Some(&p.owner), &id, &data).await
}

/// Closes an open socket; its `close` event follows.
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn gateway_close<R: Runtime>(
    webview: Webview<R>,
    svc: State<'_, AppService>,
    id: String,
    code: Option<u16>,
    reason: Option<String>,
) -> Result<bool, AppError> {
    let p = page(&webview, &svc)?;
    svc.gateway_close_as(Some(&p.owner), &id, code, reason)
}

/// Raw bytes as the invoke's body: `invoke('plugin:sew-app|upload_bytes', bytes, {headers:
/// {'sew-path': '/uploads/9/data', 'content-type': 'image/png'}})`, with `content-range`, and
/// `sew-server` with `sew-account` for another account than the active one.
#[tauri::command]
pub(crate) async fn upload_bytes<R: Runtime>(
    webview: Webview<R>,
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
    let key = scoped_key(&svc, &page(&webview, &svc)?, key)?;
    let up = Upload {
        path,
        content_type: get("content-type"),
        content_range: get("content-range"),
        data: UploadData::Bytes(bytes.clone()),
    };
    svc.upload(Some(key), up).await
}

/// Opens the native file dialog and answers a handle per file the person picked (none when
/// they cancel). Only the page that asked may use the handles.
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn pick_files<R: Runtime>(
    app: AppHandle<R>,
    webview: Webview<R>,
    svc: State<'_, AppService>,
    multiple: Option<bool>,
    title: Option<String>,
) -> Result<Vec<PickedFile>, AppError> {
    let p = page(&webview, &svc)?;
    let paths = dialog_paths(&app, multiple.unwrap_or(false), title).await?;
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        out.push(svc.picked_file_for(&path, Some(p.owner.clone()))?);
    }
    Ok(out)
}

/// The paths the person picks in the native file dialog (rfd, on the main thread as macOS
/// needs; answered off it).
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
async fn dialog_paths<R: Runtime>(
    app: &AppHandle<R>,
    multiple: bool,
    title: Option<String>,
) -> Result<Vec<std::path::PathBuf>, AppError> {
    let (tx, rx) = tokio::sync::oneshot::channel::<Vec<std::path::PathBuf>>();
    let paths = |r: Option<Vec<rfd::FileHandle>>| -> Vec<std::path::PathBuf> {
        r.unwrap_or_default()
            .iter()
            .map(|h| h.path().to_path_buf())
            .collect()
    };
    app.run_on_main_thread(move || {
        let mut d = rfd::AsyncFileDialog::new();
        if let Some(t) = title {
            d = d.set_title(t);
        }
        if multiple {
            let picking = d.pick_files();
            std::thread::spawn(move || {
                let _ = tx.send(paths(tauri::async_runtime::block_on(picking)));
            });
        } else {
            let picking = d.pick_file();
            std::thread::spawn(move || {
                let one = tauri::async_runtime::block_on(picking).map(|h| vec![h]);
                let _ = tx.send(paths(one));
            });
        }
    })
    .map_err(|e| AppError::with_detail(code::WINDOW, e.to_string()))?;
    Ok(rx.await.unwrap_or_default())
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
async fn dialog_paths<R: Runtime>(
    _app: &AppHandle<R>,
    _multiple: bool,
    _title: Option<String>,
) -> Result<Vec<std::path::PathBuf>, AppError> {
    Err(AppError::with_detail(
        code::WINDOW,
        "the file dialog is on desktop only",
    ))
}

/// A picked file read in Rust and posted as an upload's bytes (its own permission,
/// `sew-app:upload-file`). `file` is a handle `pick_files` gave this page (or the host gave
/// it); a path or another page's handle is refused with the same `file` answer as a file
/// that can't be read.
#[tauri::command(rename_all = "snake_case")]
pub(crate) async fn upload_file<R: Runtime>(
    webview: Webview<R>,
    svc: State<'_, AppService>,
    key: Option<AccountKey>,
    path: String,
    file: String,
    content_type: Option<String>,
    content_range: Option<String>,
) -> Result<ApiReply, AppError> {
    let p = page(&webview, &svc)?;
    let key = scoped_key(&svc, &p, key)?;
    let local = svc.picked_path(&file, Some(&p.owner))?;
    let up = Upload {
        path,
        content_type,
        content_range,
        data: UploadData::File(local),
    };
    svc.upload(Some(key), up).await
}

/// The bundled UI only: the pairing links a `pair_request` or `pair_refused` signal is about.
#[tauri::command]
pub(crate) async fn pending_pair<R: Runtime>(
    webview: Webview<R>,
    svc: State<'_, AppService>,
) -> Result<PendingPair, AppError> {
    page(&webview, &svc)?.caller.bundled_only()?;
    Ok(svc.pending_pair())
}

/// The bundled UI only: the deep links `deep_link` signals were about (each answered once).
#[tauri::command]
pub(crate) async fn pending_deep_links<R: Runtime>(
    webview: Webview<R>,
    svc: State<'_, AppService>,
) -> Result<Vec<DeepLinkEvent>, AppError> {
    page(&webview, &svc)?.caller.bundled_only()?;
    Ok(svc.pending_deep_links())
}
