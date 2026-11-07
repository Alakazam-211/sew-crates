//! The capture side of video (feature `video-core`): cameras and screens through a kessel-video
//! backend, as handles the publish side and the pre-join preview take.
//!
//! - **[`VideoCapture`]** wraps the app's one `VideoBackend`: capabilities, cameras, the OS
//!   permissions (asked only when not yet determined), the screen list with PNG thumbnails, the
//!   OS screen picker, and opening a camera or a screen.
//! - **[`CameraSource`] and [`ScreenSource`]** are running sources: frames ([`Capture::try_frame`]),
//!   events in Sew's vocabulary ([`CaptureEvent`]), the constraints they run at, the content hint
//!   for their encoders ([`ContentHint::Camera`] / [`ContentHint::Screen`]) and one wake-up
//!   ([`Capture::ready`], [`Capture::wake`] for the encoders fed from them). Dropping one stops
//!   the device (the camera light goes off).
//! - Every refusal is a [`CaptureError`] whose [`CaptureError::code`] is the plugin's reject code
//!   (`camera_denied`, `picker_cancelled`, `screen_unsupported`, ...).

use std::sync::{Arc, Mutex, Weak};

use kessel_video::{
    CameraInfo, Capabilities, CaptureConstraints, ContentHint, EndReason, EventRx, FrameSource,
    Interruption, Permission, PickOptions, PixelFormat, ScreenKind, ScreenSelection,
    ScreenSourceInfo, SourceEvent, SourceStats, VideoBackend, VideoError, Wake,
};
use tokio::sync::{Notify, oneshot};

/// The widest screen-source thumbnail (`screen_sources`), in pixels.
pub const THUMBNAIL_MAX_WIDTH: u32 = 320;

/// Why a camera or screen could not be opened. [`CaptureError::code`] is the plugin's code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CaptureError {
    /// No video here (the null backend, a backend shut down, no camera API).
    #[error("video_unsupported")]
    VideoUnsupported,
    /// The OS camera permission is denied or restricted.
    #[error("camera_denied")]
    CameraDenied,
    /// Another app holds the camera.
    #[error("camera_busy")]
    CameraBusy,
    /// No such camera (or none at all).
    #[error("camera_not_found")]
    CameraNotFound,
    /// The camera went away while opening.
    #[error("camera_lost")]
    CameraLost,
    /// No encoder for the codecs wanted.
    #[error("codec_unavailable")]
    CodecUnavailable,
    /// No screen capture here (Linux without the portal or libpipewire, phones), or no OS
    /// picker where one was asked for.
    #[error("screen_unsupported")]
    ScreenUnsupported,
    /// The OS screen-recording permission is denied.
    #[error("screen_denied")]
    ScreenDenied,
    /// The person cancelled the OS picker.
    #[error("picker_cancelled")]
    PickerCancelled,
    /// The listed display or window is gone (or was never listed).
    #[error("screen_not_found")]
    ScreenNotFound,
    /// An encoder failed and could not be reopened (the preview).
    #[error("encoder_lost")]
    EncoderLost,
    /// Any other backend failure (`VideoError::code` says which).
    #[error("capture_failed: {0}")]
    Failed(VideoError),
}

impl CaptureError {
    /// The plugin's reject code.
    pub fn code(&self) -> &'static str {
        match self {
            CaptureError::VideoUnsupported => "video_unsupported",
            CaptureError::CameraDenied => "camera_denied",
            CaptureError::CameraBusy => "camera_busy",
            CaptureError::CameraNotFound => "camera_not_found",
            CaptureError::CameraLost => "camera_lost",
            CaptureError::CodecUnavailable => "codec_unavailable",
            CaptureError::ScreenUnsupported => "screen_unsupported",
            CaptureError::ScreenDenied => "screen_denied",
            CaptureError::PickerCancelled => "picker_cancelled",
            CaptureError::ScreenNotFound => "screen_not_found",
            CaptureError::EncoderLost => "encoder_lost",
            CaptureError::Failed(_) => "capture_failed",
        }
    }
}

fn camera_error(e: VideoError) -> CaptureError {
    match e {
        VideoError::NoPermission => CaptureError::CameraDenied,
        VideoError::DeviceBusy => CaptureError::CameraBusy,
        VideoError::NotFound => CaptureError::CameraNotFound,
        VideoError::DeviceLost => CaptureError::CameraLost,
        VideoError::Unsupported(_) | VideoError::ShutDown => CaptureError::VideoUnsupported,
        other => CaptureError::Failed(other),
    }
}

fn screen_error(e: VideoError) -> CaptureError {
    match e {
        VideoError::NoPermission => CaptureError::ScreenDenied,
        VideoError::NotFound | VideoError::DeviceLost => CaptureError::ScreenNotFound,
        VideoError::Unsupported(_) | VideoError::ShutDown => CaptureError::ScreenUnsupported,
        other => CaptureError::Failed(other),
    }
}

fn pick_error(e: VideoError) -> CaptureError {
    match e {
        // kessel-video's answer to a cancelled picker.
        VideoError::NotFound => CaptureError::PickerCancelled,
        other => screen_error(other),
    }
}

// ───────────────────────────── events ─────────────────────────────

/// A camera or a screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CaptureKind {
    /// A camera.
    Camera,
    /// A display or window.
    Screen,
}

/// Why a source paused without ending (frames stop; the publication stays).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CapturePause {
    /// A shared window is minimised or occluded.
    Hidden,
    /// Phones: the app went to the background.
    Background,
    /// Another app took the camera.
    InUseByAnother,
    /// Thermal pressure: the OS stopped the camera.
    Thermal,
    /// A reason kessel-video added later.
    Other,
}

/// Why a source ended (terminal: drop it, unpublish, release the lease).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CaptureEnd {
    /// It was closed (the backend shut down).
    Closed,
    /// The device went away (unplugged, display disconnected).
    DeviceLost,
    /// The person stopped it from the OS's UI (the share bar, the portal).
    UserStopped,
    /// The shared window closed.
    SourceClosed,
    /// The OS permission was revoked while running.
    Revoked,
}

/// A source's events, in order, in Sew's vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CaptureEvent {
    /// The first frame, and after any change of the delivered size or rate.
    Started {
        /// Delivered width.
        width: u32,
        /// Delivered height.
        height: u32,
        /// Delivered rate.
        fps: u32,
    },
    /// Paused by the OS: no frames until [`CaptureEvent::Resumed`].
    Paused(CapturePause),
    /// Back after a pause (the encoders' next frame should be a keyframe).
    Resumed,
    /// Terminal: no more frames.
    Ended(CaptureEnd),
    /// A non-terminal problem.
    Error(VideoError),
}

impl CaptureEnd {
    /// The `local_video.reason` for a `kind` source that ended this way: `user_stopped`,
    /// `source_closed` (a screen's display or window gone), `camera_lost`, `camera_denied` /
    /// `screen_denied` (the permission revoked while sending), `unpublished` (closed).
    pub fn reason(self, kind: CaptureKind) -> &'static str {
        let screen = kind == CaptureKind::Screen;
        match self {
            CaptureEnd::UserStopped => "user_stopped",
            CaptureEnd::SourceClosed => "source_closed",
            CaptureEnd::Revoked if screen => "screen_denied",
            CaptureEnd::Revoked => "camera_denied",
            CaptureEnd::DeviceLost if screen => "source_closed",
            CaptureEnd::DeviceLost => "camera_lost",
            CaptureEnd::Closed => "unpublished",
        }
    }
}

pub(crate) fn map_event(e: SourceEvent) -> Option<CaptureEvent> {
    Some(match e {
        SourceEvent::Started { width, height, fps } => CaptureEvent::Started { width, height, fps },
        SourceEvent::Interrupted(i) => CaptureEvent::Paused(match i {
            Interruption::Hidden => CapturePause::Hidden,
            Interruption::Background => CapturePause::Background,
            Interruption::InUseByAnother => CapturePause::InUseByAnother,
            Interruption::Thermal => CapturePause::Thermal,
            _ => CapturePause::Other,
        }),
        SourceEvent::Resumed => CaptureEvent::Resumed,
        SourceEvent::Ended(r) => CaptureEvent::Ended(match r {
            EndReason::Closed => CaptureEnd::Closed,
            EndReason::DeviceLost => CaptureEnd::DeviceLost,
            EndReason::UserStopped => CaptureEnd::UserStopped,
            EndReason::SourceClosed => CaptureEnd::SourceClosed,
            EndReason::Revoked => CaptureEnd::Revoked,
            _ => CaptureEnd::DeviceLost,
        }),
        SourceEvent::Error(e) => CaptureEvent::Error(e),
        _ => return None,
    })
}

/// A [`Wake`] that wakes `n` (from any thread; cheap, never blocks).
fn notify_wake(n: &Arc<Notify>) -> Wake {
    let n = Arc::clone(n);
    Arc::new(move || n.notify_one())
}

/// A source's wake: its [`Notify`] and, once the publish side takes the source, the bridge's
/// pipeline wake as well ([`Capture::chain_wake`]).
#[derive(Default)]
struct WakeHub {
    ready: Arc<Notify>,
    chained: Mutex<Option<Wake>>,
}

impl WakeHub {
    fn wake(self: &Arc<WakeHub>) -> Wake {
        let hub = Arc::clone(self);
        Arc::new(move || {
            hub.ready.notify_one();
            let chained = hub
                .chained
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some(w) = chained {
                w();
            }
        })
    }
}

// ───────────────────────────── sources ─────────────────────────────

/// A running camera or screen source. Dropping it stops the device.
pub struct Capture {
    kind: CaptureKind,
    source: Box<dyn FrameSource>,
    hub: Arc<WakeHub>,
    constraints: CaptureConstraints,
    device: Option<String>,
    ended: Option<CaptureEnd>,
    paused: Option<CapturePause>,
}

impl std::fmt::Debug for Capture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Capture")
            .field("kind", &self.kind)
            .field("device", &self.device)
            .field("ended", &self.ended)
            .field("paused", &self.paused)
            .finish_non_exhaustive()
    }
}

impl Capture {
    fn new(
        kind: CaptureKind,
        source: Box<dyn FrameSource>,
        hub: Arc<WakeHub>,
        constraints: CaptureConstraints,
        device: Option<String>,
    ) -> Capture {
        Capture {
            kind,
            source,
            hub,
            constraints,
            device,
            ended: None,
            paused: None,
        }
    }

    /// Camera or screen.
    pub fn kind(&self) -> CaptureKind {
        self.kind
    }

    /// The content hint for the encoders this source feeds: [`ContentHint::Camera`] (keep the
    /// frame rate) or [`ContentHint::Screen`] (keep the detail).
    pub fn content_hint(&self) -> ContentHint {
        match self.kind {
            CaptureKind::Screen => ContentHint::Screen,
            _ => ContentHint::Camera,
        }
    }

    /// The camera's id as asked (`None`: the default camera), or the listed screen's id
    /// (`None`: the OS picker's choice).
    pub fn device(&self) -> Option<&str> {
        self.device.as_deref()
    }

    /// What the source was last asked for.
    pub fn constraints(&self) -> CaptureConstraints {
        self.constraints
    }

    /// New size and rate without reopening (from the next frame; a
    /// [`CaptureEvent::Started`] follows with the delivered values).
    pub fn set_constraints(&mut self, c: CaptureConstraints) -> Result<(), CaptureError> {
        let r = self.source.set_constraints(c);
        r.map_err(|e| match self.kind {
            CaptureKind::Camera => camera_error(e),
            _ => screen_error(e),
        })?;
        self.constraints = c;
        Ok(())
    }

    /// Stop delivering frames but keep the device open (nothing wants a layer and no
    /// self-view is open).
    pub fn set_paused(&mut self, paused: bool) {
        self.source.set_paused(paused);
    }

    /// The next frame (CPU planes, or a native buffer when asked with `prefer_native`), if one
    /// is queued. The queue is 2 deep, oldest dropped; `None` once ended.
    pub fn try_frame(&mut self) -> Option<kessel_video::VideoFrame> {
        if self.ended.is_some() {
            return None;
        }
        self.source.try_recv()
    }

    /// The next event, if one is queued.
    pub fn try_event(&mut self) -> Option<CaptureEvent> {
        loop {
            let e = map_event(self.source.try_event()?);
            match &e {
                Some(CaptureEvent::Ended(end)) => self.ended = Some(*end),
                Some(CaptureEvent::Paused(p)) => self.paused = Some(*p),
                Some(CaptureEvent::Resumed) => self.paused = None,
                _ => {}
            }
            if e.is_some() {
                return e;
            }
        }
    }

    /// Waits until the source (or an encoder given [`Capture::wake`]) queued something. Drain
    /// [`Capture::try_event`] and [`Capture::try_frame`] after each wake; one wake-up is kept
    /// when nobody waits, so none is lost between a drain and the next wait.
    pub async fn ready(&self) {
        self.hub.ready.notified().await;
    }

    /// The source's wake-up as a kessel-video [`Wake`], for the encoders this source feeds, so
    /// one task waits on frames and encoded outputs alike.
    pub fn wake(&self) -> Wake {
        notify_wake(&self.hub.ready)
    }

    /// The source's wake-up itself.
    pub fn notifier(&self) -> Arc<Notify> {
        Arc::clone(&self.hub.ready)
    }

    /// From now on the source's frames and events also call `wake` (the bridge's pipeline
    /// thread, which waits on its own wake-up rather than on [`Capture::ready`]).
    pub(crate) fn chain_wake(&self, wake: Wake) {
        *self
            .hub
            .chained
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(wake);
        // Anything already queued is seen at once.
        self.hub.wake()();
    }

    /// Why it ended, once it has ([`CaptureEvent::Ended`] was read).
    pub fn ended(&self) -> Option<CaptureEnd> {
        self.ended
    }

    /// Why it is paused, while it is.
    pub fn paused(&self) -> Option<CapturePause> {
        self.paused
    }

    /// The source's counters since open.
    pub fn stats(&self) -> SourceStats {
        self.source.stats()
    }
}

/// The raw source, for the publish side ([`crate::VideoBridge::publish_capture`] and
/// [`crate::SourceFeed`]): frames and kessel-video's own events, without [`Capture`]'s
/// bookkeeping.
impl FrameSource for Capture {
    fn try_recv(&mut self) -> Option<kessel_video::VideoFrame> {
        self.source.try_recv()
    }

    fn try_event(&mut self) -> Option<SourceEvent> {
        self.source.try_event()
    }

    fn set_constraints(&mut self, c: CaptureConstraints) -> Result<(), VideoError> {
        self.source.set_constraints(c)?;
        self.constraints = c;
        Ok(())
    }

    fn set_paused(&mut self, paused: bool) {
        self.source.set_paused(paused);
    }

    fn stats(&self) -> SourceStats {
        self.source.stats()
    }
}

/// A running camera ([`VideoCapture::open_camera`]). Everything of [`Capture`] through `Deref`.
#[derive(Debug)]
pub struct CameraSource(Capture);

/// A running screen or window capture ([`VideoCapture::open_screen`]), with
/// [`ContentHint::Screen`]. Everything of [`Capture`] through `Deref`.
#[derive(Debug)]
pub struct ScreenSource(Capture);

macro_rules! capture_handle {
    ($t:ty) => {
        impl std::ops::Deref for $t {
            type Target = Capture;
            fn deref(&self) -> &Capture {
                &self.0
            }
        }

        impl std::ops::DerefMut for $t {
            fn deref_mut(&mut self) -> &mut Capture {
                &mut self.0
            }
        }

        impl $t {
            /// The source as a plain [`Capture`].
            pub fn into_capture(self) -> Capture {
                self.0
            }
        }
    };
}

capture_handle!(CameraSource);
capture_handle!(ScreenSource);

/// What to share ([`VideoCapture::open_screen`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScreenPick {
    /// A display or window id from [`VideoCapture::screen_sources`].
    Listed(String),
    /// The OS picker. `parent_window` is the app window it opens over (Windows needs it: the
    /// `HWND` as an integer; ignored elsewhere).
    System {
        /// The owning window's native handle.
        parent_window: Option<u64>,
    },
}

/// One display or window to share, with its thumbnail as a PNG.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ScreenChoice {
    /// The id [`ScreenPick::Listed`] takes.
    pub id: String,
    /// Display or window.
    pub kind: ScreenKind,
    /// The display's name or the window's title.
    pub title: String,
    /// The window's app, when known.
    pub app: Option<String>,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// A PNG at most [`THUMBNAIL_MAX_WIDTH`] wide (8-bit RGBA), when the backend gave a preview.
    pub thumbnail_png: Option<Vec<u8>>,
}

/// Screen constraints: at most `max_width` x `max_height` at `fps`, the cursor drawn, and this
/// app's own windows left out of display capture where the OS allows it
/// ([`VideoCapture::open_screen`] clears that where it doesn't).
pub fn screen_constraints(max_width: u32, max_height: u32, fps: u32) -> CaptureConstraints {
    let mut c = CaptureConstraints::new(max_width, max_height, fps);
    c.cursor = true;
    c.exclude_own_windows = true;
    c
}

// ───────────────────────────── the backend ─────────────────────────────

/// The app's video capture over one kessel-video backend. Cheap to clone; clones share the
/// camera: one opener holds it at a time (a new preview, or a camera opened for the publish,
/// stops the running preview first).
#[derive(Clone)]
pub struct VideoCapture {
    backend: Arc<dyn VideoBackend>,
    /// The running preview, if any (it holds the camera).
    preview: Arc<Mutex<Option<Weak<super::preview::Shared>>>>,
}

impl std::fmt::Debug for VideoCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoCapture")
            .field("backend", &self.backend.capabilities().backend)
            .finish()
    }
}

/// Runs `f` on the blocking pool (opening a device may block on the OS).
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, VideoError> + Send + 'static,
) -> Result<T, VideoError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| VideoError::os(0, format!("capture task: {e}")))?
}

impl VideoCapture {
    /// Capture through `backend` (`kessel_video::system`, `null()`, a fake or a fixture).
    pub fn new(backend: Arc<dyn VideoBackend>) -> VideoCapture {
        VideoCapture {
            backend,
            preview: Arc::new(Mutex::new(None)),
        }
    }

    /// Stops the running preview (its camera closes now, its channel ends).
    pub fn stop_preview(&self) {
        let running = self
            .preview
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .and_then(|w| w.upgrade());
        if let Some(p) = running {
            p.stop();
        }
    }

    /// A preview is running (it holds the camera).
    pub fn preview_running(&self) -> bool {
        self.preview
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(Weak::upgrade)
            .is_some_and(|p| p.running())
    }

    /// The preview that now holds the camera.
    pub(crate) fn set_preview(&self, p: &Arc<super::preview::Shared>) {
        *self
            .preview
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::downgrade(p));
    }

    /// No video: kessel-video's null backend.
    pub fn null() -> VideoCapture {
        VideoCapture::new(kessel_video::null())
    }

    /// The backend (for encoders and decoders).
    pub fn backend(&self) -> &Arc<dyn VideoBackend> {
        &self.backend
    }

    /// What this machine can do now.
    pub fn capabilities(&self) -> Capabilities {
        self.backend.capabilities()
    }

    /// Which way the camera `device` faces (`None`: the default one, as `open_camera` picks
    /// it); `Unknown` when it isn't listed.
    pub fn camera_facing(&self, device: Option<&str>) -> kessel_video::Facing {
        let cams = self.cameras();
        match device {
            Some(id) => cams.iter().find(|c| c.id == id),
            None => cams.iter().find(|c| c.is_default).or(cams.first()),
        }
        .map_or(kessel_video::Facing::Unknown, |c| c.facing)
    }

    /// The cameras now (opens none, never prompts).
    pub fn cameras(&self) -> Vec<CameraInfo> {
        self.backend.cameras()
    }

    /// The camera permission (never prompts).
    pub fn camera_permission(&self) -> Permission {
        self.backend.camera_permission()
    }

    /// The screen-recording permission (never prompts).
    pub fn screen_permission(&self) -> Permission {
        self.backend.screen_permission()
    }

    /// Machine-wide events: camera hot-plug, capability changes, thermal, background.
    pub fn events(&self) -> EventRx {
        self.backend.subscribe()
    }

    /// Asks the camera permission: the OS prompt when not yet determined, else the state now.
    pub async fn request_camera_permission(&self) -> Permission {
        let (tx, rx) = oneshot::channel();
        self.backend.request_camera_permission(Box::new(move |p| {
            let _ = tx.send(p);
        }));
        rx.await.unwrap_or(Permission::Denied)
    }

    /// Asks the screen-recording permission (macOS); `Granted` where there is nothing to ask.
    pub async fn request_screen_permission(&self) -> Permission {
        let (tx, rx) = oneshot::channel();
        self.backend.request_screen_permission(Box::new(move |p| {
            let _ = tx.send(p);
        }));
        rx.await.unwrap_or(Permission::Denied)
    }

    pub(crate) async fn camera_allowed(&self) -> Result<(), CaptureError> {
        let p = match self.camera_permission() {
            Permission::NotDetermined => self.request_camera_permission().await,
            p => p,
        };
        match p {
            Permission::Granted => Ok(()),
            _ => Err(CaptureError::CameraDenied),
        }
    }

    async fn screen_allowed(&self) -> Result<(), CaptureError> {
        let p = match self.screen_permission() {
            Permission::NotDetermined => self.request_screen_permission().await,
            p => p,
        };
        match p {
            Permission::Granted => Ok(()),
            _ => Err(CaptureError::ScreenDenied),
        }
    }

    /// Opens a camera (`None`: the default) at `constraints`: the permission first (the OS
    /// prompt only when not yet determined), then the device, taken over from a running
    /// preview (stopped first: one opener holds the camera). Returns once it is open; frames
    /// follow on [`Capture::ready`].
    pub async fn open_camera(
        &self,
        device: Option<&str>,
        constraints: CaptureConstraints,
    ) -> Result<CameraSource, CaptureError> {
        if !self.capabilities().camera {
            return Err(CaptureError::VideoUnsupported);
        }
        self.camera_allowed().await?;
        self.stop_preview();
        let hub = Arc::new(WakeHub::default());
        let wake = hub.wake();
        let backend = Arc::clone(&self.backend);
        let id = device.map(str::to_owned);
        let asked = id.clone();
        let source = blocking(move || backend.open_camera(asked.as_deref(), constraints, wake))
            .await
            .map_err(camera_error)?;
        Ok(CameraSource(Capture::new(
            CaptureKind::Camera,
            source,
            hub,
            constraints,
            id,
        )))
    }

    /// The displays and windows to share, each with a PNG thumbnail when the backend has one.
    /// `[]` where only the OS picker exists; `screen_unsupported` with no screen capture.
    pub async fn screen_sources(&self) -> Result<Vec<ScreenChoice>, CaptureError> {
        let screen = self.capabilities().screen;
        if !screen.supported {
            return Err(CaptureError::ScreenUnsupported);
        }
        if !screen.list {
            return Ok(Vec::new());
        }
        let backend = Arc::clone(&self.backend);
        let list = blocking(move || {
            let list = backend.screen_sources()?;
            Ok(list.iter().map(choice).collect::<Vec<_>>())
        })
        .await
        .map_err(screen_error)?;
        Ok(list)
    }

    /// Shows the OS picker and answers the choice (`picker_cancelled` when cancelled).
    async fn pick(&self, opts: PickOptions) -> Result<ScreenSelection, CaptureError> {
        let (tx, rx) = oneshot::channel();
        self.backend.pick_screen(
            opts,
            Box::new(move |r| {
                let _ = tx.send(r);
            }),
        );
        match rx.await {
            Ok(r) => r.map_err(pick_error),
            Err(_) => Err(CaptureError::ScreenUnsupported),
        }
    }

    /// Opens a display or window: screen capture must exist here (`screen_unsupported`: Linux
    /// without the xdg portal or libpipewire), the screen-recording permission (macOS), then the
    /// listed source or the OS picker's choice ([`ScreenPick::System`] passes the parent window
    /// on, which Windows needs), then capture at `constraints` (see [`screen_constraints`]).
    /// The source's encoders take [`ContentHint::Screen`].
    pub async fn open_screen(
        &self,
        pick: ScreenPick,
        constraints: CaptureConstraints,
    ) -> Result<ScreenSource, CaptureError> {
        let screen = self.capabilities().screen;
        if !screen.supported {
            return Err(CaptureError::ScreenUnsupported);
        }
        self.screen_allowed().await?;
        let (selection, device) = match pick {
            ScreenPick::Listed(id) => {
                if !screen.list {
                    return Err(CaptureError::ScreenNotFound);
                }
                (ScreenSelection::Listed(id.clone()), Some(id))
            }
            ScreenPick::System { parent_window } => {
                if !screen.system_picker {
                    return Err(CaptureError::ScreenUnsupported);
                }
                let mut opts = PickOptions::new();
                opts.displays = true;
                opts.windows = screen.windows;
                if let Some(h) = parent_window {
                    opts = opts.with_parent_window(h);
                }
                (self.pick(opts).await?, None)
            }
        };
        let mut c = constraints;
        c.exclude_own_windows &= screen.exclude_own_windows;
        let hub = Arc::new(WakeHub::default());
        let wake = hub.wake();
        let backend = Arc::clone(&self.backend);
        let source = blocking(move || backend.open_screen(&selection, c, wake))
            .await
            .map_err(screen_error)?;
        Ok(ScreenSource(Capture::new(
            CaptureKind::Screen,
            source,
            hub,
            c,
            device,
        )))
    }
}

// ───────────────────────────── thumbnails ─────────────────────────────

fn choice(s: &ScreenSourceInfo) -> ScreenChoice {
    ScreenChoice {
        id: s.id.clone(),
        kind: s.kind,
        title: s.title.clone(),
        app: s.app.clone(),
        width: s.width,
        height: s.height,
        thumbnail_png: thumbnail_png(s),
    }
}

/// The thumbnail's size: kessel-video's `thumbnail_width` and `thumbnail_height`
/// ([`ScreenSourceInfo::thumbnail_size`]), checked against its planes. `None` when the backend
/// gave no size, or one the planes don't hold.
fn thumbnail_size(s: &ScreenSourceInfo, p: &kessel_video::Planes) -> Option<(u32, u32)> {
    let (w, h) = s.thumbnail_size()?;
    p.validate(w, h).ok()?;
    Some((w, h))
}

/// A source's thumbnail as an 8-bit RGBA PNG at most [`THUMBNAIL_MAX_WIDTH`] wide (scaled down
/// keeping the aspect ratio, each pixel the average of its box). `None` without a thumbnail,
/// without its size, or with one that isn't RGBA or BGRA.
pub fn thumbnail_png(s: &ScreenSourceInfo) -> Option<Vec<u8>> {
    let p = s.thumbnail.as_ref()?;
    let bgra = match p.format {
        PixelFormat::Rgba => false,
        PixelFormat::Bgra => true,
        _ => return None,
    };
    let (w, h) = thumbnail_size(s, p)?;
    let (tw, th) = if w > THUMBNAIL_MAX_WIDTH {
        kessel_video::fit_within(w, h, THUMBNAIL_MAX_WIDTH, h)
    } else {
        (w, h)
    };
    let rgba = shrink_rgba(p, (w, h), (tw, th), bgra);
    encode_png(tw, th, &rgba)
}

/// `p`'s 8-bit 4-channel pixels (`from`) as tight RGBA at `to` (at most `from`): each output
/// pixel averages its box of input pixels. Straight on the RGB values: no YUV round trip, so a
/// thumbnail keeps its colours and alpha.
fn shrink_rgba(p: &kessel_video::Planes, from: (u32, u32), to: (u32, u32), bgra: bool) -> Vec<u8> {
    let (w, h) = (from.0 as usize, from.1 as usize);
    let (tw, th) = (to.0 as usize, to.1 as usize);
    let (stride, off) = (p.strides[0] as usize, p.offsets[0]);
    let mut out = Vec::with_capacity(tw * th * 4);
    for ty in 0..th {
        let (y0, y1) = (ty * h / th, ((ty + 1) * h / th).max(ty * h / th + 1).min(h));
        for tx in 0..tw {
            let (x0, x1) = (tx * w / tw, ((tx + 1) * w / tw).max(tx * w / tw + 1).min(w));
            let mut sum = [0u64; 4];
            for y in y0..y1 {
                let row = &p.data[off + y * stride..];
                for x in x0..x1 {
                    for (c, v) in sum.iter_mut().enumerate() {
                        *v += u64::from(row[x * 4 + c]);
                    }
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u64;
            let avg = sum.map(|v| u8::try_from((v + n / 2) / n).unwrap_or(u8::MAX));
            let px = if bgra {
                [avg[2], avg[1], avg[0], avg[3]]
            } else {
                avg
            };
            out.extend_from_slice(&px);
        }
    }
    out
}

/// An 8-bit RGBA PNG of `rgba` (tightly packed rows).
fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, width, height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header().ok()?;
    w.write_image_data(rgba).ok()?;
    w.finish().ok()?;
    Some(out)
}

#[cfg(test)]
mod tests;
