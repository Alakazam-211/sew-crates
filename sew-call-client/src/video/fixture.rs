//! `FixtureBackend` (features `testing` and `video-core`): a kessel-video backend whose encoders
//! replay real pre-encoded simulcast streams, so what the native side publishes in a test is
//! what a browser decodes.
//!
//! - **VP8** (libvpx, three temporal layers 0-2-1-2, a keyframe every 28 frames) and **H.264**
//!   (x264 Constrained Baseline 3.1, SPS and PPS before every IDR, a keyframe every 30), each at
//!   320×180, 640×360 and 1280×720, 300 frames at 30 fps, frame-aligned across the three sizes;
//!   the files are `vp8_sim_{q,h,f}.ivf` and `h264_sim_{q,h,f}.h264` in the directory given to
//!   [`FixtureBackend::load`].
//! - An encoder plays the stream of its codec whose height is nearest the asked one, one frame
//!   per `submit` (looping); `request_keyframe` (or a keyframe option, or `set_size`) jumps to
//!   the stream's next keyframe. Outputs keep the input's `capture_us`; the pixels are ignored.
//!   VP8 frames report their temporal layer when the encoder was asked for three.
//! - One camera, which delivers blank frames at the asked size and rate. No decoders, no
//!   screens.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kessel_video::{
    Bytes, CameraInfo, Capabilities, CaptureConstraints, CaptureMode, CodecCapability,
    CodecDirection, DecoderConfig, Done, EncodeOptions, EncodedFrame, EncoderConfig, EncoderInfo,
    EncoderStats, EventHub, EventRx, Facing, FrameSource, H264Capability, H264Level, H264Profile,
    Implementation, LayerInfo, Permission, PickOptions, PixelFormat, Rates, ScreenSelection,
    ScreenSourceInfo, SourceEvent, SourceStats, VideoBackend, VideoCodec, VideoDecoder,
    VideoEncoder, VideoError, VideoFrame, Wake,
};

use super::container::{h264_access_units, ivf_frames};
use super::rtp::vp8::is_keyframe;

/// Why the fixtures could not be loaded.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum FixtureError {
    /// A file could not be read.
    #[error("{path}: {source}")]
    Io {
        /// The file.
        path: PathBuf,
        /// The error.
        source: std::io::Error,
    },
    /// A file did not parse as the stream it should hold.
    #[error("{file}: {detail}")]
    Parse {
        /// The file name.
        file: String,
        /// What was wrong.
        detail: String,
    },
}

/// The sizes of the q, h and f streams.
const LAYERS: [(&str, u32, u32); 3] = [("q", 320, 180), ("h", 640, 360), ("f", 1280, 720)];
/// The fixtures' temporal pattern (VP8).
const VP8_PATTERN: [u8; 4] = [0, 2, 1, 2];
/// Encoded frames queued per encoder before `submit` answers `Busy`.
const ENCODER_QUEUE: usize = 8;
const FPS: u32 = 30;

#[derive(Debug)]
struct Stream {
    codec: VideoCodec,
    file: String,
    width: u32,
    height: u32,
    /// (keyframe, bytes) per frame.
    frames: Vec<(bool, Bytes)>,
}

impl Stream {
    /// The keyframe at or after `i` (wrapping).
    fn next_key(&self, i: usize) -> usize {
        let n = self.frames.len();
        (0..n)
            .map(|k| (i + k) % n)
            .find(|&k| self.frames[k].0)
            .unwrap_or(0)
    }
}

/// A backend replaying the fixture streams (see the module docs).
pub struct FixtureBackend {
    streams: Vec<Arc<Stream>>,
    events: EventHub,
    shut: Arc<AtomicBool>,
}

impl std::fmt::Debug for FixtureBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FixtureBackend")
            .field("streams", &self.streams.len())
            .finish_non_exhaustive()
    }
}

fn read(dir: &Path, file: &str) -> Result<Vec<u8>, FixtureError> {
    let path = dir.join(file);
    std::fs::read(&path).map_err(|source| FixtureError::Io { path, source })
}

impl FixtureBackend {
    /// Loads the six streams from `dir`.
    pub fn load(dir: impl AsRef<Path>) -> Result<Arc<FixtureBackend>, FixtureError> {
        let dir = dir.as_ref();
        let mut streams = Vec::new();
        for (rid, width, height) in LAYERS {
            let file = format!("vp8_sim_{rid}.ivf");
            let frames: Vec<(bool, Bytes)> = ivf_frames(&read(dir, &file)?)
                .map_err(|detail| FixtureError::Parse {
                    file: file.clone(),
                    detail,
                })?
                .into_iter()
                .map(|f| (is_keyframe(&f), Bytes::from(f)))
                .collect();
            streams.push((VideoCodec::Vp8, file, width, height, frames));
            let file = format!("h264_sim_{rid}.h264");
            let frames: Vec<(bool, Bytes)> = h264_access_units(&read(dir, &file)?)
                .into_iter()
                .map(|(key, au)| (key, Bytes::from(au)))
                .collect();
            streams.push((VideoCodec::H264, file, width, height, frames));
        }
        let mut out = Vec::new();
        for (codec, file, width, height, frames) in streams {
            if frames.is_empty() || !frames[0].0 {
                return Err(FixtureError::Parse {
                    file,
                    detail: "no frames, or the first is not a keyframe".into(),
                });
            }
            out.push(Arc::new(Stream {
                codec,
                file,
                width,
                height,
                frames,
            }));
        }
        Ok(Arc::new(FixtureBackend {
            streams: out,
            events: EventHub::default(),
            shut: Arc::new(AtomicBool::new(false)),
        }))
    }

    fn live(&self) -> Result<(), VideoError> {
        if self.shut.load(Ordering::SeqCst) {
            Err(VideoError::ShutDown)
        } else {
            Ok(())
        }
    }

    /// The stream of `codec` whose height is nearest `height`.
    fn stream(&self, codec: VideoCodec, height: u32) -> Option<Arc<Stream>> {
        self.streams
            .iter()
            .filter(|s| s.codec == codec)
            .min_by_key(|s| s.height.abs_diff(height))
            .cloned()
    }

    fn camera() -> CameraInfo {
        let mut cam = CameraInfo::new("fixture:camera", "Fixture camera", Facing::External, true);
        cam.modes = vec![CaptureMode::new(1280, 720, FPS, PixelFormat::I420)];
        cam
    }
}

impl VideoBackend for FixtureBackend {
    fn capabilities(&self) -> Capabilities {
        let mut caps = Capabilities::new("fixture", "fixture");
        caps.camera = true;
        let formats = vec![
            PixelFormat::I420,
            PixelFormat::Nv12,
            PixelFormat::Bgra,
            PixelFormat::Rgba,
        ];
        let mut vp8 = CodecCapability::new(
            VideoCodec::Vp8,
            CodecDirection::Encode,
            Implementation::Software,
            "fixture VP8 (libvpx L1T3)",
            1280,
            720,
            FPS,
        );
        vp8.max_temporal_layers = 3;
        vp8.formats = formats.clone();
        let mut h264 = CodecCapability::new(
            VideoCodec::H264,
            CodecDirection::Encode,
            Implementation::Software,
            "fixture H.264 (x264 Constrained Baseline)",
            1280,
            720,
            FPS,
        );
        h264.h264 = Some(H264Capability::new(
            vec![H264Profile::ConstrainedBaseline],
            H264Level(31),
        ));
        h264.formats = formats;
        caps.codecs = vec![vp8, h264];
        caps
    }

    fn now_us(&self) -> u64 {
        kessel_video::now_us()
    }

    fn cameras(&self) -> Vec<CameraInfo> {
        vec![FixtureBackend::camera()]
    }

    fn camera_permission(&self) -> Permission {
        Permission::Granted
    }

    fn request_camera_permission(&self, done: Done<Permission>) {
        done(Permission::Granted);
    }

    fn open_camera(
        &self,
        id: Option<&str>,
        constraints: CaptureConstraints,
        wake: Wake,
    ) -> Result<Box<dyn FrameSource>, VideoError> {
        self.live()?;
        if id.is_some_and(|id| id != FixtureBackend::camera().id) {
            return Err(VideoError::NotFound);
        }
        Ok(Box::new(BlankSource::open(constraints, wake)?))
    }

    fn screen_permission(&self) -> Permission {
        Permission::Denied
    }

    fn request_screen_permission(&self, done: Done<Permission>) {
        done(Permission::Denied);
    }

    fn screen_sources(&self) -> Result<Vec<ScreenSourceInfo>, VideoError> {
        Ok(Vec::new())
    }

    fn pick_screen(&self, _opts: PickOptions, done: Done<Result<ScreenSelection, VideoError>>) {
        done(Err(VideoError::NotFound));
    }

    fn open_screen(
        &self,
        _source: &ScreenSelection,
        _constraints: CaptureConstraints,
        _wake: Wake,
    ) -> Result<Box<dyn FrameSource>, VideoError> {
        Err(VideoError::Unsupported(
            "the fixture backend has no screens".into(),
        ))
    }

    fn encoder(
        &self,
        config: EncoderConfig,
        wake: Wake,
    ) -> Result<Box<dyn VideoEncoder>, VideoError> {
        self.live()?;
        config.validate()?;
        let codec = config.codec.codec();
        let stream = self
            .stream(codec, config.height)
            .ok_or_else(|| VideoError::Unsupported(format!("no {codec:?} fixture")))?;
        Ok(Box::new(FixtureEncoder {
            streams: self
                .streams
                .iter()
                .filter(|s| s.codec == codec)
                .cloned()
                .collect(),
            stream,
            config,
            wake,
            next: 0,
            want_key: true,
            out: VecDeque::new(),
            stats: EncoderStats::default(),
            shut: self.shut.clone(),
        }))
    }

    fn decoder(
        &self,
        _config: DecoderConfig,
        _wake: Wake,
    ) -> Result<Box<dyn VideoDecoder>, VideoError> {
        Err(VideoError::Unsupported(
            "the fixture backend has no decoders".into(),
        ))
    }

    fn convert(
        &self,
        frame: &VideoFrame,
        width: u32,
        height: u32,
        format: PixelFormat,
    ) -> Result<VideoFrame, VideoError> {
        kessel_video::convert(frame, width, height, format)
    }

    fn subscribe(&self) -> EventRx {
        self.events.subscribe()
    }

    fn shutdown(&self) {
        self.shut.store(true, Ordering::SeqCst);
    }
}

struct FixtureEncoder {
    /// This codec's streams (for `set_size`).
    streams: Vec<Arc<Stream>>,
    stream: Arc<Stream>,
    config: EncoderConfig,
    wake: Wake,
    next: usize,
    want_key: bool,
    out: VecDeque<EncodedFrame>,
    stats: EncoderStats,
    shut: Arc<AtomicBool>,
}

impl VideoEncoder for FixtureEncoder {
    fn submit(&mut self, frame: &VideoFrame, opts: EncodeOptions) -> Result<(), VideoError> {
        if self.shut.load(Ordering::SeqCst) {
            return Err(VideoError::ShutDown);
        }
        if self.out.len() >= ENCODER_QUEUE {
            self.stats.busy += 1;
            return Err(VideoError::Busy);
        }
        self.stats.frames_in += 1;
        if std::mem::take(&mut self.want_key) || opts.keyframe {
            self.next = self.stream.next_key(self.next);
        }
        let s = &self.stream;
        let i = self.next % s.frames.len();
        self.next = (i + 1) % s.frames.len();
        let (key, data) = s.frames[i].clone();
        let mut f = EncodedFrame::new(s.codec, data, key, frame.capture_us, s.width, s.height);
        if s.codec == VideoCodec::Vp8 && self.config.temporal_layers >= 3 && !key {
            let tid = VP8_PATTERN[i % 4];
            f.layer = LayerInfo::new(tid, tid > 0 && matches!(i % 4, 1 | 2));
        }
        self.stats.frames_out += 1;
        self.stats.keyframes += u64::from(key);
        self.stats.bytes_out += f.data.len() as u64;
        self.out.push_back(f);
        (self.wake)();
        Ok(())
    }

    fn poll(&mut self) -> Option<Result<EncodedFrame, VideoError>> {
        self.out.pop_front().map(Ok)
    }

    fn request_keyframe(&mut self) {
        self.want_key = true;
    }

    fn set_rates(&mut self, rates: Rates) -> Result<(), VideoError> {
        self.config.bitrate_bps = rates.bitrate_bps;
        self.config.fps = rates.fps;
        Ok(())
    }

    fn set_size(&mut self, width: u32, height: u32) -> Result<(), VideoError> {
        let s = self
            .streams
            .iter()
            .min_by_key(|s| s.height.abs_diff(height))
            .cloned()
            .ok_or_else(|| VideoError::Unsupported("no fixture stream".into()))?;
        // The streams are frame-aligned: the position carries over.
        self.stream = s;
        self.config.width = width;
        self.config.height = height;
        self.want_key = true;
        Ok(())
    }

    fn info(&self) -> EncoderInfo {
        EncoderInfo::new(
            Implementation::Software,
            format!("fixture {}", self.stream.file),
            self.config,
        )
    }

    fn stats(&self) -> EncoderStats {
        self.stats
    }
}

/// A camera of blank frames at the asked size and rate (the encoders ignore the pixels).
struct BlankSource {
    frame: VideoFrame,
    fps: u32,
    next_due_us: u64,
    paused: bool,
    events: VecDeque<SourceEvent>,
    stats: SourceStats,
    fps_shared: Arc<Mutex<u32>>,
    alive: Arc<AtomicBool>,
}

impl BlankSource {
    fn open(c: CaptureConstraints, wake: Wake) -> Result<BlankSource, VideoError> {
        let width = c.max_width.unwrap_or(1280).min(1280) & !1;
        let height = c.max_height.unwrap_or(720).min(720) & !1;
        let fps = c.fps.unwrap_or(FPS).clamp(1, FPS);
        let frame = VideoFrame::filled(PixelFormat::I420, width, height, [16, 128, 128], 0)?;
        let alive = Arc::new(AtomicBool::new(true));
        let fps_shared = Arc::new(Mutex::new(fps));
        let (ticker, rate) = (alive.clone(), fps_shared.clone());
        std::thread::Builder::new()
            .name("sew-fixture-camera".into())
            .spawn(move || {
                while ticker.load(Ordering::SeqCst) {
                    let fps = *rate.lock().unwrap_or_else(PoisonError::into_inner);
                    std::thread::sleep(Duration::from_micros(1_000_000 / u64::from(fps.max(1))));
                    wake();
                }
            })
            .map_err(|e| VideoError::Os {
                code: 0,
                detail: format!("spawn: {e}"),
            })?;
        let mut stats = SourceStats::default();
        set_shape(&mut stats, width, height, fps);
        Ok(BlankSource {
            frame,
            fps,
            next_due_us: kessel_video::now_us(),
            paused: false,
            events: VecDeque::from([SourceEvent::Started { width, height, fps }]),
            stats,
            fps_shared,
            alive,
        })
    }
}

fn set_shape(stats: &mut SourceStats, width: u32, height: u32, fps: u32) {
    stats.width = width;
    stats.height = height;
    stats.fps = fps;
}

impl Drop for BlankSource {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
    }
}

impl FrameSource for BlankSource {
    fn try_recv(&mut self) -> Option<VideoFrame> {
        let now = kessel_video::now_us();
        if self.paused || now < self.next_due_us {
            return None;
        }
        let step = 1_000_000 / u64::from(self.fps.max(1));
        // A consumer that fell behind gets the newest frame.
        let behind = (now - self.next_due_us) / step;
        self.stats.dropped += behind;
        self.next_due_us += (behind + 1) * step;
        self.stats.frames += 1;
        let mut f = self.frame.clone();
        f.capture_us = now;
        Some(f)
    }

    fn try_event(&mut self) -> Option<SourceEvent> {
        self.events.pop_front()
    }

    fn set_constraints(&mut self, c: CaptureConstraints) -> Result<(), VideoError> {
        let width = c.max_width.unwrap_or(self.frame.width).min(1280) & !1;
        let height = c.max_height.unwrap_or(self.frame.height).min(720) & !1;
        self.fps = c.fps.unwrap_or(self.fps).clamp(1, FPS);
        *self
            .fps_shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = self.fps;
        self.frame = VideoFrame::filled(PixelFormat::I420, width, height, [16, 128, 128], 0)?;
        set_shape(&mut self.stats, width, height, self.fps);
        self.events.push_back(SourceEvent::Started {
            width,
            height,
            fps: self.fps,
        });
        Ok(())
    }

    fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    fn stats(&self) -> SourceStats {
        self.stats
    }
}

// They read the fixture bitstreams: only in Sew's tree (`sew_fixtures`, build.rs).
#[cfg(test)]
#[cfg(sew_fixtures)]
mod tests {
    use super::*;
    use kessel_video::no_wake;

    fn backend() -> Arc<FixtureBackend> {
        FixtureBackend::load(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../sew-media-testclient/fixtures"
        ))
        .expect("fixtures")
    }

    fn frame(capture_us: u64) -> VideoFrame {
        VideoFrame::filled(PixelFormat::I420, 64, 36, [16, 128, 128], capture_us).expect("frame")
    }

    #[test]
    fn vp8_replays_the_layers_with_their_temporal_ids() {
        let b = backend();
        let mut cfg = EncoderConfig::new(kessel_video::CodecConfig::Vp8, 640, 360, 30, 500_000);
        cfg.temporal_layers = 3;
        let mut e = b.encoder(cfg, no_wake()).expect("encoder");
        let mut tids = Vec::new();
        for i in 0..8u64 {
            e.submit(&frame(i * 33_333), EncodeOptions::default())
                .expect("submit");
            let f = e.poll().expect("output").expect("frame");
            assert_eq!((f.codec, f.width, f.height), (VideoCodec::Vp8, 640, 360));
            assert_eq!(f.capture_us, i * 33_333);
            assert_eq!(f.keyframe, i == 0);
            assert_eq!(f.keyframe, is_keyframe(&f.data));
            tids.push(f.layer.temporal_id);
        }
        assert_eq!(tids, [0, 2, 1, 2, 0, 2, 1, 2]);
        // A keyframe on request: the stream's next one (frame 28).
        e.request_keyframe();
        e.submit(&frame(9), EncodeOptions::default())
            .expect("submit");
        let f = e.poll().expect("output").expect("frame");
        assert!(f.keyframe);
        // A layer switch keeps the position and starts on a keyframe.
        e.set_size(1280, 720).expect("size");
        e.submit(&frame(10), EncodeOptions::default())
            .expect("submit");
        let f = e.poll().expect("output").expect("frame");
        assert!(f.keyframe && f.width == 1280);
    }

    #[test]
    fn h264_replays_normal_access_units_and_answers_busy() {
        use crate::video::rtp::normalise::{check, normalise};
        let b = backend();
        let mut e = b
            .encoder(EncoderConfig::h264(320, 180, 30, 150_000), no_wake())
            .expect("encoder");
        for i in 0..ENCODER_QUEUE as u64 {
            e.submit(&frame(i), EncodeOptions::default())
                .expect("submit");
        }
        assert_eq!(
            e.submit(&frame(99), EncodeOptions::default()),
            Err(VideoError::Busy)
        );
        let mut n = 0;
        while let Some(f) = e.poll() {
            let f = f.expect("frame");
            assert_eq!(normalise(&f.data), f.data);
            check(&f.data).expect("SPS and PPS before each IDR");
            assert_eq!((f.width, f.height), (320, 180));
            n += 1;
        }
        assert_eq!(n, ENCODER_QUEUE);
        assert_eq!(e.stats().busy, 1);
        assert!(
            b.decoder(DecoderConfig::new(VideoCodec::H264), no_wake())
                .is_err()
        );
        b.shutdown();
        assert_eq!(
            e.submit(&frame(100), EncodeOptions::default()),
            Err(VideoError::ShutDown)
        );
    }

    #[test]
    fn the_camera_paces_blank_frames() {
        let b = backend();
        let mut cam = b
            .open_camera(None, CaptureConstraints::new(640, 360, 30), no_wake())
            .expect("camera");
        assert!(matches!(
            cam.try_event(),
            Some(SourceEvent::Started {
                width: 640,
                height: 360,
                fps: 30
            })
        ));
        let mut got = 0;
        let until = std::time::Instant::now() + Duration::from_millis(400);
        while std::time::Instant::now() < until {
            if let Some(f) = cam.try_recv() {
                assert_eq!((f.width, f.height), (640, 360));
                got += 1;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!((8..=14).contains(&got), "{got} frames in 400 ms at 30 fps");
    }
}
