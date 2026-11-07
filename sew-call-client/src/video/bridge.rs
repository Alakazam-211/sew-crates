//! `VideoBridge` (feature `video-core`): a kessel-video backend's cameras, screens and encoders
//! feeding a call's video publications, as `AudioBridge` connects kessel-audio.
//!
//! [`VideoBridge::set_camera`] turns the camera on in the order Sew's side asks:
//! 1. the join's policy (`video_enabled`, `camera_allowed`), else `video_not_allowed`;
//! 2. the OS permission (prompting once), else `camera_denied`;
//! 3. the camera lease ([`crate::Call::set_video_lease`]);
//! 4. the camera, at the policy's camera hints (`camera_busy`, `camera_not_found`), opened
//!    through the bridge's [`VideoCapture`] (the same device rules as the pre-join preview,
//!    which it takes the camera over from: one opener holds the camera);
//! 5. the codec ([`choose_codec`]: VP8 when the policy lists it and an encoder exists, else
//!    H.264) and one encoder per layer of [`crate::plan_layers`], opened from the top down: a
//!    `NoResources` (or the codec's `max_instances`) drops the top layer and tries again, down
//!    to one; nothing opens: `codec_unavailable`, the camera closed and the lease released;
//! 6. the publication ([`crate::Call::publish_video`]) with the layers that opened.
//!
//! A screen ([`VideoBridge::publish_capture`] with a [`crate::ScreenSource`], or
//! [`VideoBridge::publish_source`] with a source the host opened) takes the same steps under
//! the `self_stream` lease with the screen content hint.
//!
//! Then one thread per publication: each captured frame goes to every active layer's encoder
//! (a native buffer another encoder can't take is converted to CPU planes), each encoded frame
//! to the engine, and the engine's controls back: keyframes, rates and which layers run
//! (with none running and no self-view the source pauses), a republish's new layers, the end.
//! A lost encoder session is reopened with a keyframe. A device switch closes the old camera
//! first (a phone runs one camera at a time), then feeds the new camera's frames to the same
//! encoders (resized if the aspect changed), a keyframe first; a new layer set republishes. A
//! new camera that doesn't open brings the old one back; neither: `camera_lost`.
//!
//! The source's own events ([`VideoBridge::events`]): `Paused` (a hidden window, another app
//! took the camera) stops the frames but keeps the publication; `Resumed` asks every layer for
//! a keyframe; `Ended` unpublishes, releases the lease, and the engine's `LocalVideo` says why
//! (`user_stopped`, `source_closed`, `camera_lost`, `screen_denied`, …).
//!
//! **Self-view** ([`VideoBridge::self_view`]): the plaintext output of one layer per
//! publication (the camera's `h`, else its lowest layer at 360p or more; the screen's `q`,
//! else its lowest) goes to this device's own view as well. No second encoder; nothing of it
//! leaves the process.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use kessel_video::{
    Capabilities, CaptureConstraints, CodecCapability, CodecConfig, CodecDirection, ContentHint,
    EncodeOptions, EncoderConfig, Facing, FrameBuffer, FrameSource, PixelFormat, Rates,
    SourceEvent, VideoBackend, VideoEncoder, VideoError as KError, Wake,
};
use serde::Serialize;
use tokio::sync::{broadcast, mpsc};

use super::capture::{
    Capture, CaptureEnd, CaptureError, CaptureEvent, CaptureKind, VideoCapture, map_event,
};
use super::preview::LocalFrame;
use super::publish::{
    EncodedVideo, LayerSpec, VideoControl, VideoCtl, VideoError, VideoPublication, VideoPublish,
    VideoSender, plan_layers,
};
use super::types::{VideoCodec, VideoSource};
use crate::call::Call;

/// `local_video`'s reason for a camera switch that publishes again (a new layer set).
pub(crate) const CAMERA_SWITCH: &str = "camera_switch";
/// `local_video`'s reason when the publication's encoders all failed and none reopened.
pub(crate) const ENCODER_LOST: &str = "encoder_lost";

/// How long the bridge waits for a source's first `Started`.
const START_WAIT: Duration = Duration::from_secs(3);
/// Items the self-view channel holds (a full channel drops the frame and asks a keyframe).
const SELF_VIEW_QUEUE: usize = 8;
/// Source events kept for a slow reader of [`VideoBridge::events`].
const EVENT_QUEUE: usize = 32;

/// What to do with the camera ([`VideoBridge::set_camera`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct CameraRequest {
    /// On or off.
    pub on: bool,
    /// The camera (`None`: the default). With the camera on, another id switches to it.
    pub device: Option<String>,
    /// At most this many VP8 temporal layers (`None`: what the encoder offers, up to 3).
    pub temporal_layers: Option<u8>,
    /// With the camera off: `local_video`'s reason (`None`: `unpublished`), so a log tells who
    /// turned it off (the app's button, the system's call UI, …).
    pub reason: Option<String>,
}

impl CameraRequest {
    /// The default camera on.
    pub fn on() -> CameraRequest {
        CameraRequest {
            on: true,
            device: None,
            temporal_layers: None,
            reason: None,
        }
    }

    /// The camera off.
    pub fn off() -> CameraRequest {
        CameraRequest::default()
    }

    /// With camera `device`.
    pub fn with_device(mut self, device: &str) -> CameraRequest {
        self.device = Some(device.to_owned());
        self
    }

    /// With `reason` as `local_video`'s reason for an off.
    pub fn with_reason(mut self, reason: &str) -> CameraRequest {
        self.reason = Some(reason.to_owned());
        self
    }

    /// With at most `n` VP8 temporal layers.
    pub fn with_temporal_layers(mut self, n: u8) -> CameraRequest {
        self.temporal_layers = Some(n);
        self
    }
}

/// The camera's state after [`VideoBridge::set_camera`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "camera", rename_all = "snake_case")]
#[non_exhaustive]
pub enum CameraState {
    /// Off.
    Off,
    /// Published.
    On {
        /// The codec.
        codec: VideoCodec,
        /// The layers that opened, lowest first.
        layers: Vec<LayerSpec>,
        /// The camera's delivered size.
        width: u32,
        /// Its height.
        height: u32,
    },
}

/// A published source's event ([`VideoBridge::events`]), in capture's vocabulary.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SourceNote {
    /// The camera or the screen.
    pub source: VideoSource,
    /// What happened: `Paused` (frames stopped, the publication kept), `Resumed` (a keyframe
    /// was asked of every layer), `Ended` (unpublished, the lease released).
    pub event: CaptureEvent,
}

/// What the self-view channel carries ([`VideoBridge::self_view`]).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum SelfViewItem {
    /// One plaintext frame of the source's self-view layer (the first a keyframe).
    Frame {
        /// The camera or the screen.
        source: VideoSource,
        /// The frame.
        frame: LocalFrame,
    },
    /// The publication ended: the self-view of that source resets.
    Ended {
        /// The camera or the screen.
        source: VideoSource,
    },
}

/// This device's own camera and screen frames while they are published.
pub type SelfViewFrames = mpsc::Receiver<SelfViewItem>;

fn our_codec(c: kessel_video::VideoCodec) -> Option<VideoCodec> {
    match c {
        kessel_video::VideoCodec::Vp8 => Some(VideoCodec::Vp8),
        kessel_video::VideoCodec::H264 => Some(VideoCodec::H264),
        _ => None,
    }
}

fn their_codec(c: VideoCodec) -> kessel_video::VideoCodec {
    match c {
        VideoCodec::Vp8 => kessel_video::VideoCodec::Vp8,
        VideoCodec::H264 => kessel_video::VideoCodec::H264,
    }
}

/// The codec a native sender publishes in: VP8 when the policy lists it and the backend has a
/// VP8 encoder (kessel-video's pure-Rust one, or the OS's), else H.264 when the policy lists it
/// and an H.264 encoder exists; `None`: `codec_unavailable`.
pub fn choose_codec(
    policy: &[sew_proto::voice::VideoCodec],
    caps: &Capabilities,
) -> Option<(VideoCodec, CodecCapability)> {
    for want in [VideoCodec::Vp8, VideoCodec::H264] {
        let listed = policy
            .iter()
            .any(|c| super::publish::codec_of_policy(*c) == Some(want));
        if !listed {
            continue;
        }
        if let Some(cap) = caps.best(their_codec(want), CodecDirection::Encode) {
            return Some((want, cap.clone()));
        }
    }
    None
}

fn camera_error(e: &KError) -> VideoError {
    match e {
        KError::NoPermission => VideoError::CameraDenied,
        KError::DeviceBusy => VideoError::CameraBusy,
        KError::NotFound | KError::DeviceLost => VideoError::CameraNotFound,
        other => VideoError::PublishFailed(format!("camera: {}", other.code())),
    }
}

/// A capture refusal of the camera as the publish side's error (the same codes).
fn capture_error(e: &CaptureError) -> VideoError {
    match e {
        CaptureError::CameraDenied => VideoError::CameraDenied,
        CaptureError::CameraBusy => VideoError::CameraBusy,
        CaptureError::CameraNotFound | CaptureError::CameraLost => VideoError::CameraNotFound,
        CaptureError::CodecUnavailable => VideoError::CodecUnavailable,
        CaptureError::Failed(k) => camera_error(k),
        other => VideoError::PublishFailed(format!("camera: {}", other.code())),
    }
}

fn capture_kind(kind: VideoSource) -> CaptureKind {
    match kind {
        VideoSource::Camera => CaptureKind::Camera,
        VideoSource::ScreenShare => CaptureKind::Screen,
    }
}

/// The layer whose output is the self-view: the camera's `h` (else its lowest layer at 360p
/// or more, else its top), the screen's `q` (else its lowest). `layers` lowest first.
fn self_view_rid(kind: VideoSource, layers: &[LayerSpec]) -> Option<String> {
    let want = match kind {
        VideoSource::Camera => "h",
        VideoSource::ScreenShare => "q",
    };
    if let Some(l) = layers.iter().find(|l| l.rid == want) {
        return Some(l.rid.clone());
    }
    let pick = match kind {
        VideoSource::Camera => layers
            .iter()
            .find(|l| l.height >= 360)
            .or_else(|| layers.last()),
        VideoSource::ScreenShare => layers.first(),
    };
    pick.map(|l| l.rid.clone())
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The camera's capture box: the policy's, and on a phone at most 720p (1280 × 720 either way
/// round; the thermal and power caps, `Call::set_video_caps`, work below it).
fn camera_box(max_width: u32, max_height: u32, phone: bool) -> (u32, u32) {
    if !phone {
        return (max_width, max_height);
    }
    if max_width >= max_height {
        (max_width.min(1280), max_height.min(720))
    } else {
        (max_width.min(720), max_height.min(1280))
    }
}

/// A wake that sets a flag and notifies the pipeline thread.
#[derive(Default)]
struct Bell {
    rung: Mutex<bool>,
    cv: Condvar,
}

impl Bell {
    fn ring(&self) {
        *self.rung.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.cv.notify_one();
    }

    fn wait(&self, at_most: Duration) {
        let g = self.rung.lock().unwrap_or_else(PoisonError::into_inner);
        let (mut g, _) = self
            .cv
            .wait_timeout_while(g, at_most, |r| !*r)
            .unwrap_or_else(PoisonError::into_inner);
        *g = false;
    }

    fn wake(self: &Arc<Bell>) -> Wake {
        let b = self.clone();
        Arc::new(move || b.ring())
    }
}

fn content_of(kind: VideoSource) -> ContentHint {
    match kind {
        VideoSource::Camera => ContentHint::Camera,
        VideoSource::ScreenShare => ContentHint::Screen,
    }
}

fn encoder_config(codec: VideoCodec, l: &LayerSpec, content: ContentHint) -> EncoderConfig {
    let bps = l.max_kbps.saturating_mul(1000).max(10_000);
    let mut cfg = match codec {
        VideoCodec::Vp8 => EncoderConfig::new(CodecConfig::Vp8, l.width, l.height, l.fps, bps),
        VideoCodec::H264 => EncoderConfig::h264(l.width, l.height, l.fps, bps),
    };
    cfg.temporal_layers = if codec == VideoCodec::Vp8 {
        l.temporal_layers.clamp(1, 3)
    } else {
        1
    };
    cfg.content = content;
    cfg
}

/// One layer's encoder in the pipeline.
struct Enc {
    spec: LayerSpec,
    enc: Box<dyn VideoEncoder>,
    active: bool,
}

/// Opens one encoder per layer, from the top down: `NoResources` (or `max_instances`) drops
/// the top layer and tries again, down to one layer.
fn open_encoders(
    backend: &Arc<dyn VideoBackend>,
    codec: VideoCodec,
    cap: &CodecCapability,
    mut plan: Vec<LayerSpec>,
    content: ContentHint,
    wake: &Wake,
) -> Vec<Enc> {
    if let Some(m) = cap.max_instances {
        plan.truncate(usize::try_from(m.max(1)).unwrap_or(1));
    }
    while !plan.is_empty() {
        let mut opened = Vec::new();
        let mut failed = false;
        for l in plan.iter().rev() {
            match backend.encoder(encoder_config(codec, l, content), wake.clone()) {
                Ok(enc) => opened.push(Enc {
                    spec: l.clone(),
                    enc,
                    active: true,
                }),
                Err(e) => {
                    tracing::info!(target: "sew_call_client", rid = %l.rid, error = %e, "an encoder did not open; one layer fewer");
                    failed = true;
                    break;
                }
            }
        }
        if !failed {
            opened.reverse();
            return opened;
        }
        drop(opened);
        plan.pop();
    }
    Vec::new()
}

/// Waits (on the blocking pool) for a source's first `Started` and answers its delivered size
/// (even), or `fallback` when none came within [`START_WAIT`]. A source that ends first is
/// `ended`; a camera's error is its code.
async fn wait_started(
    mut src: Box<dyn FrameSource>,
    fallback: (u32, u32),
    camera: bool,
    ended: VideoError,
) -> Result<(Box<dyn FrameSource>, (u32, u32)), VideoError> {
    tokio::task::spawn_blocking(move || {
        let until = std::time::Instant::now() + START_WAIT;
        let size = loop {
            match src.try_event() {
                Some(SourceEvent::Started { width, height, .. }) => break (width, height),
                Some(SourceEvent::Ended(_)) => return Err(ended),
                Some(SourceEvent::Error(e)) if camera => return Err(camera_error(&e)),
                Some(_) => {}
                None if std::time::Instant::now() >= until => break fallback,
                None => std::thread::sleep(Duration::from_millis(5)),
            }
        };
        Ok((src, (size.0 & !1, size.1 & !1)))
    })
    .await
    .map_err(|_| VideoError::PublishFailed("source thread".to_owned()))?
}

/// An open source and what the pipeline needs to know of it.
struct Feed {
    src: Box<dyn FrameSource>,
    size: (u32, u32),
    facing: Facing,
}

/// Messages from the bridge to its pipeline thread.
enum ToPipe {
    /// A device switch: frames now come from this source (a camera facing this way), with the
    /// encoders at these sizes.
    Source(Box<dyn FrameSource>, Vec<LayerSpec>, Facing),
    /// A device switch begins: the current source is dropped (its camera closed) and the
    /// pipeline holds, the publication and the lease kept, until the next `Source`. Phones
    /// can't run two cameras at once: the old one closes before the new one opens. The pipeline
    /// answers once the source is gone.
    Release(std::sync::mpsc::Sender<()>),
    /// The app went to the background (`true`) or came back: no frame is captured or encoded
    /// while paused; on the return every encoder makes a keyframe first.
    Pause(bool),
}

/// The pipeline's source between a [`ToPipe::Release`] and the next [`ToPipe::Source`]: no
/// frames, no events.
struct Released;

impl FrameSource for Released {
    fn try_recv(&mut self) -> Option<kessel_video::VideoFrame> {
        None
    }
    fn try_event(&mut self) -> Option<SourceEvent> {
        None
    }
    fn set_constraints(&mut self, _c: CaptureConstraints) -> Result<(), KError> {
        Ok(())
    }
    fn set_paused(&mut self, _paused: bool) {}
    fn stats(&self) -> kessel_video::SourceStats {
        kessel_video::SourceStats::default()
    }
}

/// How long a switch waits for the pipeline to drop the old source.
const RELEASE_WAIT: Duration = Duration::from_secs(3);

struct Running {
    stop: Arc<AtomicBool>,
    /// The pipeline thread is over (the source or the publication ended).
    ended: Arc<AtomicBool>,
    bell: Arc<Bell>,
    to_pipe: std::sync::mpsc::Sender<ToPipe>,
    thread: Option<std::thread::JoinHandle<()>>,
    codec: VideoCodec,
    cap: CodecCapability,
    layers: Vec<LayerSpec>,
    size: (u32, u32),
    device: Option<String>,
    /// The temporal layers asked for.
    temporal: Option<u8>,
}

impl Running {
    fn state(&self) -> CameraState {
        CameraState::On {
            codec: self.codec,
            layers: self.layers.clone(),
            width: self.size.0,
            height: self.size.1,
        }
    }

    fn alive(&self) -> bool {
        !self.ended.load(Ordering::SeqCst)
    }

    fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.bell.ring();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

async fn stop_running(r: Running) {
    tokio::task::spawn_blocking(move || r.stop()).await.ok();
}

/// The two plans have the same layers (by rid, in order).
fn same_rids(a: &[LayerSpec], b: &[LayerSpec]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.rid == y.rid)
}

/// Has `r`'s pipeline drop its source ([`ToPipe::Release`]) and waits until it did.
async fn release_source(r: &Running) -> Result<(), VideoError> {
    let (tx, rx) = std::sync::mpsc::channel();
    if r.to_pipe.send(ToPipe::Release(tx)).is_err() {
        return Err(VideoError::Ended);
    }
    r.bell.ring();
    match tokio::task::spawn_blocking(move || rx.recv_timeout(RELEASE_WAIT)).await {
        Ok(Ok(())) => Ok(()),
        _ => Err(VideoError::Ended),
    }
}

/// What every pipeline reports to: the source events and the self-view.
#[derive(Clone)]
struct Taps {
    events: broadcast::Sender<SourceNote>,
    self_view: Arc<Mutex<Option<mpsc::Sender<SelfViewItem>>>>,
}

/// A publication's self-view keyframe flag and its pipeline's wake.
type SelfKey = (Arc<AtomicBool>, Arc<Bell>);

/// The video bridge of one app: a kessel-video backend into the current call.
pub struct VideoBridge {
    capture: VideoCapture,
    camera: tokio::sync::Mutex<Option<Running>>,
    /// Publications from sources opened outside [`VideoBridge::set_camera`]
    /// ([`VideoBridge::publish_source`], [`VideoBridge::publish_capture`]).
    feeds: tokio::sync::Mutex<HashMap<VideoSource, Running>>,
    taps: Taps,
    /// Each publication's self-view keyframe flag and its pipeline's wake.
    self_keys: Mutex<HashMap<VideoSource, SelfKey>>,
}

/// A frame source a host opened itself (a screen or window from its picker, a camera with its
/// own preview), handed to [`VideoBridge::publish_source`]: any kessel-video [`FrameSource`]
/// and the size it delivers.
pub struct SourceFeed {
    /// The frames.
    pub frames: Box<dyn FrameSource>,
    /// Its delivered width.
    pub width: u32,
    /// Its delivered height.
    pub height: u32,
}

impl std::fmt::Debug for SourceFeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceFeed")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

impl SourceFeed {
    /// `frames` delivering `width × height`.
    pub fn new(frames: Box<dyn FrameSource>, width: u32, height: u32) -> SourceFeed {
        SourceFeed {
            frames,
            width,
            height,
        }
    }
}

impl std::fmt::Debug for VideoBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoBridge").finish_non_exhaustive()
    }
}

impl VideoBridge {
    /// A bridge over `backend` (one per app).
    pub fn new(backend: Arc<dyn VideoBackend>) -> VideoBridge {
        VideoBridge::with_capture(VideoCapture::new(backend))
    }

    /// A bridge over the app's `capture` (its clones share the camera with the bridge: a
    /// preview opened through one of them hands the camera over to the camera publish).
    pub fn with_capture(capture: VideoCapture) -> VideoBridge {
        VideoBridge {
            capture,
            camera: tokio::sync::Mutex::new(None),
            feeds: tokio::sync::Mutex::new(HashMap::new()),
            taps: Taps {
                events: broadcast::channel(EVENT_QUEUE).0,
                self_view: Arc::new(Mutex::new(None)),
            },
            self_keys: Mutex::new(HashMap::new()),
        }
    }

    /// The capture the bridge opens its cameras through.
    pub fn capture(&self) -> &VideoCapture {
        &self.capture
    }

    /// The backend.
    pub fn backend(&self) -> &Arc<dyn VideoBackend> {
        self.capture.backend()
    }

    /// The published sources' events (pauses, resumes, ends), from now on.
    pub fn events(&self) -> broadcast::Receiver<SourceNote> {
        self.taps.events.subscribe()
    }

    /// The self-view from now on: each publication's self-view layer (see the module docs),
    /// until the receiver is dropped or a newer one is asked for. While one is open the
    /// source never pauses, even with no layer wanted by anyone.
    pub fn self_view(&self) -> SelfViewFrames {
        let (tx, rx) = mpsc::channel(SELF_VIEW_QUEUE);
        *lock(&self.taps.self_view) = Some(tx);
        for (key, bell) in lock(&self.self_keys).values() {
            key.store(true, Ordering::Release);
            bell.ring();
        }
        rx
    }

    /// The self-view of `source` needs a keyframe (a sink opened, became visible or failed).
    /// Never blocks.
    pub fn self_view_keyframe(&self, source: VideoSource) {
        if let Some((key, bell)) = lock(&self.self_keys).get(&source) {
            key.store(true, Ordering::Release);
            bell.ring();
        }
    }

    /// The camera now: [`CameraState::On`] while [`VideoBridge::set_camera`] publishes it.
    pub async fn camera_state(&self) -> CameraState {
        match self.camera.lock().await.as_ref() {
            Some(r) if r.alive() => r.state(),
            _ => CameraState::Off,
        }
    }

    /// The camera device [`VideoBridge::set_camera`] publishes (`None`: the default, or off).
    pub async fn camera_device(&self) -> Option<String> {
        self.camera
            .lock()
            .await
            .as_ref()
            .filter(|r| r.alive())
            .and_then(|r| r.device.clone())
    }

    /// Publishes `kind` from a source the host opened (see [`SourceFeed`]): the policy (video
    /// on, the source allowed), the lease (`self_video` or `self_stream`), the codec and one
    /// encoder per layer of the policy's hints for that source, the publication and the
    /// pipeline. The bridge owns the source from here; [`VideoBridge::stop_source`] ends it,
    /// and so does the source's own end.
    pub async fn publish_source(
        &self,
        call: &Call,
        kind: VideoSource,
        feed: SourceFeed,
        temporal_layers: Option<u8>,
    ) -> Result<CameraState, VideoError> {
        let size = (feed.width & !1, feed.height & !1);
        let bell = Arc::new(Bell::default());
        let feed_in = Feed {
            src: feed.frames,
            size,
            facing: Facing::Unknown,
        };
        self.publish_frames(call, kind, feed_in, temporal_layers, bell)
            .await
    }

    /// Publishes `kind` from a camera or screen opened through [`VideoCapture`] (a
    /// [`crate::ScreenSource`] with [`crate::ScreenSource::into_capture`]): its size from its
    /// first `Started` (at most its constraints), then as [`VideoBridge::publish_source`].
    pub async fn publish_capture(
        &self,
        call: &Call,
        kind: VideoSource,
        capture: Capture,
        temporal_layers: Option<u8>,
    ) -> Result<CameraState, VideoError> {
        let bell = Arc::new(Bell::default());
        capture.chain_wake(bell.wake());
        let facing = match kind {
            VideoSource::Camera => self.capture.camera_facing(capture.device()),
            VideoSource::ScreenShare => Facing::Unknown,
        };
        let c = capture.constraints();
        let fallback = (c.max_width.unwrap_or(1280), c.max_height.unwrap_or(720));
        let ended = match kind {
            VideoSource::Camera => VideoError::CameraNotFound,
            VideoSource::ScreenShare => VideoError::PublishFailed("screen: ended".to_owned()),
        };
        let (src, size) = wait_started(
            Box::new(capture),
            fallback,
            kind == VideoSource::Camera,
            ended,
        )
        .await?;
        let feed = Feed { src, size, facing };
        self.publish_frames(call, kind, feed, temporal_layers, bell)
            .await
    }

    async fn publish_frames(
        &self,
        call: &Call,
        kind: VideoSource,
        feed: Feed,
        temporal_layers: Option<u8>,
        bell: Arc<Bell>,
    ) -> Result<CameraState, VideoError> {
        let mut feeds = self.feeds.lock().await;
        if feeds.get(&kind).is_some_and(|r| !r.alive())
            && let Some(r) = feeds.remove(&kind)
        {
            stop_running(r).await;
        }
        if feeds.contains_key(&kind) {
            return Err(VideoError::AlreadyPublished);
        }
        let policy = call.media_policy().ok_or(VideoError::NotAllowed)?;
        let allowed = match kind {
            VideoSource::Camera => policy.camera_allowed,
            VideoSource::ScreenShare => policy.screen_allowed,
        };
        if !policy.video_enabled || !allowed {
            return Err(VideoError::NotAllowed);
        }
        call.set_video_lease(kind, true).await?;
        let started = self
            .start_from(call, &policy, kind, feed, temporal_layers, None, bell)
            .await;
        match started {
            Ok(r) => {
                let state = r.state();
                feeds.insert(kind, r);
                Ok(state)
            }
            Err(e) => {
                let _ = call.set_video_lease(kind, false).await;
                Err(e)
            }
        }
    }

    /// Ends a [`VideoBridge::publish_source`] or [`VideoBridge::publish_capture`] publication:
    /// the track unpublished, the source dropped (closed), the lease released.
    pub async fn stop_source(&self, call: &Call, kind: VideoSource) -> Result<(), VideoError> {
        let running = self.feeds.lock().await.remove(&kind);
        if let Some(r) = running {
            stop_running(r).await;
        }
        match call.unpublish_video(kind, true).await {
            Ok(()) | Err(VideoError::NotPublished) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Turns the camera on (see the module docs), switches it to another device, or turns it
    /// off (the track unpublished, the camera closed, the lease released).
    pub async fn set_camera(
        &self,
        call: &Call,
        req: CameraRequest,
    ) -> Result<CameraState, VideoError> {
        let mut slot = self.camera.lock().await;
        if slot.as_ref().is_some_and(|r| !r.alive())
            && let Some(r) = slot.take()
        {
            // Its source or publication ended (unpublished already).
            stop_running(r).await;
        }
        if !req.on {
            if let Some(r) = slot.take() {
                stop_running(r).await;
            }
            let reason = req.reason.as_deref().unwrap_or(super::publish::UNPUBLISHED);
            tracing::info!(target: "sew_call_client", %reason, "the camera turns off");
            return match call
                .unpublish_video_because(VideoSource::Camera, true, reason)
                .await
            {
                Ok(()) | Err(VideoError::NotPublished) => Ok(CameraState::Off),
                Err(e) => Err(e),
            };
        }
        let policy = call.media_policy().ok_or(VideoError::NotAllowed)?;
        if !policy.video_enabled || !policy.camera_allowed {
            return Err(VideoError::NotAllowed);
        }
        if let Some(r) = slot.as_ref() {
            if r.device == req.device {
                return Ok(r.state());
            }
            return self.switch(call, &mut slot, &policy, req).await;
        }
        self.capture
            .camera_allowed()
            .await
            .map_err(|e| capture_error(&e))?;
        call.set_video_lease(VideoSource::Camera, true).await?;
        match self.start(call, &policy, &req).await {
            Ok(r) => {
                let state = r.state();
                *slot = Some(r);
                Ok(state)
            }
            Err(e) => {
                let _ = call.set_video_lease(VideoSource::Camera, false).await;
                Err(e)
            }
        }
    }

    /// Switches the published camera to `device` (`None`: the default) when the camera is on;
    /// [`CameraState::Off`] and nothing opened when it is off (a host's camera choice between
    /// `set_camera` calls).
    pub async fn switch_camera(
        &self,
        call: &Call,
        device: Option<String>,
    ) -> Result<CameraState, VideoError> {
        let mut slot = self.camera.lock().await;
        let Some(r) = slot.as_ref().filter(|r| r.alive()) else {
            return Ok(CameraState::Off);
        };
        if r.device == device {
            return Ok(r.state());
        }
        let mut req = CameraRequest::on();
        req.device = device;
        req.temporal_layers = r.temporal;
        let policy = call.media_policy().ok_or(VideoError::NotAllowed)?;
        self.switch(call, &mut slot, &policy, req).await
    }

    /// Opens the camera through the capture (taking it over from a running preview); waits
    /// for its delivered size.
    async fn open(
        &self,
        device: Option<String>,
        policy: &sew_proto::voice::MediaPolicy,
        bell: &Arc<Bell>,
    ) -> Result<(Box<dyn FrameSource>, (u32, u32)), VideoError> {
        let hint = &policy.camera;
        let phone = cfg!(any(target_os = "ios", target_os = "android"));
        let (w, h) = camera_box(hint.max_width, hint.max_height, phone);
        let mut c = CaptureConstraints::new(w.max(16), h.max(16), hint.max_fps.clamp(1, 240));
        c.prefer_native = true;
        let cam = self
            .capture
            .open_camera(device.as_deref(), c)
            .await
            .map_err(|e| capture_error(&e))?
            .into_capture();
        cam.chain_wake(bell.wake());
        let fallback = (c.max_width.unwrap_or(1280), c.max_height.unwrap_or(720));
        wait_started(Box::new(cam), fallback, true, VideoError::CameraNotFound).await
    }

    fn plan(
        policy: &sew_proto::voice::MediaPolicy,
        kind: VideoSource,
        codec: VideoCodec,
        cap: &CodecCapability,
        temporal_layers: Option<u8>,
        size: (u32, u32),
    ) -> Vec<LayerSpec> {
        let tl = if codec == VideoCodec::Vp8 {
            let offered = cap.max_temporal_layers.clamp(1, 3);
            temporal_layers.map_or(offered, |t| t.clamp(1, offered))
        } else {
            1
        };
        let mut plan = plan_layers(super::publish::hint_of(kind, policy), size.0, size.1);
        for l in &mut plan {
            l.temporal_layers = tl;
        }
        plan
    }

    async fn start(
        &self,
        call: &Call,
        policy: &sew_proto::voice::MediaPolicy,
        req: &CameraRequest,
    ) -> Result<Running, VideoError> {
        let bell = Arc::new(Bell::default());
        let (src, size) = self.open(req.device.clone(), policy, &bell).await?;
        let facing = self.capture.camera_facing(req.device.as_deref());
        self.start_from(
            call,
            policy,
            VideoSource::Camera,
            Feed { src, size, facing },
            req.temporal_layers,
            req.device.clone(),
            bell,
        )
        .await
    }

    /// The codec, the encoders, the publication and the pipeline for an open source.
    #[allow(clippy::too_many_arguments)]
    async fn start_from(
        &self,
        call: &Call,
        policy: &sew_proto::voice::MediaPolicy,
        kind: VideoSource,
        feed: Feed,
        temporal_layers: Option<u8>,
        device: Option<String>,
        bell: Arc<Bell>,
    ) -> Result<Running, VideoError> {
        let Feed { src, size, facing } = feed;
        let backend = Arc::clone(self.backend());
        let Some((codec, cap)) = choose_codec(&policy.codecs, &backend.capabilities()) else {
            return Err(VideoError::CodecUnavailable);
        };
        let plan = VideoBridge::plan(policy, kind, codec, &cap, temporal_layers, size);
        let wake = bell.wake();
        let cap2 = cap.clone();
        let content = content_of(kind);
        let b2 = Arc::clone(&backend);
        let encs = tokio::task::spawn_blocking(move || {
            open_encoders(&b2, codec, &cap2, plan, content, &wake)
        })
        .await
        .map_err(|_| VideoError::PublishFailed("encoder thread".to_owned()))?;
        if encs.is_empty() {
            return Err(VideoError::CodecUnavailable);
        }
        let layers: Vec<LayerSpec> = encs.iter().map(|e| e.spec.clone()).collect();
        let publication = call
            .publish_video(VideoPublish::new(
                kind,
                codec,
                layers.clone(),
                size.0,
                size.1,
            ))
            .await?;
        tracing::info!(target: "sew_call_client", source = ?kind, codec = codec.as_str(), layers = layers.len(), width = size.0, height = size.1, "video source on");
        let stop = Arc::new(AtomicBool::new(false));
        let ended = Arc::new(AtomicBool::new(false));
        let self_key = Arc::new(AtomicBool::new(false));
        lock(&self.self_keys).insert(kind, (Arc::clone(&self_key), Arc::clone(&bell)));
        let (to_pipe, from_bridge) = std::sync::mpsc::channel();
        let mut pipe = Pipe {
            backend,
            kind,
            codec,
            content,
            src,
            facing,
            was_facing: facing,
            facing_since_us: 0,
            released_us: None,
            self_rid: self_view_rid(kind, &layers),
            encs: encs.into_iter().map(|e| (e.spec.rid.clone(), e)).collect(),
            publication,
            ctl: call.video_ctl(),
            taps: self.taps.clone(),
            stop: stop.clone(),
            ended: ended.clone(),
            self_key,
            bell: bell.clone(),
            from_bridge,
            paused: false,
            hidden: false,
            scene_paused: false,
            end_asked: false,
        };
        let name = match kind {
            VideoSource::Camera => "sew-video-camera",
            VideoSource::ScreenShare => "sew-video-screen",
        };
        let thread = std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || pipe.run())
            .map_err(|e| VideoError::PublishFailed(format!("spawn: {e}")))?;
        Ok(Running {
            stop,
            ended,
            bell,
            to_pipe,
            thread: Some(thread),
            codec,
            cap,
            layers,
            size,
            device,
            temporal: temporal_layers,
        })
    }

    /// A device switch: the old camera closes first (the publication and the lease kept, the
    /// frames held), then the new camera's frames feed the same encoders (resized for its
    /// aspect, a keyframe first); a different layer set publishes again with it. The new camera
    /// not opening: the old one again and the error; that failing too: the camera ends with
    /// `camera_lost`.
    async fn switch(
        &self,
        call: &Call,
        slot: &mut Option<Running>,
        policy: &sew_proto::voice::MediaPolicy,
        req: CameraRequest,
    ) -> Result<CameraState, VideoError> {
        let Some(r) = slot.as_mut() else {
            return Err(VideoError::NotPublished);
        };
        let bell = Arc::clone(&r.bell);
        let began = std::time::Instant::now();
        // One camera at a time (iOS has no multi-cam session; opening the second camera
        // interrupts the first): the old source closes before the new one opens.
        release_source(r).await?;
        let (src, size) = match self.open(req.device.clone(), policy, &bell).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(target: "sew_call_client", code = e.code(), device = ?req.device, "the new camera did not open: back to the old one");
                match self.open(r.device.clone(), policy, &bell).await {
                    Ok((src, size)) => {
                        let plan = VideoBridge::plan(
                            policy,
                            VideoSource::Camera,
                            r.codec,
                            &r.cap,
                            r.temporal,
                            size,
                        );
                        let plan = if same_rids(&plan, &r.layers) {
                            plan
                        } else {
                            r.layers.clone()
                        };
                        let facing = self.capture.camera_facing(r.device.as_deref());
                        if r.to_pipe
                            .send(ToPipe::Source(src, plan.clone(), facing))
                            .is_err()
                        {
                            return Err(VideoError::Ended);
                        }
                        r.bell.ring();
                        r.layers = plan;
                        r.size = size;
                        tracing::info!(target: "sew_call_client", gap_ms = began.elapsed().as_millis() as u64, "the old camera is back");
                    }
                    Err(e2) => {
                        tracing::warn!(target: "sew_call_client", code = e2.code(), "the old camera did not open again: camera_lost");
                        if let Some(r) = slot.take() {
                            stop_running(r).await;
                        }
                        let reason = CaptureEnd::DeviceLost.reason(CaptureKind::Camera);
                        let _ = self.taps.events.send(SourceNote {
                            source: VideoSource::Camera,
                            event: CaptureEvent::Ended(CaptureEnd::DeviceLost),
                        });
                        let _ = call
                            .unpublish_video_because(VideoSource::Camera, true, reason)
                            .await;
                    }
                }
                return Err(e);
            }
        };
        let plan = VideoBridge::plan(
            policy,
            VideoSource::Camera,
            r.codec,
            &r.cap,
            req.temporal_layers,
            size,
        );
        if same_rids(&plan, &r.layers) {
            let facing = self.capture.camera_facing(req.device.as_deref());
            if r.to_pipe
                .send(ToPipe::Source(src, plan.clone(), facing))
                .is_err()
            {
                return Err(VideoError::Ended);
            }
            r.bell.ring();
            r.layers = plan;
            r.size = size;
            r.device = req.device.clone();
            tracing::info!(target: "sew_call_client", device = ?req.device, gap_ms = began.elapsed().as_millis() as u64, "the camera switched");
            return Ok(r.state());
        }
        if let Some(r) = slot.take() {
            stop_running(r).await;
        }
        tracing::info!(target: "sew_call_client", "the camera switches with a new layer set: published again");
        call.unpublish_video_because(VideoSource::Camera, false, CAMERA_SWITCH)
            .await?;
        let r = self
            .start_from(
                call,
                policy,
                VideoSource::Camera,
                Feed {
                    src,
                    size,
                    facing: self.capture.camera_facing(req.device.as_deref()),
                },
                req.temporal_layers,
                req.device.clone(),
                bell,
            )
            .await?;
        let state = r.state();
        *slot = Some(r);
        Ok(state)
    }

    /// Pauses (`true`) or resumes the camera's pipeline, keeping its publication and lease (a
    /// phone app in the background): no frame is captured (the source is paused) or encoded
    /// while paused, and on the return every encoder is asked for a keyframe before the first
    /// frame is encoded. [`crate::Call::set_video_muted`] tells subscribers. `false`: no camera
    /// runs.
    pub async fn pause_camera(&self, paused: bool) -> bool {
        let slot = self.camera.lock().await;
        let Some(r) = slot.as_ref() else {
            return false;
        };
        if r.to_pipe.send(ToPipe::Pause(paused)).is_err() {
            return false;
        }
        r.bell.ring();
        true
    }

    /// Stops every pipeline without touching the call (the call ended).
    pub async fn close(&self) {
        let mut all: Vec<Running> = self.feeds.lock().await.drain().map(|(_, r)| r).collect();
        all.extend(self.camera.lock().await.take());
        for r in all {
            stop_running(r).await;
        }
    }
}

/// The pipeline of one publication (its own thread).
struct Pipe {
    backend: Arc<dyn VideoBackend>,
    kind: VideoSource,
    codec: VideoCodec,
    content: ContentHint,
    src: Box<dyn FrameSource>,
    /// Which way the source's camera faces (`Unknown`: a screen, a host's source).
    facing: Facing,
    /// The camera's before the last switch, for frames captured before `facing_since_us`.
    was_facing: Facing,
    facing_since_us: u64,
    /// When the last [`ToPipe::Release`] dropped the source (the backend clock).
    released_us: Option<u64>,
    encs: BTreeMap<String, Enc>,
    /// The layer whose output is the self-view.
    self_rid: Option<String>,
    publication: VideoPublication,
    ctl: VideoCtl,
    taps: Taps,
    stop: Arc<AtomicBool>,
    ended: Arc<AtomicBool>,
    /// The self-view needs a keyframe.
    self_key: Arc<AtomicBool>,
    bell: Arc<Bell>,
    from_bridge: std::sync::mpsc::Receiver<ToPipe>,
    /// The source is paused by the pipeline (nothing wants a frame, or the app in the
    /// background).
    paused: bool,
    /// The source is paused by the OS (a hidden window, another app took the camera).
    hidden: bool,
    /// The app is in the background ([`ToPipe::Pause`]).
    scene_paused: bool,
    /// The pipeline asked the engine to end the publication (all its encoders lost).
    end_asked: bool,
}

impl Pipe {
    fn run(&mut self) {
        self.pump();
        self.ended.store(true, Ordering::SeqCst);
        self.to_self_view(SelfViewItem::Ended { source: self.kind });
    }

    fn pump(&mut self) {
        let sender = self.publication.sender();
        while !self.stop.load(Ordering::SeqCst) {
            if !self.controls() {
                return;
            }
            while let Ok(m) = self.from_bridge.try_recv() {
                match m {
                    ToPipe::Source(src, sizes, facing) => {
                        self.src = src;
                        // Frames captured from the release on (else from now) are the new
                        // camera's.
                        self.was_facing = self.facing;
                        self.facing = facing;
                        self.facing_since_us = self
                            .released_us
                            .take()
                            .unwrap_or_else(|| self.backend.now_us());
                        if std::mem::take(&mut self.hidden) {
                            // The old source's pause ended with it: this one runs.
                            let _ = self.taps.events.send(SourceNote {
                                source: self.kind,
                                event: CaptureEvent::Resumed,
                            });
                        }
                        self.src.set_paused(self.paused);
                        // The new camera's first frame is a keyframe on every layer.
                        for e in self.encs.values_mut() {
                            e.enc.request_keyframe();
                        }
                        for l in sizes {
                            if let Some(e) = self.encs.get_mut(&l.rid)
                                && (e.spec.width, e.spec.height) != (l.width, l.height)
                            {
                                let _ = e.enc.set_size(l.width, l.height);
                                e.spec.width = l.width;
                                e.spec.height = l.height;
                            }
                        }
                    }
                    ToPipe::Release(done) => {
                        self.src = Box::new(Released);
                        self.released_us = Some(self.backend.now_us());
                        let _ = done.send(());
                    }
                    ToPipe::Pause(p) => self.scene(p),
                }
            }
            if !self.source_events() {
                return;
            }
            let self_view = self.self_view_open();
            self.repause(self_view);
            if self.self_key.swap(false, Ordering::AcqRel)
                && let Some(e) = self.self_rid.as_ref().and_then(|r| self.encs.get_mut(r))
            {
                e.enc.request_keyframe();
            }
            if !self.hidden
                && !self.scene_paused
                && let Some(frame) = self.latest()
            {
                self.submit(&frame, self_view);
            }
            self.drain(&sender);
            self.bell.wait(Duration::from_millis(20));
        }
    }

    /// The source's events; `false` once it ended (unpublished with its reason).
    fn source_events(&mut self) -> bool {
        while let Some(ev) = self.src.try_event() {
            let Some(ev) = map_event(ev) else { continue };
            let mut over = false;
            match &ev {
                CaptureEvent::Paused(why) => {
                    tracing::info!(target: "sew_call_client", source = ?self.kind, ?why, "the video source paused");
                    self.hidden = true;
                }
                CaptureEvent::Resumed => {
                    self.hidden = false;
                    for e in self.encs.values_mut() {
                        e.enc.request_keyframe();
                    }
                }
                CaptureEvent::Ended(end) => {
                    let reason = end.reason(capture_kind(self.kind));
                    tracing::info!(target: "sew_call_client", source = ?self.kind, reason, "the video source ended");
                    self.ctl.end(self.kind, reason);
                    over = true;
                }
                _ => {}
            }
            let _ = self.taps.events.send(SourceNote {
                source: self.kind,
                event: ev,
            });
            if over {
                return false;
            }
        }
        true
    }

    fn self_view_open(&self) -> bool {
        lock(&self.taps.self_view)
            .as_ref()
            .is_some_and(|tx| !tx.is_closed())
    }

    fn to_self_view(&self, item: SelfViewItem) {
        let full = match lock(&self.taps.self_view).as_ref() {
            Some(tx) => matches!(tx.try_send(item), Err(mpsc::error::TrySendError::Full(_))),
            None => false,
        };
        if full {
            // A dropped frame breaks the self-view's decoder: the next one is a keyframe.
            self.self_key.store(true, Ordering::Release);
        }
    }

    /// Nobody wants any layer and no self-view is open, or the app is in the background: the
    /// source pauses.
    fn repause(&mut self, self_view: bool) {
        let want = !self.scene_paused && (self_view || self.encs.values().any(|e| e.active));
        if want == self.paused {
            self.paused = !want;
            self.src.set_paused(self.paused);
        }
    }

    /// The engine's controls; `false` once the publication ended.
    fn controls(&mut self) -> bool {
        while let Some(c) = self.publication.try_control() {
            match c {
                VideoControl::Keyframe { rid } => {
                    if let Some(e) = self.encs.get_mut(&rid) {
                        e.enc.request_keyframe();
                    }
                }
                VideoControl::Rates(rates) => {
                    for r in rates {
                        if let Some(e) = self.encs.get_mut(&r.rid) {
                            e.active = r.active;
                            if r.active {
                                let _ = e
                                    .enc
                                    .set_rates(Rates::new(r.bitrate_bps.max(10_000), r.fps.max(1)));
                            }
                        }
                    }
                }
                VideoControl::Republish(layers) => self.relayer(layers),
                VideoControl::Ended { reason } => {
                    tracing::info!(target: "sew_call_client", source = ?self.kind, %reason, "the video publication ended");
                    return false;
                }
            }
        }
        true
    }

    /// The app's scene: paused in the background; back in front, every encoder makes a
    /// keyframe before the first frame it encodes (frames captured before the pause are
    /// dropped).
    fn scene(&mut self, paused: bool) {
        if paused == self.scene_paused {
            return;
        }
        self.scene_paused = paused;
        if paused {
            tracing::info!(target: "sew_call_client", "the camera pauses (background)");
        } else {
            tracing::info!(target: "sew_call_client", "the camera resumes with a keyframe");
            while self.src.try_recv().is_some() {}
            for e in self.encs.values_mut() {
                e.enc.request_keyframe();
            }
        }
        let self_view = self.self_view_open();
        self.repause(self_view);
    }

    /// A republish's layers: existing encoders resized, new ones opened, gone ones closed.
    fn relayer(&mut self, layers: Vec<LayerSpec>) {
        self.self_rid = self_view_rid(self.kind, &layers);
        let wake = self.bell.wake();
        let mut next = BTreeMap::new();
        for l in layers {
            match self.encs.remove(&l.rid) {
                Some(mut e) => {
                    if (e.spec.width, e.spec.height) != (l.width, l.height) {
                        let _ = e.enc.set_size(l.width, l.height);
                    }
                    let _ = e.enc.set_rates(Rates::new(
                        l.max_kbps.saturating_mul(1000).max(10_000),
                        l.fps.max(1),
                    ));
                    e.spec = l.clone();
                    next.insert(l.rid.clone(), e);
                }
                None => match self
                    .backend
                    .encoder(encoder_config(self.codec, &l, self.content), wake.clone())
                {
                    Ok(enc) => {
                        next.insert(
                            l.rid.clone(),
                            Enc {
                                spec: l,
                                enc,
                                active: true,
                            },
                        );
                    }
                    Err(e) => {
                        tracing::info!(target: "sew_call_client", rid = %l.rid, error = %e, "a republished layer's encoder did not open");
                    }
                },
            }
        }
        self.encs = next;
    }

    /// The newest captured frame (older ones are dropped).
    fn latest(&mut self) -> Option<kessel_video::VideoFrame> {
        let mut last = None;
        while let Some(f) = self.src.try_recv() {
            last = Some(f);
        }
        last
    }

    /// To every active layer's encoder, and to the self-view layer's while a self-view is
    /// open.
    fn submit(&mut self, frame: &kessel_video::VideoFrame, self_view: bool) {
        let mut planes: Option<kessel_video::VideoFrame> = None;
        for (rid, e) in &mut self.encs {
            let for_self = self_view && self.self_rid.as_ref() == Some(rid);
            if !e.active && !for_self {
                continue;
            }
            match e.enc.submit(frame, EncodeOptions::default()) {
                Ok(()) => {}
                Err(KError::Unsupported(_)) if matches!(frame.buffer, FrameBuffer::Native(_)) => {
                    // A native buffer this encoder can't take: CPU planes instead.
                    if planes.is_none() {
                        planes = self
                            .backend
                            .convert(frame, frame.width, frame.height, PixelFormat::I420)
                            .ok();
                    }
                    if let Some(p) = &planes {
                        let _ = e.enc.submit(p, EncodeOptions::default());
                    }
                }
                // Busy: this frame is dropped for that layer (counted by the encoder).
                Err(_) => {}
            }
        }
    }

    fn drain(&mut self, sender: &VideoSender) {
        let mut dead = Vec::new();
        let mut local = Vec::new();
        for (rid, e) in &mut self.encs {
            let for_self = self.self_rid.as_ref() == Some(rid);
            while let Some(r) = e.enc.poll() {
                match r {
                    Ok(f) => {
                        let Some(codec) = our_codec(f.codec) else {
                            continue;
                        };
                        if for_self {
                            local.push(LocalFrame {
                                codec,
                                data: f.data.clone(),
                                keyframe: f.keyframe,
                                capture_us: f.capture_us,
                                facing: if f.capture_us >= self.facing_since_us {
                                    self.facing
                                } else {
                                    self.was_facing
                                },
                                width: f.width,
                                height: f.height,
                            });
                        }
                        if !e.active {
                            // Encoded for the self-view only: the engine doesn't want it.
                            continue;
                        }
                        sender.push(
                            EncodedVideo::new(
                                rid,
                                codec,
                                f.data,
                                f.keyframe,
                                f.capture_us,
                                f.width,
                                f.height,
                            )
                            .with_layer(f.layer.temporal_id, f.layer.layer_sync),
                        );
                    }
                    Err(err) => {
                        tracing::info!(target: "sew_call_client", rid = %rid, error = %err, "an encoder session was lost; reopening");
                        dead.push(rid.clone());
                        break;
                    }
                }
            }
        }
        for frame in local {
            self.to_self_view(SelfViewItem::Frame {
                source: self.kind,
                frame,
            });
        }
        let wake = self.bell.wake();
        let any_dead = !dead.is_empty();
        for rid in dead {
            let Some(e) = self.encs.get_mut(&rid) else {
                continue;
            };
            match self.backend.encoder(
                encoder_config(self.codec, &e.spec, self.content),
                wake.clone(),
            ) {
                Ok(mut enc) => {
                    enc.request_keyframe();
                    e.enc = enc;
                }
                Err(err) => {
                    tracing::info!(target: "sew_call_client", rid = %rid, error = %err, "a lost encoder did not reopen: its layer stops");
                    self.encs.remove(&rid);
                }
            }
        }
        if any_dead && self.encs.is_empty() && !self.end_asked {
            tracing::info!(target: "sew_call_client", source = ?self.kind, "no encoder left: unpublishing");
            self.end_asked = true;
            self.ctl.end(self.kind, ENCODER_LOST);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kessel_video::{CodecCapability, Implementation};

    fn caps(codecs: &[kessel_video::VideoCodec]) -> Capabilities {
        let mut c = Capabilities::new("test", "test");
        for k in codecs {
            c.codecs.push(CodecCapability::new(
                *k,
                CodecDirection::Encode,
                Implementation::Software,
                "enc",
                1280,
                720,
                30,
            ));
        }
        c
    }

    #[test]
    fn vp8_first_then_h264_and_only_what_the_policy_lists() {
        use sew_proto::voice::VideoCodec as P;
        let both = caps(&[
            kessel_video::VideoCodec::H264,
            kessel_video::VideoCodec::Vp8,
        ]);
        let pick = |policy: &[P], c: &Capabilities| choose_codec(policy, c).map(|(c, _)| c);
        assert_eq!(pick(&[P::Vp8, P::H264], &both), Some(VideoCodec::Vp8));
        assert_eq!(pick(&[P::H264, P::Vp8], &both), Some(VideoCodec::Vp8));
        assert_eq!(pick(&[P::H264], &both), Some(VideoCodec::H264));
        let h264_only = caps(&[kessel_video::VideoCodec::H264]);
        assert_eq!(pick(&[P::Vp8, P::H264], &h264_only), Some(VideoCodec::H264));
        assert_eq!(pick(&[P::Vp8], &h264_only), None, "codec_unavailable");
        assert_eq!(pick(&[], &both), None);
    }

    /// The keyframe flags of the frames queued now.
    fn queued(rx: &mut mpsc::Receiver<super::super::publish::VideoIn>) -> Vec<bool> {
        let mut out = Vec::new();
        while let Ok(f) = rx.try_recv() {
            out.push(f.frame.keyframe);
        }
        out
    }

    fn collect_until(
        rx: &mut mpsc::Receiver<super::super::publish::VideoIn>,
        what: &str,
        n: usize,
    ) -> Vec<bool> {
        let until = std::time::Instant::now() + Duration::from_secs(10);
        let mut got = Vec::new();
        while got.len() < n {
            assert!(
                std::time::Instant::now() < until,
                "timed out waiting for {what}"
            );
            got.extend(queued(rx));
            std::thread::sleep(Duration::from_millis(5));
        }
        got
    }

    #[test]
    fn a_background_pause_stops_capture_and_encoding_and_the_return_starts_at_a_keyframe() {
        let backend: Arc<dyn VideoBackend> =
            kessel_video::fake::FakeBackend::new(kessel_video::fake::FakeConfig::default());
        let bell = Arc::new(Bell::default());
        let src = backend
            .open_camera(None, CaptureConstraints::new(640, 360, 30), bell.wake())
            .expect("the fake camera opens");
        let cap = backend
            .capabilities()
            .best(kessel_video::VideoCodec::H264, CodecDirection::Encode)
            .expect("the fake's H.264")
            .clone();
        let encs = open_encoders(
            &backend,
            VideoCodec::H264,
            &cap,
            vec![LayerSpec::new("q", 320, 180, 150, 30)],
            ContentHint::Camera,
            &bell.wake(),
        );
        assert_eq!(encs.len(), 1);
        let (publication, mut rx, _ctl) = VideoPublication::for_test(VideoSource::Camera);
        let stop = Arc::new(AtomicBool::new(false));
        let (to_pipe, from_bridge) = std::sync::mpsc::channel();
        let (cmd, _cmds) = mpsc::unbounded_channel();
        let mut pipe = Pipe {
            backend: backend.clone(),
            kind: VideoSource::Camera,
            codec: VideoCodec::H264,
            content: ContentHint::Camera,
            src,
            facing: Facing::Unknown,
            was_facing: Facing::Unknown,
            facing_since_us: 0,
            released_us: None,
            encs: encs.into_iter().map(|e| (e.spec.rid.clone(), e)).collect(),
            self_rid: None,
            publication,
            ctl: VideoCtl::new(cmd),
            taps: Taps {
                events: broadcast::channel(EVENT_QUEUE).0,
                self_view: Arc::new(Mutex::new(None)),
            },
            stop: stop.clone(),
            ended: Arc::new(AtomicBool::new(false)),
            self_key: Arc::new(AtomicBool::new(false)),
            bell: bell.clone(),
            from_bridge,
            paused: false,
            hidden: false,
            scene_paused: false,
            end_asked: false,
        };
        let thread = std::thread::spawn(move || pipe.run());

        // Sending: a keyframe, then deltas.
        let first = collect_until(&mut rx, "the first frames", 5);
        assert!(first[0], "the first frame is a keyframe");
        assert!(first[1..].iter().all(|k| !k), "then deltas only: {first:?}");

        // The background: nothing is encoded (frames already queued may still drain).
        to_pipe.send(ToPipe::Pause(true)).expect("pause");
        bell.ring();
        std::thread::sleep(Duration::from_millis(100));
        let _ = queued(&mut rx);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(queued(&mut rx), Vec::<bool>::new(), "no frame while paused");
        // A repeat changes nothing.
        to_pipe.send(ToPipe::Pause(true)).expect("pause");
        bell.ring();
        std::thread::sleep(Duration::from_millis(100));
        assert!(queued(&mut rx).is_empty());

        // The return: the first frame is a keyframe, then deltas again.
        to_pipe.send(ToPipe::Pause(false)).expect("resume");
        bell.ring();
        let back = collect_until(&mut rx, "frames after the return", 4);
        assert!(back[0], "the return starts at a keyframe: {back:?}");
        assert!(back[1..].iter().all(|k| !k), "one keyframe: {back:?}");

        stop.store(true, Ordering::SeqCst);
        bell.ring();
        thread.join().expect("the pipeline ends");
    }

    #[test]
    fn a_source_that_ends_asks_the_engine_to_unpublish_with_its_reason() {
        let mut cfg = kessel_video::fake::FakeConfig::default();
        cfg.source_events = vec![(
            Duration::from_millis(300),
            SourceEvent::Ended(kessel_video::EndReason::DeviceLost),
        )];
        let backend: Arc<dyn VideoBackend> = kessel_video::fake::FakeBackend::new(cfg);
        let bell = Arc::new(Bell::default());
        let src = backend
            .open_camera(None, CaptureConstraints::new(640, 360, 30), bell.wake())
            .expect("the fake camera opens");
        let cap = backend
            .capabilities()
            .best(kessel_video::VideoCodec::H264, CodecDirection::Encode)
            .expect("the fake's H.264")
            .clone();
        let encs = open_encoders(
            &backend,
            VideoCodec::H264,
            &cap,
            vec![LayerSpec::new("q", 320, 180, 150, 30)],
            ContentHint::Camera,
            &bell.wake(),
        );
        let (publication, mut rx, _ctl) = VideoPublication::for_test(VideoSource::Camera);
        let (cmd, mut cmds) = mpsc::unbounded_channel();
        let (_to_pipe, from_bridge) = std::sync::mpsc::channel();
        let mut pipe = Pipe {
            backend: backend.clone(),
            kind: VideoSource::Camera,
            codec: VideoCodec::H264,
            content: ContentHint::Camera,
            src,
            facing: Facing::Unknown,
            was_facing: Facing::Unknown,
            facing_since_us: 0,
            released_us: None,
            encs: encs.into_iter().map(|e| (e.spec.rid.clone(), e)).collect(),
            self_rid: None,
            publication,
            ctl: VideoCtl::new(cmd),
            taps: Taps {
                events: broadcast::channel(EVENT_QUEUE).0,
                self_view: Arc::new(Mutex::new(None)),
            },
            stop: Arc::new(AtomicBool::new(false)),
            ended: Arc::new(AtomicBool::new(false)),
            self_key: Arc::new(AtomicBool::new(false)),
            bell: bell.clone(),
            from_bridge,
            paused: false,
            hidden: false,
            scene_paused: false,
            end_asked: false,
        };
        let thread = std::thread::spawn(move || pipe.run());
        let _ = collect_until(&mut rx, "frames before the end", 2);
        let until = std::time::Instant::now() + Duration::from_secs(10);
        while !thread.is_finished() {
            assert!(
                std::time::Instant::now() < until,
                "the pipeline ends with its source"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        thread.join().expect("the pipeline ends");
        match cmds.try_recv().expect("the engine is asked to unpublish") {
            crate::call::Cmd::Publish(super::super::publish::VideoCmd::Unpublish {
                source,
                release,
                reason,
                ..
            }) => {
                assert_eq!(source, VideoSource::Camera);
                assert!(release, "the lease is released");
                assert_eq!(reason, "camera_lost");
            }
            _ => panic!("not an unpublish"),
        }
        assert!(cmds.try_recv().is_err(), "once");
    }

    /// A camera pipeline on its own thread, and what a test holds of it.
    struct TestPipe {
        to_pipe: std::sync::mpsc::Sender<ToPipe>,
        rx: mpsc::Receiver<super::super::publish::VideoIn>,
        notes: broadcast::Receiver<SourceNote>,
        stop: Arc<AtomicBool>,
        bell: Arc<Bell>,
        thread: Option<std::thread::JoinHandle<()>>,
        _ctl: mpsc::UnboundedSender<VideoControl>,
        _cmds: mpsc::UnboundedReceiver<crate::call::Cmd>,
    }

    impl TestPipe {
        /// The pipeline over `src`: one H.264 q layer on `backend`.
        fn start(
            backend: &Arc<dyn VideoBackend>,
            src: Box<dyn FrameSource>,
            bell: &Arc<Bell>,
        ) -> TestPipe {
            let cap = backend
                .capabilities()
                .best(kessel_video::VideoCodec::H264, CodecDirection::Encode)
                .expect("the fake's H.264")
                .clone();
            let encs = open_encoders(
                backend,
                VideoCodec::H264,
                &cap,
                vec![LayerSpec::new("q", 320, 180, 150, 30)],
                ContentHint::Camera,
                &bell.wake(),
            );
            assert_eq!(encs.len(), 1);
            let (publication, rx, ctl) = VideoPublication::for_test(VideoSource::Camera);
            let (cmd, cmds) = mpsc::unbounded_channel();
            let events = broadcast::channel(EVENT_QUEUE).0;
            let notes = events.subscribe();
            let stop = Arc::new(AtomicBool::new(false));
            let (to_pipe, from_bridge) = std::sync::mpsc::channel();
            let mut pipe = Pipe {
                backend: backend.clone(),
                kind: VideoSource::Camera,
                codec: VideoCodec::H264,
                content: ContentHint::Camera,
                src,
                facing: Facing::Front,
                was_facing: Facing::Front,
                facing_since_us: 0,
                released_us: None,
                encs: encs.into_iter().map(|e| (e.spec.rid.clone(), e)).collect(),
                self_rid: None,
                publication,
                ctl: VideoCtl::new(cmd),
                taps: Taps {
                    events,
                    self_view: Arc::new(Mutex::new(None)),
                },
                stop: stop.clone(),
                ended: Arc::new(AtomicBool::new(false)),
                self_key: Arc::new(AtomicBool::new(false)),
                bell: bell.clone(),
                from_bridge,
                paused: false,
                hidden: false,
                scene_paused: false,
                end_asked: false,
            };
            let thread = std::thread::spawn(move || pipe.run());
            TestPipe {
                to_pipe,
                rx,
                notes,
                stop,
                bell: bell.clone(),
                thread: Some(thread),
                _ctl: ctl,
                _cmds: cmds,
            }
        }

        fn send(&self, m: ToPipe) {
            self.to_pipe.send(m).expect("the pipeline runs");
            self.bell.ring();
        }

        /// The next pause, resume or end note within 10 s (a loud failure naming `what`
        /// otherwise; `Started` and errors skipped).
        fn next_note(&mut self, what: &str) -> CaptureEvent {
            let until = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                match self.notes.try_recv() {
                    Ok(n) if is_state(&n.event) => return n.event,
                    Ok(_) => {}
                    Err(broadcast::error::TryRecvError::Empty) => {}
                    Err(e) => panic!("{what}: the notes failed: {e}"),
                }
                assert!(
                    std::time::Instant::now() < until,
                    "timed out waiting for {what}"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        /// Nothing is encoded for a while (frames already queued drain first).
        fn assert_quiet(&mut self, what: &str) {
            std::thread::sleep(Duration::from_millis(100));
            let _ = queued(&mut self.rx);
            std::thread::sleep(Duration::from_millis(300));
            assert_eq!(queued(&mut self.rx), Vec::<bool>::new(), "{what}");
        }

        fn finish(mut self) {
            self.stop.store(true, Ordering::SeqCst);
            self.bell.ring();
            if let Some(t) = self.thread.take() {
                t.join().expect("the pipeline ends");
            }
        }
    }

    /// A pause, a resume or an end.
    fn is_state(e: &CaptureEvent) -> bool {
        matches!(
            e,
            CaptureEvent::Paused(_) | CaptureEvent::Resumed | CaptureEvent::Ended(_)
        )
    }

    fn fake_camera(backend: &Arc<dyn VideoBackend>, bell: &Arc<Bell>) -> Box<dyn FrameSource> {
        backend
            .open_camera(None, CaptureConstraints::new(640, 360, 30), bell.wake())
            .expect("the fake camera opens")
    }

    /// Another app takes the camera (`camera_busy`): no frame is encoded while it holds it,
    /// and when it gives it back the pipeline resumes on its own, a keyframe first.
    #[test]
    fn a_camera_busy_pause_resumes_with_a_keyframe_when_it_ends() {
        let mut cfg = kessel_video::fake::FakeConfig::default();
        cfg.source_events = vec![
            (
                Duration::from_millis(400),
                SourceEvent::Interrupted(kessel_video::Interruption::InUseByAnother),
            ),
            (Duration::from_millis(1200), SourceEvent::Resumed),
        ];
        let backend: Arc<dyn VideoBackend> = kessel_video::fake::FakeBackend::new(cfg);
        let bell = Arc::new(Bell::default());
        let src = fake_camera(&backend, &bell);
        let mut p = TestPipe::start(&backend, src, &bell);
        let first = collect_until(&mut p.rx, "the first frames", 3);
        assert!(first[0], "the first frame is a keyframe");

        assert_eq!(
            p.next_note("the camera_busy pause"),
            CaptureEvent::Paused(super::super::capture::CapturePause::InUseByAnother)
        );
        p.assert_quiet("no frame while another app holds the camera");

        assert_eq!(p.next_note("the resume"), CaptureEvent::Resumed);
        let back = collect_until(&mut p.rx, "frames after the resume", 4);
        assert!(back[0], "the resume starts at a keyframe: {back:?}");
        assert!(back[1..].iter().all(|k| !k), "one keyframe: {back:?}");
        p.finish();
    }

    /// A switch on a phone: the old camera, interrupted when another opener took it, is
    /// released (dropped: no frames, the publication kept); the new source's first frame is a
    /// keyframe and the old pause is reported over (`Resumed`): what is sent is what
    /// `local_video` says.
    #[test]
    fn a_released_source_holds_the_pipeline_and_the_next_one_starts_at_a_keyframe() {
        let mut cfg = kessel_video::fake::FakeConfig::default();
        cfg.source_events = vec![(
            Duration::from_millis(400),
            SourceEvent::Interrupted(kessel_video::Interruption::InUseByAnother),
        )];
        let old_backend: Arc<dyn VideoBackend> = kessel_video::fake::FakeBackend::new(cfg);
        let backend: Arc<dyn VideoBackend> =
            kessel_video::fake::FakeBackend::new(kessel_video::fake::FakeConfig::default());
        let bell = Arc::new(Bell::default());
        let src = fake_camera(&old_backend, &bell);
        let mut p = TestPipe::start(&backend, src, &bell);
        let _ = collect_until(&mut p.rx, "the first frames", 3);
        assert!(matches!(
            p.next_note("the old camera's pause"),
            CaptureEvent::Paused(_)
        ));

        let (tx, done) = std::sync::mpsc::channel();
        p.send(ToPipe::Release(tx));
        done.recv_timeout(Duration::from_secs(5))
            .expect("the pipeline drops the source and says so");
        p.assert_quiet("no frame while released");
        assert!(
            p.thread.as_ref().is_some_and(|t| !t.is_finished()),
            "the publication is kept"
        );

        let next = fake_camera(&backend, &bell);
        p.send(ToPipe::Source(
            next,
            vec![LayerSpec::new("q", 320, 180, 150, 30)],
            Facing::Back,
        ));
        assert_eq!(
            p.next_note("the pause reported over"),
            CaptureEvent::Resumed
        );
        let back = collect_until(&mut p.rx, "the new camera's frames", 4);
        assert!(back[0], "the new camera starts at a keyframe: {back:?}");
        assert!(back[1..].iter().all(|k| !k), "one keyframe: {back:?}");
        p.finish();
    }

    /// A switch to a running source (no pause before): its first frame is a keyframe too,
    /// and no note is sent.
    #[test]
    fn a_switched_source_starts_at_a_keyframe() {
        let backend: Arc<dyn VideoBackend> =
            kessel_video::fake::FakeBackend::new(kessel_video::fake::FakeConfig::default());
        let bell = Arc::new(Bell::default());
        let src = fake_camera(&backend, &bell);
        let mut p = TestPipe::start(&backend, src, &bell);
        let first = collect_until(&mut p.rx, "the first frames", 6);
        assert!(first[0] && first[1..].iter().all(|k| !k), "{first:?}");
        let (tx, done) = std::sync::mpsc::channel();
        p.send(ToPipe::Release(tx));
        done.recv_timeout(Duration::from_secs(5)).expect("released");
        p.assert_quiet("no frame while released");
        let next = fake_camera(&backend, &bell);
        p.send(ToPipe::Source(
            next,
            vec![LayerSpec::new("q", 320, 180, 150, 30)],
            Facing::Back,
        ));
        let back = collect_until(&mut p.rx, "the new camera's frames", 4);
        assert!(back[0], "the switch starts at a keyframe: {back:?}");
        let mut notes = Vec::new();
        while let Ok(n) = p.notes.try_recv() {
            notes.push(n.event);
        }
        assert!(
            !notes.iter().any(is_state),
            "no pause, no resume: {notes:?}"
        );
        p.finish();
    }

    #[test]
    fn phones_capture_at_most_720p() {
        assert_eq!(camera_box(1920, 1080, true), (1280, 720));
        assert_eq!(camera_box(1080, 1920, true), (720, 1280));
        assert_eq!(camera_box(640, 360, true), (640, 360));
        assert_eq!(camera_box(1920, 1080, false), (1920, 1080));
    }

    #[test]
    fn encoders_open_from_the_top_and_drop_layers_at_the_instance_limit() {
        let mut cfg = kessel_video::fake::FakeConfig::default();
        cfg.max_instances = Some(2);
        let backend: Arc<dyn VideoBackend> = kessel_video::fake::FakeBackend::new(cfg);
        let mut cap = caps(&[kessel_video::VideoCodec::H264]).codecs.remove(0);
        let plan = vec![
            LayerSpec::new("q", 320, 180, 150, 30),
            LayerSpec::new("h", 640, 360, 500, 30),
            LayerSpec::new("f", 1280, 720, 1700, 30),
        ];
        let wake = kessel_video::no_wake();
        // The backend's limit: f opens, h opens, q fails: drop f and try again (q, h).
        let encs = open_encoders(
            &backend,
            VideoCodec::H264,
            &cap,
            plan.clone(),
            ContentHint::Camera,
            &wake,
        );
        let rids: Vec<&str> = encs.iter().map(|e| e.spec.rid.as_str()).collect();
        assert_eq!(rids, vec!["q", "h"]);
        drop(encs);
        // The capability's limit cuts the plan first.
        cap.max_instances = Some(1);
        let encs = open_encoders(
            &backend,
            VideoCodec::H264,
            &cap,
            plan,
            ContentHint::Camera,
            &wake,
        );
        assert_eq!(encs.len(), 1);
        assert_eq!(encs[0].spec.rid, "q");
    }

    #[test]
    fn the_self_view_layer_is_the_cameras_h_and_the_screens_q() {
        let q = LayerSpec::new("q", 320, 180, 150, 30);
        let h = LayerSpec::new("h", 640, 360, 500, 30);
        let f = LayerSpec::new("f", 1280, 720, 1700, 30);
        let cam = VideoSource::Camera;
        let scr = VideoSource::ScreenShare;
        let all = vec![q.clone(), h.clone(), f.clone()];
        assert_eq!(self_view_rid(cam, &all).as_deref(), Some("h"));
        assert_eq!(self_view_rid(scr, &all).as_deref(), Some("q"));
        // No h: the camera's lowest layer at 360p or more; the screen's lowest.
        let qf = vec![q.clone(), f.clone()];
        assert_eq!(self_view_rid(cam, &qf).as_deref(), Some("f"));
        let hf = vec![h.clone(), f];
        assert_eq!(self_view_rid(scr, &hf).as_deref(), Some("h"));
        // A camera below 360p: its top layer.
        assert_eq!(self_view_rid(cam, &[q]).as_deref(), Some("q"));
        assert_eq!(self_view_rid(cam, &[]), None);
    }

    #[test]
    fn capture_refusals_keep_the_publish_codes() {
        for (e, code) in [
            (CaptureError::CameraDenied, "camera_denied"),
            (CaptureError::CameraBusy, "camera_busy"),
            (CaptureError::CameraNotFound, "camera_not_found"),
            (CaptureError::CameraLost, "camera_not_found"),
            (CaptureError::CodecUnavailable, "codec_unavailable"),
            (CaptureError::Failed(KError::DeviceBusy), "camera_busy"),
            (CaptureError::VideoUnsupported, "publish_failed"),
        ] {
            assert_eq!(capture_error(&e).code(), code, "{e:?}");
        }
    }
}
