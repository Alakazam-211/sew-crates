//! The real engine's capture side (feature `video-core`): `sew-call-client`'s [`VideoCapture`]
//! over the app's kessel-video backend, in the plugin's shapes.
//!
//! - **Capabilities** (`video_caps`): the backend's camera and screen support, its name, and
//!   `publish_codecs`: VP8 and H.264 where the backend has an encoder for them.
//! - **Devices** (`video_devices`, `set_video_device`, the `video_devices` event): the
//!   backend's cameras, the chosen one, and the OS permissions as the four agreed words
//!   (`granted`, `denied`, `prompt`, `unsupported`). Hot-plug comes from the backend's events,
//!   watched from the first subscription.
//! - **Screens** (`screen_sources`): displays and windows with their thumbnails as base64 PNGs
//!   at most 320 px wide; `[]` where only the OS picker exists.
//! - **Preview** (`preview`, `preview_keyframe`): the camera outside a call through
//!   [`sew_call_client::Preview`] (one 360p VP8 encoder, no lease, nothing sent), its frames as
//!   `local:camera` items; off (or a new preview) closes the camera at once. The capture is the
//!   camera publish's own (`VideoBridge::capture`): `camera {on}` takes the camera over from
//!   the preview, one opener at a time.
//! - **The screen-source side** for the publish: [`EngineCapture::open_screen`] (screen
//!   capture present, the permission, the listed source or the OS picker with the app window
//!   as its parent, capture with the screen content hint) and [`local_video_after`] (a
//!   source's pause and end events as the `local_video` event).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use base64::Engine as _;
use serde_json::json;
use sew_call_client::kessel_video::{self, BackendEvent, Permission};
use sew_call_client::{
    CaptureError, CaptureEvent, CaptureKind, CapturePause, Preview, PreviewItem, ScreenChoice,
    ScreenPick, VideoCapture, screen_constraints,
};
use tokio::sync::{broadcast, mpsc};

use super::lock;
use crate::engine::EngineError;
use crate::video::{
    EncodedVideoFrame, LOCAL_CAMERA, ParentWindow, PreviewRequest, ScreenRequest, VideoCodec,
    VideoFrames, VideoRecvItem,
};
use crate::wire::{
    Camera, DeviceChoice, ErrorInfo, Facing, LocalVideo, LocalVideoState, PermissionState,
    ScreenCaps, ScreenKind, ScreenSource, VideoCaps, VideoDevices, VideoPermissions, video_code,
    video_reason,
};

/// How often the backend's machine-wide events are read for camera hot-plug (after the first
/// `video_devices` subscription).
const HOTPLUG_POLL: Duration = Duration::from_millis(500);

/// The largest screen share by default: 1080p at 15 fps (the room's `policy.screen` may
/// lower it).
pub(crate) const SCREEN_MAX: (u32, u32, u32) = (1920, 1080, 15);

/// The engine's capture state (one per engine).
pub(super) struct EngineCapture {
    shared: Arc<Shared>,
}

struct Shared {
    capture: Mutex<Option<VideoCapture>>,
    /// The camera chosen with `set_video_device` (`None`: the OS default).
    camera: Mutex<Option<String>>,
    preview: Mutex<Option<Preview>>,
    devices_tx: broadcast::Sender<VideoDevices>,
    watching: AtomicBool,
}

/// A capture refusal as the plugin's error: the reject code, with the backend's own code for
/// `capture_failed`.
pub(crate) fn capture_error(e: &CaptureError) -> EngineError {
    match e {
        CaptureError::Failed(v) => EngineError::Failed(ErrorInfo::with_detail(
            video_code::CAPTURE_FAILED,
            json!({ "error": v.code(), "detail": v.to_string() }),
        )),
        other => EngineError::failed(other.code()),
    }
}

fn unsupported() -> EngineError {
    EngineError::failed(video_code::VIDEO_UNSUPPORTED)
}

/// An OS permission as the wire's word: restricted (MDM, parental controls) is `denied`, not
/// yet asked is `prompt`, and a capture this device doesn't have is `unsupported`.
pub(crate) fn permission_state(p: Permission, supported: bool) -> PermissionState {
    if !supported {
        return PermissionState::Unsupported;
    }
    match p {
        Permission::Granted => PermissionState::Granted,
        Permission::NotDetermined => PermissionState::Prompt,
        _ => PermissionState::Denied,
    }
}

pub(crate) fn facing(f: kessel_video::Facing) -> Facing {
    match f {
        kessel_video::Facing::Front => Facing::Front,
        kessel_video::Facing::Back => Facing::Back,
        kessel_video::Facing::External => Facing::External,
        _ => Facing::Unknown,
    }
}

fn wire_codec(c: sew_call_client::VideoCodec) -> Option<VideoCodec> {
    match c {
        sew_call_client::VideoCodec::Vp8 => Some(VideoCodec::Vp8),
        sew_call_client::VideoCodec::H264 => Some(VideoCodec::H264),
        _ => None,
    }
}

/// One `screen_sources` entry, its thumbnail base64 (standard alphabet, padded).
pub(crate) fn screen_source(c: &ScreenChoice) -> ScreenSource {
    ScreenSource {
        id: c.id.clone(),
        kind: match c.kind {
            kessel_video::ScreenKind::Window => ScreenKind::Window,
            _ => ScreenKind::Display,
        },
        title: c.title.clone(),
        app: c.app.clone(),
        width: c.width,
        height: c.height,
        thumbnail_png: c
            .thumbnail_png
            .as_ref()
            .map(|png| base64::engine::general_purpose::STANDARD.encode(png)),
    }
}

/// What `screen {on: true, source?}` shares: the listed source, or the OS picker over the app
/// window (Windows needs that window: `PickOptions::with_parent_window`).
pub(crate) fn screen_pick(source: Option<&str>, parent: Option<ParentWindow>) -> ScreenPick {
    match source {
        Some(id) => ScreenPick::Listed(id.to_owned()),
        None => ScreenPick::System {
            // The HWND's bits, as kessel-video takes them.
            parent_window: parent.map(|ParentWindow(h)| h as u64),
        },
    }
}

/// `local_video` after a camera's or screen's event (`None`: no change to tell). Paused:
/// `hidden`, `background`, `thermal`, `camera_busy` (another app took the camera); resumed:
/// back `on`; ended: `off` with `user_stopped`, `source_closed`, `camera_lost`, or
/// `camera_denied` / `screen_denied` for a permission revoked while sending.
pub(crate) fn local_video_after(
    kind: CaptureKind,
    event: &CaptureEvent,
    now: &LocalVideo,
) -> Option<LocalVideo> {
    let screen = kind == CaptureKind::Screen;
    let (state, reason) = match event {
        CaptureEvent::Paused(p) => (
            LocalVideoState::Paused,
            Some(match p {
                CapturePause::Hidden => video_reason::HIDDEN,
                CapturePause::Background => video_reason::BACKGROUND,
                CapturePause::Thermal => video_reason::THERMAL,
                CapturePause::InUseByAnother => video_code::CAMERA_BUSY,
                _ => video_reason::PAUSED,
            }),
        ),
        CaptureEvent::Resumed => (LocalVideoState::On, None),
        // The same reasons as the engine's `LocalVideo` when the bridge unpublishes it.
        CaptureEvent::Ended(end) => (LocalVideoState::Off, Some(end.reason(kind))),
        _ => return None,
    };
    let mut out = now.clone();
    if screen {
        out.screen = state;
    } else {
        out.camera = state;
    }
    out.reason = reason.map(str::to_owned);
    (out != *now).then_some(out)
}

impl Shared {
    fn capture(&self) -> Option<VideoCapture> {
        lock(&self.capture).clone()
    }

    fn devices(&self) -> Result<VideoDevices, EngineError> {
        let cap = self.capture().ok_or_else(unsupported)?;
        let caps = cap.capabilities();
        Ok(VideoDevices {
            cameras: cap
                .cameras()
                .iter()
                .map(|c| Camera {
                    id: c.id.clone(),
                    name: c.name.clone(),
                    facing: facing(c.facing),
                    default: c.is_default,
                })
                .collect(),
            camera: lock(&self.camera).clone(),
            permission: VideoPermissions {
                camera: permission_state(cap.camera_permission(), caps.camera),
                screen: permission_state(cap.screen_permission(), caps.screen.supported),
            },
        })
    }

    fn emit_devices(&self) {
        if let Ok(d) = self.devices() {
            let _ = self.devices_tx.send(d);
        }
    }
}

impl EngineCapture {
    pub(super) fn new() -> EngineCapture {
        let (devices_tx, _) = broadcast::channel(16);
        EngineCapture {
            shared: Arc::new(Shared {
                capture: Mutex::new(None),
                camera: Mutex::new(None),
                preview: Mutex::new(None),
                devices_tx,
                watching: AtomicBool::new(false),
            }),
        }
    }

    /// Capture through `backend` from now on (tests).
    #[cfg(test)]
    pub(super) fn set_backend(&self, backend: Arc<dyn kessel_video::VideoBackend>) {
        *lock(&self.shared.capture) = Some(VideoCapture::new(backend));
    }

    /// Capture through the engine's `capture` (the camera publish's own), unless it has one.
    pub(super) fn attach(&self, capture: impl FnOnce() -> Option<VideoCapture>) {
        let mut c = lock(&self.shared.capture);
        if c.is_none() {
            *c = capture();
        }
    }

    /// The capture, when the engine has a backend.
    pub(super) fn capture(&self) -> Option<VideoCapture> {
        self.shared.capture()
    }

    /// The camera chosen with `set_video_device` (`None`: the OS default).
    pub(super) fn chosen_camera(&self) -> Option<String> {
        lock(&self.shared.camera).clone()
    }

    /// `video_caps`: receive is always on (`video-core`); camera, screen and the publish
    /// codecs (its encoders: VP8, H.264) are the backend's. With a backend the self-view sinks
    /// are fed in a call as well as by the preview (`self_view`), and `set_video_device`
    /// switches a published camera (`live_switch`).
    pub(super) fn caps(&self) -> VideoCaps {
        let Some(cap) = self.capture() else {
            return VideoCaps {
                receive: true,
                ..VideoCaps::default()
            };
        };
        let caps = cap.capabilities();
        VideoCaps {
            camera: caps.camera,
            screen: ScreenCaps {
                supported: caps.screen.supported,
                system_picker: caps.screen.supported && caps.screen.system_picker,
                list: caps.screen.supported && caps.screen.list,
                windows: caps.screen.supported && caps.screen.windows,
            },
            publish_codecs: [
                (kessel_video::VideoCodec::Vp8, VideoCodec::Vp8),
                (kessel_video::VideoCodec::H264, VideoCodec::H264),
            ]
            .into_iter()
            .filter(|(k, _)| {
                caps.best(*k, kessel_video::CodecDirection::Encode)
                    .is_some()
            })
            .map(|(_, ours)| ours)
            .collect(),
            decode_codecs: Vec::new(),
            receive: true,
            self_view: true,
            live_switch: true,
            backend: caps.backend.to_owned(),
        }
    }

    pub(super) fn devices(&self) -> Result<VideoDevices, EngineError> {
        self.shared.devices()
    }

    /// `set_video_device`: kept for the next preview and camera (a running preview keeps its
    /// camera until it is opened again; the engine switches a published camera itself).
    pub(super) fn set_device(&self, camera: DeviceChoice) -> Result<(), EngineError> {
        let cap = self.capture().ok_or_else(unsupported)?;
        let id = match camera {
            DeviceChoice::Default => None,
            DeviceChoice::Id(id) => {
                if !cap.cameras().iter().any(|c| c.id == id) {
                    return Err(EngineError::Failed(ErrorInfo::with_detail(
                        video_code::CAMERA_NOT_FOUND,
                        json!({ "camera": id }),
                    )));
                }
                Some(id)
            }
        };
        *lock(&self.shared.camera) = id;
        self.shared.emit_devices();
        Ok(())
    }

    /// The camera list's changes: hot-plug and capability changes from the backend's events
    /// (read every [`HOTPLUG_POLL`] from the first subscription while the engine lives), and
    /// every `set_video_device`.
    pub(super) fn device_events(&self) -> Option<broadcast::Receiver<VideoDevices>> {
        let cap = self.capture()?;
        let rx = self.shared.devices_tx.subscribe();
        if !self.shared.watching.swap(true, Ordering::AcqRel) {
            let events = cap.events();
            let weak: Weak<Shared> = Arc::downgrade(&self.shared);
            let spawned = std::thread::Builder::new()
                .name("sew-voice-cameras".into())
                .spawn(move || {
                    loop {
                        std::thread::sleep(HOTPLUG_POLL);
                        let Some(shared) = weak.upgrade() else { return };
                        let mut changed = false;
                        while let Some(e) = events.try_recv() {
                            changed |= matches!(
                                e,
                                BackendEvent::Cameras(_) | BackendEvent::CapabilitiesChanged
                            );
                        }
                        if changed {
                            shared.emit_devices();
                        }
                    }
                });
            if let Err(e) = spawned {
                tracing::warn!(error = %e, "sew-voice: no camera hot-plug thread");
            }
        }
        Some(rx)
    }

    pub(super) async fn screen_sources(&self) -> Result<Vec<ScreenSource>, EngineError> {
        let cap = self.capture().ok_or_else(unsupported)?;
        let list = cap.screen_sources().await.map_err(|e| capture_error(&e))?;
        Ok(list.iter().map(screen_source).collect())
    }

    /// `preview {on, device?}`: any running preview closes first (its camera with it); on,
    /// the camera (`device`, else the chosen one, else the default) opens with one 360p
    /// encoder and its frames come as `local:camera` items.
    pub(super) async fn preview(
        &self,
        req: PreviewRequest,
    ) -> Result<Option<VideoFrames>, EngineError> {
        let old = lock(&self.shared.preview).take();
        drop(old);
        if !req.on {
            return Ok(None);
        }
        let cap = self.capture().ok_or_else(unsupported)?;
        let device = match req.device {
            Some(DeviceChoice::Default) => None,
            Some(DeviceChoice::Id(id)) => Some(id),
            None => lock(&self.shared.camera).clone(),
        };
        let (preview, rx) = cap
            .preview(device.as_deref())
            .await
            .map_err(|e| capture_error(&e))?;
        let (tx, plugin_rx) = mpsc::channel(super::video::FRAME_QUEUE);
        tokio::spawn(map_preview(rx, tx));
        let old = lock(&self.shared.preview).replace(preview);
        drop(old);
        Ok(Some(plugin_rx))
    }

    pub(super) fn preview_keyframe(&self) {
        if let Some(p) = lock(&self.shared.preview).as_ref() {
            p.request_keyframe();
        }
    }

    /// The source side of `screen {on: true, source?}` (the engine's screen publish takes the
    /// answer): screen capture present (`screen_unsupported`: Linux without the xdg portal or
    /// libpipewire), the screen-recording permission (`screen_denied`), the listed source
    /// (`screen_not_found`) or the OS picker over `req.parent_window` (`picker_cancelled`),
    /// then capture at 1080p15 (at most the room's `cap`: `policy.screen`'s width, height and
    /// rate) with the screen content hint.
    pub(crate) async fn open_screen(
        &self,
        req: &ScreenRequest,
        cap: Option<(u32, u32, u32)>,
    ) -> Result<sew_call_client::ScreenSource, EngineError> {
        let capture = self
            .capture()
            .ok_or_else(|| EngineError::failed(video_code::SCREEN_UNSUPPORTED))?;
        let (mut w, mut h, mut fps) = SCREEN_MAX;
        if let Some((cw, ch, cfps)) = cap {
            let below = |ours: u32, theirs: u32| if theirs > 0 { ours.min(theirs) } else { ours };
            w = below(w, cw).max(16);
            h = below(h, ch).max(16);
            fps = below(fps, cfps).max(1);
        }
        capture
            .open_screen(
                screen_pick(req.source.as_deref(), req.parent_window),
                screen_constraints(w, h, fps),
            )
            .await
            .map_err(|e| capture_error(&e))
    }
}

/// The preview's items as the plugin's `local:camera` items, until either side closes. The
/// preview's end (the camera ended or its encoder was lost) is a `Removed`: the self-view
/// sinks reset.
async fn map_preview(mut rx: sew_call_client::PreviewFrames, tx: mpsc::Sender<VideoRecvItem>) {
    let mut last = None;
    while let Some(item) = rx.recv().await {
        let facing_now = match &item {
            PreviewItem::Frame(f) => Some(facing(f.facing)),
            _ => None,
        };
        let out = match item {
            PreviewItem::Frame(f) => {
                let Some(codec) = wire_codec(f.codec) else {
                    continue;
                };
                VideoRecvItem::Frame(EncodedVideoFrame {
                    track: LOCAL_CAMERA.into(),
                    codec,
                    data: f.data.to_vec(),
                    keyframe: f.keyframe,
                    // 90 kHz from the capture clock (self-view never meets RTP).
                    rtp_ts: (f.capture_us.wrapping_mul(9) / 100) as u32,
                    arrival_us: f.capture_us,
                    width: f.width,
                    height: f.height,
                })
            }
            _ => VideoRecvItem::Removed {
                track: LOCAL_CAMERA.into(),
            },
        };
        if let Some(change) = super::video::facing_change(&mut last, facing_now, &out)
            && tx.send(change).await.is_err()
        {
            return;
        }
        let over = matches!(out, VideoRecvItem::Removed { .. });
        if tx.send(out).await.is_err() || over {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kessel_video::fake::{FakeBackend, FakeConfig, FakePermission};
    use sew_call_client::CaptureEnd;

    fn engine(cfg: FakeConfig) -> EngineCapture {
        let e = EngineCapture::new();
        e.set_backend(FakeBackend::new(cfg));
        e
    }

    /// The fake's default script with `f`'s changes (`FakeConfig` is non-exhaustive).
    fn fake(f: impl FnOnce(&mut FakeConfig)) -> FakeConfig {
        let mut c = FakeConfig::default();
        f(&mut c);
        c
    }

    #[test]
    fn permissions_are_the_four_agreed_words() {
        let cases = [
            (Permission::Granted, true, "granted"),
            (Permission::Denied, true, "denied"),
            (Permission::Restricted, true, "denied"),
            (Permission::NotDetermined, true, "prompt"),
            (Permission::Granted, false, "unsupported"),
            (Permission::Denied, false, "unsupported"),
        ];
        for (p, supported, word) in cases {
            assert_eq!(
                serde_json::to_value(permission_state(p, supported)).ok(),
                Some(json!(word)),
                "{p:?} supported={supported}"
            );
        }
    }

    #[test]
    fn caps_and_devices_come_from_the_backend() {
        let none = EngineCapture::new();
        assert_eq!(
            none.caps(),
            VideoCaps {
                receive: true,
                ..VideoCaps::default()
            },
            "no backend: receive only"
        );
        assert_eq!(
            none.devices().map_err(|e| e.to_info().code),
            Err("video_unsupported".to_owned())
        );
        assert!(none.device_events().is_none());

        let e = engine(fake(|c| {
            c.camera_permission = FakePermission::Prompt {
                answer: Permission::Granted,
                after: Duration::from_millis(1),
            };
        }));
        let caps = e.caps();
        assert!(caps.camera && caps.receive);
        assert!(
            caps.self_view && caps.live_switch,
            "the in-call self-view and the live camera switch"
        );
        assert_eq!(
            caps.screen,
            ScreenCaps {
                supported: true,
                system_picker: true,
                list: true,
                windows: true
            }
        );
        assert_eq!(
            caps.publish_codecs,
            vec![VideoCodec::Vp8, VideoCodec::H264],
            "the fake's pure-Rust VP8 and its structural H.264"
        );
        assert_eq!(caps.backend, "fake");
        let d = e.devices().expect("devices");
        assert_eq!(
            serde_json::to_value(&d).ok(),
            Some(json!({
                "cameras": [{"id": "fake:camera:0", "name": "Fake Camera", "facing": "front", "default": true}],
                "camera": null,
                "permission": {"camera": "prompt", "screen": "granted"},
            }))
        );

        let null = EngineCapture::new();
        null.set_backend(kessel_video::null());
        let d = null.devices().expect("devices");
        assert!(d.cameras.is_empty());
        assert_eq!(
            (d.permission.camera, d.permission.screen),
            (PermissionState::Unsupported, PermissionState::Unsupported)
        );
        assert!(!null.caps().camera && !null.caps().screen.supported);
        assert!(null.caps().publish_codecs.is_empty());
        // The flags say what this plugin does, not what the machine has.
        assert!(null.caps().self_view && null.caps().live_switch);
        assert!(
            !none.caps().self_view && !none.caps().live_switch,
            "no backend"
        );
        assert_eq!(null.caps().backend, "null");
    }

    #[tokio::test]
    async fn the_chosen_camera_is_checked_kept_and_announced() {
        let e = engine(FakeConfig::default());
        let mut rx = e.device_events().expect("events");
        let err = e
            .set_device(DeviceChoice::Id("cam:none".into()))
            .expect_err("unknown camera");
        assert_eq!(err.to_info().code, "camera_not_found");
        assert_eq!(err.to_info().detail, json!({"camera": "cam:none"}));
        e.set_device(DeviceChoice::Id("fake:camera:0".into()))
            .expect("known camera");
        let d = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("an event")
            .expect("devices");
        assert_eq!(d.camera.as_deref(), Some("fake:camera:0"));
        e.set_device(DeviceChoice::Default).expect("default");
        assert_eq!(e.devices().expect("devices").camera, None);
    }

    #[tokio::test]
    async fn camera_hot_plug_is_a_devices_event() {
        let mut two = FakeConfig::default().cameras;
        let mut usb = kessel_video::CameraInfo::new(
            "fake:camera:usb",
            "USB Camera",
            kessel_video::Facing::External,
            false,
        );
        usb.modes = two[0].modes.clone();
        two.push(usb);
        let e = engine(fake(|c| {
            c.hotplug = vec![(Duration::from_millis(200), two)];
        }));
        let mut rx = e.device_events().expect("events");
        let d = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("hot-plug within 10 s")
            .expect("devices");
        assert_eq!(d.cameras.len(), 2);
        assert_eq!(d.cameras[1].facing, Facing::External);
    }

    #[tokio::test]
    async fn screen_sources_carry_base64_png_thumbnails() {
        let mut window = kessel_video::ScreenSourceInfo::new(
            "w1",
            kessel_video::ScreenKind::Window,
            "Notes",
            800,
            600,
        );
        window.app = Some("Notes App".into());
        // A 640x480 grey RGBA preview: scaled to 320x240.
        window.thumbnail = kessel_video::Planes::tight(
            kessel_video::PixelFormat::Rgba,
            640,
            480,
            vec![128u8; 640 * 480 * 4],
        )
        .ok();
        (window.thumbnail_width, window.thumbnail_height) = (640, 480);
        let display = kessel_video::ScreenSourceInfo::new(
            "d0",
            kessel_video::ScreenKind::Display,
            "Display",
            1920,
            1080,
        );
        let e = engine(fake(|c| c.screens = vec![window, display]));
        let list = e.screen_sources().await.expect("listed");
        let mut v = serde_json::to_value(&list).expect("json");
        let png = v[0]
            .as_object_mut()
            .and_then(|o| o.remove("thumbnail_png"))
            .and_then(|t| t.as_str().map(str::to_owned))
            .expect("a thumbnail");
        assert_eq!(
            v,
            json!([
                {"id": "w1", "kind": "window", "title": "Notes", "app": "Notes App", "width": 800, "height": 600},
                {"id": "d0", "kind": "display", "title": "Display", "width": 1920, "height": 1080},
            ]),
            "no app and no thumbnail are left out"
        );
        let png = base64::engine::general_purpose::STANDARD
            .decode(png)
            .expect("standard base64");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let wh = (
            u32::from_be_bytes([png[16], png[17], png[18], png[19]]),
            u32::from_be_bytes([png[20], png[21], png[22], png[23]]),
        );
        assert_eq!(wh, (320, 240), "at most 320 px wide");

        let mut picker_only = engine(FakeConfig::default());
        picker_only.set_backend(kessel_video::null());
        assert_eq!(
            picker_only
                .screen_sources()
                .await
                .map_err(|e| e.to_info().code),
            Err("screen_unsupported".to_owned())
        );
        picker_only = EngineCapture::new();
        assert_eq!(
            picker_only
                .screen_sources()
                .await
                .map_err(|e| e.to_info().code),
            Err("video_unsupported".to_owned())
        );
    }

    #[test]
    fn the_picker_gets_the_app_window() {
        assert_eq!(
            screen_pick(None, Some(ParentWindow(0x0001_0a2c))),
            ScreenPick::System {
                parent_window: Some(0x0001_0a2c)
            }
        );
        assert_eq!(
            screen_pick(None, None),
            ScreenPick::System {
                parent_window: None
            }
        );
        assert_eq!(
            screen_pick(Some("fake:window:1"), Some(ParentWindow(7))),
            ScreenPick::Listed("fake:window:1".into())
        );
    }

    #[tokio::test]
    async fn the_screen_source_side_refuses_with_the_plugins_codes() {
        let none = EngineCapture::new();
        let req = ScreenRequest {
            on: true,
            source: None,
            parent_window: None,
        };
        let code = |r: Result<sew_call_client::ScreenSource, EngineError>| {
            r.map(|_| ()).map_err(|e| e.to_info().code)
        };
        assert_eq!(
            code(none.open_screen(&req, None).await),
            Err("screen_unsupported".into())
        );
        let null = EngineCapture::new();
        null.set_backend(kessel_video::null());
        assert_eq!(
            code(null.open_screen(&req, None).await),
            Err("screen_unsupported".into())
        );
        let cancel = engine(fake(|c| {
            c.pick = kessel_video::fake::FakePick::Cancel;
        }));
        assert_eq!(
            code(cancel.open_screen(&req, None).await),
            Err("picker_cancelled".into())
        );
        let denied = engine(fake(|c| {
            c.screen_permission = FakePermission::Denied;
        }));
        assert_eq!(
            code(denied.open_screen(&req, None).await),
            Err("screen_denied".into())
        );
        let ok = engine(FakeConfig::default());
        let gone = ScreenRequest {
            source: Some("fake:window:gone".into()),
            ..req.clone()
        };
        assert_eq!(
            code(ok.open_screen(&gone, None).await),
            Err("screen_not_found".into())
        );
        let s = ok
            .open_screen(&req, None)
            .await
            .expect("the picker's display");
        assert_eq!(s.content_hint(), kessel_video::ContentHint::Screen);
        assert_eq!(s.constraints().max_width, Some(1920));
        assert_eq!(s.constraints().fps, Some(15));
        drop(s);
        // The room's screen hint lowers it, never raises it.
        let s = ok
            .open_screen(&req, Some((1280, 720, 30)))
            .await
            .expect("the picker's display");
        assert_eq!(s.constraints().max_width, Some(1280));
        assert_eq!(s.constraints().max_height, Some(720));
        assert_eq!(s.constraints().fps, Some(15));
    }

    #[test]
    fn source_events_become_local_video() {
        let on = LocalVideo {
            camera: LocalVideoState::On,
            screen: LocalVideoState::On,
            reason: None,
        };
        let cases: [(CaptureKind, CaptureEvent, serde_json::Value); 9] = [
            (
                CaptureKind::Screen,
                CaptureEvent::Paused(CapturePause::Hidden),
                json!({"camera": "on", "screen": "paused", "reason": "hidden"}),
            ),
            (
                CaptureKind::Screen,
                CaptureEvent::Ended(CaptureEnd::UserStopped),
                json!({"camera": "on", "screen": "off", "reason": "user_stopped"}),
            ),
            (
                CaptureKind::Screen,
                CaptureEvent::Ended(CaptureEnd::SourceClosed),
                json!({"camera": "on", "screen": "off", "reason": "source_closed"}),
            ),
            (
                CaptureKind::Screen,
                CaptureEvent::Ended(CaptureEnd::Revoked),
                json!({"camera": "on", "screen": "off", "reason": "screen_denied"}),
            ),
            (
                CaptureKind::Screen,
                CaptureEvent::Ended(CaptureEnd::DeviceLost),
                json!({"camera": "on", "screen": "off", "reason": "source_closed"}),
            ),
            (
                CaptureKind::Camera,
                CaptureEvent::Ended(CaptureEnd::DeviceLost),
                json!({"camera": "off", "screen": "on", "reason": "camera_lost"}),
            ),
            (
                CaptureKind::Camera,
                CaptureEvent::Ended(CaptureEnd::Revoked),
                json!({"camera": "off", "screen": "on", "reason": "camera_denied"}),
            ),
            (
                CaptureKind::Camera,
                CaptureEvent::Paused(CapturePause::InUseByAnother),
                json!({"camera": "paused", "screen": "on", "reason": "camera_busy"}),
            ),
            (
                CaptureKind::Camera,
                CaptureEvent::Paused(CapturePause::Thermal),
                json!({"camera": "paused", "screen": "on", "reason": "thermal"}),
            ),
        ];
        for (kind, ev, want) in cases {
            let got = local_video_after(kind, &ev, &on).expect("a change");
            assert_eq!(
                serde_json::to_value(&got).ok(),
                Some(want),
                "{kind:?} {ev:?}"
            );
        }
        let paused = local_video_after(
            CaptureKind::Screen,
            &CaptureEvent::Paused(CapturePause::Hidden),
            &on,
        )
        .expect("paused");
        let back = local_video_after(CaptureKind::Screen, &CaptureEvent::Resumed, &paused)
            .expect("resumed");
        assert_eq!(back, on, "back on, no reason");
        assert_eq!(
            local_video_after(CaptureKind::Screen, &CaptureEvent::Resumed, &on),
            None,
            "nothing changed"
        );
        assert_eq!(
            local_video_after(
                CaptureKind::Camera,
                &CaptureEvent::Started {
                    width: 640,
                    height: 360,
                    fps: 30
                },
                &on
            ),
            None
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_preview_sends_local_camera_frames_and_closes() {
        let e = engine(FakeConfig::default());
        let mut rx = e
            .preview(PreviewRequest {
                on: true,
                device: None,
            })
            .await
            .expect("preview")
            .expect("frames");
        // The camera's facing first (the fake's camera faces front), then its frames.
        let facing = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("an item within 10 s");
        assert_eq!(
            facing,
            Some(VideoRecvItem::Facing {
                track: LOCAL_CAMERA.into(),
                facing: Facing::Front,
            })
        );
        let first = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("a frame within 10 s");
        let Some(VideoRecvItem::Frame(f)) = first else {
            panic!("a frame, not {first:?}");
        };
        assert_eq!(f.track, LOCAL_CAMERA);
        assert_eq!(f.codec, VideoCodec::Vp8);
        assert!(f.keyframe);
        assert_eq!(f.height, 360);
        e.preview_keyframe();
        let key = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match rx.recv().await {
                    Some(VideoRecvItem::Frame(f)) if f.keyframe => return f,
                    Some(VideoRecvItem::Frame(_)) => {}
                    other => panic!("frames only, not {other:?}"),
                }
            }
        })
        .await
        .expect("a keyframe on request");
        assert_eq!(key.track, LOCAL_CAMERA);
        let off = e
            .preview(PreviewRequest {
                on: false,
                device: None,
            })
            .await
            .expect("off");
        assert!(off.is_none());
        assert!(lock(&e.shared.preview).is_none());
        // The camera closed: the preview's channel, then the plugin's, end.
        tokio::time::timeout(Duration::from_secs(10), async {
            while rx.recv().await.is_some() {}
        })
        .await
        .expect("the frames end");

        let denied = engine(fake(|c| {
            c.camera_permission = FakePermission::Denied;
        }));
        let err = denied
            .preview(PreviewRequest {
                on: true,
                device: None,
            })
            .await
            .expect_err("denied");
        assert_eq!(err.to_info().code, "camera_denied");
        let none = EngineCapture::new();
        let err = none
            .preview(PreviewRequest {
                on: true,
                device: Some(DeviceChoice::Default),
            })
            .await
            .expect_err("no backend");
        assert_eq!(err.to_info().code, "video_unsupported");
    }
}
