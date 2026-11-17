//! The Tauri glue: the plugin builder and the event emitter (feature `tauri`).

use std::path::PathBuf;
use std::sync::Arc;

use sew_call_client::rustls::crypto::CryptoProvider;
use tauri::plugin::{Builder as PluginBuilder, TauriPlugin};
use tauri::{AppHandle, Emitter, Manager, RunEvent, Runtime, WindowEvent};

use crate::engine::{JoinProvider, VoiceEngine};
use crate::listener::{VoiceControl, VoiceListener};
use crate::service::{EventSink, Spawn, VoiceService};
use crate::wire::PluginEvent;
use crate::{DeviceKeyStore, DevicePlatform, HardwareDeviceKey};

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
///
/// A native shim (CallKit, Telecom) adds a [`VoiceListener`] with [`Builder::listener`] and
/// acts through [`VoiceControl`], which the plugin manages as Tauri state:
/// `app.state::<VoiceControl>().inner().clone()`.
pub struct Builder {
    provider: Arc<dyn JoinProvider>,
    engine: Option<Arc<dyn VoiceEngine>>,
    data_dir: Option<PathBuf>,
    app_id: Option<String>,
    device_keys: Option<Arc<dyn DeviceKeyStore>>,
    listener: Option<Arc<dyn VoiceListener>>,
    device: DeviceOptions,
    crypto_provider: Option<Arc<CryptoProvider>>,
    noise_removal: Option<crate::wire::NoiseRemoval>,
    android_unprocessed: Option<bool>,
    #[cfg(feature = "dfn3")]
    dfn3_weights: Option<PathBuf>,
    #[cfg(feature = "dfn3")]
    dfn3_idle_unload: Option<std::time::Duration>,
}

/// Row 11: the device key in hardware and the enrolment platform; and the video backend.
#[derive(Default)]
struct DeviceOptions {
    hardware_key: Option<Arc<dyn HardwareDeviceKey>>,
    platform: Option<DevicePlatform>,
    #[cfg(feature = "video-core")]
    video_backend: Option<Arc<dyn sew_call_client::kessel_video::VideoBackend>>,
    #[cfg(feature = "video-core")]
    android_video: Option<sew_call_client::kessel_video::AndroidContext>,
}

impl std::fmt::Debug for Builder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Builder")
            .field("engine", &self.engine.is_some())
            .field("data_dir", &self.data_dir)
            .field("app_id", &self.app_id)
            .field("device_key_store", &self.device_keys.is_some())
            .field("listener", &self.listener.is_some())
            .field("hardware_device_key", &self.device.hardware_key.is_some())
            .field("device_platform", &self.device.platform)
            .field("crypto_provider", &self.crypto_provider.is_some())
            .field("noise_removal", &self.noise_removal)
            .field("android_unprocessed_supported", &self.android_unprocessed)
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
            listener: None,
            device: DeviceOptions::default(),
            crypto_provider: None,
            noise_removal: None,
            android_unprocessed: None,
            #[cfg(feature = "dfn3")]
            dfn3_weights: None,
            #[cfg(feature = "dfn3")]
            dfn3_idle_unload: None,
        }
    }

    /// Where DeepFilterNet3's weights are (feature `dfn3`): the file the app bundles,
    /// `sew_call_client::DFN3_WEIGHTS_FILE`. Default: `dfn3/dfn3_ll.nnef.tar` in the app's
    /// resource directory (bundle `dfn3_ll.nnef.tar`, `NOTICE`, `LICENSE-MIT` and
    /// `LICENSE-APACHE` under `dfn3/`). At start the plugin only records the path and checks
    /// the file against its pinned sha256 (on a thread of its own; nothing stays in memory).
    /// The model is loaded when a call first needs it (a call starting in Standard, or the
    /// first switch to Standard in a call): that call starts on RNNoise (on phones, the OS's
    /// voice processing alone) with `model_reason: loading` and steps up to DeepFilterNet3
    /// when it is ready, without a click. After the
    /// last call the model is released once [`Builder::dfn3_idle_unload`]'s time passes. If
    /// the file is missing or not the pinned one, Standard is RNNoise and the noise removal
    /// state says `model_reason: unavailable`.
    #[cfg(feature = "dfn3")]
    pub fn dfn3_weights(mut self, path: impl Into<PathBuf>) -> Builder {
        self.dfn3_weights = Some(path.into());
        self
    }

    /// How long DeepFilterNet3's model stays in memory after the last call ends (feature
    /// `dfn3`): `sew_call_client::DFN3_IDLE_UNLOAD` by default, so a call back or the next
    /// meeting soon after finds it loaded, and the memory is back once calls stop. The next
    /// call after it loads the model again. `Duration::MAX` keeps it for the app's life.
    #[cfg(feature = "dfn3")]
    pub fn dfn3_idle_unload(mut self, after: std::time::Duration) -> Builder {
        self.dfn3_idle_unload = Some(after);
        self
    }

    /// The microphone's noise removal at start (the app's default, or the person's stored
    /// choice): [`crate::NoiseRemoval::Standard`] if never called. `set_noise_removal` changes
    /// it later. An engine that can't run the mode keeps its own and logs a warning.
    pub fn noise_removal(mut self, mode: crate::wire::NoiseRemoval) -> Builder {
        self.noise_removal = Some(mode);
        self
    }

    /// Android: whether this device has the unprocessed audio source, as the app's Android
    /// side reads it (`AudioManager.getProperty(AudioManager.PROPERTY_SUPPORT_AUDIO_SOURCE_UNPROCESSED)`
    /// is `"true"`). Noise removal Off captures the microphone raw: from that source when the
    /// device has it, else from the voice recognition source (the default when this is never
    /// called; the plugin has no Android code of its own to read it).
    /// [`crate::VoiceControl::set_android_unprocessed_supported`] sets it later. iOS and desktop
    /// ignore it.
    pub fn android_unprocessed_supported(mut self, supported: bool) -> Builder {
        self.android_unprocessed = Some(supported);
        self
    }

    /// A native observer of the call ([`VoiceListener`], at most one: a second call replaces
    /// the first). It is called after each `sew-voice://` event, in order, under the plugin's
    /// lock, whether or not a webview exists or is awake.
    pub fn listener(mut self, listener: Arc<dyn VoiceListener>) -> Builder {
        self.listener = Some(listener);
        self
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

    /// The device's private key in hardware: a non-exportable P-256 key in the Secure Enclave
    /// (iOS) or StrongBox/TEE (Android) behind the host's [`HardwareDeviceKey`]. The engine
    /// never falls back to a software key while it is set (a key error fails the join with
    /// `e2ee_refused`); it replaces [`Builder::device_key_store`]. The public record stays under
    /// [`Builder::data_dir`]. Without it, today's software key.
    pub fn hardware_device_key(mut self, key: Arc<dyn HardwareDeviceKey>) -> Builder {
        self.device.hardware_key = Some(key);
        self
    }

    /// The platform the device enrols as. Default from `target_os`: `ios`, `android`,
    /// otherwise `desktop`.
    pub fn device_platform(mut self, platform: DevicePlatform) -> Builder {
        self.device.platform = Some(platform);
        self
    }

    /// The engine's video backend instead of kessel-video's `system` for this OS (feature
    /// `video-core`): for tests, or a host that shares its own backend.
    #[cfg(feature = "video-core")]
    pub fn video_backend(
        mut self,
        backend: Arc<dyn sew_call_client::kessel_video::VideoBackend>,
    ) -> Builder {
        self.device.video_backend = Some(backend);
        self
    }

    /// Android (feature `video-core`): the app's JNI handles for kessel-video's Android
    /// backend, so it reads the CAMERA permission:
    /// `let ctx = ndk_context::android_context();`
    /// `unsafe { AndroidContext::new(ctx.vm(), ctx.context()) }`. Without it the plugin uses a
    /// context with no handles (the permission reads as `prompt` until a camera opens). The
    /// plugin drives its background flag and orientation from
    /// [`VoiceControl::app_scene`] and [`VoiceControl::device_orientation`]; other platforms
    /// ignore it.
    #[cfg(feature = "video-core")]
    pub fn android_video_context(
        mut self,
        ctx: sew_call_client::kessel_video::AndroidContext,
    ) -> Builder {
        self.device.android_video = Some(ctx);
        self
    }

    /// The rustls crypto provider of the engine's TLS connections (signalling, TURN-TLS, Sew's
    /// API): `sew_call_client::ClientConfig::with_crypto_provider`. Default: the process
    /// default (`CryptoProvider::install_default`), else the built-in ring provider (feature
    /// `builtin-ring-provider`, on by default); with neither, a join fails (`engine_stopped`,
    /// the detail naming `no_crypto_provider`).
    pub fn crypto_provider(mut self, provider: Arc<CryptoProvider>) -> Builder {
        self.crypto_provider = Some(provider);
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
            listener,
            device,
            crypto_provider,
            noise_removal,
            android_unprocessed,
            #[cfg(feature = "dfn3")]
            dfn3_weights,
            #[cfg(feature = "dfn3")]
            dfn3_idle_unload,
        } = self;
        PluginBuilder::new(crate::PLUGIN_NAME)
            .invoke_handler(tauri::generate_handler![
                crate::commands::join,
                crate::commands::leave,
                crate::commands::mute,
                crate::commands::devices,
                crate::commands::set_device,
                crate::commands::state,
                crate::commands::consent,
                crate::commands::withdraw_consent,
                crate::commands::start_recording,
                crate::commands::stop_recording,
                crate::commands::set_recording_rights,
                crate::commands::forget_server,
                crate::commands::known_device_keys,
                crate::commands::call_details,
                crate::commands::ring,
                crate::commands::ring_ended,
                crate::commands::video_caps,
                crate::commands::video_devices,
                crate::commands::set_video_device,
                crate::commands::camera,
                crate::commands::screen_sources,
                crate::commands::screen,
                crate::commands::preview,
                crate::commands::video_sink,
                crate::commands::video_sink_update,
                crate::commands::video_sink_close,
                crate::commands::request_keyframe,
                crate::commands::camera_permission,
                crate::commands::request_camera_permission,
                crate::commands::set_video_codec_preference,
                crate::commands::set_noise_removal,
                crate::commands::noise_removal,
            ])
            .setup(move |app, _api| {
                let engine = match engine {
                    Some(engine) => engine,
                    None => {
                        default_engine(app, data_dir, app_id, device_keys, device, crypto_provider)?
                    }
                };
                if let Some(mode) = noise_removal
                    && let Err(e) = engine.set_noise_removal(mode)
                {
                    tracing::warn!(mode = mode.as_str(), error = %e, "sew-voice: noise removal not set");
                }
                if let Some(supported) = android_unprocessed {
                    engine.set_android_unprocessed_supported(supported);
                }
                #[cfg(feature = "dfn3")]
                if let Some(after) = dfn3_idle_unload {
                    engine.set_dfn3_idle_unload(after);
                }
                #[cfg(feature = "dfn3")]
                load_dfn3(app, &engine, dfn3_weights);
                let sink = Arc::new(TauriSink { app: app.clone() });
                let spawn: Spawn = Arc::new(|fut| {
                    tauri::async_runtime::spawn(fut);
                });
                let service = match listener {
                    Some(l) => VoiceService::with_listener(engine, provider, sink, l, spawn),
                    None => VoiceService::new(engine, provider, sink, spawn),
                };
                service.start();
                // The native shim's handle (CallKit, Telecom), over the same service.
                app.manage(VoiceControl::new(service.clone()));
                app.manage(service);
                Ok(())
            })
            // The app's lifecycle as Tauri reports it: the app resumed (iOS: it became active;
            // Android: `onResume`), or a window gained or lost focus. Each is a moment the
            // device is busy: the live call's noise removal opens a grace window. (Tauri has no
            // event for going to the background on phones; the native shim reports that with
            // `VoiceControl::app_scene`, which does the same.)
            .on_event(|app, event| {
                if matches!(
                    event,
                    RunEvent::Resumed
                        | RunEvent::WindowEvent {
                            event: WindowEvent::Focused(_),
                            ..
                        }
                ) {
                    app_scene_changed(app);
                }
            })
            .build()
    }
}

/// The app's scene or focus changed: to the service's engine (a grace window for the live
/// call's noise removal). Before the plugin's setup ran there is no service yet.
fn app_scene_changed<R: Runtime>(app: &AppHandle<R>) {
    if let Some(service) = app.try_state::<VoiceService>() {
        service.app_scene_changed();
    }
}

/// Gives `engine` DeepFilterNet3's weights on a thread of its own (the pinned sha256 is checked
/// by reading the file through; the model is loaded when a call first needs it): `path`, or
/// `dfn3/<weights file>` in the app's resources.
#[cfg(feature = "dfn3")]
fn load_dfn3<R: Runtime>(app: &AppHandle<R>, engine: &Arc<dyn VoiceEngine>, path: Option<PathBuf>) {
    let path = match path {
        Some(p) => p,
        None => match app.path().resource_dir() {
            Ok(dir) => dir.join("dfn3").join(sew_call_client::DFN3_WEIGHTS_FILE),
            Err(e) => {
                tracing::warn!(error = %e, "sew-voice: no resource directory; noise removal stays RNNoise");
                return;
            }
        },
    };
    let engine = Arc::clone(engine);
    let spawned = std::thread::Builder::new()
        .name("sew-voice-dfn3".into())
        .spawn(move || match engine.set_dfn3_weights(&path) {
            Ok(()) => tracing::info!(path = %path.display(), "sew-voice: DeepFilterNet3 weights found; loaded when a call needs them"),
            Err(e) => tracing::warn!(
                path = %path.display(),
                error = %e,
                "sew-voice: DeepFilterNet3 weights refused; noise removal stays RNNoise"
            ),
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "sew-voice: DeepFilterNet3 loader thread");
    }
}

/// `sew-call-client`'s engine for this app.
#[cfg(feature = "audio")]
fn default_engine<R: Runtime>(
    app: &AppHandle<R>,
    data_dir: Option<PathBuf>,
    app_id: Option<String>,
    device_keys: Option<Arc<dyn DeviceKeyStore>>,
    device: DeviceOptions,
    crypto_provider: Option<Arc<CryptoProvider>>,
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
    // After the host store: the hardware key wins, with no fallback.
    if let Some(key) = device.hardware_key {
        cfg = cfg.with_hardware_device_key(key);
    }
    if let Some(platform) = device.platform {
        cfg = cfg.with_device_platform(platform);
    }
    if let Some(provider) = crypto_provider {
        cfg = cfg.with_crypto_provider(provider);
    }
    #[allow(unused_mut)]
    let mut engine = crate::real::CallClientEngine::with_mls(cfg);
    #[cfg(feature = "video-core")]
    {
        if let Some(ctx) = device.android_video {
            engine = engine.with_android_video_context(ctx);
        }
        if let Some(backend) = device.video_backend {
            engine = engine.with_video_backend(backend);
        }
    }
    Ok(Arc::new(engine))
}

/// Without the `audio` feature there is no default engine: [`Builder::engine`] is required.
#[cfg(not(feature = "audio"))]
fn default_engine<R: Runtime>(
    _app: &AppHandle<R>,
    _data_dir: Option<PathBuf>,
    _app_id: Option<String>,
    _device_keys: Option<Arc<dyn DeviceKeyStore>>,
    _device: DeviceOptions,
    _crypto_provider: Option<Arc<CryptoProvider>>,
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
