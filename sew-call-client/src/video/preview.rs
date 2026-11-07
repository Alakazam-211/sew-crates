//! The pre-join camera preview (feature `video-core`): the camera outside a call, one 360p
//! encoder (VP8, kessel-video's pure-Rust `vp8-encoder`; H.264 only on a backend without VP8),
//! no lease and nothing sent. Its encoded **plaintext** frames are for this device's own
//! self-view sink (`local:camera`) and never leave the process.
//!
//! The camera opens only on [`VideoCapture::preview`] and closes the moment the [`Preview`] is
//! stopped or dropped (the light goes off then, not when a task gets round to it).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use bytes::Bytes;
use kessel_video::{
    Capabilities, CaptureConstraints, CodecConfig, CodecDirection, EncodeOptions, EncoderConfig,
    Facing, VideoBackend, VideoEncoder, VideoError,
};
use tokio::sync::{Notify, mpsc};

use super::capture::{CameraSource, CaptureEnd, CaptureError, CaptureEvent, VideoCapture};
use super::types::VideoCodec;

/// The preview's largest coded size (the camera's aspect ratio is kept inside it).
pub const PREVIEW_WIDTH: u32 = 640;
/// The preview's largest coded height.
pub const PREVIEW_HEIGHT: u32 = 360;
/// The preview's frame rate.
pub const PREVIEW_FPS: u32 = 30;
/// The preview encoder's bitrate (it is never sent; enough for a clean self-view).
pub const PREVIEW_BITRATE_BPS: u32 = 800_000;
/// Items the preview's channel holds; when the reader falls behind, the camera's own 2-deep
/// queue drops raw frames (never an encoded one, so the stream stays decodable).
pub const PREVIEW_QUEUE: usize = 8;
/// Encoder failures in a row before the preview gives up (`encoder_lost`).
const MAX_ENCODER_FAILURES: u32 = 3;

/// One encoded plaintext frame of this device's own camera or screen (self-view).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LocalFrame {
    /// The codec.
    pub codec: VideoCodec,
    /// A raw VP8 frame, or one H.264 Annex B access unit with 4-byte start codes.
    pub data: Bytes,
    /// A keyframe.
    pub keyframe: bool,
    /// The capture time (kessel-video's clock, µs).
    pub capture_us: u64,
    /// The coded width.
    pub width: u32,
    /// The coded height.
    pub height: u32,
    /// Which way the camera that captured it faces (a host mirrors a front camera's
    /// self-view frame by frame: a switch lands on the new camera's first frame); `Unknown`
    /// for a screen.
    pub facing: Facing,
}

/// What the preview's channel carries.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PreviewItem {
    /// An encoded frame for the self-view sink. The first is a keyframe.
    Frame(LocalFrame),
    /// The camera ended (unplugged, permission revoked, the backend shut down): the preview is
    /// over.
    Ended(CaptureEnd),
    /// The preview failed (`encoder_lost`) and is over.
    Failed(CaptureError),
}

/// The preview's frames.
pub type PreviewFrames = mpsc::Receiver<PreviewItem>;

/// The codec the preview encodes with on these capabilities: VP8 when the backend has a VP8
/// encoder (every backend with kessel-video's `vp8`), else H.264, else none.
pub fn preview_codec(caps: &Capabilities) -> Option<VideoCodec> {
    if caps
        .best(kessel_video::VideoCodec::Vp8, CodecDirection::Encode)
        .is_some()
    {
        Some(VideoCodec::Vp8)
    } else if caps
        .best(kessel_video::VideoCodec::H264, CodecDirection::Encode)
        .is_some()
    {
        Some(VideoCodec::H264)
    } else {
        None
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// A running preview. Dropping it (or [`Preview::stop`]) closes the camera at once.
pub struct Preview {
    shared: Arc<Shared>,
    codec: VideoCodec,
    device: Option<String>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl std::fmt::Debug for Preview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Preview")
            .field("codec", &self.codec)
            .field("device", &self.device)
            .field("running", &self.is_running())
            .finish()
    }
}

pub(crate) struct Shared {
    state: Mutex<Option<State>>,
    keyframe: AtomicBool,
    ready: Arc<Notify>,
}

impl Shared {
    /// Closes the camera and the encoder now; the channel ends.
    pub(crate) fn stop(&self) {
        let state = lock(&self.state).take();
        drop(state);
        self.ready.notify_one();
    }

    pub(crate) fn running(&self) -> bool {
        lock(&self.state).is_some()
    }
}

struct State {
    camera: CameraSource,
    backend: Arc<dyn VideoBackend>,
    codec: VideoCodec,
    facing: Facing,
    encoder: Option<Box<dyn VideoEncoder>>,
    /// The encoder's coded size.
    size: (u32, u32),
    failures: u32,
}

impl Preview {
    /// The codec of its frames.
    pub fn codec(&self) -> VideoCodec {
        self.codec
    }

    /// The camera it was opened on (`None`: the default camera).
    pub fn device(&self) -> Option<&str> {
        self.device.as_deref()
    }

    /// The next frame is a keyframe (a self-view sink opened, became visible or failed).
    /// Never blocks.
    pub fn request_keyframe(&self) {
        self.shared.keyframe.store(true, Ordering::Release);
        self.shared.ready.notify_one();
    }

    /// The camera and the encoder are open.
    pub fn is_running(&self) -> bool {
        self.shared.running()
    }

    /// Closes the camera and the encoder now; the channel ends.
    pub fn stop(&self) {
        self.shared.stop();
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        self.stop();
        if let Some(t) = self.task.take() {
            t.abort();
        }
    }
}

impl VideoCapture {
    /// The camera outside a call (`device`: `None` for the default): the permission (the OS
    /// prompt only when not yet determined), the camera at up to 640x360 and 30 fps, and one
    /// encoder at that size (the camera's aspect ratio kept). Its frames come on the returned
    /// channel; nothing is sent anywhere and no lease is taken. Needs a tokio runtime. One
    /// opener holds the camera: a running preview stops first, and a camera opened later
    /// through this `VideoCapture` (the bridge's camera publish) stops this one.
    pub async fn preview(
        &self,
        device: Option<&str>,
    ) -> Result<(Preview, PreviewFrames), CaptureError> {
        let caps = self.capabilities();
        if !caps.camera {
            return Err(CaptureError::VideoUnsupported);
        }
        let codec = preview_codec(&caps).ok_or(CaptureError::CodecUnavailable)?;
        let mut c = CaptureConstraints::new(PREVIEW_WIDTH, PREVIEW_HEIGHT, PREVIEW_FPS);
        // The pure-Rust VP8 encoder takes CPU planes; an OS H.264 encoder takes its own buffers.
        c.prefer_native = codec == VideoCodec::H264;
        let camera = self.open_camera(device, c).await?;
        let facing = self.camera_facing(device);
        let ready = camera.notifier();
        let shared = Arc::new(Shared {
            state: Mutex::new(Some(State {
                camera,
                backend: Arc::clone(self.backend()),
                codec,
                facing,
                encoder: None,
                size: (0, 0),
                failures: 0,
            })),
            keyframe: AtomicBool::new(false),
            ready,
        });
        self.set_preview(&shared);
        let (tx, rx) = mpsc::channel(PREVIEW_QUEUE);
        let task = tokio::spawn(run(Arc::clone(&shared), tx));
        Ok((
            Preview {
                shared,
                codec,
                device: device.map(str::to_owned),
                task: Some(task),
            },
            rx,
        ))
    }
}

/// Camera → encoder → channel, until the preview stops, the camera ends or the reader goes.
async fn run(shared: Arc<Shared>, tx: mpsc::Sender<PreviewItem>) {
    loop {
        // A wake from the camera or the encoder; the timeout only guards a lost wake.
        let _ = tokio::time::timeout(Duration::from_millis(250), shared.ready.notified()).await;
        let key = shared.keyframe.swap(false, Ordering::AcqRel);
        let (items, over) = {
            let mut g = lock(&shared.state);
            let Some(st) = g.as_mut() else { return };
            st.step(key)
        };
        for item in items {
            if tx.send(item).await.is_err() {
                lock(&shared.state).take();
                return;
            }
        }
        if over {
            lock(&shared.state).take();
            return;
        }
    }
}

impl State {
    fn open_encoder(&mut self, w: u32, h: u32) -> Result<(), VideoError> {
        let mut cfg = match self.codec {
            VideoCodec::H264 => EncoderConfig::h264(w, h, PREVIEW_FPS, PREVIEW_BITRATE_BPS),
            _ => EncoderConfig::new(CodecConfig::Vp8, w, h, PREVIEW_FPS, PREVIEW_BITRATE_BPS),
        };
        cfg.content = self.camera.content_hint();
        self.encoder = Some(self.backend.encoder(cfg, self.camera.wake())?);
        self.size = (w, h);
        Ok(())
    }

    /// One pass: the camera's events and frames into the encoder, its outputs out. Answers the
    /// items to send and whether the preview is over.
    fn step(&mut self, keyframe: bool) -> (Vec<PreviewItem>, bool) {
        let mut out = Vec::new();
        let mut keyframe = keyframe;
        while let Some(e) = self.camera.try_event() {
            match e {
                CaptureEvent::Ended(end) => {
                    out.push(PreviewItem::Ended(end));
                    return (out, true);
                }
                CaptureEvent::Resumed => keyframe = true,
                _ => {}
            }
        }
        if keyframe && let Some(enc) = self.encoder.as_mut() {
            enc.request_keyframe();
        }
        while let Some(frame) = self.camera.try_frame() {
            let fit =
                kessel_video::fit_within(frame.width, frame.height, PREVIEW_WIDTH, PREVIEW_HEIGHT);
            if self.encoder.is_some() && self.size != fit {
                let resized = self.encoder.as_mut().map(|e| e.set_size(fit.0, fit.1));
                match resized {
                    Some(Ok(())) => self.size = fit,
                    _ => self.encoder = None,
                }
            }
            if self.encoder.is_none() && self.open_encoder(fit.0, fit.1).is_err() {
                self.failures += 1;
                break;
            }
            let Some(enc) = self.encoder.as_mut() else {
                break;
            };
            match enc.submit(&frame, EncodeOptions::default()) {
                Ok(()) | Err(VideoError::Busy) => {}
                Err(_) => {
                    self.encoder = None;
                    self.failures += 1;
                }
            }
        }
        if let Some(enc) = self.encoder.as_mut() {
            while let Some(r) = enc.poll() {
                match r {
                    Ok(f) => {
                        self.failures = 0;
                        out.push(PreviewItem::Frame(LocalFrame {
                            codec: self.codec,
                            data: f.data,
                            keyframe: f.keyframe,
                            capture_us: f.capture_us,
                            width: f.width,
                            height: f.height,
                            facing: self.facing,
                        }));
                    }
                    Err(_) => {
                        // A dead encoder: a new one opens on the next frame (a keyframe first).
                        self.encoder = None;
                        self.failures += 1;
                        break;
                    }
                }
            }
        }
        if self.failures >= MAX_ENCODER_FAILURES {
            out.push(PreviewItem::Failed(CaptureError::EncoderLost));
            return (out, true);
        }
        (out, false)
    }
}
