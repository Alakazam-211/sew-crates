//! The real engine's video backend (feature `video-core`): kessel-video's `system` with the
//! plugin's `video`, built on first use with the phone's `AndroidContext`, or the host's own
//! (`CallClientEngine::with_video_backend`). It answers the camera permission, carries the
//! backend's background signal, gives the native sinks their VP8 decoder, and is the camera
//! and screen publisher's (`VideoBridge`, made once over the same backend) and the capture's
//! (the bridge's `VideoCapture`: cameras, screens, the preview; `real/capture.rs`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use sew_call_client::kessel_video::{AndroidContext, BackendEvent, Permission, VideoBackend};
use sew_call_client::{VideoBridge, VideoCapture};
use tokio::sync::broadcast;

use super::lock;
use crate::wire::PermissionState;

/// How often the backend's machine-wide events are read (they have no wake).
fn event_poll() -> Duration {
    if cfg!(test) {
        Duration::from_millis(5)
    } else {
        Duration::from_millis(100)
    }
}

/// The engine's video backend and what hangs off it.
pub(super) struct EngineVideo {
    /// The host's own backend ([`super::CallClientEngine::with_video_backend`]).
    given: Mutex<Option<Arc<dyn VideoBackend>>>,
    /// Built on first use.
    backend: OnceLock<Option<Arc<dyn VideoBackend>>>,
    /// The Android backend's context: the host's, else one without JNI handles (the camera
    /// permission then reads as `prompt`). Other platforms ignore it.
    android: Mutex<AndroidContext>,
    /// The backend's `Background(bool)` events.
    background_tx: broadcast::Sender<bool>,
    /// The event thread runs.
    events_on: AtomicBool,
    /// The camera and screen publisher over [`EngineVideo::backend`] (kessel-video's null
    /// backend when there is none), made on first use.
    bridge: OnceLock<Arc<VideoBridge>>,
}

impl Default for EngineVideo {
    fn default() -> EngineVideo {
        EngineVideo {
            given: Mutex::new(None),
            backend: OnceLock::new(),
            android: Mutex::new(AndroidContext::default()),
            background_tx: broadcast::channel(16).0,
            events_on: AtomicBool::new(false),
            bridge: OnceLock::new(),
        }
    }
}

impl EngineVideo {
    pub(super) fn set_backend(&self, backend: Arc<dyn VideoBackend>) {
        *lock(&self.given) = Some(backend);
    }

    pub(super) fn set_android(&self, ctx: AndroidContext) {
        *lock(&self.android) = ctx;
    }

    /// The backend, built on the first call.
    pub(super) fn backend(&self) -> Option<Arc<dyn VideoBackend>> {
        self.backend
            .get_or_init(|| {
                if let Some(b) = lock(&self.given).clone() {
                    return Some(b);
                }
                self.system()
            })
            .clone()
    }

    /// The camera and screen publisher, over the same backend (made on the first call).
    pub(super) fn bridge(&self) -> Arc<VideoBridge> {
        Arc::clone(self.bridge.get_or_init(|| {
            let b = self
                .backend()
                .unwrap_or_else(sew_call_client::kessel_video::null);
            Arc::new(VideoBridge::new(b))
        }))
    }

    /// The capture (cameras, screens, the preview) the bridge opens its cameras through, so
    /// one opener holds the camera; `None` without a backend.
    pub(super) fn capture(&self) -> Option<VideoCapture> {
        self.backend()?;
        Some(self.bridge().capture().clone())
    }

    /// kessel-video's backend for this OS (the plugin's `video`), with the phone's context.
    #[cfg(feature = "video")]
    fn system(&self) -> Option<Arc<dyn VideoBackend>> {
        let mut cfg = sew_call_client::kessel_video::SystemConfig::default();
        cfg.android = Some(lock(&self.android).clone());
        match sew_call_client::kessel_video::system(cfg) {
            Ok(b) => Some(b),
            Err(e) => {
                tracing::warn!(error = %e, "sew-voice: no video backend");
                None
            }
        }
    }

    /// Without the plugin's `video` there is no OS backend (only the host's).
    #[cfg(not(feature = "video"))]
    fn system(&self) -> Option<Arc<dyn VideoBackend>> {
        None
    }

    /// The app's scene, for the Android backend (cameras close in the background there).
    pub(super) fn app_scene(&self, background: bool) {
        lock(&self.android).set_background(background);
    }

    pub(super) fn device_orientation(&self, degrees: u32) {
        lock(&self.android).set_device_orientation(degrees);
    }

    pub(super) fn camera_permission(&self) -> PermissionState {
        self.backend().map_or(PermissionState::Unsupported, |b| {
            permission(b.camera_permission())
        })
    }

    pub(super) async fn request_camera_permission(&self) -> PermissionState {
        let Some(b) = self.backend() else {
            return PermissionState::Unsupported;
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        b.request_camera_permission(Box::new(move |p| {
            let _ = tx.send(p);
        }));
        // Dropped uncalled only after a shutdown: then it is what the OS says now.
        match rx.await {
            Ok(p) => permission(p),
            Err(_) => permission(b.camera_permission()),
        }
    }

    /// The backend's `Background(bool)` events, from a thread that reads them while the
    /// engine lives (`owner` is the engine's). `None` without a backend.
    pub(super) fn background_events(
        &self,
        owner: Weak<super::EngineInner>,
    ) -> Option<broadcast::Receiver<bool>> {
        let b = self.backend()?;
        let rx = self.background_tx.subscribe();
        if !self.events_on.swap(true, Ordering::AcqRel) {
            let events = b.subscribe();
            drop(b);
            let spawned = std::thread::Builder::new()
                .name("sew-voice-video-events".into())
                .spawn(move || {
                    loop {
                        std::thread::sleep(event_poll());
                        let Some(inner) = owner.upgrade() else { return };
                        while let Some(e) = events.try_recv() {
                            if let BackendEvent::Background(bg) = e {
                                let _ = inner.video.background_tx.send(bg);
                            }
                        }
                    }
                });
            if let Err(e) = spawned {
                tracing::warn!(error = %e, "sew-voice: no video event thread");
            }
        }
        Some(rx)
    }
}

/// The wire's permission of kessel-video's (restricted is `denied`; not asked is `prompt`).
fn permission(p: Permission) -> PermissionState {
    match p {
        Permission::Granted => PermissionState::Granted,
        Permission::NotDetermined => PermissionState::Prompt,
        _ => PermissionState::Denied,
    }
}
