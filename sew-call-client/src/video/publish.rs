//! Publishing video: the camera (and later the screen share) as the engine sends it.
//!
//! The engine core never names kessel-video. A producer (the `VideoBridge`, or a host with its
//! own codecs) hands it encoded plaintext frames, one per simulcast layer
//! ([`VideoPublication::push`]), and follows the engine's [`VideoControl`]s: keyframes the SFU
//! asks for, which layers run at what rate (dynacast and the bandwidth split), a republish
//! after a policy change, and the end of the publication.
//!
//! In order, for one publication:
//! 1. **The lease** ([`crate::Call::set_video_lease`]): `POST /api/v1/me/voice/state` with
//!    `self_video` (camera) or `self_stream` (screen share). Sew's refusals map to the
//!    [`VideoError`] codes (`video_slots_full`, `media_busy` with its `Retry-After`,
//!    `media_capacity`, `video_disabled`).
//! 2. **The publication** ([`crate::Call::publish_video`]): an `AddTrackRequest` with the
//!    layers that opened (`VideoLayer` q/h/f as LOW/MEDIUM/HIGH, bitrate = the layer's cap),
//!    `simulcast_codecs` naming the codec (receivers learn it from there) and GCM, through the
//!    publish check (`e2ee::check_publish`: no lease, no video). On `TrackPublished` the keys
//!    must confirm GCM (else `e2ee_downgraded`); then a sendonly `m=video` with
//!    `a=rid:<rid> send` and `a=simulcast:send` for those layers, and a renegotiation.
//! 3. **Frames:** for each layer's encoded frame: H.264 normalised (4-byte start codes, no AUD)
//!    and checked (an IDR has SPS and PPS before it; a violation is logged once and the frame
//!    dropped), the RTP timestamp from the capture time (`capture_us × 90 / 1000` from a random
//!    base, the same on every layer), encrypted with LkGcm under the layer's SSRC, packetised
//!    (VP8 RFC 7741 with the T field always written, H.264 mode 1 at MTU 1200), a Dependency
//!    Descriptor on every packet, written nackable (str0m answers NACKs over RTX).
//! 4. **Keyframes:** the SFU's PLI or FIR for a layer, at most one per 300 ms; also for a layer
//!    that starts again (dynacast, the split) and after a renegotiated send section.
//! 5. **Dynacast:** `SubscribedQualityUpdate` sets which layers anyone wants; the others get no
//!    frames (their encoders stay open). Ignored with the policy's `dynacast: false`.
//! 6. **The split** (`video/alloc.rs`): str0m's estimate over the layers, with hysteresis; the
//!    desired bitrate (str0m probes up to it) is the estimate at which every wanted layer runs
//!    at its cap with the raise margin. A layer never runs below its size's floor.
//! 7. **Policy updates:** new caps for the same layers apply in place; a new layer set (a tier
//!    change) republishes the track with the new layers, keeping the lease; video turned off
//!    unpublishes and releases the lease.
//! 8. **The end:** [`crate::Call::unpublish_video`] makes the send section inactive (the SFU
//!    unpublishes the track) and releases the lease when asked. A full reconnect publishes
//!    again on the new PC; a re-join takes the lease again first.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use serde::Serialize;
use sew_lk_compat::Session;
use sew_lkproto::livekit::{
    AddTrackRequest, SimulcastCodec, SubscribedQualityUpdate, TrackInfo, TrackSource, TrackType,
    VideoLayer,
};
use sew_proto::voice::{MediaPolicy, TrackHint};
use tokio::sync::{broadcast, mpsc, oneshot};

use super::alloc::{AllocLayer, Allocator};
use super::rtp::MTU;
use super::rtp::h264::H264Sender;
use super::rtp::normalise::{check, normalise};
use super::rtp::vp8::{FrameLayer, Vp8Sender, Vp8Starts};
use super::types::{VideoCodec, VideoSource};
use crate::call::CallEvent;
use crate::e2ee::{KeySession, Leases, PublishRefused, VideoFrameCodec, check_publish};
use crate::pc::{BWE_INITIAL_BPS, Pc};

/// Encoded frames the engine's video queue holds (all layers of all publications).
pub(crate) const VIDEO_QUEUE: usize = 96;
/// `local_video`'s reason when the app unpublished ([`crate::Call::unpublish_video`]).
pub(crate) const UNPUBLISHED: &str = "unpublished";
/// `local_video`'s reason when the publication's `TrackPublished` came with nobody waiting for
/// its handle (the caller of [`crate::Call::publish_video`] gave up).
pub(crate) const PUBLISHER_GONE: &str = "publisher_gone";
/// How long a publication waits for the SFU's `TrackPublished`.
const PUBLISH_TIMEOUT: Duration = Duration::from_secs(10);
/// Keyframe requests for one layer closer than this coalesce.
const KEYFRAME_EVERY: Duration = Duration::from_millis(300);

/// LiveKit's mime type of a codec (`video/VP8`, `video/H264`).
fn mime(c: VideoCodec) -> &'static str {
    match c {
        VideoCodec::Vp8 => "video/VP8",
        VideoCodec::H264 => "video/H264",
    }
}

fn frame_codec(c: VideoCodec) -> VideoFrameCodec {
    match c {
        VideoCodec::Vp8 => VideoFrameCodec::Vp8,
        VideoCodec::H264 => VideoFrameCodec::H264,
    }
}

/// The codec of Sew's policy value, if the engine sends it (VP8, H.264).
#[cfg(feature = "video-core")]
pub(crate) fn codec_of_policy(c: sew_proto::voice::VideoCodec) -> Option<VideoCodec> {
    match c {
        sew_proto::voice::VideoCodec::Vp8 => Some(VideoCodec::Vp8),
        sew_proto::voice::VideoCodec::H264 => Some(VideoCodec::H264),
        _ => None,
    }
}

fn track_source(s: VideoSource) -> TrackSource {
    match s {
        VideoSource::Camera => TrackSource::Camera,
        VideoSource::ScreenShare => TrackSource::ScreenShare,
    }
}

fn track_name(s: VideoSource) -> &'static str {
    match s {
        VideoSource::Camera => "camera",
        VideoSource::ScreenShare => "screen",
    }
}

/// The policy's hints for a source.
pub(crate) fn hint_of(s: VideoSource, policy: &MediaPolicy) -> &TrackHint {
    match s {
        VideoSource::Camera => &policy.camera,
        VideoSource::ScreenShare => &policy.screen,
    }
}

fn allowed(s: VideoSource, policy: &MediaPolicy) -> bool {
    match s {
        VideoSource::Camera => policy.camera_allowed,
        VideoSource::ScreenShare => policy.screen_allowed,
    }
}

/// One simulcast layer of a publication.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct LayerSpec {
    /// `q`, `h` or `f`.
    pub rid: String,
    /// Coded width (even).
    pub width: u32,
    /// Coded height (even).
    pub height: u32,
    /// The layer's cap, kbit/s.
    pub max_kbps: u32,
    /// Frame rate.
    pub fps: u32,
    /// Temporal layers the encoder makes (1 to 3; VP8 only above 1).
    pub temporal_layers: u8,
}

impl LayerSpec {
    /// A layer with one temporal layer.
    pub fn new(rid: &str, width: u32, height: u32, max_kbps: u32, fps: u32) -> LayerSpec {
        LayerSpec {
            rid: rid.to_owned(),
            width,
            height,
            max_kbps,
            fps,
            temporal_layers: 1,
        }
    }
}

fn even(v: u64) -> u32 {
    u32::try_from(v & !1).unwrap_or(u32::MAX - 1)
}

/// The layers of `hint` for a capture of `capture_width × capture_height`, lowest first. A
/// hint layer's `height` is its short side (`180p`, `360p`, `720p`): each layer whose short side
/// is at most the capture's, the long side scaled to the capture's aspect (even), in the
/// capture's orientation (a portrait phone camera gets portrait layers, 180 × 320 rather than
/// 100 × 180), at the hint's frame rate. A capture smaller than every layer gets the lowest one
/// at the capture's size.
pub fn plan_layers(hint: &TrackHint, capture_width: u32, capture_height: u32) -> Vec<LayerSpec> {
    let (cw, ch) = (capture_width.max(2), capture_height.max(2));
    let portrait = ch > cw;
    let (short, long) = if portrait { (cw, ch) } else { (ch, cw) };
    let fps = hint.max_fps.max(1);
    let mut out: Vec<LayerSpec> = hint
        .layers
        .iter()
        .filter(|l| l.height <= short && l.height > 0)
        .map(|l| {
            let s = even(u64::from(l.height));
            let g = even(u64::from(l.height) * u64::from(long) / u64::from(short)).max(2);
            let (width, height) = if portrait { (s, g) } else { (g, s) };
            LayerSpec::new(&l.rid, width, height, l.max_kbps, fps)
        })
        .collect();
    if out.is_empty()
        && let Some(l) = hint.layers.first()
    {
        out.push(LayerSpec::new(
            &l.rid,
            even(cw.into()),
            even(ch.into()),
            l.max_kbps,
            fps,
        ));
    }
    out
}

/// What to publish ([`crate::Call::publish_video`]).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct VideoPublish {
    /// Camera or screen.
    pub source: VideoSource,
    /// The codec of every layer.
    pub codec: VideoCodec,
    /// The layers whose encoders opened, lowest first ([`plan_layers`]).
    pub layers: Vec<LayerSpec>,
    /// The capture's size (a policy update plans new layers from it).
    pub capture_width: u32,
    /// The capture's height.
    pub capture_height: u32,
}

impl VideoPublish {
    /// A publication of `layers` in `codec` from a capture of `capture_width × capture_height`.
    pub fn new(
        source: VideoSource,
        codec: VideoCodec,
        layers: Vec<LayerSpec>,
        capture_width: u32,
        capture_height: u32,
    ) -> VideoPublish {
        VideoPublish {
            source,
            codec,
            layers,
            capture_width,
            capture_height,
        }
    }
}

/// One encoded plaintext frame of one layer (the engine encrypts it).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct EncodedVideo {
    /// The layer.
    pub rid: String,
    /// The codec.
    pub codec: VideoCodec,
    /// VP8: one frame; H.264: one access unit (normalised by the engine).
    pub data: Bytes,
    /// A keyframe.
    pub keyframe: bool,
    /// The capture time, µs (the same for every layer of one capture).
    pub capture_us: u64,
    /// VP8: the temporal layer (0 = base).
    pub temporal_id: u8,
    /// VP8: RFC 7741's Y bit.
    pub layer_sync: bool,
    /// Coded width.
    pub width: u32,
    /// Coded height.
    pub height: u32,
}

impl EncodedVideo {
    /// A frame of layer `rid` on the base temporal layer.
    pub fn new(
        rid: &str,
        codec: VideoCodec,
        data: Bytes,
        keyframe: bool,
        capture_us: u64,
        width: u32,
        height: u32,
    ) -> EncodedVideo {
        EncodedVideo {
            rid: rid.to_owned(),
            codec,
            data,
            keyframe,
            capture_us,
            temporal_id: 0,
            layer_sync: keyframe,
            width,
            height,
        }
    }

    /// With its temporal layer (VP8).
    pub fn with_layer(mut self, temporal_id: u8, layer_sync: bool) -> EncodedVideo {
        self.temporal_id = temporal_id;
        self.layer_sync = layer_sync;
        self
    }
}

/// One layer's run state ([`VideoControl::Rates`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct LayerRate {
    /// The layer.
    pub rid: String,
    /// Frames are sent: someone wants it (dynacast) and the estimate covers it.
    pub active: bool,
    /// Its encoder's bitrate, bit/s (0 when inactive).
    pub bitrate_bps: u32,
    /// Its frame rate.
    pub fps: u32,
}

/// What the engine tells the producer of a publication.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum VideoControl {
    /// The next frame of this layer must be a keyframe.
    Keyframe {
        /// The layer.
        rid: String,
    },
    /// Which layers run, at what rates (only active layers need frames).
    Rates(Vec<LayerRate>),
    /// The policy changed the layer set: the producer moves its encoders to these layers (the
    /// engine republishes the track; the lease is kept).
    Republish(Vec<LayerSpec>),
    /// The publication ended (`unpublished`, `video_disabled`, `server_unpublished`,
    /// `lease_lost`, …): close the source and the encoders.
    Ended {
        /// Why.
        reason: String,
    },
}

/// Why a lease or a publication failed: [`VideoError::code`] is the plugin's error code.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum VideoError {
    /// The join's policy has video off, or this session may not hold the lease
    /// (`camera_allowed` / `screen_allowed`).
    #[error("video_not_allowed")]
    NotAllowed,
    /// Sew refused: `[voice.video] enabled = false` (`403 policy_denied video_disabled`).
    #[error("video_disabled")]
    Disabled,
    /// The room's camera cap (`422 video_slots_full`).
    #[error("video_slots_full")]
    VideoSlotsFull,
    /// The room's screen-share cap (`422 screen_slots_full`).
    #[error("screen_slots_full")]
    ScreenSlotsFull,
    /// The media budget refuses new video now (`503 media_busy`).
    #[error("media_busy")]
    MediaBusy {
        /// `Retry-After`, seconds.
        retry_after: Option<u64>,
    },
    /// The install's video publisher cap (`503 media_capacity`).
    #[error("media_capacity")]
    MediaCapacity,
    /// No lease for the source (the publish check).
    #[error("no_lease")]
    NoLease,
    /// The SFU answered the publication without GCM: refused for the rest of the call.
    #[error("e2ee_downgraded")]
    Downgraded,
    /// No encoder for any codec the policy allows (or no layer opened).
    #[error("codec_unavailable")]
    CodecUnavailable,
    /// The camera permission is denied or restricted.
    #[error("camera_denied")]
    CameraDenied,
    /// Another app holds the camera.
    #[error("camera_busy")]
    CameraBusy,
    /// No such camera.
    #[error("camera_not_found")]
    CameraNotFound,
    /// The source already has a publication.
    #[error("already_published")]
    AlreadyPublished,
    /// The source has no publication.
    #[error("not_published")]
    NotPublished,
    /// The SFU refused the publication or did not answer in time.
    #[error("publish_failed: {0}")]
    PublishFailed(String),
    /// Sew refused the lease for another reason (`code`, the HTTP status), or the door could
    /// not be reached (status 0).
    #[error("lease_failed: {status} {code}")]
    Lease {
        /// The HTTP status.
        status: u16,
        /// Sew's `error.code` (or the transport's failure).
        code: String,
    },
    /// The call is over.
    #[error("ended")]
    Ended,
}

impl VideoError {
    /// The stable code (the plugin's `{code}`).
    pub fn code(&self) -> &'static str {
        match self {
            VideoError::NotAllowed => "video_not_allowed",
            VideoError::Disabled => "video_disabled",
            VideoError::VideoSlotsFull => "video_slots_full",
            VideoError::ScreenSlotsFull => "screen_slots_full",
            VideoError::MediaBusy { .. } => "media_busy",
            VideoError::MediaCapacity => "media_capacity",
            VideoError::NoLease => "no_lease",
            VideoError::Downgraded => "e2ee_downgraded",
            VideoError::CodecUnavailable => "codec_unavailable",
            VideoError::CameraDenied => "camera_denied",
            VideoError::CameraBusy => "camera_busy",
            VideoError::CameraNotFound => "camera_not_found",
            VideoError::AlreadyPublished => "already_published",
            VideoError::NotPublished => "not_published",
            VideoError::PublishFailed(_) => "publish_failed",
            VideoError::Lease { .. } => "lease_failed",
            VideoError::Ended => "ended",
        }
    }

    /// Sew's refusal of a lease as the error a host shows.
    pub(crate) fn from_lease(r: &crate::api::LeaseRefused) -> VideoError {
        use sew_proto::voice::reasons as vr;
        match (r.status, r.reason.as_deref()) {
            (_, Some(vr::VIDEO_SLOTS_FULL)) => VideoError::VideoSlotsFull,
            (_, Some(vr::SCREEN_SLOTS_FULL)) => VideoError::ScreenSlotsFull,
            (_, Some(vr::MEDIA_BUSY)) => VideoError::MediaBusy {
                retry_after: r.retry_after,
            },
            (_, Some(vr::MEDIA_CAPACITY)) => VideoError::MediaCapacity,
            (_, Some(vr::VIDEO_DISABLED)) => VideoError::Disabled,
            (status, _) => VideoError::Lease {
                status,
                code: r
                    .code
                    .clone()
                    .unwrap_or_else(|| r.detail.clone())
                    .chars()
                    .take(64)
                    .collect(),
            },
        }
    }
}

/// The local video's state ([`CallEvent::LocalVideo`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct LocalVideo {
    /// Camera or screen.
    pub source: VideoSource,
    /// Published now.
    pub on: bool,
    /// Why it went off (`video_disabled`, `server_unpublished`, `lease_lost`, …).
    pub reason: Option<String>,
}

impl LocalVideo {
    /// `source` on or off, and why.
    pub fn new(source: VideoSource, on: bool, reason: Option<String>) -> LocalVideo {
        LocalVideo { source, on, reason }
    }
}

/// The send-side video counters ([`crate::Call::video_stats`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct VideoSendStats {
    /// Frames encrypted and sent, per `source/rid`.
    pub frames_sent: BTreeMap<String, u64>,
    /// Packets written.
    pub packets_sent: u64,
    /// Keyframes sent.
    pub keyframes_sent: u64,
    /// Frames not sent: the layer paused (dynacast or the split), no send path yet, no key,
    /// waiting for a keyframe, or the producer's queue full.
    pub frames_not_sent: u64,
    /// H.264 access units that broke the bitstream rules (dropped).
    pub malformed: u64,
    /// Keyframes asked of the producer (the SFU's requests, resumes).
    pub keyframes_asked: u64,
    /// The SFU's keyframe requests (PLI and FIR).
    pub sfu_keyframe_requests: u64,
    /// Republications after a policy change.
    pub republishes: u64,
    /// The latest send-side estimate, bit/s.
    pub estimate_bps: u64,
}

/// One frame for the engine (the queue between producers and the engine).
#[derive(Debug)]
pub(crate) struct VideoIn {
    pub(crate) publication: u64,
    pub(crate) frame: EncodedVideo,
}

/// A live publication's handle ([`crate::Call::publish_video`]): push frames, follow controls.
#[derive(Debug)]
pub struct VideoPublication {
    source: VideoSource,
    id: u64,
    track_sid: String,
    published_mime: String,
    frames: mpsc::Sender<VideoIn>,
    control: mpsc::UnboundedReceiver<VideoControl>,
    stats: Arc<Mutex<VideoSendStats>>,
    ends: mpsc::UnboundedSender<ProducerEnd>,
}

impl VideoPublication {
    /// Camera or screen.
    pub fn source(&self) -> VideoSource {
        self.source
    }

    /// The SFU's track sid.
    pub fn track_sid(&self) -> &str {
        &self.track_sid
    }

    /// The codec the SFU confirmed in its `TrackPublished` (the track's mime type, e.g.
    /// `video/H264`; its first simulcast codec's when the track's own is empty; empty when
    /// the SFU named none).
    pub fn published_mime(&self) -> &str {
        &self.published_mime
    }

    /// Queues one encoded frame. Never blocks: a full queue drops it (and counts it).
    pub fn push(&self, frame: EncodedVideo) {
        self.sender().push(frame);
    }

    /// A cloneable frame sender (for an encoder thread).
    pub fn sender(&self) -> VideoSender {
        VideoSender {
            id: self.id,
            frames: self.frames.clone(),
            stats: self.stats.clone(),
        }
    }

    /// The next control, if one is queued.
    pub fn try_control(&mut self) -> Option<VideoControl> {
        self.control.try_recv().ok()
    }

    /// The next control (`None`: the call is gone).
    pub async fn control(&mut self) -> Option<VideoControl> {
        self.control.recv().await
    }

    /// The producer can't go on (its source ended, its encoders are gone): the engine
    /// unpublishes the track, releases its lease and says `local_video` off with `reason`
    /// (`source_ended:<why>`, `encoder_failed:<code>`, …). The producer then gets
    /// [`VideoControl::Ended`].
    pub fn end(&self, reason: &str) {
        let _ = self.ends.send(ProducerEnd {
            publication: self.id,
            reason: reason.to_owned(),
        });
    }
}

/// A producer's [`VideoPublication::end`], for the engine.
#[derive(Debug)]
pub(crate) struct ProducerEnd {
    pub(crate) publication: u64,
    pub(crate) reason: String,
}

#[cfg(all(test, feature = "video-core"))]
impl VideoPublication {
    /// A publication outside an engine: its frames on the receiver, its controls from the
    /// sender (the bridge's pipeline tests).
    pub(crate) fn for_test(
        source: VideoSource,
    ) -> (
        VideoPublication,
        mpsc::Receiver<VideoIn>,
        mpsc::UnboundedSender<VideoControl>,
    ) {
        let (p, rx, ctl, _) = VideoPublication::for_test_with_ends(source);
        (p, rx, ctl)
    }

    /// [`VideoPublication::for_test`] with the producer's [`VideoPublication::end`]s, as
    /// `(publication id, reason)`.
    #[allow(clippy::type_complexity)]
    pub(crate) fn for_test_with_ends(
        source: VideoSource,
    ) -> (
        VideoPublication,
        mpsc::Receiver<VideoIn>,
        mpsc::UnboundedSender<VideoControl>,
        mpsc::UnboundedReceiver<ProducerEnd>,
    ) {
        let (frames, rx) = mpsc::channel(VIDEO_QUEUE);
        let (ctl, control) = mpsc::unbounded_channel();
        let (ends, ends_rx) = mpsc::unbounded_channel();
        let p = VideoPublication {
            source,
            id: 1,
            track_sid: "TR_test".to_owned(),
            published_mime: "video/VP8".to_owned(),
            frames,
            control,
            stats: Arc::default(),
            ends,
        };
        (p, rx, ctl, ends_rx)
    }
}

/// The frame half of a [`VideoPublication`], cloneable.
#[derive(Clone)]
pub struct VideoSender {
    id: u64,
    frames: mpsc::Sender<VideoIn>,
    stats: Arc<Mutex<VideoSendStats>>,
}

impl std::fmt::Debug for VideoSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoSender").finish_non_exhaustive()
    }
}

impl VideoSender {
    /// Queues one encoded frame (see [`VideoPublication::push`]).
    pub fn push(&self, frame: EncodedVideo) {
        if self
            .frames
            .try_send(VideoIn {
                publication: self.id,
                frame,
            })
            .is_err()
            && let Ok(mut s) = self.stats.lock()
        {
            s.frames_not_sent += 1;
        }
    }
}

/// Send caps on top of the policy (thermal pressure, low power): applied before the split.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct VideoCaps {
    /// Each layer's cap, percent of the policy's (1 to 100).
    pub bitrate_pct: u8,
    /// At most this many layers, from the lowest (`None`: all).
    pub max_layers: Option<u8>,
    /// At most this frame rate (`None`: the layers').
    pub max_fps: Option<u32>,
}

impl Default for VideoCaps {
    fn default() -> VideoCaps {
        VideoCaps {
            bitrate_pct: 100,
            max_layers: None,
            max_fps: None,
        }
    }
}

impl VideoCaps {
    /// No caps.
    pub fn none() -> VideoCaps {
        VideoCaps::default()
    }

    /// Caps of `bitrate_pct` percent, at most `max_layers` layers and `max_fps`.
    pub fn new(bitrate_pct: u8, max_layers: Option<u8>, max_fps: Option<u32>) -> VideoCaps {
        VideoCaps {
            bitrate_pct: bitrate_pct.clamp(1, 100),
            max_layers,
            max_fps,
        }
    }
}

/// The engine's video commands (from the `Call` handle).
pub(crate) enum VideoCmd {
    Lease {
        source: VideoSource,
        on: bool,
        reply: oneshot::Sender<Result<(), VideoError>>,
    },
    Publish {
        req: VideoPublish,
        reply: oneshot::Sender<Result<VideoPublication, VideoError>>,
    },
    Unpublish {
        source: VideoSource,
        release: bool,
        /// `local_video`'s reason for the off.
        reason: String,
        reply: oneshot::Sender<Result<(), VideoError>>,
    },
    Caps(VideoCaps),
    /// The source's track muted or unmuted at the SFU (`MuteTrackRequest`), keeping the
    /// publication and its lease (a phone app in the background).
    Mute {
        source: VideoSource,
        muted: bool,
    },
}

impl VideoCmd {
    /// Answers the command without running it (a held join: no media).
    pub(crate) fn refuse(self) {
        match self {
            VideoCmd::Lease { reply, .. } | VideoCmd::Unpublish { reply, .. } => {
                let _ = reply.send(Err(VideoError::Ended));
            }
            VideoCmd::Publish { reply, .. } => {
                let _ = reply.send(Err(VideoError::Ended));
            }
            VideoCmd::Caps(_) | VideoCmd::Mute { .. } => {}
        }
    }
}

/// Ends a call's publication from any thread, without waiting (the bridge's pipeline, when
/// its camera or screen ended).
#[cfg(feature = "video-core")]
#[derive(Clone)]
pub(crate) struct VideoCtl {
    cmd: mpsc::UnboundedSender<crate::call::Cmd>,
}

#[cfg(feature = "video-core")]
impl VideoCtl {
    pub(crate) fn new(cmd: mpsc::UnboundedSender<crate::call::Cmd>) -> VideoCtl {
        VideoCtl { cmd }
    }

    /// Unpublishes `source` with `reason` and releases its lease.
    pub(crate) fn end(&self, source: VideoSource, reason: &'static str) {
        let (reply, _) = oneshot::channel();
        let _ = self
            .cmd
            .send(crate::call::Cmd::Publish(VideoCmd::Unpublish {
                source,
                release: true,
                reason: reason.to_owned(),
                reply,
            }));
    }
}

/// A lease call's answer, back on the engine task.
pub(crate) struct LeaseDone {
    source: VideoSource,
    on: bool,
    result: Result<(), VideoError>,
    reply: Option<oneshot::Sender<Result<(), VideoError>>>,
    /// A re-join's lease for a live publication: publish it again on success.
    relink: bool,
}

/// What the publisher's queues deliver to the engine loop.
pub(crate) enum VideoStep {
    Frame(VideoIn),
    Lease(LeaseDone),
    /// A producer ended its publication ([`VideoPublication::end`]).
    End(ProducerEnd),
}

/// The engine's state the publisher works with, borrowed for one call.
pub(crate) struct Ctx<'a> {
    pub(crate) pc: Option<&'a mut Pc>,
    pub(crate) session: &'a mut Session,
    pub(crate) keys: Option<&'a Arc<dyn KeySession>>,
    /// Our participant sid (the msid stream of our send sections).
    pub(crate) stream: &'a str,
    pub(crate) media_up: bool,
    /// The session has joined (an `AddTrackRequest` can go).
    pub(crate) joined: bool,
    pub(crate) now: Instant,
    pub(crate) events: &'a broadcast::Sender<CallEvent>,
}

/// The HTTP side of a lease call.
#[derive(Clone)]
pub(crate) struct LeaseApi {
    pub(crate) http: reqwest::Client,
    pub(crate) base: url::Url,
    pub(crate) token: Option<String>,
}

enum Packetiser {
    Vp8(Vp8Sender),
    H264(H264Sender),
}

struct Layer {
    spec: LayerSpec,
    ssrc: Option<u32>,
    seq: u64,
    pack: Packetiser,
    /// Someone subscribes to it (dynacast).
    wanted: bool,
    /// The split runs it, at this rate (bit/s).
    rate: Option<u64>,
    /// Frames are dropped until a keyframe (a start, a resume, a new send path).
    need_key: bool,
    last_key_ask: Option<Instant>,
    /// What the producer was told last.
    told: Option<LayerRate>,
}

impl Layer {
    fn new(spec: LayerSpec, codec: VideoCodec, first_seq: u64) -> Layer {
        let size = (
            u16::try_from(spec.width).unwrap_or(u16::MAX),
            u16::try_from(spec.height).unwrap_or(u16::MAX),
        );
        let pack = match codec {
            VideoCodec::Vp8 => Packetiser::Vp8(Vp8Sender::new(
                MTU,
                spec.temporal_layers,
                Vp8Starts {
                    picture_id: rand::random::<u16>() & 0x7fff,
                    tl0_pic_idx: rand::random(),
                    frame_number: rand::random(),
                },
                size,
            )),
            VideoCodec::H264 => Packetiser::H264(H264Sender::new(MTU, rand::random(), size)),
        };
        Layer {
            spec,
            ssrc: None,
            seq: first_seq,
            pack,
            wanted: true,
            rate: None,
            need_key: true,
            last_key_ask: None,
            told: None,
        }
    }

    fn active(&self) -> bool {
        self.wanted && self.rate.is_some()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Waiting for a PC and a joined session (or a re-join's lease).
    Pending,
    /// The `AddTrackRequest` is out.
    Requested { since: Instant },
    /// `TrackPublished` came: the send section is negotiating.
    Published,
    /// The send section is negotiated: frames flow.
    Live,
}

/// The caller's answer channel and the publication's control queue, until `TrackPublished`.
type Reply = (
    oneshot::Sender<Result<VideoPublication, VideoError>>,
    mpsc::UnboundedReceiver<VideoControl>,
);

struct Publication {
    id: u64,
    source: VideoSource,
    codec: VideoCodec,
    capture: (u32, u32),
    cid: String,
    state: State,
    track_sid: Option<String>,
    mid: Option<String>,
    pt: Option<u8>,
    layers: Vec<Layer>,
    rtp_base: u32,
    control: mpsc::UnboundedSender<VideoControl>,
    /// The caller's handle, until `TrackPublished` (or a refusal).
    reply: Option<Reply>,
    /// A republication's old send section, made inactive once the new one is published.
    replaces: Option<String>,
    /// The SFU's record of the track, published before the keys started: the keys confirm it
    /// when they do.
    unconfirmed: Option<TrackInfo>,
    /// Muted at the SFU ([`VideoCmd::Mute`]); sent again after a republish or a reconnect.
    muted: bool,
}

impl Publication {
    fn tell(&self, c: VideoControl) {
        let _ = self.control.send(c);
    }

    fn rtp_ts(&self, capture_us: u64) -> u32 {
        // 90 kHz from the capture clock, from a random base; the same on every layer.
        let ticks = capture_us.wrapping_mul(90) / 1000;
        self.rtp_base.wrapping_add(ticks as u32)
    }
}

fn new_cid(source: VideoSource) -> String {
    let p = match source {
        VideoSource::Camera => "cam",
        VideoSource::ScreenShare => "scr",
    };
    format!("{p}-{:08x}", rand::random::<u32>())
}

fn first_seq() -> u64 {
    u64::from(rand::random::<u16>() & 0x7fff)
}

/// LiveKit's quality of the `i`th layer from the lowest (q/h/f = LOW/MEDIUM/HIGH).
fn quality(i: usize) -> i32 {
    i.min(2) as i32
}

/// The engine's video publications.
pub(crate) struct Publisher {
    frames_tx: mpsc::Sender<VideoIn>,
    frames_rx: mpsc::Receiver<VideoIn>,
    lease_tx: mpsc::UnboundedSender<LeaseDone>,
    lease_rx: mpsc::UnboundedReceiver<LeaseDone>,
    ends_tx: mpsc::UnboundedSender<ProducerEnd>,
    ends_rx: mpsc::UnboundedReceiver<ProducerEnd>,
    pubs: Vec<Publication>,
    next_id: u64,
    leases: Leases,
    /// The current policy (the join answer's, then updates).
    policy: Option<MediaPolicy>,
    /// The join answer's `camera_allowed` and `screen_allowed` (kept for the whole join).
    allowed: (bool, bool),
    alloc: Allocator,
    estimate: Option<u64>,
    desired: u64,
    caps: VideoCaps,
    pub(crate) stats: Arc<Mutex<VideoSendStats>>,
    malformed_logged: bool,
}

impl std::fmt::Debug for Publisher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Publisher")
            .field("publications", &self.pubs.len())
            .field("leases", &self.leases)
            .finish_non_exhaustive()
    }
}

impl Publisher {
    pub(crate) fn new(stats: Arc<Mutex<VideoSendStats>>) -> Publisher {
        let (frames_tx, frames_rx) = mpsc::channel(VIDEO_QUEUE);
        let (lease_tx, lease_rx) = mpsc::unbounded_channel();
        let (ends_tx, ends_rx) = mpsc::unbounded_channel();
        Publisher {
            frames_tx,
            frames_rx,
            lease_tx,
            lease_rx,
            ends_tx,
            ends_rx,
            pubs: Vec::new(),
            next_id: 1,
            leases: Leases::none(),
            policy: None,
            allowed: (false, false),
            alloc: Allocator::default(),
            estimate: None,
            desired: 0,
            caps: VideoCaps::default(),
            stats,
            malformed_logged: false,
        }
    }

    /// The next frame or lease answer (cancel-safe).
    pub(crate) async fn recv(&mut self) -> VideoStep {
        tokio::select! {
            Some(f) = self.frames_rx.recv() => VideoStep::Frame(f),
            Some(l) = self.lease_rx.recv() => VideoStep::Lease(l),
            Some(e) = self.ends_rx.recv() => VideoStep::End(e),
        }
    }

    /// A join answer's policy (the first join, a re-join): its `camera_allowed` and
    /// `screen_allowed` hold for the whole join.
    pub(crate) fn joined_with(&mut self, policy: Option<&MediaPolicy>) {
        self.allowed = policy.map_or((false, false), |p| (p.camera_allowed, p.screen_allowed));
        self.policy = policy.cloned();
    }

    /// Whether a new PC needs the video codecs (video on, and a camera or screen allowed).
    pub(crate) fn may_send(&self) -> bool {
        // Like receive, video is negotiated only in a `video-core` build.
        cfg!(feature = "video-core")
            && self
                .policy
                .as_ref()
                .is_some_and(|p| p.video_enabled && (self.allowed.0 || self.allowed.1))
    }

    fn set_lease(&mut self, source: VideoSource, on: bool) {
        let (c, s) = (self.leases.camera, self.leases.screen);
        self.leases = match source {
            VideoSource::Camera => Leases::new(on, s),
            VideoSource::ScreenShare => Leases::new(c, on),
        };
    }

    fn effective_policy(&self) -> Option<MediaPolicy> {
        let mut p = self.policy.clone()?;
        p.camera_allowed = self.allowed.0;
        p.screen_allowed = self.allowed.1;
        Some(p)
    }

    fn stat(&self, f: impl FnOnce(&mut VideoSendStats)) {
        if let Ok(mut s) = self.stats.lock() {
            f(&mut s);
        }
    }

    // -------------------------------------------------------------------------- commands

    pub(crate) fn command(&mut self, cx: &mut Ctx<'_>, api: LeaseApi, cmd: VideoCmd) {
        match cmd {
            VideoCmd::Lease { source, on, reply } => {
                if on {
                    match self.effective_policy() {
                        Some(p) if p.video_enabled && allowed(source, &p) => {}
                        _ => {
                            let _ = reply.send(Err(VideoError::NotAllowed));
                            return;
                        }
                    }
                }
                self.lease(api, source, on, Some(reply), false);
            }
            VideoCmd::Publish { req, reply } => self.publish(cx, req, reply),
            VideoCmd::Unpublish {
                source,
                release,
                reason,
                reply,
            } => {
                let found = self.pubs.iter().position(|p| p.source == source);
                match found {
                    Some(i) => {
                        let p = self.pubs.remove(i);
                        tracing::info!(target: "sew_call_client", source = ?source, %reason, release, "video unpublished by the app");
                        self.end(cx, p, &reason);
                    }
                    None if !release => {
                        let _ = reply.send(Err(VideoError::NotPublished));
                        return;
                    }
                    None => {}
                }
                if release {
                    self.lease(api, source, false, Some(reply), false);
                } else {
                    let _ = reply.send(Ok(()));
                }
            }
            VideoCmd::Caps(c) => {
                self.caps = c;
                self.reallocate(cx);
            }
            VideoCmd::Mute { source, muted } => {
                for p in self.pubs.iter_mut().filter(|p| p.source == source) {
                    if p.muted == muted {
                        continue;
                    }
                    p.muted = muted;
                    if p.state == State::Published
                        && let Some(sid) = &p.track_sid
                    {
                        cx.session.mute(sid, muted);
                    }
                    tracing::info!(target: "sew_call_client", source = ?source, muted, "video muted");
                }
            }
        }
    }

    fn lease(
        &mut self,
        api: LeaseApi,
        source: VideoSource,
        on: bool,
        reply: Option<oneshot::Sender<Result<(), VideoError>>>,
        relink: bool,
    ) {
        let Some(token) = api.token else {
            if let Some(r) = reply {
                let _ = r.send(Err(VideoError::Lease {
                    status: 0,
                    code: "no_call_credential".to_owned(),
                }));
            }
            return;
        };
        let tx = self.lease_tx.clone();
        let (video, stream) = match source {
            VideoSource::Camera => (Some(on), None),
            VideoSource::ScreenShare => (None, Some(on)),
        };
        tokio::spawn(async move {
            let result = crate::api::video_state(&api.http, &api.base, &token, video, stream)
                .await
                .map_err(|r| VideoError::from_lease(&r));
            let _ = tx.send(LeaseDone {
                source,
                on,
                result,
                reply,
                relink,
            });
        });
    }

    /// A producer ended its publication ([`VideoPublication::end`]): unpublished, the lease
    /// released.
    pub(crate) fn on_producer_end(&mut self, cx: &mut Ctx<'_>, api: LeaseApi, e: ProducerEnd) {
        let Some(i) = self.pubs.iter().position(|p| p.id == e.publication) else {
            return;
        };
        let p = self.pubs.remove(i);
        let source = p.source;
        tracing::info!(target: "sew_call_client", source = ?source, reason = %e.reason, "the producer ended its video");
        self.end(cx, p, &e.reason);
        self.lease(api, source, false, None, false);
    }

    pub(crate) fn on_lease(&mut self, cx: &mut Ctx<'_>, d: LeaseDone) {
        if d.result.is_ok() {
            self.set_lease(d.source, d.on);
            tracing::info!(target: "sew_call_client", source = ?d.source, on = d.on, "video lease");
        } else if d.on {
            tracing::info!(target: "sew_call_client", source = ?d.source, error = ?d.result, "video lease refused");
        }
        if d.relink {
            match &d.result {
                Ok(()) => {
                    for i in 0..self.pubs.len() {
                        if self.pubs[i].source == d.source && self.pubs[i].state == State::Pending {
                            self.request(cx, i);
                        }
                    }
                }
                Err(e) => {
                    let reason = e.code().to_owned();
                    let gone: Vec<Publication> = {
                        let (gone, keep) = std::mem::take(&mut self.pubs)
                            .into_iter()
                            .partition(|p| p.source == d.source);
                        self.pubs = keep;
                        gone
                    };
                    for p in gone {
                        self.end(cx, p, &reason);
                    }
                }
            }
        }
        if let Some(r) = d.reply {
            let _ = r.send(d.result);
        }
    }

    fn publish(
        &mut self,
        cx: &mut Ctx<'_>,
        req: VideoPublish,
        reply: oneshot::Sender<Result<VideoPublication, VideoError>>,
    ) {
        match self.effective_policy() {
            // Without `video-core` the PC carries no video codecs (voice-only signalling).
            Some(p)
                if cfg!(feature = "video-core") && p.video_enabled && allowed(req.source, &p) => {}
            _ => {
                let _ = reply.send(Err(VideoError::NotAllowed));
                return;
            }
        }
        if self.pubs.iter().any(|p| p.source == req.source) {
            let _ = reply.send(Err(VideoError::AlreadyPublished));
            return;
        }
        if req.layers.is_empty() {
            let _ = reply.send(Err(VideoError::CodecUnavailable));
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        let (control, control_rx) = mpsc::unbounded_channel();
        let layers = req
            .layers
            .into_iter()
            .map(|l| Layer::new(l, req.codec, first_seq()))
            .collect();
        self.pubs.push(Publication {
            id,
            source: req.source,
            codec: req.codec,
            capture: (req.capture_width, req.capture_height),
            cid: new_cid(req.source),
            state: State::Pending,
            track_sid: None,
            mid: None,
            pt: None,
            layers,
            rtp_base: rand::random(),
            control,
            reply: Some((reply, control_rx)),
            replaces: None,
            unconfirmed: None,
            muted: false,
        });
        let i = self.pubs.len() - 1;
        self.request(cx, i);
    }

    /// The `AddTrackRequest` of publication `i`, through the publish check, if the session can
    /// take it now (else it waits in `Pending`).
    fn request(&mut self, cx: &mut Ctx<'_>, i: usize) {
        if !cx.joined || cx.pc.is_none() {
            self.pubs[i].state = State::Pending;
            return;
        }
        let leases = self.leases;
        let p = &mut self.pubs[i];
        let layers: Vec<VideoLayer> = p
            .layers
            .iter()
            .enumerate()
            .map(|(k, l)| VideoLayer {
                quality: quality(k),
                width: l.spec.width,
                height: l.spec.height,
                bitrate: l.spec.max_kbps.saturating_mul(1000),
                rid: l.spec.rid.clone(),
                ..VideoLayer::default()
            })
            .collect();
        let (w, h) = p
            .layers
            .last()
            .map_or((0, 0), |l| (l.spec.width, l.spec.height));
        let mut add = AddTrackRequest {
            cid: p.cid.clone(),
            name: track_name(p.source).to_owned(),
            r#type: TrackType::Video as i32,
            source: track_source(p.source) as i32,
            width: w,
            height: h,
            layers: layers.clone(),
            simulcast_codecs: vec![SimulcastCodec {
                codec: mime(p.codec).to_owned(),
                cid: p.cid.clone(),
                layers,
                ..SimulcastCodec::default()
            }],
            ..AddTrackRequest::default()
        };
        match check_publish(&mut add, leases) {
            Ok(()) => {
                cx.session.add_track(add);
                p.state = State::Requested { since: cx.now };
            }
            Err(e) => {
                let p = self.pubs.remove(i);
                let err = match e {
                    PublishRefused::NoLease => VideoError::NoLease,
                    PublishRefused::Downgraded => VideoError::Downgraded,
                    other => VideoError::PublishFailed(other.to_string()),
                };
                self.fail(cx, p, err);
            }
        }
    }

    /// A publication that never went live: the caller hears why.
    fn fail(&mut self, cx: &mut Ctx<'_>, mut p: Publication, err: VideoError) {
        self.alloc.forget(p.id);
        let code = err.code().to_owned();
        match p.reply.take() {
            Some((reply, _)) => {
                let _ = reply.send(Err(err));
            }
            None => self.end(cx, p, &code),
        }
    }

    /// The end of a publication: its send section goes inactive, the producer is told.
    fn end(&mut self, cx: &mut Ctx<'_>, mut p: Publication, reason: &str) {
        self.alloc.forget(p.id);
        if let Some(pc) = cx.pc.as_deref_mut() {
            if let Some(mid) = &p.mid {
                pc.stop_send(mid);
            }
            if let Some(mid) = p.replaces.take() {
                pc.stop_send(&mid);
            }
        }
        if let Some((reply, _)) = p.reply.take() {
            let _ = reply.send(Err(VideoError::PublishFailed(reason.to_owned())));
        }
        p.tell(VideoControl::Ended {
            reason: reason.to_owned(),
        });
        let _ = cx.events.send(CallEvent::LocalVideo(LocalVideo {
            source: p.source,
            on: false,
            reason: Some(reason.to_owned()),
        }));
        self.reallocate(cx);
    }

    // -------------------------------------------------------------------------- signalling

    /// `TrackPublished` for `cid`: `true` if it was one of ours.
    pub(crate) fn on_track_published(
        &mut self,
        cx: &mut Ctx<'_>,
        cid: &str,
        track: &TrackInfo,
    ) -> bool {
        let Some(i) = self.pubs.iter().position(|p| p.cid == cid) else {
            return false;
        };
        if !matches!(self.pubs[i].state, State::Requested { .. }) {
            return true;
        }
        let gcm = track.encryption == sew_lkproto::livekit::encryption::Type::Gcm as i32;
        let confirmed = match cx.keys {
            Some(k) => k.published(track),
            // The keys confirm it once they start (`on_keys`); until then no frame can be
            // sealed, and a track without GCM is refused here already.
            None => gcm,
        };
        if !confirmed {
            let p = self.pubs.remove(i);
            self.fail(cx, p, VideoError::Downgraded);
            return true;
        }
        let p = &mut self.pubs[i];
        p.unconfirmed = cx.keys.is_none().then(|| track.clone());
        p.track_sid = Some(track.sid.clone());
        p.state = State::Published;
        p.mid = None;
        p.pt = None;
        for l in &mut p.layers {
            l.ssrc = None;
        }
        let rids: Vec<String> = p.layers.iter().map(|l| l.spec.rid.clone()).collect();
        if let Some(pc) = cx.pc.as_deref_mut() {
            pc.add_send_video(cx.stream, &p.cid, &rids);
        }
        tracing::info!(target: "sew_call_client", source = ?p.source, codec = p.codec.as_str(), layers = rids.len(), "video published");
        if p.muted {
            // Muted before this (re)publication: the SFU hears it again.
            cx.session.mute(&track.sid, true);
        }
        if let Some((reply, control)) = p.reply.take() {
            let handle = VideoPublication {
                source: p.source,
                id: p.id,
                track_sid: track.sid.clone(),
                published_mime: if track.mime_type.is_empty() {
                    track
                        .codecs
                        .first()
                        .map(|c| c.mime_type.clone())
                        .unwrap_or_default()
                } else {
                    track.mime_type.clone()
                },
                frames: self.frames_tx.clone(),
                control,
                stats: self.stats.clone(),
                ends: self.ends_tx.clone(),
            };
            if reply.send(Ok(handle)).is_err() {
                // The caller is gone: nobody produces frames.
                let p = self.pubs.remove(i);
                tracing::info!(target: "sew_call_client", source = ?p.source, "video published with nobody to produce it");
                self.end(cx, p, PUBLISHER_GONE);
                return true;
            }
        }
        let p = &self.pubs[i];
        let _ = cx.events.send(CallEvent::LocalVideo(LocalVideo {
            source: p.source,
            on: true,
            reason: None,
        }));
        true
    }

    /// After an answer: the send paths of publications whose section is now negotiated.
    pub(crate) fn on_answer(&mut self, cx: &mut Ctx<'_>) {
        let Some(pc) = cx.pc.as_deref_mut() else {
            return;
        };
        let mut live = false;
        for p in &mut self.pubs {
            if p.state != State::Published {
                continue;
            }
            let Some(mid) = pc.mid_for_track(&p.cid) else {
                continue;
            };
            let Some(pt) = pc.video_pt(&mid, p.codec) else {
                continue;
            };
            let ssrcs: Vec<Option<u32>> = p
                .layers
                .iter()
                .map(|l| pc.tx_ssrc_rid(&mid, &l.spec.rid))
                .collect();
            if ssrcs.iter().any(Option::is_none) {
                continue;
            }
            for (l, s) in p.layers.iter_mut().zip(ssrcs) {
                l.ssrc = s;
                l.need_key = true;
            }
            p.mid = Some(mid);
            p.pt = Some(pt);
            p.state = State::Live;
            if let Some(old) = p.replaces.take() {
                pc.stop_send(&old);
            }
            live = true;
        }
        if live {
            self.reallocate(cx);
            for i in 0..self.pubs.len() {
                if self.pubs[i].state == State::Live {
                    let rids: Vec<String> = self.pubs[i]
                        .layers
                        .iter()
                        .map(|l| l.spec.rid.clone())
                        .collect();
                    for rid in rids {
                        self.ask_keyframe(i, &rid, cx.now, true);
                    }
                }
            }
        }
    }

    /// The keys started: they confirm the publications the SFU confirmed before (a downgrade
    /// ends the publication).
    pub(crate) fn on_keys(&mut self, cx: &mut Ctx<'_>) {
        let Some(k) = cx.keys else { return };
        let refused: Vec<usize> = self
            .pubs
            .iter_mut()
            .enumerate()
            .filter_map(|(i, p)| {
                let t = p.unconfirmed.take()?;
                (!k.published(&t)).then_some(i)
            })
            .collect();
        for i in refused.into_iter().rev() {
            let p = self.pubs.remove(i);
            self.end(cx, p, VideoError::Downgraded.code());
        }
    }

    /// The server unpublished one of our tracks (a lease it took back, a revoked permission).
    pub(crate) fn on_track_unpublished(&mut self, cx: &mut Ctx<'_>, sid: &str) {
        let Some(i) = self
            .pubs
            .iter()
            .position(|p| p.track_sid.as_deref() == Some(sid))
        else {
            return;
        };
        let p = self.pubs.remove(i);
        tracing::info!(target: "sew_call_client", source = ?p.source, "the SFU unpublished our video");
        self.end(cx, p, "server_unpublished");
    }

    /// Dynacast: the qualities anyone subscribes to (per codec; the old flat list when the
    /// update names no codec of ours).
    #[allow(deprecated)]
    pub(crate) fn on_subscribed_quality(&mut self, cx: &mut Ctx<'_>, u: &SubscribedQualityUpdate) {
        if self.policy.as_ref().is_some_and(|p| !p.dynacast) {
            return;
        }
        let Some(i) = self
            .pubs
            .iter()
            .position(|p| p.track_sid.as_deref() == Some(u.track_sid.as_str()))
        else {
            return;
        };
        let codec = self.pubs[i].codec;
        let qualities = u
            .subscribed_codecs
            .iter()
            .find(|c| {
                let c = c.codec.to_ascii_lowercase();
                c == codec.as_str() || c == mime(codec).to_ascii_lowercase()
            })
            .map(|c| &c.qualities)
            .unwrap_or(&u.subscribed_qualities);
        let mut changed = false;
        for (k, l) in self.pubs[i].layers.iter_mut().enumerate() {
            let wanted = qualities
                .iter()
                .find(|q| q.quality == quality(k))
                .is_none_or(|q| q.enabled);
            if wanted != l.wanted {
                l.wanted = wanted;
                changed = true;
            }
        }
        if changed {
            tracing::debug!(target: "sew_call_client", track = %u.track_sid, "dynacast");
            self.reallocate(cx);
        }
    }

    /// The SFU's PLI or FIR for one of our layers.
    pub(crate) fn on_keyframe_request(&mut self, now: Instant, mid: &str, rid: Option<&str>) {
        let Some(i) = self.pubs.iter().position(|p| p.mid.as_deref() == Some(mid)) else {
            return;
        };
        self.stat(|s| s.sfu_keyframe_requests += 1);
        let rids: Vec<String> = match rid {
            Some(r) => vec![r.to_owned()],
            None => self.pubs[i]
                .layers
                .iter()
                .map(|l| l.spec.rid.clone())
                .collect(),
        };
        for r in rids {
            self.ask_keyframe(i, &r, now, false);
        }
    }

    fn ask_keyframe(&mut self, i: usize, rid: &str, now: Instant, force: bool) {
        let p = &mut self.pubs[i];
        let Some(l) = p.layers.iter_mut().find(|l| l.spec.rid == rid) else {
            return;
        };
        if !force
            && l.last_key_ask
                .is_some_and(|t| now.saturating_duration_since(t) < KEYFRAME_EVERY)
        {
            return;
        }
        l.last_key_ask = Some(now);
        p.tell(VideoControl::Keyframe {
            rid: rid.to_owned(),
        });
        self.stat(|s| s.keyframes_asked += 1);
    }

    /// str0m's new estimate.
    pub(crate) fn on_estimate(&mut self, cx: &mut Ctx<'_>, bps: u64) {
        self.estimate = Some(bps);
        self.stat(|s| s.estimate_bps = bps);
        self.reallocate(cx);
    }

    /// A policy update (`media_policy_updated`, a move's `voice_server_updated`).
    pub(crate) fn on_policy(&mut self, cx: &mut Ctx<'_>, api: &LeaseApi, policy: MediaPolicy) {
        let enabled = policy.video_enabled;
        self.policy = Some(policy);
        if !enabled {
            let gone = std::mem::take(&mut self.pubs);
            let sources: Vec<VideoSource> = gone.iter().map(|p| p.source).collect();
            for p in gone {
                self.end(cx, p, sew_proto::voice::reasons::VIDEO_DISABLED);
            }
            for s in sources {
                self.lease(api.clone(), s, false, None, false);
            }
            return;
        }
        let Some(policy) = self.policy.clone() else {
            return;
        };
        for i in 0..self.pubs.len() {
            let p = &self.pubs[i];
            let mut plan = plan_layers(hint_of(p.source, &policy), p.capture.0, p.capture.1);
            for l in &mut plan {
                if let Some(old) = p.layers.iter().find(|o| o.spec.rid == l.rid) {
                    l.temporal_layers = old.spec.temporal_layers;
                }
            }
            let same = plan.len() == p.layers.len()
                && plan.iter().zip(&p.layers).all(|(n, o)| {
                    n.rid == o.spec.rid && n.width == o.spec.width && n.height == o.spec.height
                });
            if same {
                // New caps (and frame rate) in place.
                let p = &mut self.pubs[i];
                for (n, o) in plan.into_iter().zip(&mut p.layers) {
                    o.spec.max_kbps = n.max_kbps;
                    o.spec.fps = n.fps;
                }
                continue;
            }
            self.republish(cx, i, plan);
        }
        self.reallocate(cx);
    }

    /// A tier change: the track again with the new layers, under the same lease. The old send
    /// section goes inactive once the new one is negotiated; until then the old layers stop.
    fn republish(&mut self, cx: &mut Ctx<'_>, i: usize, plan: Vec<LayerSpec>) {
        if plan.is_empty() {
            return;
        }
        self.stat(|s| s.republishes += 1);
        let p = &mut self.pubs[i];
        tracing::info!(target: "sew_call_client", source = ?p.source, layers = plan.len(), "video republished for the policy");
        self.alloc.forget(p.id);
        let codec = p.codec;
        p.tell(VideoControl::Republish(plan.clone()));
        p.replaces = match (p.mid.take(), p.replaces.take()) {
            (Some(m), None) => Some(m),
            (Some(m), Some(old)) => {
                if let Some(pc) = cx.pc.as_deref_mut() {
                    pc.stop_send(&old);
                }
                Some(m)
            }
            (None, r) => r,
        };
        p.layers = plan
            .into_iter()
            .map(|l| Layer::new(l, codec, first_seq()))
            .collect();
        p.cid = new_cid(p.source);
        p.track_sid = None;
        p.pt = None;
        self.request(cx, i);
    }

    /// A new PC (the first, a full reconnect's): every publication again on it.
    pub(crate) fn on_new_pc(&mut self, cx: &mut Ctx<'_>) {
        for i in 0..self.pubs.len() {
            let p = &mut self.pubs[i];
            p.mid = None;
            p.replaces = None;
            p.pt = None;
            p.track_sid = None;
            p.cid = new_cid(p.source);
            for l in &mut p.layers {
                l.ssrc = None;
                l.seq = first_seq();
                l.need_key = true;
            }
            p.state = State::Pending;
            self.alloc.forget(p.id);
            self.request(cx, i);
        }
    }

    /// The PC went (teardown): nothing is sent until a new one.
    pub(crate) fn on_teardown(&mut self) {
        for p in &mut self.pubs {
            p.mid = None;
            p.replaces = None;
            p.state = State::Pending;
            for l in &mut p.layers {
                l.ssrc = None;
            }
        }
    }

    /// A re-join (a new voice entry): the leases are gone; each publication takes its lease
    /// again, then publishes again.
    pub(crate) fn on_rejoined(&mut self, policy: Option<&MediaPolicy>, api: &LeaseApi) {
        self.joined_with(policy);
        self.leases = Leases::none();
        let mut sources: Vec<VideoSource> = Vec::new();
        for p in &mut self.pubs {
            p.state = State::Pending;
            p.mid = None;
            p.replaces = None;
            if !sources.contains(&p.source) {
                sources.push(p.source);
            }
        }
        for s in sources {
            self.lease(api.clone(), s, true, None, true);
        }
    }

    /// Everything stops (the call ends or is held): the producers are told.
    pub(crate) fn close(&mut self, reason: &str) {
        for mut p in std::mem::take(&mut self.pubs) {
            if let Some((reply, _)) = p.reply.take() {
                let _ = reply.send(Err(VideoError::Ended));
            }
            p.tell(VideoControl::Ended {
                reason: reason.to_owned(),
            });
        }
        self.leases = Leases::none();
    }

    // -------------------------------------------------------------------------- timers

    pub(crate) fn deadline(&self) -> Option<Instant> {
        let publishing = self
            .pubs
            .iter()
            .filter_map(|p| match p.state {
                State::Requested { since } => Some(since + PUBLISH_TIMEOUT),
                _ => None,
            })
            .min();
        match (publishing, self.alloc.deadline()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    pub(crate) fn on_timeout(&mut self, cx: &mut Ctx<'_>) {
        let late: Vec<usize> = self
            .pubs
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                matches!(p.state, State::Requested { since } if cx.now >= since + PUBLISH_TIMEOUT)
            })
            .map(|(i, _)| i)
            .collect();
        for i in late.into_iter().rev() {
            let p = self.pubs.remove(i);
            tracing::info!(target: "sew_call_client", source = ?p.source, "no TrackPublished for the video");
            self.fail(
                cx,
                p,
                VideoError::PublishFailed("no answer from the media server".to_owned()),
            );
        }
        if self.alloc.deadline().is_some_and(|d| cx.now >= d) {
            self.reallocate(cx);
        }
    }

    // -------------------------------------------------------------------------- the split

    fn capped_layers(&self) -> Vec<AllocLayer> {
        let mut out = Vec::new();
        for p in &self.pubs {
            if p.state != State::Live {
                continue;
            }
            for (k, l) in p.layers.iter().enumerate() {
                if self.caps.max_layers.is_some_and(|m| k >= usize::from(m)) {
                    continue;
                }
                let max =
                    u64::from(l.spec.max_kbps) * 1000 * u64::from(self.caps.bitrate_pct) / 100;
                out.push(AllocLayer {
                    key: (p.id, k),
                    screen: p.source == VideoSource::ScreenShare,
                    max_bps: max.max(10_000),
                    wanted: l.wanted,
                    min_bps: super::alloc::min_bitrate_bps(l.spec.height.min(l.spec.width)),
                });
            }
        }
        out
    }

    /// Runs the split and tells each producer which layers run at what rate; a layer that
    /// starts again needs a keyframe.
    fn reallocate(&mut self, cx: &mut Ctx<'_>) {
        let layers = self.capped_layers();
        let est = self.estimate.unwrap_or(BWE_INITIAL_BPS);
        let grants = self.alloc.update(cx.now, est, &layers);
        let desired: u64 = layers.iter().filter(|l| l.wanted).map(|l| l.max_bps).sum();
        if desired != self.desired {
            self.desired = desired;
            if let Some(pc) = cx.pc.as_deref_mut() {
                pc.set_desired_bitrate(desired);
            }
        }
        let max_fps = self.caps.max_fps;
        let mut resumed: Vec<(usize, String)> = Vec::new();
        for (i, p) in self.pubs.iter_mut().enumerate() {
            if p.state != State::Live {
                continue;
            }
            let mut rates = Vec::new();
            let mut changed = false;
            for (k, l) in p.layers.iter_mut().enumerate() {
                let was = l.active();
                l.rate = grants.get(&(p.id, k)).copied();
                let fps = max_fps.map_or(l.spec.fps, |m| l.spec.fps.min(m.max(1)));
                let rate = LayerRate {
                    rid: l.spec.rid.clone(),
                    active: l.active(),
                    bitrate_bps: l.rate.map_or(0, |r| u32::try_from(r).unwrap_or(u32::MAX)),
                    fps,
                };
                if l.active() && !was {
                    l.need_key = true;
                    resumed.push((i, l.spec.rid.clone()));
                }
                if l.told.as_ref() != Some(&rate) {
                    changed = true;
                    l.told = Some(rate.clone());
                }
                rates.push(rate);
            }
            if changed {
                p.tell(VideoControl::Rates(rates));
            }
        }
        for (i, rid) in resumed {
            self.ask_keyframe(i, &rid, cx.now, true);
        }
    }

    // -------------------------------------------------------------------------- frames

    /// One encoded frame from a producer: normalised, encrypted, packetised and written.
    pub(crate) fn on_frame(&mut self, cx: &mut Ctx<'_>, v: VideoIn) {
        let not_sent = |s: &mut VideoSendStats| s.frames_not_sent += 1;
        let Some(i) = self.pubs.iter().position(|p| p.id == v.publication) else {
            self.stat(not_sent);
            return;
        };
        let f = v.frame;
        let (Some(pc), Some(keys)) = (cx.pc.as_deref_mut(), cx.keys) else {
            self.stat(not_sent);
            return;
        };
        let p = &mut self.pubs[i];
        if p.state != State::Live || !cx.media_up || f.codec != p.codec || f.data.is_empty() {
            self.stat(not_sent);
            return;
        }
        let ts = p.rtp_ts(f.capture_us);
        let (Some(mid), Some(pt)) = (p.mid.clone(), p.pt) else {
            self.stat(not_sent);
            return;
        };
        let source = p.source;
        let codec = p.codec;
        let Some(l) = p.layers.iter_mut().find(|l| l.spec.rid == f.rid) else {
            self.stat(not_sent);
            return;
        };
        let Some(ssrc) = l.ssrc else {
            self.stat(not_sent);
            return;
        };
        if !l.active() || (l.need_key && !f.keyframe) {
            self.stat(not_sent);
            return;
        }
        let plain: Vec<u8> = match codec {
            VideoCodec::H264 => {
                let au = normalise(&f.data);
                if let Err(e) = check(&au) {
                    if !self.malformed_logged {
                        self.malformed_logged = true;
                        tracing::warn!(target: "sew_call_client", error = %e, "an H.264 access unit broke the bitstream rules; dropped");
                    }
                    if let Ok(mut s) = self.stats.lock() {
                        s.malformed += 1;
                        s.frames_not_sent += 1;
                    }
                    return;
                }
                au
            }
            VideoCodec::Vp8 => f.data.to_vec(),
        };
        let mut sealed = Vec::with_capacity(plain.len() + 64);
        if keys
            .encrypt_video(frame_codec(codec), &plain, ssrc, ts, &mut sealed)
            .is_err()
            || sealed.is_empty()
        {
            // No send key (or the frame did not encrypt): dropped, never sent in the clear.
            self.stat(not_sent);
            return;
        }
        let packets = match &mut l.pack {
            Packetiser::Vp8(s) => s.packetize(
                &sealed,
                FrameLayer {
                    keyframe: f.keyframe,
                    temporal_id: f.temporal_id,
                    layer_sync: f.layer_sync,
                },
            ),
            Packetiser::H264(s) => match s.packetize(&sealed, f.keyframe) {
                Ok(p) => p,
                Err(_) => {
                    self.stat(not_sent);
                    return;
                }
            },
        };
        let n = packets.len() as u64;
        let mut ok = true;
        for pk in packets {
            let seq = l.seq;
            l.seq += 1;
            if pc
                .write_video(
                    &mid, &f.rid, pt, seq, ts, cx.now, pk.payload, pk.marker, pk.dd,
                )
                .is_err()
            {
                ok = false;
            }
        }
        if f.keyframe {
            l.need_key = false;
        }
        let key = format!("{}/{}", source_name(source), f.rid);
        if let Ok(mut s) = self.stats.lock() {
            if ok {
                *s.frames_sent.entry(key).or_default() += 1;
                s.packets_sent += n;
                s.keyframes_sent += u64::from(f.keyframe);
            } else {
                s.frames_not_sent += 1;
            }
        }
    }
}

fn source_name(s: VideoSource) -> &'static str {
    match s {
        VideoSource::Camera => "camera",
        VideoSource::ScreenShare => "screen",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sew_proto::voice::LayerHint;

    fn hint() -> TrackHint {
        TrackHint {
            max_width: 1280,
            max_height: 720,
            max_fps: 30,
            layers: vec![
                LayerHint {
                    rid: "q".into(),
                    height: 180,
                    max_kbps: 150,
                },
                LayerHint {
                    rid: "h".into(),
                    height: 360,
                    max_kbps: 500,
                },
                LayerHint {
                    rid: "f".into(),
                    height: 720,
                    max_kbps: 1700,
                },
            ],
        }
    }

    #[test]
    fn layers_follow_the_hints_up_to_the_capture() {
        let l = plan_layers(&hint(), 1280, 720);
        let got: Vec<(&str, u32, u32, u32)> = l
            .iter()
            .map(|l| (l.rid.as_str(), l.width, l.height, l.max_kbps))
            .collect();
        assert_eq!(
            got,
            vec![
                ("q", 320, 180, 150),
                ("h", 640, 360, 500),
                ("f", 1280, 720, 1700)
            ]
        );
        assert!(l.iter().all(|l| l.fps == 30 && l.temporal_layers == 1));
        // A 640×480 capture: q and h only, at 4:3 and even widths.
        let l = plan_layers(&hint(), 640, 480);
        let got: Vec<(&str, u32, u32)> = l
            .iter()
            .map(|l| (l.rid.as_str(), l.width, l.height))
            .collect();
        assert_eq!(got, vec![("q", 240, 180), ("h", 480, 360)]);
        // Smaller than every layer: the lowest at the capture's size.
        let l = plan_layers(&hint(), 160, 90);
        assert_eq!((l.len(), l[0].width, l[0].height), (1, 160, 90));
        // A portrait phone camera (720 × 1280): portrait layers on the short side.
        let l = plan_layers(&hint(), 720, 1280);
        let got: Vec<(&str, u32, u32)> = l
            .iter()
            .map(|l| (l.rid.as_str(), l.width, l.height))
            .collect();
        assert_eq!(
            got,
            vec![("q", 180, 320), ("h", 360, 640), ("f", 720, 1280)]
        );
    }

    #[test]
    fn lease_refusals_map_to_the_plugin_codes() {
        let r = |status, reason: Option<&str>, retry| crate::api::LeaseRefused {
            status,
            code: Some("x".to_owned()),
            reason: reason.map(str::to_owned),
            retry_after: retry,
            detail: String::new(),
        };
        assert_eq!(
            VideoError::from_lease(&r(422, Some("video_slots_full"), None)).code(),
            "video_slots_full"
        );
        assert_eq!(
            VideoError::from_lease(&r(503, Some("media_busy"), Some(30))),
            VideoError::MediaBusy {
                retry_after: Some(30)
            }
        );
        assert_eq!(
            VideoError::from_lease(&r(503, Some("media_capacity"), None)).code(),
            "media_capacity"
        );
        assert_eq!(
            VideoError::from_lease(&r(403, Some("video_disabled"), None)).code(),
            "video_disabled"
        );
        assert_eq!(
            VideoError::from_lease(&r(422, Some("screen_slots_full"), None)).code(),
            "screen_slots_full"
        );
        assert_eq!(
            VideoError::from_lease(&r(403, None, None)),
            VideoError::Lease {
                status: 403,
                code: "x".to_owned()
            }
        );
    }

    #[test]
    fn the_rtp_clock_is_the_capture_clock_at_90_khz() {
        let (control, _rx) = mpsc::unbounded_channel();
        let p = Publication {
            id: 1,
            source: VideoSource::Camera,
            codec: VideoCodec::Vp8,
            capture: (1280, 720),
            cid: "c".into(),
            state: State::Pending,
            track_sid: None,
            mid: None,
            pt: None,
            layers: Vec::new(),
            rtp_base: u32::MAX - 10,
            control,
            reply: None,
            replaces: None,
            unconfirmed: None,
            muted: false,
        };
        assert_eq!(p.rtp_ts(0), u32::MAX - 10);
        assert_eq!(p.rtp_ts(1_000_000), (u32::MAX - 10).wrapping_add(90_000));
        assert_eq!(p.rtp_ts(33_333), (u32::MAX - 10).wrapping_add(2_999));
        assert_eq!(quality(0), 0);
        assert_eq!(quality(2), 2);
        assert_eq!(quality(5), 2);
    }
}
