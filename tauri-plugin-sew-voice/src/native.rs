//! Native video sinks (feature `video-core`): a call's video in Rust, for the host's own views
//! (a phone's picture in picture: `AVSampleBufferDisplayLayer` and
//! `AVPictureInPictureController` on iOS), not over the webview bridge.
//!
//! [`crate::VoiceControl::open_native_sink`] opens one on a track, or on the **active
//! speaker** ([`NativeSinkTarget::ActiveSpeaker`]): the sink follows whoever speaks (their
//! camera, else their screen), asks the media server for the layer that fits its `max_size`
//! (about 360p for `640 x 360`), and asks for a keyframe on every switch. It is the one sink that
//! stays enabled while the app is in the background ([`crate::VoiceControl::app_scene`]).
//!
//! What the consumer gets ([`NativeVideoEvent`], on the sink's own thread, in order, never
//! under the plugin's locks):
//! - **H.264**: each access unit as it arrived (Annex B, 4-byte start codes, SPS and PPS
//!   before every IDR), for a display layer that decodes H.264 itself; [`h264_annex_b_to_avcc`]
//!   and [`h264_parameter_sets`] make the `CMSampleBuffer` and its format description;
//! - **VP8**: decoded frames, through the video backend's decoder (kessel-video's pure-Rust
//!   VP8), NV12 at most `max_size`, turned into an OS buffer (`PixelFormat::Native`: a
//!   `CVPixelBuffer` on Apple) where the backend can; without a video backend, VP8 tracks show
//!   nothing;
//! - a [`NativeVideoEvent::Reset`] when the track breaks or goes away, and a
//!   [`NativeVideoEvent::Switched`] when an active-speaker sink moves. The first frame after a
//!   reset, a switch, an open or a dropped frame is a keyframe with `discontinuity` set.
//!
//! Frames come decrypted and checked, as for the webview's sinks; the keys never leave the
//! engine. Dropping the [`NativeSink`] closes it.
//!
//! **Keeping up.** Each sink has its own thread (on Apple at the user-interactive QoS class,
//! set before the VP8 decoder's thread is started from it). A VP8 frame goes to the decoder
//! without blocking; the sink takes its output when the decoder's wake callback runs (a job on
//! the sink's channel), never on the next input or a timer. When the decoder's input queue (4)
//! is full (`Busy`), frames wait in order in the sink's backlog ([`DECODE_BACKLOG`]) and go in as
//! outputs make room, so a burst of frames is absorbed; a keyframe supersedes the deltas waiting
//! before it; a backlog overflow drops what waits and the sink skips to a keyframe (asked of the
//! sender) that goes out with `discontinuity`: no frame is ever dropped without that.
//! [`NativeSink::stats`] has the counters ([`NativeSinkStats`]: the decode time split at the
//! wake, the consumer's time, the queues, drops by reason) and the sink logs them every
//! [`STATS_LOG_INTERVAL`].
//!
//! **The step-down.** A sink that drops more than [`STEP_DOWN_DROPS`] frames within
//! [`STEP_DOWN_WINDOW`] (any reason) asks the media server for the low layer
//! ([`LOW_LAYER`], 180p) instead of the one its `max_size` fits; after [`STEP_UP_AFTER`] with no
//! drop and a decode time (moving average) under a third of the 30 fps frame interval, it asks
//! for its own layer again. A step-down within [`STEP_UP_AFTER`] of a step-up doubles the wait
//! before the next step-up (at most 8 times [`STEP_UP_AFTER`]).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::{Duration, Instant};

use sew_call_client::kessel_video::{
    self as kv, DecoderConfig, EncodedInput, PixelFormat, VideoBackend, VideoDecoder,
};

use crate::sink::{NativeTarget, Pushed};
use crate::video::{EncodedVideoFrame, VideoCodec};

/// What a native sink shows.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NativeSinkTarget {
    /// One remote track (a `video` entry's `track`).
    Track(String),
    /// Whoever speaks: their camera, else their screen; before anyone speaks, the first
    /// remote camera (else screen).
    ActiveSpeaker,
}

/// One frame for a native sink's consumer.
#[derive(Debug, Clone)]
pub struct NativeVideoFrame {
    /// The remote track's `track_sid`.
    pub track: String,
    /// A keyframe (an IDR for H.264).
    pub keyframe: bool,
    /// The first frame after an open, a reset, a switch or a dropped frame: flush the layer
    /// (`AVSampleBufferDisplayLayer.flush()`) before it.
    pub discontinuity: bool,
    /// Arrival time in µs (the engine's clock), for presentation timestamps.
    pub timestamp_us: u64,
    /// The RTP timestamp (90 kHz).
    pub rtp_ts: u32,
    /// The frame.
    pub payload: NativePayload,
}

/// A native frame's content.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum NativePayload {
    /// One H.264 access unit in Annex B with 4-byte start codes (SPS and PPS before every IDR).
    H264 {
        /// The access unit.
        access_unit: Vec<u8>,
        /// The coded width from a keyframe's SPS; `0` when the frame doesn't say.
        width: u32,
        /// The coded height; `0` when the frame doesn't say.
        height: u32,
    },
    /// A decoded frame (VP8): an OS buffer where the backend makes one, else NV12 planes; at
    /// most the sink's `max_size`, aspect kept.
    Decoded(kv::VideoFrame),
}

impl NativePayload {
    /// macOS and iOS (the plugin's `video`): the `CVPixelBufferRef` of a decoded frame whose
    /// buffer is the Apple backend's (`NativePayload::Decoded` with a native buffer), for a
    /// `CMSampleBuffer` on an `AVSampleBufferDisplayLayer`. `None` for H.264 access units and
    /// for CPU planes. kessel-video's `apple_pixel_buffer`: the pointer is borrowed and stays
    /// valid while this payload (or a clone of its frame) lives; `CVBufferRetain` it to keep it
    /// longer (and `CVBufferRelease` it then), and treat it as read-only.
    #[cfg(all(feature = "video", any(target_os = "macos", target_os = "ios")))]
    pub fn apple_pixel_buffer(&self) -> Option<*mut std::ffi::c_void> {
        match self {
            NativePayload::Decoded(frame) => match &frame.buffer {
                kv::FrameBuffer::Native(buffer) => kv::apple_pixel_buffer(buffer),
                _ => None,
            },
            _ => None,
        }
    }
}

/// What a native sink's consumer hears.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum NativeVideoEvent {
    /// A frame.
    Frame(NativeVideoFrame),
    /// The track broke (a gap, a decryption failure) or went away: drop what is queued; the
    /// next frame is a keyframe.
    Reset {
        /// The track.
        track: String,
    },
    /// An active-speaker sink moved to another track (`None`: no remote video to show). Frames
    /// of the new track start at a keyframe.
    Switched {
        /// The new track.
        track: Option<String>,
        /// Its participant's Sew member id.
        member_id: Option<String>,
    },
}

/// A native sink's consumer (the host's picture in picture). Called on the sink's own thread,
/// in order, one at a time, never under the plugin's locks; it may block briefly (a slow
/// consumer drops frames at the sink, which then waits for a keyframe), and it may call
/// [`NativeSink`]'s methods.
pub trait NativeVideoConsumer: Send + Sync + 'static {
    /// One event.
    fn on_video(&self, event: NativeVideoEvent);
}

impl<F: Fn(NativeVideoEvent) + Send + Sync + 'static> NativeVideoConsumer for F {
    fn on_video(&self, event: NativeVideoEvent) {
        self(event)
    }
}

/// Frames a native sink's queue holds before it drops (and waits for a keyframe).
pub const NATIVE_QUEUE: usize = 8;

/// An open native sink ([`crate::VoiceControl::open_native_sink`]). Dropping it closes it.
pub struct NativeSink {
    pub(crate) id: u32,
    pub(crate) control: Arc<dyn NativeSinkControl>,
    pub(crate) stats: NativeStats,
}

impl std::fmt::Debug for NativeSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeSink")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// The service side of a [`NativeSink`] (the plugin's `VoiceService`).
pub(crate) trait NativeSinkControl: Send + Sync + 'static {
    fn track(&self, id: u32) -> Option<String>;
    fn resize(&self, id: u32, width: u32, height: u32);
    fn keyframe(&self, id: u32);
    fn close(&self, id: u32);
}

impl NativeSink {
    /// The sink's id (a plugin-wide sink id, never a webview's).
    pub fn id(&self) -> u32 {
        self.id
    }

    /// The track it shows now (`None`: an active-speaker sink with nobody to show, or closed).
    pub fn track(&self) -> Option<String> {
        self.control.track(self.id).filter(|t| !t.is_empty())
    }

    /// The sink's counters now: frames in and out, decode and consumer times, the queue, drops
    /// by reason and the layer step-down ([`NativeSinkStats`]).
    pub fn stats(&self) -> NativeSinkStats {
        self.stats.get()
    }

    /// A new largest size (device pixels): the server's layer and the VP8 output follow.
    pub fn set_max_size(&self, width: u32, height: u32) {
        self.control.resize(self.id, width, height);
    }

    /// The consumer's decoder or layer failed: the sink waits for the next keyframe and the
    /// sender is asked for one (rate-limited).
    pub fn request_keyframe(&self) {
        self.control.keyframe(self.id);
    }

    /// Closes the sink (as dropping it does): with no sink left, its track is turned off at the
    /// media server again.
    pub fn close(self) {}
}

impl Drop for NativeSink {
    fn drop(&mut self) {
        self.control.close(self.id);
    }
}

// ───────────────────────────── stats and the step-down ─────────────────────────────

/// A native sink's counters ([`NativeSink::stats`]): cumulative since the open, except where a
/// field says otherwise. The sink also logs them (`tracing`, info) every
/// [`STATS_LOG_INTERVAL`] while it is open.
///
/// Drops by reason: `dropped_queue_full` (the sink's thread was [`NATIVE_QUEUE`] frames behind:
/// the consumer or the thread itself is slow), `dropped_busy` (more than [`DECODE_BACKLOG`]
/// frames waited for room in the decoder: the decoder is behind), `dropped_superseded` (waiting
/// deltas a keyframe made pointless), `dropped_decoder_output` (decoded frames the sink's thread
/// didn't take before the decoder's output queue of 2 overflowed) and `skipped_to_keyframe`
/// (deltas skipped after a drop or an error, until the keyframe it asked for).
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize)]
#[non_exhaustive]
pub struct NativeSinkStats {
    /// Frames the sink was handed (after the gating).
    pub frames_in: u64,
    /// Frames to the consumer (decoded VP8 frames and H.264 access units).
    pub frames_out: u64,
    /// Frames to the consumer per second over the last full log window.
    pub fps: f32,
    /// The last VP8 frame's decode time, µs: from its submit to its output reaching the sink's
    /// thread (queueing in the decoder included).
    pub decode_us: u32,
    /// The decode time's moving average (1/8 per frame), µs.
    pub decode_us_avg: u32,
    /// The largest decode time in the last full log window, µs.
    pub decode_us_peak: u32,
    /// The decode time's first part, moving average, µs: from the submit to the decoder's wake
    /// callback for the output (the decoder's queue and its decode; for an output the sink
    /// took right after a submit, before its wake, up to that take).
    pub submit_to_wake_us_avg: u32,
    /// Its largest in the last full log window, µs.
    pub submit_to_wake_us_peak: u32,
    /// The decode time's second part, moving average, µs: from the wake callback to the sink's
    /// thread taking the output (the thread's own wake-up and what it was doing).
    pub wake_to_drain_us_avg: u32,
    /// Its largest in the last full log window, µs.
    pub wake_to_drain_us_peak: u32,
    /// Inputs in the decoder now (submitted, no output yet) or waiting for room in it.
    pub decoder_in_flight: u32,
    /// The most there ever were (the decoder's input queue holds 4; the rest wait in the
    /// sink's backlog of [`DECODE_BACKLOG`]).
    pub decoder_in_flight_peak: u32,
    /// Inputs the decoder had no room for (`Busy`) that went in later, in order.
    pub busy_retried: u64,
    /// Delta frames waiting for room in the decoder that a keyframe made pointless (dropped
    /// without a keyframe request: the keyframe is there).
    pub dropped_superseded: u64,
    /// The OS-buffer conversion's moving average, µs (`0` where frames stay CPU planes).
    pub convert_us_avg: u32,
    /// The consumer's time per frame (`on_video`), moving average, µs.
    pub consumer_us_avg: u32,
    /// The consumer's largest time in the last full log window, µs.
    pub consumer_us_peak: u32,
    /// Frames waiting for the sink's thread now.
    pub queue_depth: u32,
    /// The most frames that ever waited for it.
    pub queue_peak: u32,
    /// Frames dropped because [`NATIVE_QUEUE`] frames already waited.
    pub dropped_queue_full: u64,
    /// Frames dropped because more than [`DECODE_BACKLOG`] waited for room in the decoder.
    pub dropped_busy: u64,
    /// Decoded frames the decoder dropped because the sink's thread didn't take them in time.
    pub dropped_decoder_output: u64,
    /// Delta frames skipped while the sink waited for a keyframe after a drop or an error.
    pub skipped_to_keyframe: u64,
    /// Decoder errors (each resets the consumer and asks for a keyframe).
    pub decode_errors: u64,
    /// Keyframes the sink asked the sender for.
    pub keyframes_asked: u64,
    /// Decoded frames handed over as CPU planes (NV12), where no OS buffer was made.
    pub cpu_planes: u64,
    /// The sink asks for the low layer now ([`LOW_LAYER`]).
    pub low_layer: bool,
    /// Times it stepped down to the low layer.
    pub step_downs: u64,
    /// Times it stepped back up.
    pub step_ups: u64,
}

/// VP8 frames a native sink keeps waiting, in order, while the decoder's own input queue (4) is
/// full; one more drops them and the sink skips to a keyframe.
pub const DECODE_BACKLOG: usize = 8;

/// More drops than this within [`STEP_DOWN_WINDOW`] step a native sink down to the low layer.
pub const STEP_DOWN_DROPS: usize = 3;
/// The window [`STEP_DOWN_DROPS`] counts in.
pub const STEP_DOWN_WINDOW: Duration = Duration::from_secs(2);
/// A stepped-down sink steps back up after this long with no drop and a decode time under a
/// third of the frame interval; each step-down within this long of a step-up doubles the wait
/// (up to 8 times), so a sink that can't hold the upper layer doesn't flap.
pub const STEP_UP_AFTER: Duration = Duration::from_secs(10);
/// How often an open native sink logs its [`NativeSinkStats`].
pub const STATS_LOG_INTERVAL: Duration = Duration::from_secs(10);
/// The box a stepped-down native sink asks the media server for (the 180p layer).
pub const LOW_LAYER: (u32, u32) = crate::sink::LOW_LAYER;

/// The step-down's and the log's timing (the constants above; shorter in tests).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Tuning {
    /// [`DECODE_BACKLOG`].
    pub(crate) backlog: usize,
    pub(crate) step_down_drops: usize,
    pub(crate) step_down_window: Duration,
    pub(crate) step_up_after: Duration,
    /// The frame interval at 30 fps: a decode under a third of it keeps up.
    pub(crate) frame_interval: Duration,
    pub(crate) log_every: Duration,
    /// How often the thread looks at the clock when no job comes.
    pub(crate) tick: Duration,
}

impl Tuning {
    pub(crate) const DEFAULT: Tuning = Tuning {
        backlog: DECODE_BACKLOG,
        step_down_drops: STEP_DOWN_DROPS,
        step_down_window: STEP_DOWN_WINDOW,
        step_up_after: STEP_UP_AFTER,
        frame_interval: Duration::from_micros(33_333),
        log_every: STATS_LOG_INTERVAL,
        tick: Duration::from_millis(500),
    };
}

/// What the router side and the sink's thread share.
struct Shared {
    /// Frames sent to the thread and not yet taken.
    queued: AtomicUsize,
    meter: Mutex<Meter>,
}

impl Shared {
    fn meter(&self) -> MutexGuard<'_, Meter> {
        self.meter.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Default)]
struct Meter {
    stats: NativeSinkStats,
    /// Recent drops (every reason), for the step-down.
    drops: VecDeque<Instant>,
    last_drop: Option<Instant>,
}

impl Meter {
    fn dropped(&mut self, now: Instant, n: u64) {
        for _ in 0..n.min(64) {
            self.drops.push_back(now);
        }
        self.last_drop = Some(now);
    }
}

/// A native sink's counters, for [`NativeSink::stats`].
#[derive(Clone)]
pub(crate) struct NativeStats(Arc<Shared>);

impl NativeStats {
    pub(crate) fn get(&self) -> NativeSinkStats {
        let mut s = self.0.meter().stats;
        s.queue_depth = u32::try_from(self.0.queued.load(Ordering::Acquire)).unwrap_or(u32::MAX);
        s
    }
}

/// What the sink's thread asks of the service.
pub(crate) struct Hooks {
    /// The sender is asked for a keyframe (rate-limited there).
    pub(crate) ask_keyframe: Box<dyn Fn() + Send>,
    /// The sink steps down to the low layer (`true`) or back up.
    pub(crate) low_layer: Box<dyn Fn(bool) + Send>,
}

/// Where a native sink's VP8 decoders come from (the engine's video backend; a fake in tests).
pub(crate) trait DecodeSource: Send + Sync + 'static {
    fn decoder(
        &self,
        config: DecoderConfig,
        wake: kv::Wake,
    ) -> Result<Box<dyn VideoDecoder>, kv::VideoError>;
    /// Whether it makes OS buffers ([`DecodeSource::native`] is worth a try).
    fn makes_native(&self) -> bool;
    /// The frame as an OS buffer (Apple: a `CVPixelBuffer`).
    fn native(&self, frame: &kv::VideoFrame) -> Result<kv::VideoFrame, kv::VideoError>;
}

struct BackendSource {
    backend: Arc<dyn VideoBackend>,
    /// Asked once (the capabilities are a fresh list per call).
    apple: bool,
}

impl DecodeSource for BackendSource {
    fn decoder(
        &self,
        config: DecoderConfig,
        wake: kv::Wake,
    ) -> Result<Box<dyn VideoDecoder>, kv::VideoError> {
        self.backend.decoder(config, wake)
    }

    fn makes_native(&self) -> bool {
        self.apple
    }

    fn native(&self, frame: &kv::VideoFrame) -> Result<kv::VideoFrame, kv::VideoError> {
        self.backend
            .convert(frame, frame.width, frame.height, PixelFormat::Native)
    }
}

// ───────────────────────────── the worker ─────────────────────────────

enum Job {
    Frame(EncodedVideoFrame, bool),
    Reset(String),
    Switched(Option<String>, Option<String>),
    Resize(u32, u32),
    /// The decoder has output (when its wake callback ran).
    Wake(Instant),
    /// The sink closed.
    Stop,
    /// Tests: answered once every job before it is done.
    #[cfg(test)]
    Ping(mpsc::Sender<()>),
}

/// The sink's side in the router: a bounded frame count over the worker's channel (control
/// messages always go through).
pub(crate) struct NativeWorker {
    tx: mpsc::Sender<Job>,
    shared: Arc<Shared>,
}

impl NativeWorker {
    /// Starts the sink's thread. The hooks are called from that thread.
    pub(crate) fn start(
        consumer: Arc<dyn NativeVideoConsumer>,
        backend: Option<Arc<dyn VideoBackend>>,
        max_size: (u32, u32),
        hooks: Hooks,
    ) -> std::io::Result<Arc<NativeWorker>> {
        let source = backend.map(|backend| {
            let apple = backend.capabilities().backend == "apple";
            Arc::new(BackendSource { backend, apple }) as Arc<dyn DecodeSource>
        });
        NativeWorker::start_with(consumer, source, max_size, hooks, Tuning::DEFAULT)
    }

    pub(crate) fn start_with(
        consumer: Arc<dyn NativeVideoConsumer>,
        source: Option<Arc<dyn DecodeSource>>,
        max_size: (u32, u32),
        hooks: Hooks,
        tuning: Tuning,
    ) -> std::io::Result<Arc<NativeWorker>> {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            queued: AtomicUsize::new(0),
            meter: Mutex::new(Meter::default()),
        });
        let now = Instant::now();
        let w = Worker {
            consumer,
            native_ok: source.as_ref().is_some_and(|s| s.makes_native()),
            source,
            fit: max_size,
            decoder: None,
            out_seen: 0,
            pending: VecDeque::new(),
            backlog: VecDeque::new(),
            skip_to_key: false,
            shared: Arc::clone(&shared),
            wake: tx.clone(),
            hooks,
            tuning,
            track: None,
            low: false,
            low_since: now,
            last_up: None,
            up_wait: tuning.step_up_after,
            window: Window::new(now),
        };
        std::thread::Builder::new()
            .name("sew-voice-native-sink".into())
            .spawn(move || w.run(rx))?;
        Ok(Arc::new(NativeWorker { tx, shared }))
    }

    /// The sink's counters.
    pub(crate) fn stats(&self) -> NativeStats {
        NativeStats(Arc::clone(&self.shared))
    }
}

impl Drop for NativeWorker {
    fn drop(&mut self) {
        // The sink closed (the router dropped it): the thread ends.
        let _ = self.tx.send(Job::Stop);
    }
}

impl NativeTarget for NativeWorker {
    fn push(&self, frame: &EncodedVideoFrame, discontinuity: bool) -> Pushed {
        if self.shared.queued.load(Ordering::Acquire) >= NATIVE_QUEUE {
            // The thread is NATIVE_QUEUE frames behind (its consumer or its scheduling, never
            // the decoder: a submit doesn't block): the router waits for a keyframe.
            let mut m = self.shared.meter();
            m.stats.frames_in += 1;
            m.stats.dropped_queue_full += 1;
            m.dropped(Instant::now(), 1);
            return Pushed::Dropped;
        }
        let depth = self.shared.queued.fetch_add(1, Ordering::AcqRel) + 1;
        {
            let mut m = self.shared.meter();
            m.stats.frames_in += 1;
            let depth = u32::try_from(depth).unwrap_or(u32::MAX);
            m.stats.queue_peak = m.stats.queue_peak.max(depth);
        }
        if self
            .tx
            .send(Job::Frame(frame.clone(), discontinuity))
            .is_err()
        {
            return Pushed::Closed;
        }
        Pushed::Sent
    }

    fn reset(&self, track: &str) -> Pushed {
        match self.tx.send(Job::Reset(track.to_owned())) {
            Ok(()) => Pushed::Sent,
            Err(_) => Pushed::Closed,
        }
    }

    fn resize(&self, width: u32, height: u32) {
        let _ = self.tx.send(Job::Resize(width, height));
    }

    fn switched(&self, track: Option<&str>, member_id: Option<&str>) -> Pushed {
        let job = Job::Switched(track.map(str::to_owned), member_id.map(str::to_owned));
        match self.tx.send(job) {
            Ok(()) => Pushed::Sent,
            Err(_) => Pushed::Closed,
        }
    }
}

/// A VP8 input waiting for its decoded frame.
struct Pending {
    capture_us: u64,
    track: String,
    rtp_ts: u32,
    keyframe: bool,
    discontinuity: bool,
    /// When the sink first submitted it.
    submitted: Instant,
}

/// The log window's own figures.
struct Window {
    start: Instant,
    frames: u64,
    decode_peak: u32,
    to_wake_peak: u32,
    to_drain_peak: u32,
    consumer_peak: u32,
}

impl Window {
    fn new(start: Instant) -> Window {
        Window {
            start,
            frames: 0,
            decode_peak: 0,
            to_wake_peak: 0,
            to_drain_peak: 0,
            consumer_peak: 0,
        }
    }
}

struct Worker {
    consumer: Arc<dyn NativeVideoConsumer>,
    source: Option<Arc<dyn DecodeSource>>,
    fit: (u32, u32),
    decoder: Option<Box<dyn VideoDecoder>>,
    /// The decoder's `dropped_out` already counted.
    out_seen: u64,
    pending: VecDeque<Pending>,
    /// Frames the decoder had no room for (`Busy`, its input queue of 4 full), in order: they
    /// go in as its outputs make room (on each wake and before each new frame). A keyframe
    /// supersedes the deltas waiting before it; more than `tuning.backlog` waiting drops them
    /// all and the sink skips to a keyframe.
    backlog: VecDeque<(EncodedInput, Pending)>,
    /// After a dropped frame: deltas are skipped until a keyframe (asked for), which goes out
    /// with `discontinuity`.
    skip_to_key: bool,
    shared: Arc<Shared>,
    wake: mpsc::Sender<Job>,
    hooks: Hooks,
    tuning: Tuning,
    /// The track of the last frame (for the reset a decoder failure sends, and the log).
    track: Option<String>,
    /// The source makes OS buffers and hasn't failed to yet.
    native_ok: bool,
    /// The step-down: on the low layer now, since when, the last step-up and the wait before
    /// the next.
    low: bool,
    low_since: Instant,
    last_up: Option<Instant>,
    up_wait: Duration,
    window: Window,
}

fn micros(d: Duration) -> u32 {
    u32::try_from(d.as_micros()).unwrap_or(u32::MAX)
}

/// A moving average with weight 1/8 for the new value.
fn ewma(avg: u32, x: u32) -> u32 {
    if avg == 0 { x } else { avg - avg / 8 + x / 8 }
}

impl Worker {
    fn run(mut self, rx: mpsc::Receiver<Job>) {
        raise_thread_priority();
        // Ends when the sink closes.
        loop {
            match rx.recv_timeout(self.tuning.tick) {
                Ok(job) => {
                    if !self.job(job) {
                        return;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
            self.housekeep(Instant::now());
        }
    }

    /// One job; `false` when the sink closed.
    fn job(&mut self, job: Job) -> bool {
        match job {
            Job::Frame(f, discontinuity) => {
                self.shared.queued.fetch_sub(1, Ordering::AcqRel);
                self.frame(f, discontinuity);
            }
            Job::Reset(track) => {
                self.reset_decoder();
                self.consumer.on_video(NativeVideoEvent::Reset { track });
            }
            Job::Switched(track, member_id) => {
                self.reset_decoder();
                self.consumer
                    .on_video(NativeVideoEvent::Switched { track, member_id });
            }
            Job::Resize(w, h) => {
                self.fit = (w, h);
                if let Some(d) = self.decoder.as_mut() {
                    d.set_fit(fit(self.fit));
                }
            }
            Job::Wake(at) => {
                self.drain(Some(at));
                self.flush_backlog();
            }
            Job::Stop => return false,
            #[cfg(test)]
            Job::Ping(tx) => {
                let _ = tx.send(());
            }
        }
        true
    }

    /// Forget the decoder's state (it stays open: no new thread per keyframe); the next input
    /// it takes is a keyframe.
    fn reset_decoder(&mut self) {
        if let Some(d) = self.decoder.as_mut() {
            d.reset();
        }
        self.pending.clear();
        self.backlog.clear();
        self.skip_to_key = false;
    }

    fn drop_decoder(&mut self) {
        self.decoder = None;
        self.out_seen = 0;
        self.pending.clear();
        self.backlog.clear();
        self.skip_to_key = false;
    }

    fn frame(&mut self, f: EncodedVideoFrame, discontinuity: bool) {
        if self.track.as_deref() != Some(f.track.as_str()) {
            self.track = Some(f.track.clone());
        }
        match f.codec {
            VideoCodec::H264 => self.deliver(NativeVideoFrame {
                track: f.track,
                keyframe: f.keyframe,
                discontinuity,
                timestamp_us: f.arrival_us,
                rtp_ts: f.rtp_ts,
                payload: NativePayload::H264 {
                    access_unit: f.data,
                    width: f.width,
                    height: f.height,
                },
            }),
            VideoCodec::Vp8 => self.vp8(f, discontinuity),
        }
    }

    fn vp8(&mut self, f: EncodedVideoFrame, discontinuity: bool) {
        let mut discontinuity = discontinuity;
        if discontinuity {
            self.reset_decoder();
        }
        // Older frames the decoder had no room for go first.
        self.flush_backlog();
        if self.skip_to_key {
            if !f.keyframe {
                self.shared.meter().stats.skipped_to_keyframe += 1;
                return;
            }
            self.skip_to_key = false;
            discontinuity = true;
        }
        if self.decoder.is_none() && !f.keyframe {
            // After a failure: nothing until the keyframe the sink asked for.
            self.shared.meter().stats.skipped_to_keyframe += 1;
            return;
        }
        if self.decoder.is_none() {
            let Some(source) = &self.source else { return };
            let mut cfg = DecoderConfig::new(kv::VideoCodec::Vp8);
            cfg.output = PixelFormat::Nv12;
            cfg.fit = fit(self.fit);
            let wake = self.wake.clone();
            let wake: kv::Wake = Arc::new(move || {
                let _ = wake.send(Job::Wake(Instant::now()));
            });
            match source.decoder(cfg, wake) {
                Ok(d) => {
                    self.decoder = Some(d);
                    self.out_seen = 0;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "sew-voice: no VP8 decoder for a native sink");
                    return;
                }
            }
        }
        let input = EncodedInput::new(kv::VideoCodec::Vp8, f.data, f.keyframe, f.arrival_us);
        let mut meta = Pending {
            capture_us: f.arrival_us,
            track: f.track,
            rtp_ts: f.rtp_ts,
            keyframe: f.keyframe,
            discontinuity,
            submitted: Instant::now(),
        };
        if !self.backlog.is_empty() {
            if f.keyframe {
                // Nothing before a keyframe is needed to show it: the waiting deltas go (late
                // frames the consumer would only show to catch up).
                let n = self.backlog.len() as u64;
                self.backlog.clear();
                meta.discontinuity = true;
                let mut m = self.shared.meter();
                m.stats.dropped_superseded += n;
                m.dropped(Instant::now(), n);
            } else if self.backlog.len() >= self.tuning.backlog {
                // Too far behind: the waiting frames and this one go, the sink skips to a
                // keyframe.
                let n = self.backlog.len() as u64 + 1;
                self.backlog.clear();
                self.lost(n);
                return;
            } else {
                self.backlog.push_back((input, meta));
                self.note_in_flight();
                return;
            }
        }
        self.submit(input, meta);
        self.drain(None);
    }

    /// Hands the decoder one input; `false` when it has no room (`Busy`): the input then
    /// waits at the back of the backlog. An error resets the sink (`decode_failed`).
    fn submit(&mut self, input: EncodedInput, meta: Pending) -> bool {
        let Some(d) = self.decoder.as_mut() else {
            return true;
        };
        match d.submit(input.clone()) {
            Ok(()) => {
                self.pending.push_back(meta);
                self.note_in_flight();
                true
            }
            Err(kv::DecodeError::Busy) => {
                self.backlog.push_back((input, meta));
                self.note_in_flight();
                false
            }
            Err(e) => {
                self.decode_failed(&e);
                true
            }
        }
    }

    /// Inputs in the decoder or waiting for room in it.
    fn in_flight(&self) -> u32 {
        u32::try_from(self.pending.len() + self.backlog.len()).unwrap_or(u32::MAX)
    }

    fn note_in_flight(&mut self) {
        let n = self.in_flight();
        let mut m = self.shared.meter();
        m.stats.decoder_in_flight = n;
        m.stats.decoder_in_flight_peak = m.stats.decoder_in_flight_peak.max(n);
    }

    /// The backlog into the decoder, in order, while it has room.
    fn flush_backlog(&mut self) {
        while self.decoder.is_some() {
            let Some((input, meta)) = self.backlog.pop_front() else {
                return;
            };
            if !self.submit(input, meta) {
                // Back to the front: `submit` put it at the back of what was left.
                if let Some(again) = self.backlog.pop_back() {
                    self.backlog.push_front(again);
                }
                return;
            }
            self.shared.meter().stats.busy_retried += 1;
        }
    }

    /// `n` frames were lost before the decoder: skip to a keyframe and ask for one.
    fn lost(&mut self, n: u64) {
        {
            let mut m = self.shared.meter();
            m.stats.dropped_busy += n;
            m.dropped(Instant::now(), n);
        }
        tracing::debug!("sew-voice: a native sink's VP8 decoder is behind: waiting for a keyframe");
        self.skip_to_key = true;
        self.ask_keyframe();
    }

    fn ask_keyframe(&mut self) {
        self.shared.meter().stats.keyframes_asked += 1;
        (self.hooks.ask_keyframe)();
    }

    /// Every decoded frame the decoder has, to the consumer.
    /// `woke`: when the decoder's wake callback ran (`None`: a take right after a submit).
    fn drain(&mut self, woke: Option<Instant>) {
        loop {
            let Some(d) = self.decoder.as_mut() else {
                return;
            };
            match d.poll() {
                None => break,
                Some(Ok(frame)) => self.decoded(frame, woke),
                Some(Err(kv::DecodeError::Busy)) => break,
                Some(Err(e)) => {
                    self.decode_failed(&e);
                    return;
                }
            }
        }
        // The decoder's own drops (its output queue, when this thread fell behind): they lose
        // no reference, so no keyframe, but they count toward the step-down.
        if let Some(d) = self.decoder.as_ref() {
            let out = d.stats().dropped_out;
            if out > self.out_seen {
                let n = out - self.out_seen;
                self.out_seen = out;
                let mut m = self.shared.meter();
                m.stats.dropped_decoder_output += n;
                m.dropped(Instant::now(), n);
            }
        }
    }

    fn decoded(&mut self, frame: kv::VideoFrame, woke: Option<Instant>) {
        // The decoder may skip inputs: outputs keep their input's capture time.
        let meta = loop {
            match self.pending.pop_front() {
                Some(p) if p.capture_us == frame.capture_us => break Some(p),
                Some(p) if p.capture_us < frame.capture_us => continue,
                Some(p) => {
                    self.pending.push_front(p);
                    break None;
                }
                None => break None,
            }
        };
        let Some(meta) = meta else { return };
        let taken = Instant::now();
        let decode_us = micros(taken.saturating_duration_since(meta.submitted));
        // A wake from before this input's submit belongs to an earlier output.
        let woke = woke.filter(|w| *w >= meta.submitted);
        let to_wake = micros(
            woke.unwrap_or(taken)
                .saturating_duration_since(meta.submitted),
        );
        let to_drain = woke.map_or(0, |w| micros(taken.saturating_duration_since(w)));
        let t = Instant::now();
        let frame = self.native(frame);
        let convert_us = self.native_ok.then(|| micros(t.elapsed()));
        {
            let mut m = self.shared.meter();
            m.stats.decode_us = decode_us;
            m.stats.decode_us_avg = ewma(m.stats.decode_us_avg, decode_us);
            m.stats.submit_to_wake_us_avg = ewma(m.stats.submit_to_wake_us_avg, to_wake);
            m.stats.wake_to_drain_us_avg = ewma(m.stats.wake_to_drain_us_avg, to_drain);
            m.stats.decoder_in_flight = self.in_flight();
            if let Some(c) = convert_us {
                m.stats.convert_us_avg = ewma(m.stats.convert_us_avg, c);
            }
            if matches!(frame.buffer, kv::FrameBuffer::Planes(_)) {
                m.stats.cpu_planes += 1;
            }
        }
        self.window.decode_peak = self.window.decode_peak.max(decode_us);
        self.window.to_wake_peak = self.window.to_wake_peak.max(to_wake);
        self.window.to_drain_peak = self.window.to_drain_peak.max(to_drain);
        self.deliver(NativeVideoFrame {
            track: meta.track,
            keyframe: meta.keyframe,
            discontinuity: meta.discontinuity,
            timestamp_us: meta.capture_us,
            rtp_ts: meta.rtp_ts,
            payload: NativePayload::Decoded(frame),
        });
    }

    /// One frame to the consumer, timed.
    fn deliver(&mut self, frame: NativeVideoFrame) {
        let t = Instant::now();
        self.consumer.on_video(NativeVideoEvent::Frame(frame));
        let us = micros(t.elapsed());
        self.window.frames += 1;
        self.window.consumer_peak = self.window.consumer_peak.max(us);
        let mut m = self.shared.meter();
        m.stats.frames_out += 1;
        m.stats.consumer_us_avg = ewma(m.stats.consumer_us_avg, us);
    }

    /// The frame as an OS buffer where the source makes one (Apple: a `CVPixelBuffer`); after
    /// the first failure (logged once) the sink hands over CPU planes without trying again.
    fn native(&mut self, frame: kv::VideoFrame) -> kv::VideoFrame {
        if !self.native_ok {
            return frame;
        }
        let Some(source) = &self.source else {
            return frame;
        };
        match source.native(&frame) {
            Ok(f) => f,
            Err(e) => {
                self.native_ok = false;
                tracing::warn!(
                    error = %e,
                    "sew-voice: a native sink can't make OS buffers: decoded frames go out as NV12 \
                     CPU planes"
                );
                frame
            }
        }
    }

    fn decode_failed(&mut self, e: &kv::DecodeError) {
        tracing::debug!(error = %e, "sew-voice: a native sink's VP8 decoder needs a keyframe");
        self.shared.meter().stats.decode_errors += 1;
        let track = self.track.clone();
        self.drop_decoder();
        if let Some(track) = track {
            self.consumer.on_video(NativeVideoEvent::Reset { track });
        }
        self.ask_keyframe();
    }

    /// The step-down, the step-up and the periodic log.
    fn housekeep(&mut self, now: Instant) {
        let t = self.tuning;
        let (recent, last_drop, decode_avg) = {
            let mut m = self.shared.meter();
            while m
                .drops
                .front()
                .is_some_and(|d| now.saturating_duration_since(*d) > t.step_down_window)
            {
                m.drops.pop_front();
            }
            (m.drops.len(), m.last_drop, m.stats.decode_us_avg)
        };
        if !self.low && recent > t.step_down_drops {
            // Back down soon after a step-up: the upper layer didn't hold; wait longer.
            self.up_wait = match self.last_up {
                Some(up) if now.saturating_duration_since(up) < t.step_up_after => {
                    (self.up_wait * 2).min(t.step_up_after * 8)
                }
                _ => t.step_up_after,
            };
            self.low = true;
            self.low_since = now;
            {
                let mut m = self.shared.meter();
                m.drops.clear();
                m.stats.low_layer = true;
                m.stats.step_downs += 1;
            }
            tracing::info!(
                drops = recent,
                track = self.track.as_deref().unwrap_or(""),
                "sew-voice: a native sink can't keep up: asking for the low layer"
            );
            (self.hooks.low_layer)(true);
        } else if self.low {
            let calm_since = last_drop.map_or(self.low_since, |d| d.max(self.low_since));
            let keeps_up = Duration::from_micros(u64::from(decode_avg)) < t.frame_interval / 3;
            if now.saturating_duration_since(calm_since) >= self.up_wait && keeps_up {
                self.low = false;
                self.last_up = Some(now);
                {
                    let mut m = self.shared.meter();
                    m.stats.low_layer = false;
                    m.stats.step_ups += 1;
                }
                tracing::info!(
                    track = self.track.as_deref().unwrap_or(""),
                    "sew-voice: a native sink keeps up again: back to its own layer"
                );
                (self.hooks.low_layer)(false);
            }
        }
        let span = now.saturating_duration_since(self.window.start);
        if span >= t.log_every {
            let fps = (self.window.frames as f64 / span.as_secs_f64()) as f32;
            let s = {
                let mut m = self.shared.meter();
                m.stats.fps = fps;
                m.stats.decode_us_peak = self.window.decode_peak;
                m.stats.submit_to_wake_us_peak = self.window.to_wake_peak;
                m.stats.wake_to_drain_us_peak = self.window.to_drain_peak;
                m.stats.consumer_us_peak = self.window.consumer_peak;
                m.stats
            };
            let depth = self.shared.queued.load(Ordering::Acquire);
            tracing::info!(
                track = self.track.as_deref().unwrap_or(""),
                fps = s.fps,
                frames_in = s.frames_in,
                frames_out = s.frames_out,
                decode_us = s.decode_us,
                decode_us_avg = s.decode_us_avg,
                decode_us_peak = s.decode_us_peak,
                submit_to_wake_us_avg = s.submit_to_wake_us_avg,
                submit_to_wake_us_peak = s.submit_to_wake_us_peak,
                wake_to_drain_us_avg = s.wake_to_drain_us_avg,
                wake_to_drain_us_peak = s.wake_to_drain_us_peak,
                decoder_in_flight = s.decoder_in_flight,
                decoder_in_flight_peak = s.decoder_in_flight_peak,
                busy_retried = s.busy_retried,
                dropped_superseded = s.dropped_superseded,
                convert_us_avg = s.convert_us_avg,
                consumer_us_avg = s.consumer_us_avg,
                consumer_us_peak = s.consumer_us_peak,
                queue_depth = depth,
                queue_peak = s.queue_peak,
                dropped_queue_full = s.dropped_queue_full,
                dropped_busy = s.dropped_busy,
                dropped_decoder_output = s.dropped_decoder_output,
                skipped_to_keyframe = s.skipped_to_keyframe,
                decode_errors = s.decode_errors,
                keyframes_asked = s.keyframes_asked,
                cpu_planes = s.cpu_planes,
                low_layer = s.low_layer,
                "sew-voice: native sink stats"
            );
            self.window = Window::new(now);
        }
    }
}

fn fit((w, h): (u32, u32)) -> Option<(u32, u32)> {
    (w > 0 && h > 0).then_some((w, h))
}

/// The sink's thread runs at the user-interactive QoS class on Apple (it feeds the picture in
/// picture on screen): a thread with no class can run at a lower one and be throttled hard on a
/// phone. The VP8 decoder's own thread is started from this one, after this call.
#[cfg(any(target_os = "ios", target_os = "macos"))]
#[allow(unsafe_code)]
fn raise_thread_priority() {
    // SAFETY: `pthread_set_qos_class_self_np` changes the calling thread's own QoS class; it
    // takes two plain values and no pointer.
    let r = unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0)
    };
    if r != 0 {
        tracing::warn!(
            error = r,
            "sew-voice: a native sink's thread kept its QoS class"
        );
    }
}

#[cfg(not(any(target_os = "ios", target_os = "macos")))]
fn raise_thread_priority() {}

// ───────────────────────────── H.264 for AVFoundation ─────────────────────────────

/// The NAL units of an Annex B access unit (3- or 4-byte start codes), without start codes.
pub fn h264_nal_units(au: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &s) in starts.iter().enumerate() {
        let mut end = starts.get(k + 1).map_or(au.len(), |&n| n - 3);
        // A 4-byte start code's leading zero, and trailing zeros, belong to no NAL unit.
        while end > s && au[end - 1] == 0 {
            end -= 1;
        }
        if end > s {
            out.push(&au[s..end]);
        }
    }
    out
}

/// An Annex B access unit as AVCC (each NAL unit behind its 4-byte big-endian length), the
/// layout a `CMSampleBuffer` for `AVSampleBufferDisplayLayer` holds. SPS and PPS stay in (the
/// layer ignores them; [`h264_parameter_sets`] gives them to the format description).
pub fn h264_annex_b_to_avcc(au: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(au.len() + 16);
    for nal in h264_nal_units(au) {
        let len = u32::try_from(nal.len()).unwrap_or(u32::MAX);
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

/// The SPS and PPS NAL units of an access unit (keyframes carry both), for
/// `CMVideoFormatDescriptionCreateFromH264ParameterSets`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct H264ParameterSets {
    /// Sequence parameter sets (NAL type 7), without start codes.
    pub sps: Vec<Vec<u8>>,
    /// Picture parameter sets (NAL type 8), without start codes.
    pub pps: Vec<Vec<u8>>,
}

/// The SPS and PPS of an Annex B access unit (both empty for a delta frame).
pub fn h264_parameter_sets(au: &[u8]) -> H264ParameterSets {
    let mut out = H264ParameterSets::default();
    for nal in h264_nal_units(au) {
        match nal.first().map(|b| b & 0x1f) {
            Some(7) => out.sps.push(nal.to_vec()),
            Some(8) => out.pps.push(nal.to_vec()),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    // A test that can't set up must fail loudly.
    #![allow(clippy::expect_used)]
    use super::*;

    // ── a scripted decoder: the test decides when inputs come out, and its input queue's room ──

    #[derive(Default)]
    struct FakeState {
        /// The input queue's room (`Busy` when full).
        room: usize,
        /// Decode at once on submit.
        auto: bool,
        inputs: VecDeque<EncodedInput>,
        outputs: VecDeque<kv::VideoFrame>,
        wake: Option<kv::Wake>,
        submits: usize,
        busy: usize,
        resets: usize,
    }

    #[derive(Clone, Default)]
    struct FakeDec(Arc<Mutex<FakeState>>);

    impl FakeDec {
        fn state(&self) -> MutexGuard<'_, FakeState> {
            self.0.lock().unwrap_or_else(PoisonError::into_inner)
        }

        /// The oldest `n` inputs come out, then the wake.
        fn complete(&self, n: usize) {
            let wake = {
                let mut s = self.state();
                for _ in 0..n {
                    let Some(i) = s.inputs.pop_front() else { break };
                    s.outputs.push_back(picture(i.capture_us));
                }
                s.wake.clone()
            };
            if let Some(w) = wake {
                w();
            }
        }
    }

    fn picture(capture_us: u64) -> kv::VideoFrame {
        let planes = kv::Planes::tight(PixelFormat::Nv12, 2, 2, vec![0u8; 6]).expect("NV12 planes");
        kv::VideoFrame::from_planes(planes, 2, 2, capture_us).expect("a frame")
    }

    struct FakeDecoder(FakeDec);

    impl VideoDecoder for FakeDecoder {
        fn submit(&mut self, frame: EncodedInput) -> Result<(), kv::DecodeError> {
            let wake = {
                let mut s = self.0.state();
                if s.inputs.len() >= s.room {
                    s.busy += 1;
                    return Err(kv::DecodeError::Busy);
                }
                s.submits += 1;
                s.inputs.push_back(frame);
                if !s.auto {
                    return Ok(());
                }
                while let Some(i) = s.inputs.pop_front() {
                    s.outputs.push_back(picture(i.capture_us));
                }
                s.wake.clone()
            };
            if let Some(w) = wake {
                w();
            }
            Ok(())
        }

        fn poll(&mut self) -> Option<Result<kv::VideoFrame, kv::DecodeError>> {
            self.0.state().outputs.pop_front().map(Ok)
        }

        fn reset(&mut self) {
            let mut s = self.0.state();
            s.inputs.clear();
            s.outputs.clear();
            s.resets += 1;
        }

        fn set_fit(&mut self, _fit: Option<(u32, u32)>) {}

        fn stats(&self) -> kv::DecoderStats {
            kv::DecoderStats::default()
        }
    }

    struct FakeSource {
        dec: FakeDec,
        makes_native: bool,
        native_calls: AtomicUsize,
    }

    impl DecodeSource for FakeSource {
        fn decoder(
            &self,
            _config: DecoderConfig,
            wake: kv::Wake,
        ) -> Result<Box<dyn VideoDecoder>, kv::VideoError> {
            self.dec.state().wake = Some(wake);
            Ok(Box::new(FakeDecoder(self.dec.clone())))
        }

        fn makes_native(&self) -> bool {
            self.makes_native
        }

        fn native(&self, _frame: &kv::VideoFrame) -> Result<kv::VideoFrame, kv::VideoError> {
            self.native_calls.fetch_add(1, Ordering::AcqRel);
            Err(kv::VideoError::Unsupported("no OS buffers here".into()))
        }
    }

    struct Rig {
        worker: Arc<NativeWorker>,
        dec: FakeDec,
        source: Arc<FakeSource>,
        seen: Arc<Mutex<Vec<NativeVideoEvent>>>,
        asked: Arc<AtomicUsize>,
        layers: Arc<Mutex<Vec<bool>>>,
    }

    impl Rig {
        fn new(room: usize, makes_native: bool, tuning: Tuning) -> Rig {
            Rig::with_consumer(room, makes_native, tuning, |_| {})
        }

        fn with_consumer(
            room: usize,
            makes_native: bool,
            tuning: Tuning,
            also: impl Fn(&NativeVideoEvent) + Send + Sync + 'static,
        ) -> Rig {
            let dec = FakeDec::default();
            dec.state().room = room;
            let source = Arc::new(FakeSource {
                dec: dec.clone(),
                makes_native,
                native_calls: AtomicUsize::new(0),
            });
            let seen = Arc::new(Mutex::new(Vec::new()));
            let asked = Arc::new(AtomicUsize::new(0));
            let layers = Arc::new(Mutex::new(Vec::new()));
            let consumer: Arc<dyn NativeVideoConsumer> = {
                let seen = Arc::clone(&seen);
                Arc::new(move |e: NativeVideoEvent| {
                    also(&e);
                    seen.lock().unwrap_or_else(PoisonError::into_inner).push(e);
                })
            };
            let hooks = Hooks {
                ask_keyframe: {
                    let asked = Arc::clone(&asked);
                    Box::new(move || {
                        asked.fetch_add(1, Ordering::AcqRel);
                    })
                },
                low_layer: {
                    let layers = Arc::clone(&layers);
                    Box::new(move |low| {
                        layers
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push(low)
                    })
                },
            };
            let worker = NativeWorker::start_with(
                consumer,
                Some(Arc::clone(&source) as Arc<dyn DecodeSource>),
                (640, 360),
                hooks,
                tuning,
            )
            .expect("the sink's thread");
            Rig {
                worker,
                dec,
                source,
                seen,
                asked,
                layers,
            }
        }

        fn push(&self, n: u32, key: bool, discontinuity: bool) -> Pushed {
            self.worker.push(&vp8(n, key), discontinuity)
        }

        /// Waits until the thread has done every job sent so far.
        fn sync(&self) {
            let (tx, rx) = mpsc::channel();
            assert!(
                self.worker.tx.send(Job::Ping(tx)).is_ok(),
                "the thread runs"
            );
            rx.recv_timeout(Duration::from_secs(10))
                .expect("the sink's thread answers within 10 s");
        }

        /// The frames out: (frame number, keyframe, discontinuity).
        fn frames(&self) -> Vec<(u32, bool, bool)> {
            self.seen
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .filter_map(|e| match e {
                    NativeVideoEvent::Frame(f) => {
                        Some((f.rtp_ts / 3000, f.keyframe, f.discontinuity))
                    }
                    _ => None,
                })
                .collect()
        }

        fn stats(&self) -> NativeSinkStats {
            self.worker.stats().get()
        }

        fn layers(&self) -> Vec<bool> {
            self.layers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    fn vp8(n: u32, keyframe: bool) -> EncodedVideoFrame {
        EncodedVideoFrame {
            track: "TR_VCtest".into(),
            codec: VideoCodec::Vp8,
            data: vec![u8::from(keyframe), 1, 2, 3],
            keyframe,
            rtp_ts: n * 3000,
            arrival_us: 1_000 + u64::from(n) * 33_333,
            width: 0,
            height: 0,
        }
    }

    fn until(what: &str, mut pred: impl FnMut() -> bool) {
        for _ in 0..1000 {
            if pred() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out after 10 s waiting for: {what}");
    }

    #[test]
    fn a_busy_decoder_gets_the_frame_again_after_its_next_output() {
        let r = Rig::new(1, false, Tuning::DEFAULT);
        assert_eq!(r.push(0, true, true), Pushed::Sent);
        r.sync();
        assert_eq!(r.push(1, false, false), Pushed::Sent);
        r.sync();
        let counts = {
            let s = r.dec.state();
            (s.submits, s.busy)
        };
        assert_eq!(counts, (1, 1), "frame 1 found no room");
        r.dec.complete(1);
        r.sync();
        assert_eq!(
            r.dec.state().submits,
            2,
            "frame 1 went in after frame 0 came out"
        );
        r.dec.complete(1);
        r.sync();
        assert_eq!(r.frames(), vec![(0, true, true), (1, false, false)]);
        let st = r.stats();
        assert_eq!(st.dropped_busy, 0);
        assert_eq!(st.skipped_to_keyframe, 0);
        assert_eq!(st.keyframes_asked, 0);
        assert_eq!(r.asked.load(Ordering::Acquire), 0);
        assert_eq!((st.frames_in, st.frames_out), (2, 2));
        assert!(st.decode_us > 0 && st.decode_us_avg > 0, "{st:?}");
        assert_eq!(st.cpu_planes, 2);
        assert_eq!(st.busy_retried, 1);
        assert_eq!((st.decoder_in_flight, st.decoder_in_flight_peak), (0, 2));
        // Both outputs came on a wake: the sync after complete() waited for the thread, so
        // the wait to the wake is most of the time.
        assert!(
            st.submit_to_wake_us_avg > 0 && st.submit_to_wake_us_avg <= st.decode_us_avg,
            "{st:?}"
        );
    }

    #[test]
    fn a_burst_waits_in_order_for_room_in_the_decoder() {
        let r = Rig::new(1, false, Tuning::DEFAULT);
        for n in 0..6 {
            r.push(n, n == 0, n == 0);
        }
        r.sync();
        assert_eq!(
            r.stats().decoder_in_flight,
            6,
            "one in the decoder, five waiting"
        );
        for _ in 0..6 {
            r.dec.complete(1);
            r.sync();
        }
        let want: Vec<_> = (0..6).map(|n| (n, n == 0, n == 0)).collect();
        assert_eq!(r.frames(), want, "every frame, in order");
        let st = r.stats();
        assert_eq!(st.busy_retried, 5);
        assert_eq!((st.dropped_busy, st.dropped_superseded), (0, 0));
        assert_eq!(r.asked.load(Ordering::Acquire), 0);
    }

    #[test]
    fn a_keyframe_supersedes_the_deltas_waiting_before_it() {
        let r = Rig::new(1, false, Tuning::DEFAULT);
        for n in 0..4 {
            r.push(n, n == 0, n == 0);
        }
        r.push(4, true, false);
        r.sync();
        for _ in 0..3 {
            r.dec.complete(1);
            r.sync();
        }
        assert_eq!(
            r.frames(),
            vec![(0, true, true), (4, true, true)],
            "the waiting deltas go; the keyframe flushes"
        );
        let st = r.stats();
        assert_eq!((st.dropped_superseded, st.dropped_busy), (3, 0));
        assert_eq!(
            r.asked.load(Ordering::Acquire),
            0,
            "the keyframe is already there"
        );
    }

    #[test]
    fn a_frame_lost_to_a_busy_decoder_breaks_the_track_until_a_keyframe() {
        let r = Rig::new(
            1,
            false,
            Tuning {
                backlog: 1,
                ..Tuning::DEFAULT
            },
        );
        r.push(0, true, true);
        r.sync();
        r.push(1, false, false); // waits: no room
        r.sync();
        r.push(2, false, false); // the backlog is full: 1 and 2 are lost
        r.sync();
        assert_eq!(
            r.asked.load(Ordering::Acquire),
            1,
            "a keyframe is asked for"
        );
        r.push(3, false, false); // skipped
        r.sync();
        r.dec.complete(4);
        r.sync();
        r.push(4, true, false);
        r.sync();
        r.dec.complete(1);
        r.sync();
        assert_eq!(
            r.frames(),
            vec![(0, true, true), (4, true, true)],
            "nothing between the lost frame and the keyframe; the keyframe flushes"
        );
        let st = r.stats();
        assert_eq!(st.dropped_busy, 2);
        assert_eq!(st.skipped_to_keyframe, 1);
        assert_eq!(st.keyframes_asked, 1);
        assert_eq!(r.asked.load(Ordering::Acquire), 1);
        assert_eq!((st.frames_in, st.frames_out), (5, 2));
        assert_eq!(r.dec.state().resets, 0, "the decoder stays open");
    }

    #[test]
    fn a_router_discontinuity_resets_the_decoder_without_reopening_it() {
        let r = Rig::new(4, false, Tuning::DEFAULT);
        r.push(0, true, true);
        r.push(1, false, false);
        r.sync();
        r.push(2, true, true);
        r.sync();
        assert_eq!(r.dec.state().resets, 1);
        assert_eq!(r.dec.state().inputs.len(), 1, "only the keyframe is left");
        r.dec.complete(1);
        r.sync();
        assert_eq!(r.frames(), vec![(2, true, true)]);
    }

    #[test]
    fn the_queue_only_overflows_while_the_sinks_thread_is_held_up() {
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let r = {
            let gate = Arc::clone(&gate);
            Rig::with_consumer(4, false, Tuning::DEFAULT, move |_| {
                let (open, cv) = &*gate;
                let mut g = open.lock().unwrap_or_else(PoisonError::into_inner);
                while !*g {
                    g = cv.wait(g).unwrap_or_else(PoisonError::into_inner);
                }
            })
        };
        let h264 = |n: u32| EncodedVideoFrame {
            codec: VideoCodec::H264,
            ..vp8(n, n == 0)
        };
        assert_eq!(r.worker.push(&h264(0), true), Pushed::Sent);
        until(
            "the thread takes frame 0 (and blocks in the consumer)",
            || r.stats().queue_depth == 0,
        );
        for n in 1..=NATIVE_QUEUE as u32 {
            assert_eq!(r.worker.push(&h264(n), false), Pushed::Sent, "frame {n}");
        }
        assert_eq!(r.worker.push(&h264(99), false), Pushed::Dropped);
        let st = r.stats();
        assert_eq!(st.dropped_queue_full, 1);
        assert_eq!(st.queue_peak, NATIVE_QUEUE as u32);
        assert_eq!(st.queue_depth, NATIVE_QUEUE as u32);
        {
            let (open, cv) = &*gate;
            *open.lock().unwrap_or_else(PoisonError::into_inner) = true;
            cv.notify_all();
        }
        r.sync();
        let st = r.stats();
        assert_eq!(st.frames_out, 1 + NATIVE_QUEUE as u64);
        assert_eq!(st.frames_in, 2 + NATIVE_QUEUE as u64);
        assert!(st.consumer_us_avg > 0, "{st:?}");
    }

    #[test]
    fn steps_down_after_repeated_drops_and_back_up_once_it_keeps_up() {
        let tuning = Tuning {
            backlog: 1,
            step_down_drops: 3,
            step_down_window: Duration::from_secs(2),
            step_up_after: Duration::from_millis(300),
            frame_interval: Duration::from_micros(33_333),
            log_every: Duration::from_millis(100),
            tick: Duration::from_millis(20),
        };
        let r = Rig::new(1, false, tuning);
        // The decoder never answers: after the first keyframe, each keyframe waits and the
        // delta after it overflows the backlog (one frame): two lost per pair.
        let push = |n: u32, key: bool| {
            r.push(n, key, n == 0);
            r.sync();
        };
        push(0, true);
        push(1, false);
        push(2, false);
        let mut n = 3;
        for _ in 0..2 {
            push(n, true);
            push(n + 1, false);
            n += 2;
        }
        let st = r.stats();
        assert_eq!(st.dropped_busy, 6, "{st:?}");
        assert_eq!(st.dropped_queue_full, 0, "{st:?}");
        until("the step-down", || r.layers() == vec![true]);
        assert!(r.stats().low_layer);
        assert_eq!(r.stats().step_downs, 1);

        // Now it decodes at once: no drop for longer than the step-up wait.
        {
            let mut s = r.dec.state();
            s.auto = true;
            s.room = 4;
            s.inputs.clear();
        }
        let mut k = n + 1;
        r.push(k, true, true);
        let deadline = Instant::now() + Duration::from_secs(10);
        while r.layers().len() < 2 && Instant::now() < deadline {
            k += 1;
            r.push(k, false, false);
            std::thread::sleep(Duration::from_millis(30));
        }
        assert_eq!(r.layers(), vec![true, false], "one step down, one back up");
        let st = r.stats();
        assert!(!st.low_layer);
        assert_eq!((st.step_downs, st.step_ups), (1, 1));
        assert!(
            Duration::from_micros(u64::from(st.decode_us_avg)) < tuning.frame_interval / 3,
            "{st:?}"
        );
        assert!(st.fps > 0.0, "the log window ran: {st:?}");
    }

    #[test]
    fn steps_down_again_soon_after_a_step_up_wait_twice_as_long() {
        let tuning = Tuning {
            backlog: 1,
            step_down_drops: 0,
            step_down_window: Duration::from_secs(2),
            step_up_after: Duration::from_millis(200),
            frame_interval: Duration::from_micros(33_333),
            log_every: Duration::from_secs(60),
            tick: Duration::from_millis(10),
        };
        let r = Rig::new(1, false, tuning);
        let lose_one = |n: u32| {
            r.push(n, true, true);
            r.sync();
            r.push(n + 1, false, false);
            r.sync();
            r.push(n + 2, false, false);
            r.sync();
        };
        lose_one(0);
        until("the first step-down", || r.layers() == vec![true]);
        until("the first step-up", || r.layers() == vec![true, false]);
        let up = Instant::now();
        lose_one(10);
        until("the second step-down", || r.layers().len() == 3);
        until("the second step-up", || r.layers().len() == 4);
        assert!(
            up.elapsed() >= Duration::from_millis(400),
            "the second wait is twice the first: {:?}",
            up.elapsed()
        );
    }

    #[test]
    fn a_source_without_os_buffers_is_tried_once_then_cpu_planes() {
        let r = Rig::new(4, true, Tuning::DEFAULT);
        r.dec.state().auto = true;
        r.push(0, true, true);
        r.push(1, false, false);
        r.push(2, false, false);
        r.sync();
        until("three frames", || r.frames().len() == 3);
        assert_eq!(r.source.native_calls.load(Ordering::Acquire), 1);
        assert_eq!(r.stats().cpu_planes, 3);
    }

    #[test]
    fn annex_b_becomes_avcc_and_names_its_parameter_sets() {
        let au = [
            0, 0, 0, 1, 0x67, 1, 2, // SPS
            0, 0, 0, 1, 0x68, 3, // PPS
            0, 0, 1, 0x65, 9, 9, 9, 0, // IDR (3-byte start code, a trailing zero)
        ];
        assert_eq!(h264_nal_units(&au).len(), 3);
        assert_eq!(
            h264_annex_b_to_avcc(&au),
            vec![
                0, 0, 0, 3, 0x67, 1, 2, 0, 0, 0, 2, 0x68, 3, 0, 0, 0, 4, 0x65, 9, 9, 9
            ]
        );
        let ps = h264_parameter_sets(&au);
        assert_eq!(ps.sps, vec![vec![0x67, 1, 2]]);
        assert_eq!(ps.pps, vec![vec![0x68, 3]]);
        assert_eq!(
            h264_parameter_sets(&[0, 0, 0, 1, 0x41, 7]),
            H264ParameterSets::default()
        );
    }
}
