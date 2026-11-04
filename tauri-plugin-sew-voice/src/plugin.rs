//! The Tauri glue: the plugin builder and the event emitter (feature `tauri`).

use std::path::PathBuf;
use std::sync::Arc;

use tauri::plugin::{Builder as PluginBuilder, TauriPlugin};
use tauri::{AppHandle, Emitter, Manager, Runtime};

use crate::DeviceKeyStore;
use crate::engine::{JoinProvider, VoiceEngine};
use crate::service::{EventSink, Spawn, VoiceService};
use crate::wire::PluginEvent;

/// Builds the plugin: `Builder::new(provider).build()`.
///
/// The engine is `sew-call-client`'s ([`crate::real::CallClientEngine::with_mls`]: Sew's MLS
/// call keys, kessel-audio on the system's devices; feature `audio`, on by default). Its device
/// store lives under [`Builder::data_dir`] (default: the app's data directory, `sew-voice/`): the
/// device key in a 0600 file there, unless the host passes its own store
/// ([`Builder::device_key_store`]) or builds with feature `keyring-store` (the OS credential
/// store, entries under [`Builder::app_id`], default the app's identifier).
/// [`Builder::engine`] replaces it, for tests and the fake engine; the plugin never falls back
/// to the fake on its own.
pub struct Builder {
    provider: Arc<dyn JoinProvider>,
    engine: Option<Arc<dyn VoiceEngine>>,
    data_dir: Option<PathBuf>,
    app_id: Option<String>,
    device_keys: Option<Arc<dyn DeviceKeyStore>>,
}

impl std::fmt::Debug for Builder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Builder")
            .field("engine", &self.engine.is_some())
            .field("data_dir", &self.data_dir)
            .field("app_id", &self.app_id)
            .field("device_key_store", &self.device_keys.is_some())
            .finish_non_exhaustive()
    }
}

impl Builder {
    /// A plugin that joins through `provider` (the host's sign-in, in Rust).
    pub fn new(provider: impl JoinProvider) -> Builder {
        Builder::with_provider(Arc::new(provider))
    }

    /// As [`Builder::new`], for a provider the host also keeps.
    pub fn with_provider(provider: Arc<dyn JoinProvider>) -> Builder {
        Builder {
            provider,
            engine: None,
            data_dir: None,
            app_id: None,
            device_keys: None,
        }
    }

    /// Another voice engine than `sew-call-client`'s (tests, [`crate::fake::FakeEngine`]).
    pub fn engine(mut self, engine: Arc<dyn VoiceEngine>) -> Builder {
        self.engine = Some(engine);
        self
    }

    /// Where the engine keeps its device store (the pins and, without
    /// [`Builder::device_key_store`], the device and its key in a 0600 file). Default: the
    /// app's data directory, `sew-voice/`.
    pub fn data_dir(mut self, dir: impl Into<PathBuf>) -> Builder {
        self.data_dir = Some(dir.into());
        self
    }

    /// The app identifier the engine's OS credential store entries are named after
    /// (`sew-voice.<app id>`, feature `keyring-store`). Default: the app's identifier
    /// (`tauri.conf.json`).
    pub fn app_id(mut self, app_id: impl Into<String>) -> Builder {
        self.app_id = Some(app_id.into());
        self
    }

    /// The host's own store for the device's private key and public record (e.g. K2's keyring
    /// or vault): `sew_call_client::device::DeviceKeyStore`, keyed by install id, member id and
    /// login scope. Default: a 0600 key file under [`Builder::data_dir`].
    pub fn device_key_store(mut self, store: Arc<dyn DeviceKeyStore>) -> Builder {
        self.device_keys = Some(store);
        self
    }

    /// The Tauri plugin (`sew-voice`).
    pub fn build<R: Runtime>(self) -> TauriPlugin<R> {
        let Builder {
            provider,
            engine,
            data_dir,
            app_id,
            device_keys,
        } = self;
        PluginBuilder::new(crate::PLUGIN_NAME)
            .invoke_handler(tauri::generate_handler![
                crate::commands::join,
                crate::commands::leave,
                crate::commands::mute,
                crate::commands::devices,
                crate::commands::set_device,
                crate::commands::state,
            ])
            .setup(move |app, _api| {
                let engine = match engine {
                    Some(engine) => engine,
                    None => default_engine(app, data_dir, app_id, device_keys)?,
                };
                let sink = Arc::new(TauriSink { app: app.clone() });
                let spawn: Spawn = Arc::new(|fut| {
                    tauri::async_runtime::spawn(fut);
                });
                let service = VoiceService::new(engine, provider, sink, spawn);
                service.start();
                app.manage(service);
                Ok(())
            })
            .build()
    }
}

/// `sew-call-client`'s engine for this app.
#[cfg(feature = "audio")]
fn default_engine<R: Runtime>(
    app: &AppHandle<R>,
    data_dir: Option<PathBuf>,
    app_id: Option<String>,
    device_keys: Option<Arc<dyn DeviceKeyStore>>,
) -> Result<Arc<dyn VoiceEngine>, Box<dyn std::error::Error>> {
    let dir = match data_dir {
        Some(dir) => dir,
        None => app.path().app_data_dir()?.join("sew-voice"),
    };
    std::fs::create_dir_all(&dir)?;
    let app_id = app_id.unwrap_or_else(|| app.config().identifier.clone());
    let mut cfg = sew_call_client::ClientConfig::new(&app_id, dir);
    if let Some(store) = device_keys {
        cfg = cfg.with_device_key_store(store);
    }
    Ok(Arc::new(crate::real::CallClientEngine::with_mls(cfg)))
}

/// Without the `audio` feature there is no default engine: [`Builder::engine`] is required.
#[cfg(not(feature = "audio"))]
fn default_engine<R: Runtime>(
    _app: &AppHandle<R>,
    _data_dir: Option<PathBuf>,
    _app_id: Option<String>,
    _device_keys: Option<Arc<dyn DeviceKeyStore>>,
) -> Result<Arc<dyn VoiceEngine>, Box<dyn std::error::Error>> {
    Err(
        "tauri-plugin-sew-voice: built without the `audio` feature, so Builder::engine is required"
            .into(),
    )
}

/// Emits the plugin's events to every window as `sew-voice://<name>`.
pub struct TauriSink<R: Runtime> {
    app: AppHandle<R>,
}

impl<R: Runtime> std::fmt::Debug for TauriSink<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TauriSink").finish_non_exhaustive()
    }
}

impl<R: Runtime> EventSink for TauriSink<R> {
    fn emit(&self, event: &PluginEvent) {
        if let Err(e) = self.app.emit(&event.event_name(), event.payload()) {
            tracing::warn!(event = event.name(), error = %e, "sew-voice: emit failed");
        }
    }
}
