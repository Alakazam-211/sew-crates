//! The Tauri glue (feature `tauri`): the builder, the events, the remote capability, deep
//! links, single instance and the voice plugin's `forget_server`.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use tauri::ipc::CapabilityBuilder;
use tauri::plugin::{Builder as PluginBuilder, TauriPlugin};
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tauri_plugin_sew_voice::sew_call_client::rustls::crypto::CryptoProvider;
use tauri_plugin_sew_voice::{BoxFuture, VoiceControl};

use crate::deeplink::{DeepLinkEvent, DeepLinkRoute};
use crate::error::AppError;
use crate::fake::FakeLane;
use crate::gateway::SocketConnector;
use crate::lane::{LaneClient, Transport};
use crate::origin::Origin;
use crate::provider::LaneJoinProvider;
use crate::secrets::{KeychainOptions, SecretChoice};
use crate::service::{AppConfig, AppEvent, AppEventSink, AppService, VoiceForget};

/// The permissions each saved server's origin gets in the App's windows
/// ([`Builder::remote_capability`]): a capability per server, added at run time with
/// `remote.urls = ["<origin>/*"]`, so the App's live site can call the plugins while the
/// shell shows it. The bundled UI (`tauri://localhost`) is not in it; it keeps the app's own
/// capability files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCapability {
    /// The windows (labels) it applies to.
    pub windows: Vec<String>,
    /// The permissions it grants.
    pub permissions: Vec<String>,
}

impl RemoteCapability {
    /// Window `main`, with `core:event:default` and `sew-app:default`.
    pub fn new() -> RemoteCapability {
        RemoteCapability {
            windows: vec!["main".into()],
            permissions: vec!["core:event:default".into(), "sew-app:default".into()],
        }
    }

    /// Adds the voice plugin's call UI (`sew-voice:default`, `sew-voice:call-details`): only
    /// when `tauri-plugin-sew-voice` is registered (an unknown permission fails the capability).
    pub fn with_voice(mut self) -> RemoteCapability {
        for p in ["sew-voice:default", "sew-voice:call-details"] {
            self = self.permission(p);
        }
        self
    }

    /// Adds a permission (e.g. `sew-app:handoff`, `sew-voice:video`).
    pub fn permission(mut self, p: impl Into<String>) -> RemoteCapability {
        let p = p.into();
        if !self.permissions.contains(&p) {
            self.permissions.push(p);
        }
        self
    }

    /// Applies it to this window too.
    pub fn window(mut self, label: impl Into<String>) -> RemoteCapability {
        let l = label.into();
        if !self.windows.contains(&l) {
            self.windows.push(l);
        }
        self
    }

    /// The capability for `origin` (identifier `sew-app-remote-<n>`).
    pub fn build(&self, identifier: &str, origin: &Origin) -> CapabilityBuilder {
        let mut c = CapabilityBuilder::new(identifier)
            .remote(format!("{}/*", origin.as_str()))
            .local(false);
        for w in &self.windows {
            c = c.window(w.clone());
        }
        for p in &self.permissions {
            c = c.permission(p.clone());
        }
        c
    }
}

impl Default for RemoteCapability {
    fn default() -> RemoteCapability {
        RemoteCapability::new()
    }
}

/// The shell's settings, as Tauri state.
pub(crate) struct Shell {
    pub(crate) main_window: String,
    pub(crate) scheme: Option<String>,
    remote: Option<RemoteCapability>,
    added: Mutex<BTreeSet<Origin>>,
}

impl Shell {
    /// Adds the remote capability for `origin` once (a capability can't be removed at run
    /// time: a removed server's stays until the app restarts, and no window goes there).
    pub(crate) fn ensure_remote<R: Runtime>(&self, app: &AppHandle<R>, origin: &Origin) {
        let Some(rc) = &self.remote else {
            return;
        };
        let mut added = self.added.lock().unwrap_or_else(PoisonError::into_inner);
        if added.contains(origin) {
            return;
        }
        let id = format!("sew-app-remote-{}", added.len() + 1);
        match app.add_capability(rc.build(&id, origin)) {
            Ok(()) => {
                added.insert(origin.clone());
            }
            Err(e) => {
                tracing::warn!(origin = %origin, error = %e, "sew-app: the remote capability");
            }
        }
    }
}

/// Builds the plugin: `Builder::new().build()`, or [`Builder::install`] for the shell
/// plumbing too.
pub struct Builder {
    app_id: Option<String>,
    data_dir: Option<PathBuf>,
    device_name: Option<String>,
    allow_loopback_http: bool,
    secrets: SecretChoice,
    keychain: KeychainOptions,
    handoff_page: Option<String>,
    shell_version: Option<String>,
    transport: Option<Arc<dyn Transport>>,
    connector: Option<Arc<dyn SocketConnector>>,
    file_protocol: bool,
    file_origins: Vec<String>,
    crypto_provider: Option<Arc<CryptoProvider>>,
    remote: Option<RemoteCapability>,
    scheme: Option<String>,
    register_schemes: bool,
    main_window: String,
    memory_only: bool,
    provider: LaneJoinProvider,
}

impl std::fmt::Debug for Builder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Builder")
            .field("app_id", &self.app_id)
            .field("data_dir", &self.data_dir)
            .field("secrets", &self.secrets)
            .field("handoff_page", &self.handoff_page)
            .field("transport", &self.transport.is_some())
            .field("remote", &self.remote)
            .field("scheme", &self.scheme)
            .field("main_window", &self.main_window)
            .finish_non_exhaustive()
    }
}

impl Default for Builder {
    fn default() -> Builder {
        Builder::new()
    }
}

impl Builder {
    /// The plugin over the lane on HTTPS (feature `http`), the OS keychain, and the remote
    /// capability ([`RemoteCapability::new`]) for each saved server's origin in window `main`.
    pub fn new() -> Builder {
        Builder {
            app_id: None,
            data_dir: None,
            device_name: None,
            allow_loopback_http: false,
            secrets: SecretChoice::Os,
            keychain: KeychainOptions::default(),
            handoff_page: None,
            shell_version: None,
            transport: None,
            connector: None,
            file_protocol: true,
            file_origins: Vec::new(),
            crypto_provider: None,
            remote: Some(RemoteCapability::new()),
            scheme: None,
            register_schemes: cfg!(debug_assertions),
            main_window: "main".into(),
            memory_only: false,
            provider: LaneJoinProvider::unbound(),
        }
    }

    /// The fake mode: `lane` in memory, the secrets in memory, nothing saved.
    pub fn fake(lane: FakeLane) -> Builder {
        let mut b = Builder::new();
        b.connector = Some(Arc::new(lane.clone()));
        b.transport = Some(Arc::new(lane));
        b.secrets = SecretChoice::Memory;
        b.memory_only = true;
        b
    }

    /// The voice plugin's provider (`tauri_plugin_sew_voice::Builder::new(provider)`), bound
    /// to this plugin's core when the app is set up.
    pub fn join_provider(&self) -> LaneJoinProvider {
        self.provider.clone()
    }

    /// The keychain service's app id (`sew-app.<app id>`). Default: the app's identifier.
    pub fn app_id(mut self, id: impl Into<String>) -> Builder {
        self.app_id = Some(id.into());
        self
    }

    /// Where `servers.json` (and the opt-in `secrets.json`) live. Default: the app's data
    /// directory, `sew-app/`.
    pub fn data_dir(mut self, dir: impl Into<PathBuf>) -> Builder {
        self.data_dir = Some(dir.into());
        self
    }

    /// The device name sign-ins send (default `Desktop (<OS>)`).
    pub fn device_name(mut self, name: impl Into<String>) -> Builder {
        self.device_name = Some(name.into());
        self
    }

    /// Allow `http://` servers on loopback (development builds only).
    pub fn allow_loopback_http(mut self, allow: bool) -> Builder {
        self.allow_loopback_http = allow;
        self
    }

    /// The device bearer's store (default [`SecretChoice::Os`]).
    pub fn secret_store(mut self, choice: SecretChoice) -> Builder {
        self.secrets = choice;
        self
    }

    /// Phone keychain options (an access group; ignored on desktop).
    pub fn keychain(mut self, options: KeychainOptions) -> Builder {
        self.keychain = options;
        self
    }

    /// The App's page that redeems a handoff code from its fragment (`open` goes there with
    /// `#code=…&next=…`).
    pub fn handoff_page(mut self, path: impl Into<String>) -> Builder {
        self.handoff_page = Some(path.into());
        self
    }

    /// The shell's version checked against a server's `info.shell.min` (a sign-in below it is
    /// `shell_too_old`). Default: the app's own version (`tauri.conf.json`'s `version`).
    pub fn shell_version(mut self, version: impl Into<String>) -> Builder {
        self.shell_version = Some(version.into());
        self
    }

    /// Another transport than HTTPS (tests).
    pub fn transport(mut self, t: Arc<dyn Transport>) -> Builder {
        self.transport = Some(t);
        self
    }

    /// Another gateway connector than the WebSocket one (tests).
    pub fn socket_connector(mut self, c: Arc<dyn SocketConnector>) -> Builder {
        self.connector = Some(c);
        self
    }

    /// Whether the plugin registers the `sewfile:` URI scheme for the signed byte routes
    /// (default: yes).
    pub fn file_protocol(mut self, on: bool) -> Builder {
        self.file_protocol = on;
        self
    }

    /// A page origin besides the bundled UI's that may load `sewfile:` URLs (a development
    /// server's, such as `http://localhost:1420`; in a development build the configured
    /// `devUrl` is allowed already). A server's live site never should be.
    pub fn file_protocol_origin(mut self, origin: impl Into<String>) -> Builder {
        self.file_origins.push(origin.into());
        self
    }

    /// The rustls crypto provider of the HTTPS transport.
    pub fn crypto_provider(mut self, p: Arc<CryptoProvider>) -> Builder {
        self.crypto_provider = Some(p);
        self
    }

    /// The permissions each saved server's origin gets; `None`: no remote capability (the
    /// App lists its servers in its own capability files).
    pub fn remote_capability(mut self, rc: Option<RemoteCapability>) -> Builder {
        self.remote = rc;
        self
    }

    /// The deep-link scheme (`<scheme>://<host>/<path>` opens that page of a saved server).
    /// Also list it in `tauri.conf.json` (`plugins.deep-link.desktop.schemes`).
    pub fn deep_link_scheme(mut self, scheme: impl Into<String>) -> Builder {
        self.scheme = Some(scheme.into());
        self
    }

    /// Register the schemes with the OS at start on Linux and Windows (default: debug builds
    /// only; an installer registers them in a release).
    pub fn register_schemes(mut self, register: bool) -> Builder {
        self.register_schemes = register;
        self
    }

    /// The window links and `open` navigate, and a second launch focuses (default `main`).
    pub fn main_window(mut self, label: impl Into<String>) -> Builder {
        self.main_window = label.into();
        self
    }

    /// Adds single instance, deep links and notifications (in that order, single instance
    /// first as it must be), then this plugin.
    pub fn install<R: Runtime>(self, builder: tauri::Builder<R>) -> tauri::Builder<R> {
        let main = self.main_window.clone();
        builder
            .plugin(tauri_plugin_single_instance::init(
                move |app, _argv, _cwd| {
                    // The deep link in argv reaches the deep-link plugin by itself (feature
                    // `deep-link`); here the running instance comes forward.
                    focus(app, &main);
                },
            ))
            .plugin(tauri_plugin_deep_link::init())
            .plugin(tauri_plugin_notification::init())
            .plugin(self.build())
    }

    /// The Tauri plugin (`sew-app`).
    pub fn build<R: Runtime>(self) -> TauriPlugin<R> {
        let mut pb = PluginBuilder::new(crate::PLUGIN_NAME);
        if self.file_protocol {
            let extra = self.file_origins.clone();
            pb = pb.register_asynchronous_uri_scheme_protocol(
                crate::api::FILE_SCHEME,
                move |ctx, request, responder| {
                    let app = ctx.app_handle().clone();
                    let label = ctx.webview_label().to_owned();
                    let extra = extra.clone();
                    tauri::async_runtime::spawn(async move {
                        responder.respond(file_response(&app, &label, &extra, request).await);
                    });
                },
            );
        }
        pb.invoke_handler(tauri::generate_handler![
            crate::commands::servers,
            crate::commands::add_server,
            crate::commands::confirm_server,
            crate::commands::remove_server,
            crate::commands::server_info,
            crate::commands::set_active,
            crate::commands::sign_in,
            crate::commands::pair,
            crate::commands::sign_in_door,
            crate::commands::sign_in_door_set_password,
            crate::commands::pair_confirm,
            crate::commands::pair_decline,
            crate::commands::sign_out,
            crate::commands::devices,
            crate::commands::revoke_device,
            crate::commands::open,
            crate::commands::notify,
            crate::commands::handoff,
            crate::commands::api,
            crate::commands::gateway_open,
            crate::commands::gateway_send,
            crate::commands::gateway_close,
            crate::commands::upload_bytes,
            crate::commands::upload_file,
        ])
        .setup(move |app, _api| {
            let svc = self.core(app)?;
            svc.set_voice_forget(Arc::new(TauriVoiceForget { app: app.clone() }));
            match &self.connector {
                Some(c) => svc.set_socket_connector(Arc::clone(c)),
                None => {
                    if let Some(c) = default_connector(app, self.crypto_provider.clone())? {
                        svc.set_socket_connector(c);
                    }
                }
            }
            self.provider.bind(svc.clone());
            let shell = Shell {
                main_window: self.main_window.clone(),
                scheme: self.scheme.clone(),
                remote: self.remote.clone(),
                added: Mutex::new(BTreeSet::new()),
            };
            for s in svc.book().servers {
                shell.ensure_remote(app, &s.origin);
            }
            app.manage(svc.clone());
            app.manage(shell);
            if self.scheme.is_some() {
                deep_links(app, self.register_schemes);
            }
            Ok(())
        })
        .build()
    }

    fn core<R: Runtime>(
        &self,
        app: &AppHandle<R>,
    ) -> Result<AppService, Box<dyn std::error::Error>> {
        let data_dir = match (&self.data_dir, self.memory_only) {
            (Some(d), _) => Some(d.clone()),
            (None, true) => None,
            (None, false) => Some(app.path().app_data_dir()?.join("sew-app")),
        };
        let mut cfg = AppConfig::new(
            self.app_id
                .clone()
                .unwrap_or_else(|| app.config().identifier.clone()),
        );
        cfg.data_dir = data_dir;
        if let Some(n) = &self.device_name {
            cfg.device_name = n.clone();
        }
        cfg.allow_loopback_http = self.allow_loopback_http;
        cfg.secrets = self.secrets.clone();
        cfg.keychain = self.keychain.clone();
        cfg.handoff_page = self.handoff_page.clone();
        cfg.shell_version = self
            .shell_version
            .clone()
            .unwrap_or_else(|| app.package_info().version.to_string());
        let transport = match &self.transport {
            Some(t) => Arc::clone(t),
            None => default_transport(app, self.crypto_provider.clone())?,
        };
        let sink = Arc::new(TauriAppSink { app: app.clone() });
        Ok(AppService::new(cfg, LaneClient::new(transport), sink)?)
    }
}

#[cfg(feature = "http")]
fn default_transport<R: Runtime>(
    app: &AppHandle<R>,
    crypto: Option<Arc<CryptoProvider>>,
) -> Result<Arc<dyn Transport>, Box<dyn std::error::Error>> {
    let pkg = app.package_info();
    let ua = format!("{}/{} (sew-app)", pkg.name, pkg.version);
    Ok(Arc::new(crate::http::HttpTransport::new(&ua, crypto)?))
}

#[cfg(feature = "http")]
fn default_connector<R: Runtime>(
    app: &AppHandle<R>,
    crypto: Option<Arc<CryptoProvider>>,
) -> Result<Option<Arc<dyn SocketConnector>>, Box<dyn std::error::Error>> {
    let pkg = app.package_info();
    let ua = format!("{}/{} (sew-app)", pkg.name, pkg.version);
    Ok(Some(Arc::new(crate::ws::WsConnector::new(&ua, crypto)?)))
}

#[cfg(not(feature = "http"))]
fn default_connector<R: Runtime>(
    _app: &AppHandle<R>,
    _crypto: Option<Arc<CryptoProvider>>,
) -> Result<Option<Arc<dyn SocketConnector>>, Box<dyn std::error::Error>> {
    // Without `http` there is no socket unless the app passes one.
    Ok(None)
}

/// The origins (besides the bundled UI's) that may load `sewfile:` URLs: the app's own, and
/// the configured `devUrl` in a development build.
fn file_origins<R: Runtime>(app: &AppHandle<R>, extra: &[String]) -> Vec<String> {
    let mut all = extra.to_vec();
    if tauri::is_dev()
        && let Some(dev) = &app.config().build.dev_url
    {
        all.push(dev.origin().ascii_serialization());
    }
    all
}

/// One `sewfile:` request: only from the bundled UI (or a listed origin), for a signed byte
/// route of a saved account, through the lane with the bearer.
async fn file_response<R: Runtime>(
    app: &AppHandle<R>,
    label: &str,
    extra: &[String],
    request: tauri::http::Request<Vec<u8>>,
) -> tauri::http::Response<Vec<u8>> {
    use crate::api::{ByteReply, file_caller_allowed, parse_file_request};
    let refuse = |status: u16, code: &str, hint: &str| -> ByteReply {
        let body = serde_json::json!({
            "ok": false, "code": code, "detail": null, "error": { "code": code, "hint": hint }
        });
        ByteReply {
            status,
            headers: vec![(
                "content-type".to_owned(),
                "application/json; charset=utf-8".to_owned(),
            )],
            body: serde_json::to_vec(&body).unwrap_or_default(),
        }
    };
    // A webview window's webview has the window's label (Tauri's multi-webview windows are
    // behind its `unstable` feature; a webview this can't find is refused).
    let caller = app.get_webview_window(label).and_then(|w| w.url().ok());
    let allowed = caller
        .as_ref()
        .is_some_and(|u| file_caller_allowed(u, &file_origins(app, extra)));
    let reply = if !allowed {
        refuse(
            403,
            "forbidden",
            "only the app's own pages load sewfile: URLs",
        )
    } else if let Some(svc) = app.try_state::<AppService>() {
        let uri = request.uri();
        match parse_file_request(uri.path(), uri.query()) {
            Some((account, path)) => {
                let headers: Vec<(String, String)> = request
                    .headers()
                    .iter()
                    .filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned())))
                    .collect();
                svc.fetch_bytes(&account, request.method().as_str(), &path, &headers)
                    .await
            }
            None => refuse(404, "not_found", "not found"),
        }
    } else {
        refuse(503, "unavailable", "the plugin is not set up")
    };
    let mut b = tauri::http::Response::builder().status(reply.status);
    for (k, v) in &reply.headers {
        b = b.header(k.as_str(), v.as_str());
    }
    // The bundled UI's own origin may read the answer with fetch() as well as show it.
    if let Some(u) = &caller
        && allowed
    {
        b = b.header(
            "access-control-allow-origin",
            u.origin().ascii_serialization(),
        );
        b = b.header("vary", "origin");
    }
    b.body(reply.body).unwrap_or_else(|_| {
        let mut r = tauri::http::Response::new(Vec::new());
        *r.status_mut() = tauri::http::StatusCode::BAD_GATEWAY;
        r
    })
}

#[cfg(not(feature = "http"))]
fn default_transport<R: Runtime>(
    _app: &AppHandle<R>,
    _crypto: Option<Arc<CryptoProvider>>,
) -> Result<Arc<dyn Transport>, Box<dyn std::error::Error>> {
    Err(
        "tauri-plugin-sew-app: built without the `http` feature, so Builder::transport is required"
            .into(),
    )
}

/// Brings the main window forward.
pub(crate) fn focus<R: Runtime>(app: &AppHandle<R>, label: &str) {
    if let Some(w) = app.get_webview_window(label) {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn deep_links<R: Runtime>(app: &AppHandle<R>, register: bool) {
    use tauri_plugin_deep_link::DeepLinkExt;
    if app
        .try_state::<tauri_plugin_deep_link::DeepLink<R>>()
        .is_none()
    {
        tracing::warn!(
            "sew-app: a deep-link scheme is set but tauri-plugin-deep-link is not registered \
             (use Builder::install)"
        );
        return;
    }
    if register {
        register_schemes(app);
    }
    let h = app.clone();
    app.deep_link().on_open_url(move |event| {
        for url in event.urls() {
            handle_link(&h, url);
        }
    });
    if let Ok(Some(urls)) = app.deep_link().get_current() {
        for url in urls {
            handle_link(app, url);
        }
    }
}

/// Registers the configured schemes with the OS (Linux and Windows; macOS reads them from the
/// bundle).
#[cfg(any(target_os = "linux", target_os = "windows"))]
fn register_schemes<R: Runtime>(app: &AppHandle<R>) {
    use tauri_plugin_deep_link::DeepLinkExt;
    if let Err(e) = app.deep_link().register_all() {
        tracing::warn!(error = %e, "sew-app: registering the deep-link schemes");
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn register_schemes<R: Runtime>(_app: &AppHandle<R>) {}

/// Routes one deep link: a pairing link is held for the person's confirm (`pair_request`, only
/// for a saved server; else `pair_refused`); a saved server's page opens in the main window
/// (signed in through the handoff page when the active account is on that server); anything
/// else is the `deep_link` event.
pub(crate) fn handle_link<R: Runtime>(app: &AppHandle<R>, link: url::Url) {
    let (Some(svc), Some(shell)) = (app.try_state::<AppService>(), app.try_state::<Shell>()) else {
        return;
    };
    let svc = svc.inner().clone();
    let scheme = shell.scheme.clone().unwrap_or_default();
    let main = shell.main_window.clone();
    let saved: Vec<Origin> = svc.book().servers.into_iter().map(|s| s.origin).collect();
    let route = crate::deeplink::route(&link, &scheme, &saved);
    if let DeepLinkRoute::Pair { host, server, code } = route {
        // Never navigated and never passed on: the code stays here until the person confirms.
        svc.offer_pairing(&host, server.as_ref(), code);
        focus(app, &main);
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut ev = DeepLinkEvent {
            url: link.to_string(),
            server: None,
            path: None,
            navigated: false,
        };
        if let DeepLinkRoute::Open { server, url } = route {
            let path = format!(
                "{}{}{}",
                url.path(),
                url.query().map(|q| format!("?{q}")).unwrap_or_default(),
                url.fragment().map(|f| format!("#{f}")).unwrap_or_default()
            );
            ev.server = Some(server.clone());
            ev.path = Some(path.clone());
            let target = match svc.active().filter(|k| k.server == server) {
                Some(k) => svc.open_url(&k, &path).await.unwrap_or(url),
                None => url,
            };
            if let Some(w) = app.get_webview_window(&main) {
                ev.navigated = w.navigate(target).is_ok();
                focus(&app, &main);
            }
        }
        svc.emit(&AppEvent::DeepLink(ev));
    });
}

/// Emits the core's events to every window as `sew-app://<name>`.
pub struct TauriAppSink<R: Runtime> {
    app: AppHandle<R>,
}

impl<R: Runtime> std::fmt::Debug for TauriAppSink<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TauriAppSink").finish_non_exhaustive()
    }
}

impl<R: Runtime> AppEventSink for TauriAppSink<R> {
    fn emit(&self, event: &AppEvent) {
        if let Err(e) = self.app.emit(&event.event_name(), event.payload()) {
            tracing::warn!(event = event.name(), error = %e, "sew-app: emit failed");
        }
    }
}

/// The voice plugin's `forget_server` through its `VoiceControl` (Tauri state, when the voice
/// plugin is registered). A live call is left first: its session is ending anyway.
struct TauriVoiceForget<R: Runtime> {
    app: AppHandle<R>,
}

impl<R: Runtime> VoiceForget for TauriVoiceForget<R> {
    fn forget(&self, install_id: u64) -> BoxFuture<'_, Result<(), AppError>> {
        Box::pin(async move {
            let Some(vc) = self.app.try_state::<VoiceControl>() else {
                return Ok(());
            };
            let vc = vc.inner().clone();
            let failed = |e: tauri_plugin_sew_voice::ErrorInfo| {
                AppError::with_detail("voice_forget_failed", e.code)
            };
            match vc.forget_server(install_id).await {
                Ok(_) => Ok(()),
                Err(e) if e.code == "in_call" => {
                    let _ = vc.leave().await;
                    vc.forget_server(install_id)
                        .await
                        .map(|_| ())
                        .map_err(failed)
                }
                Err(e) => Err(failed(e)),
            }
        })
    }
}
