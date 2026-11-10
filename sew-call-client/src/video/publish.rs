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
//!
//! **A backup codec** ([`VideoPublish::with_backup`]: an H.264 publication with a VP8 backup):
//! one `AddTrackRequest` with two `simulcast_codecs` entries, each on its own cid, and two
//! sendonly `m=video` sections in the same offer (each with its own msid track, rids and
//! simulcast). Each codec is a lane with its own layers, SSRCs, packetisers and keyframe
//! state; a frame goes to the lane of its codec. The SFU gives each subscriber the codec its
//! answer decodes and asks for the backup only while someone gets it: the backup lane reads
//! only its own `subscribed_codecs` entry, and the producer hears
//! [`VideoControl::BackupStart`] (open the backup's encoders, the first frame a keyframe),
//! [`VideoControl::BackupRates`], [`VideoControl::BackupKeyframe`] and
//! [`VideoControl::BackupStop`] (close them; the section stays negotiated, without RTP). Both
//! lanes share one bandwidth budget; the backup's lowest layer starts without the raise
//! hysteresis. One GCM check of the track covers both sections; the same keys seal both lanes
//! (distinct SSRCs, so no IV repeats). A producer that can't run its backup, or lost its
//! primary codec, publishes again in one codec ([`VideoPublication::republish_without_backup`]).

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
/// Keyframe requests for one layer closer than this coalesce while the keyframe asked for
/// hasn't gone out yet.
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
    /// A backup codec beside the primary ([`VideoPublish::with_backup`]); `None`: one codec.
    pub backup: Option<BackupSpec>,
}

/// A publication's backup codec ([`VideoPublish::with_backup`]): VP8 beside an H.264 primary,
/// negotiated at publish and run only while some subscriber needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct BackupSpec {
    /// The backup's codec (VP8).
    pub codec: VideoCodec,
    /// Its layers, lowest first (the primary's rids and sizes, or fewer: a phone's stop at
    /// 360p). Quality LOW/MEDIUM/HIGH by position, as the primary's.
    pub layers: Vec<LayerSpec>,
}

impl BackupSpec {
    /// A backup of `layers` in `codec`.
    pub fn new(codec: VideoCodec, layers: Vec<LayerSpec>) -> BackupSpec {
        BackupSpec { codec, layers }
    }
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
            backup: None,
        }
    }

    /// With a backup codec: its section is negotiated with the primary's, and the producer
    /// runs it only between [`VideoControl::BackupStart`] and [`VideoControl::BackupStop`]. A
    /// backup in the primary's codec, or without layers, is ignored.
    pub fn with_backup(mut self, backup: BackupSpec) -> VideoPublish {
        self.backup = Some(backup);
        self
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
    /// The backup is needed (a subscriber gets it): open its encoders for these layers and send
    /// its frames (in its codec), the first a keyframe. Only for a publication with a backup.
    BackupStart(Vec<LayerSpec>),
    /// Which backup layers run, at what rates (a layer active without an encoder yet: open it,
    /// from the backup's layers, the first frame a keyframe).
    BackupRates(Vec<LayerRate>),
    /// The next frame of this backup layer must be a keyframe.
    BackupKeyframe {
        /// The backup layer.
        rid: String,
    },
    /// Nobody needs the backup now: stop its frames and close its encoders.
    BackupStop,
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
    /// The camera opened but gave no frame in time (2.5 s): nothing was published, the camera
    /// closed and the lease released.
    #[error("no_frames")]
    NoFrames,
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
            VideoError::NoFrames => "no_frames",
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
    /// The SFU's keyframe requests per `source/rid` (`source/backup/rid` for a backup's layer;
    /// a request naming no layer counts on each of its section's layers).
    pub sfu_keyframe_requests_by_layer: BTreeMap<String, u64>,
    /// Republications after a policy change (or a fall back to one codec).
    pub republishes: u64,
    /// The latest send-side estimate, bit/s.
    pub estimate_bps: u64,
    /// Frames sent per codec (`vp8`, `h264`), every source and layer (`frames_sent` keys a
    /// backup's layers `source/backup/rid`).
    pub frames_by_codec: BTreeMap<String, u64>,
    /// Keyframes sent per codec.
    pub keyframes_by_codec: BTreeMap<String, u64>,
    /// Times a backup started (some subscriber needed it).
    pub backup_starts: u64,
    /// Times a backup stopped (nobody needed it, a republish).
    pub backup_stops: u64,
    /// How long the backups have run in all, ms.
    pub backup_ms: u64,
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
    fallbacks: mpsc::UnboundedSender<ProducerFallback>,
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

    /// The producer can't run its backup (its encoders don't open), or lost its primary
    /// codec's encoder (the hardware went): the engine publishes the track again in `codec`
    /// alone with `layers`, under the same lease, so no subscriber is promised a codec that
    /// never comes. The producer then gets [`VideoControl::Republish`] with those layers and
    /// sends frames in `codec`.
    pub fn republish_without_backup(&self, codec: VideoCodec, layers: Vec<LayerSpec>) {
        let _ = self.fallbacks.send(ProducerFallback {
            publication: self.id,
            codec,
            layers,
        });
    }
}

/// A producer's [`VideoPublication::end`], for the engine.
#[derive(Debug)]
pub(crate) struct ProducerEnd {
    pub(crate) publication: u64,
    pub(crate) reason: String,
}

/// A producer's [`VideoPublication::republish_without_backup`], for the engine.
#[derive(Debug)]
pub(crate) struct ProducerFallback {
    pub(crate) publication: u64,
    pub(crate) codec: VideoCodec,
    pub(crate) layers: Vec<LayerSpec>,
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
        let (p, rx, ctl, ends, _) = VideoPublication::for_test_with_fallbacks(source);
        (p, rx, ctl, ends)
    }

    /// [`VideoPublication::for_test_with_ends`] with the producer's
    /// [`VideoPublication::republish_without_backup`]s.
    #[allow(clippy::type_complexity)]
    pub(crate) fn for_test_with_fallbacks(
        source: VideoSource,
    ) -> (
        VideoPublication,
        mpsc::Receiver<VideoIn>,
        mpsc::UnboundedSender<VideoControl>,
        mpsc::UnboundedReceiver<ProducerEnd>,
        mpsc::UnboundedReceiver<ProducerFallback>,
    ) {
        let (frames, rx) = mpsc::channel(VIDEO_QUEUE);
        let (ctl, control) = mpsc::unbounded_channel();
        let (ends, ends_rx) = mpsc::unbounded_channel();
        let (fallbacks, fallbacks_rx) = mpsc::unbounded_channel();
        let p = VideoPublication {
            source,
            id: 1,
            track_sid: "TR_test".to_owned(),
            published_mime: "video/VP8".to_owned(),
            frames,
            control,
            stats: Arc::default(),
            ends,
            fallbacks,
        };
        (p, rx, ctl, ends_rx, fallbacks_rx)
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

/// The backup lanes' own caps, on top of [`VideoCaps`] (a phone's thermal steps): at most
/// this many backup layers (never fewer than one) and this frame rate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct BackupCaps {
    pub(crate) max_layers: Option<u8>,
    pub(crate) max_fps: Option<u32>,
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
    /// The bridge's caps for the backup lanes (a phone's thermal steps).
    #[cfg_attr(not(feature = "video-core"), allow(dead_code))]
    BackupCaps(BackupCaps),
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
            VideoCmd::Caps(_) | VideoCmd::BackupCaps(_) | VideoCmd::Mute { .. } => {}
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

    /// The backup lanes' caps (a phone's thermal steps).
    pub(crate) fn backup_caps(&self, caps: BackupCaps) {
        let _ = self
            .cmd
            .send(crate::call::Cmd::Publish(VideoCmd::BackupCaps(caps)));
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
    /// A producer publishes again in one codec ([`VideoPublication::republish_without_backup`]).
    Fallback(ProducerFallback),
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
    /// A keyframe went out since `last_key_ask`: a request now is someone who missed it (a
    /// subscriber that joined after it), not one more ask for the same keyframe.
    key_sent_since_ask: bool,
    /// What the producer was told last.
    told: Option<LayerRate>,
}

impl Layer {
    fn new(spec: LayerSpec, codec: VideoCodec, first_seq: u64, wanted: bool) -> Layer {
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
            wanted,
            rate: None,
            need_key: true,
            last_key_ask: None,
            key_sent_since_ask: false,
            told: None,
        }
    }

    fn active(&self) -> bool {
        self.wanted && self.rate.is_some()
    }
}

/// One codec of a publication and its send section: the primary (lane 0, every layer wanted
/// unless dynacast says otherwise) or the backup (lane 1, VP8, nothing wanted unless its own
/// `subscribed_codecs` entry enables it; its encoders run only while it is on).
struct Lane {
    codec: VideoCodec,
    /// Its `simulcast_codecs` cid and its send section's msid track.
    cid: String,
    mid: Option<String>,
    pt: Option<u8>,
    layers: Vec<Layer>,
    backup: bool,
    /// The backup runs (`BackupStart` told, no `BackupStop` since).
    on: bool,
    /// The backup just started: its lowest wanted layer skips the raise hysteresis until it
    /// runs.
    exempt: bool,
    /// When the backup started.
    since: Option<Instant>,
}

impl Lane {
    fn new(codec: VideoCodec, specs: Vec<LayerSpec>, cid: String, backup: bool) -> Lane {
        Lane {
            codec,
            cid,
            mid: None,
            pt: None,
            layers: specs
                .into_iter()
                .map(|l| Layer::new(l, codec, first_seq(), !backup))
                .collect(),
            backup,
            on: false,
            exempt: false,
            since: None,
        }
    }

    fn rids(&self) -> Vec<String> {
        self.layers.iter().map(|l| l.spec.rid.clone()).collect()
    }

    fn video_layers(&self) -> Vec<VideoLayer> {
        self.layers
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
            .collect()
    }

    /// No send path (a new publication, a new PC): the next answer sets it.
    fn reset_send(&mut self) {
        self.mid = None;
        self.pt = None;
        for l in &mut self.layers {
            l.ssrc = None;
        }
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
    capture: (u32, u32),
    state: State,
    track_sid: Option<String>,
    /// The primary codec's lane, then the backup's (if any).
    lanes: Vec<Lane>,
    rtp_base: u32,
    control: mpsc::UnboundedSender<VideoControl>,
    /// The caller's handle, until `TrackPublished` (or a refusal).
    reply: Option<Reply>,
    /// A republication's old send sections, made inactive once the new ones are published.
    replaces: Vec<String>,
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
        // 90 kHz from the capture clock, from a random base; the same on every layer and lane.
        let ticks = capture_us.wrapping_mul(90) / 1000;
        self.rtp_base.wrapping_add(ticks as u32)
    }

    /// The track's cid (the primary's).
    fn cid(&self) -> &str {
        &self.lanes[0].cid
    }

    /// The primary codec.
    fn codec(&self) -> VideoCodec {
        self.lanes[0].codec
    }

    /// The send sections of every lane.
    fn take_mids(&mut self) -> Vec<String> {
        self.lanes.iter_mut().filter_map(|l| l.mid.take()).collect()
    }

    /// The `AddTrackRequest` before the publish check: the primary's layers, and one
    /// `simulcast_codecs` entry per lane, each on its own cid (the primary's is the track's).
    fn add_track(&self) -> AddTrackRequest {
        let layers = self.lanes[0].video_layers();
        let (w, h) = self.lanes[0]
            .layers
            .last()
            .map_or((0, 0), |l| (l.spec.width, l.spec.height));
        AddTrackRequest {
            cid: self.cid().to_owned(),
            name: track_name(self.source).to_owned(),
            r#type: TrackType::Video as i32,
            source: track_source(self.source) as i32,
            width: w,
            height: h,
            layers,
            simulcast_codecs: self
                .lanes
                .iter()
                .map(|l| SimulcastCodec {
                    codec: mime(l.codec).to_owned(),
                    cid: l.cid.clone(),
                    layers: l.video_layers(),
                    ..SimulcastCodec::default()
                })
                .collect(),
            ..AddTrackRequest::default()
        }
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

/// Whether a `subscribed_codecs` entry's name (`vp8`, `video/VP8`, …) is `codec`.
fn names_codec(name: &str, codec: VideoCodec) -> bool {
    let n = name.rsplit('/').next().unwrap_or(name);
    n.eq_ignore_ascii_case(codec.as_str())
}

/// Dynacast for one lane: which of its `n` layers anyone wants. The primary reads its own
/// codec's entry (the deprecated flat list when there is none); a quality not listed counts as
/// wanted. The backup reads only its own entry: a missing entry or quality is not wanted, and
/// the flat list is ignored.
#[allow(deprecated)]
fn lane_wanted(
    u: &SubscribedQualityUpdate,
    codec: VideoCodec,
    backup: bool,
    n: usize,
) -> Vec<bool> {
    let named = u
        .subscribed_codecs
        .iter()
        .find(|c| names_codec(&c.codec, codec))
        .map(|c| c.qualities.as_slice());
    let qualities = match named {
        Some(q) => q,
        None if backup => &[],
        None => u.subscribed_qualities.as_slice(),
    };
    (0..n)
        .map(|k| {
            let q = qualities.iter().find(|q| q.quality == quality(k));
            if backup {
                q.is_some_and(|q| q.enabled)
            } else {
                q.is_none_or(|q| q.enabled)
            }
        })
        .collect()
}

/// The backup's layers for a new primary plan (a tier change): the plan's layers whose rid the
/// old backup had (a phone's backup stays at its capped rungs), with the old backup's temporal
/// layers; none left: the plan's lowest.
pub(crate) fn backup_layers_for(plan: &[LayerSpec], old: &[LayerSpec]) -> Vec<LayerSpec> {
    let tl = old.first().map_or(1, |l| l.temporal_layers);
    let mut out: Vec<LayerSpec> = plan
        .iter()
        .filter_map(|n| {
            let o = old.iter().find(|o| o.rid == n.rid)?;
            let mut l = n.clone();
            l.temporal_layers = o.temporal_layers;
            Some(l)
        })
        .collect();
    if out.is_empty()
        && let Some(l) = plan.first()
    {
        let mut l = l.clone();
        l.temporal_layers = tl;
        out.push(l);
    }
    out
}

/// The engine's video publications.
pub(crate) struct Publisher {
    frames_tx: mpsc::Sender<VideoIn>,
    frames_rx: mpsc::Receiver<VideoIn>,
    lease_tx: mpsc::UnboundedSender<LeaseDone>,
    lease_rx: mpsc::UnboundedReceiver<LeaseDone>,
    ends_tx: mpsc::UnboundedSender<ProducerEnd>,
    ends_rx: mpsc::UnboundedReceiver<ProducerEnd>,
    fallback_tx: mpsc::UnboundedSender<ProducerFallback>,
    fallback_rx: mpsc::UnboundedReceiver<ProducerFallback>,
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
    /// The bridge's caps for the backup lanes (a phone's thermal steps).
    backup_caps: BackupCaps,
    pub(crate) stats: Arc<Mutex<VideoSendStats>>,
    malformed_logged: bool,
    /// The run time of backups that stopped, ms.
    backup_done_ms: u64,
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
        let (fallback_tx, fallback_rx) = mpsc::unbounded_channel();
        Publisher {
            frames_tx,
            frames_rx,
            lease_tx,
            lease_rx,
            ends_tx,
            ends_rx,
            fallback_tx,
            fallback_rx,
            pubs: Vec::new(),
            next_id: 1,
            leases: Leases::none(),
            policy: None,
            allowed: (false, false),
            alloc: Allocator::default(),
            estimate: None,
            desired: 0,
            caps: VideoCaps::default(),
            backup_caps: BackupCaps::default(),
            stats,
            malformed_logged: false,
            backup_done_ms: 0,
        }
    }

    /// The next frame or lease answer (cancel-safe).
    pub(crate) async fn recv(&mut self) -> VideoStep {
        tokio::select! {
            Some(f) = self.frames_rx.recv() => VideoStep::Frame(f),
            Some(l) = self.lease_rx.recv() => VideoStep::Lease(l),
            Some(e) = self.ends_rx.recv() => VideoStep::End(e),
            Some(b) = self.fallback_rx.recv() => VideoStep::Fallback(b),
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

    /// How long the backups have run in all, up to `now`, ms.
    fn backup_ms(&self, now: Instant) -> u64 {
        let running: u64 = self
            .pubs
            .iter()
            .flat_map(|p| &p.lanes)
            .filter_map(|l| l.since.filter(|_| l.on))
            .map(|t| now.saturating_duration_since(t).as_millis() as u64)
            .sum();
        self.backup_done_ms + running
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
            VideoCmd::BackupCaps(c) => {
                if c != self.backup_caps {
                    self.backup_caps = c;
                    self.reallocate(cx);
                }
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

    /// A producer can't send its backup, or lost its primary codec
    /// ([`VideoPublication::republish_without_backup`]): the publication is published again in
    /// one codec with its new layers, under the same lease (the tier-change path).
    pub(crate) fn on_producer_fallback(&mut self, cx: &mut Ctx<'_>, f: ProducerFallback) {
        let Some(i) = self.pubs.iter().position(|p| p.id == f.publication) else {
            return;
        };
        if f.layers.is_empty() {
            return;
        }
        let now = cx.now;
        self.backup_stop(i, now);
        let p = &mut self.pubs[i];
        tracing::info!(target: "sew_call_client", source = ?p.source, from = p.codec().as_str(), to = f.codec.as_str(), "video published again in one codec");
        p.lanes.truncate(1);
        p.lanes[0].codec = f.codec;
        self.republish(cx, i, f.layers);
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
        let mut lanes = vec![Lane::new(req.codec, req.layers, new_cid(req.source), false)];
        // A backup only in another codec, with layers.
        if let Some(b) = req
            .backup
            .filter(|b| b.codec != req.codec && !b.layers.is_empty())
        {
            lanes.push(Lane::new(b.codec, b.layers, new_cid(req.source), true));
        }
        self.pubs.push(Publication {
            id,
            source: req.source,
            capture: (req.capture_width, req.capture_height),
            state: State::Pending,
            track_sid: None,
            lanes,
            rtp_base: rand::random(),
            control,
            reply: Some((reply, control_rx)),
            replaces: Vec::new(),
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
        let mut add = p.add_track();
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

    /// The end of a publication: its send sections go inactive, the producer is told.
    fn end(&mut self, cx: &mut Ctx<'_>, mut p: Publication, reason: &str) {
        self.alloc.forget(p.id);
        for l in p.lanes.iter_mut().filter(|l| l.on) {
            l.on = false;
            if let Some(t) = l.since.take() {
                self.backup_done_ms += cx.now.saturating_duration_since(t).as_millis() as u64;
            }
        }
        if let Some(pc) = cx.pc.as_deref_mut() {
            for mid in p.take_mids() {
                pc.stop_send(&mid);
            }
            for mid in std::mem::take(&mut p.replaces) {
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

    // -------------------------------------------------------------------------- the backup

    /// The backup of publication `i` starts when some layer of it is wanted (the producer
    /// opens its encoders, the first frame a keyframe; no signalling) and stops when none is
    /// (the producer closes them; the section stays negotiated, without RTP).
    fn backup_transitions(&mut self, i: usize, now: Instant) {
        let p = &mut self.pubs[i];
        let Some(b) = p.lanes.get_mut(1) else {
            return;
        };
        let any = b.layers.iter().any(|l| l.wanted);
        if any && !b.on {
            b.on = true;
            b.exempt = true;
            b.since = Some(now);
            let mut start = Vec::new();
            for l in &mut b.layers {
                l.need_key = true;
                l.rate = None;
                l.told = None;
                if l.wanted {
                    start.push(l.spec.clone());
                }
            }
            tracing::info!(target: "sew_call_client", source = ?p.source, codec = b.codec.as_str(), layers = start.len(), "the backup starts");
            p.tell(VideoControl::BackupStart(start));
            self.stat(|s| s.backup_starts += 1);
        } else if !any && b.on {
            self.backup_stop(i, now);
        }
    }

    /// The backup of publication `i` stops (if it runs): its encoders close.
    fn backup_stop(&mut self, i: usize, now: Instant) {
        let p = &mut self.pubs[i];
        let Some(b) = p.lanes.get_mut(1).filter(|b| b.on) else {
            return;
        };
        b.on = false;
        b.exempt = false;
        for l in &mut b.layers {
            l.rate = None;
            l.told = None;
        }
        let ran = b
            .since
            .take()
            .map_or(0, |t| now.saturating_duration_since(t).as_millis() as u64);
        tracing::info!(target: "sew_call_client", source = ?p.source, ran_ms = ran, "the backup stops");
        p.tell(VideoControl::BackupStop);
        self.backup_done_ms += ran;
        let total = self.backup_ms(now);
        self.stat(|s| {
            s.backup_stops += 1;
            s.backup_ms = total;
        });
    }

    /// The backup of publication `i` off and not wanted (a new PC, a teardown, a republish):
    /// dynacast turns it on again.
    fn backup_reset(&mut self, i: usize, now: Instant) {
        self.backup_stop(i, now);
        if let Some(b) = self.pubs[i].lanes.get_mut(1) {
            for l in &mut b.layers {
                l.wanted = false;
            }
        }
    }

    // -------------------------------------------------------------------------- signalling

    /// `TrackPublished` for `cid`: `true` if it was one of ours.
    pub(crate) fn on_track_published(
        &mut self,
        cx: &mut Ctx<'_>,
        cid: &str,
        track: &TrackInfo,
    ) -> bool {
        let Some(i) = self.pubs.iter().position(|p| p.cid() == cid) else {
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
            // One check for the track covers both codecs: neither section is ever added.
            let p = self.pubs.remove(i);
            self.fail(cx, p, VideoError::Downgraded);
            return true;
        }
        let p = &mut self.pubs[i];
        if p.lanes.len() > 1
            && !track.codecs.is_empty()
            && !track
                .codecs
                .iter()
                .any(|c| names_codec(&c.mime_type, p.lanes[1].codec))
        {
            // The SFU kept one codec (a room without backup codecs): no backup section.
            tracing::info!(target: "sew_call_client", source = ?p.source, "the SFU took no backup codec: one codec");
            p.lanes.truncate(1);
        }
        p.unconfirmed = cx.keys.is_none().then(|| track.clone());
        p.track_sid = Some(track.sid.clone());
        p.state = State::Published;
        for l in &mut p.lanes {
            l.reset_send();
        }
        if let Some(pc) = cx.pc.as_deref_mut() {
            // Every lane's section in the same offer.
            for l in &p.lanes {
                pc.add_send_video(cx.stream, &l.cid, &l.rids());
            }
        }
        tracing::info!(target: "sew_call_client", source = ?p.source, codec = p.codec().as_str(), backup = p.lanes.get(1).map(|l| l.codec.as_str()), layers = p.lanes[0].layers.len(), "video published");
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
                fallbacks: self.fallback_tx.clone(),
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

    /// After an answer: the send paths of publications whose sections are now negotiated.
    pub(crate) fn on_answer(&mut self, cx: &mut Ctx<'_>) {
        let Some(pc) = cx.pc.as_deref_mut() else {
            return;
        };
        // A lane's section: its mid, payload type and one SSRC per layer, once negotiated.
        let resolve = |pc: &mut Pc, l: &Lane| -> Option<(String, u8, Vec<u32>)> {
            let mid = pc.mid_for_track(&l.cid)?;
            let pt = pc.video_pt(&mid, l.codec)?;
            let ssrcs: Option<Vec<u32>> = l
                .layers
                .iter()
                .map(|x| pc.tx_ssrc_rid(&mid, &x.spec.rid))
                .collect();
            Some((mid, pt, ssrcs?))
        };
        let mut live = false;
        for p in &mut self.pubs {
            if p.state != State::Published {
                continue;
            }
            let Some(primary) = resolve(pc, &p.lanes[0]) else {
                continue;
            };
            let backup = p.lanes.get(1).map(|l| resolve(pc, l));
            if let Some(None) = backup {
                // The backup's section was refused (or lost its codec): one codec.
                tracing::warn!(target: "sew_call_client", source = ?p.source, "the backup's send section was not negotiated: one codec");
                if let Some(mid) = pc.mid_for_track(&p.lanes[1].cid) {
                    pc.stop_send(&mid);
                }
                if p.lanes[1].on {
                    p.tell(VideoControl::BackupStop);
                }
                p.lanes.truncate(1);
            }
            let found = std::iter::once(primary).chain(backup.flatten());
            for (l, (mid, pt, ssrcs)) in p.lanes.iter_mut().zip(found) {
                for (x, s) in l.layers.iter_mut().zip(ssrcs) {
                    x.ssrc = Some(s);
                    x.need_key = true;
                }
                l.mid = Some(mid);
                l.pt = Some(pt);
            }
            p.state = State::Live;
            for old in std::mem::take(&mut p.replaces) {
                pc.stop_send(&old);
            }
            live = true;
        }
        if live {
            self.reallocate(cx);
            for i in 0..self.pubs.len() {
                if self.pubs[i].state != State::Live {
                    continue;
                }
                for n in 0..self.pubs[i].lanes.len() {
                    let rids = self.pubs[i].lanes[n].rids();
                    for rid in rids {
                        self.ask_keyframe(i, n, &rid, cx.now, true);
                    }
                }
            }
        }
    }

    /// The keys started: they confirm the publications the SFU confirmed before (a downgrade
    /// ends the publication, both codecs).
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

    /// Dynacast: the qualities anyone subscribes to, each lane from its own codec's entry
    /// ([`lane_wanted`]); the backup starts or stops with it.
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
        let mut changed = false;
        for lane in &mut self.pubs[i].lanes {
            let wanted = lane_wanted(u, lane.codec, lane.backup, lane.layers.len());
            for (l, w) in lane.layers.iter_mut().zip(wanted) {
                if w != l.wanted {
                    l.wanted = w;
                    changed = true;
                }
            }
        }
        if changed {
            tracing::debug!(target: "sew_call_client", track = %u.track_sid, "dynacast");
            self.backup_transitions(i, cx.now);
            self.reallocate(cx);
        }
    }

    /// The SFU's PLI or FIR for one of our layers, matched to the lane by its section's mid,
    /// then to the layer by rid.
    pub(crate) fn on_keyframe_request(&mut self, now: Instant, mid: &str, rid: Option<&str>) {
        let Some((i, n)) = self.pubs.iter().enumerate().find_map(|(i, p)| {
            p.lanes
                .iter()
                .position(|l| l.mid.as_deref() == Some(mid))
                .map(|n| (i, n))
        }) else {
            return;
        };
        self.stat(|s| s.sfu_keyframe_requests += 1);
        let rids: Vec<String> = match rid {
            Some(r) => vec![r.to_owned()],
            None => self.pubs[i].lanes[n].rids(),
        };
        let (source, backup) = (self.pubs[i].source, self.pubs[i].lanes[n].backup);
        for r in &rids {
            let key = if backup {
                format!("{}/backup/{r}", source_name(source))
            } else {
                format!("{}/{r}", source_name(source))
            };
            self.stat(|s| *s.sfu_keyframe_requests_by_layer.entry(key).or_default() += 1);
        }
        for r in rids {
            self.ask_keyframe(i, n, &r, now, false);
        }
    }

    fn ask_keyframe(&mut self, i: usize, n: usize, rid: &str, now: Instant, force: bool) {
        let p = &mut self.pubs[i];
        let Some(lane) = p.lanes.get_mut(n) else {
            return;
        };
        if lane.backup && !lane.on {
            // No encoder runs: its start makes a keyframe anyway.
            return;
        }
        let backup = lane.backup;
        let Some(l) = lane.layers.iter_mut().find(|l| l.spec.rid == rid) else {
            return;
        };
        if !force
            && !l.key_sent_since_ask
            && l.last_key_ask
                .is_some_and(|t| now.saturating_duration_since(t) < KEYFRAME_EVERY)
        {
            return;
        }
        l.last_key_ask = Some(now);
        l.key_sent_since_ask = false;
        let rid = rid.to_owned();
        p.tell(if backup {
            VideoControl::BackupKeyframe { rid }
        } else {
            VideoControl::Keyframe { rid }
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
        // The backup only while the policy allows one (`backup_codec`, dynacast, its codec
        // listed): otherwise the track is published again without it.
        let backup_allowed = |c: VideoCodec| {
            policy.backup_codec
                && policy.dynacast
                && policy.codecs.iter().any(|p| codec_of_policy(*p) == Some(c))
        };
        for i in 0..self.pubs.len() {
            let p = &self.pubs[i];
            let mut plan = plan_layers(hint_of(p.source, &policy), p.capture.0, p.capture.1);
            for l in &mut plan {
                if let Some(old) = p.lanes[0].layers.iter().find(|o| o.spec.rid == l.rid) {
                    l.temporal_layers = old.spec.temporal_layers;
                }
            }
            let drop_backup = p.lanes.get(1).is_some_and(|b| !backup_allowed(b.codec));
            let same = plan.len() == p.lanes[0].layers.len()
                && plan.iter().zip(&p.lanes[0].layers).all(|(n, o)| {
                    n.rid == o.spec.rid && n.width == o.spec.width && n.height == o.spec.height
                });
            if same && !drop_backup {
                // New caps (and frame rate) in place, on both lanes.
                let p = &mut self.pubs[i];
                for lane in &mut p.lanes {
                    for o in &mut lane.layers {
                        if let Some(n) = plan.iter().find(|n| n.rid == o.spec.rid) {
                            o.spec.max_kbps = n.max_kbps;
                            o.spec.fps = n.fps;
                        }
                    }
                }
                continue;
            }
            if drop_backup {
                tracing::info!(target: "sew_call_client", source = ?p.source, "the policy no longer allows the backup: one codec");
                self.backup_stop(i, cx.now);
                self.pubs[i].lanes.truncate(1);
            }
            self.republish(cx, i, plan);
        }
        self.reallocate(cx);
    }

    /// A tier change (or a fall back to one codec): the track again with the new layers, under
    /// the same lease. The old send sections go inactive once the new ones are negotiated;
    /// until then the old layers stop. The backup (if any) starts off again.
    fn republish(&mut self, cx: &mut Ctx<'_>, i: usize, plan: Vec<LayerSpec>) {
        if plan.is_empty() {
            return;
        }
        self.stat(|s| s.republishes += 1);
        self.backup_reset(i, cx.now);
        let p = &mut self.pubs[i];
        tracing::info!(target: "sew_call_client", source = ?p.source, layers = plan.len(), "video republished");
        self.alloc.forget(p.id);
        p.tell(VideoControl::Republish(plan.clone()));
        let mids = p.take_mids();
        if !mids.is_empty() {
            // The sections before the last republish's (never negotiated) go now.
            if let Some(pc) = cx.pc.as_deref_mut() {
                for old in std::mem::take(&mut p.replaces) {
                    pc.stop_send(&old);
                }
            }
            p.replaces = mids;
        }
        let source = p.source;
        let backup = p.lanes.get(1).map(|b| {
            let old: Vec<LayerSpec> = b.layers.iter().map(|l| l.spec.clone()).collect();
            (b.codec, backup_layers_for(&plan, &old))
        });
        if let Some((codec, layers)) = backup {
            p.lanes[1] = Lane::new(codec, layers, new_cid(source), true);
        }
        let codec = p.lanes[0].codec;
        p.lanes[0] = Lane::new(codec, plan, new_cid(source), false);
        p.track_sid = None;
        self.request(cx, i);
    }

    /// A new PC (the first, a full reconnect's): every publication again on it.
    pub(crate) fn on_new_pc(&mut self, cx: &mut Ctx<'_>) {
        for i in 0..self.pubs.len() {
            self.backup_reset(i, cx.now);
            let p = &mut self.pubs[i];
            p.replaces.clear();
            p.track_sid = None;
            for lane in &mut p.lanes {
                lane.reset_send();
                lane.cid = new_cid(p.source);
                for l in &mut lane.layers {
                    l.seq = first_seq();
                    l.need_key = true;
                }
            }
            p.state = State::Pending;
            self.alloc.forget(p.id);
            self.request(cx, i);
        }
    }

    /// The PC went (teardown): nothing is sent until a new one.
    pub(crate) fn on_teardown(&mut self) {
        let now = Instant::now();
        for i in 0..self.pubs.len() {
            self.backup_reset(i, now);
            let p = &mut self.pubs[i];
            p.replaces.clear();
            p.state = State::Pending;
            for lane in &mut p.lanes {
                lane.reset_send();
            }
        }
    }

    /// A re-join (a new voice entry): the leases are gone; each publication takes its lease
    /// again, then publishes again.
    pub(crate) fn on_rejoined(&mut self, policy: Option<&MediaPolicy>, api: &LeaseApi) {
        self.joined_with(policy);
        self.leases = Leases::none();
        let now = Instant::now();
        let mut sources: Vec<VideoSource> = Vec::new();
        for i in 0..self.pubs.len() {
            self.backup_reset(i, now);
            let p = &mut self.pubs[i];
            p.state = State::Pending;
            p.replaces.clear();
            for lane in &mut p.lanes {
                lane.mid = None;
            }
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

    /// Every running lane's layers under the caps (the policy's, the host's thermal and power
    /// caps on both lanes; the backup's own caps on top). The backup keeps at least its lowest
    /// layer whatever the caps: it is a subscriber's only video.
    fn capped_layers(&self) -> Vec<AllocLayer> {
        let mut out = Vec::new();
        for p in &self.pubs {
            if p.state != State::Live {
                continue;
            }
            for (n, lane) in p.lanes.iter().enumerate() {
                if lane.backup && !lane.on {
                    continue;
                }
                let max_layers = if lane.backup {
                    let caps = [self.caps.max_layers, self.backup_caps.max_layers];
                    caps.into_iter().flatten().min().map(|m| m.max(1))
                } else {
                    self.caps.max_layers
                };
                let lowest_wanted = lane.layers.iter().position(|l| l.wanted);
                for (k, l) in lane.layers.iter().enumerate() {
                    if max_layers.is_some_and(|m| k >= usize::from(m)) {
                        continue;
                    }
                    let policy_max = u64::from(l.spec.max_kbps) * 1000;
                    let mut max = policy_max * u64::from(self.caps.bitrate_pct) / 100;
                    let min_bps = super::alloc::min_bitrate_bps(l.spec.height.min(l.spec.width));
                    let lowest = lowest_wanted == Some(k);
                    if lane.backup && lowest {
                        // A bitrate cap (heat, low power) never takes the backup's lowest layer
                        // below its floor: it would pause a subscriber's only video.
                        max = max.max(policy_max.min(min_bps));
                    }
                    out.push(AllocLayer {
                        key: (p.id, n, k),
                        screen: p.source == VideoSource::ScreenShare,
                        max_bps: max.max(10_000),
                        wanted: l.wanted,
                        min_bps,
                        exempt: lane.backup && lane.exempt && lowest,
                    });
                }
            }
        }
        out
    }

    /// The frame rate a layer of `lane` runs at under the caps.
    fn fps_of(&self, lane: &Lane, spec: &LayerSpec) -> u32 {
        let mut fps = spec.fps;
        if let Some(m) = self.caps.max_fps {
            fps = fps.min(m.max(1));
        }
        if lane.backup
            && let Some(m) = self.backup_caps.max_fps
        {
            fps = fps.min(m.max(1));
        }
        fps
    }

    /// Runs the split and tells each producer which layers run at what rate (the backup's in
    /// `BackupRates`, while it runs); a layer that starts again needs a keyframe.
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
        let mut resumed: Vec<(usize, usize, String)> = Vec::new();
        let mut tells: Vec<(usize, VideoControl)> = Vec::new();
        for i in 0..self.pubs.len() {
            if self.pubs[i].state != State::Live {
                continue;
            }
            let id = self.pubs[i].id;
            for n in 0..self.pubs[i].lanes.len() {
                let fps: Vec<u32> = {
                    let lane = &self.pubs[i].lanes[n];
                    lane.layers
                        .iter()
                        .map(|l| self.fps_of(lane, &l.spec))
                        .collect()
                };
                let lane = &mut self.pubs[i].lanes[n];
                if lane.backup && !lane.on {
                    continue;
                }
                let mut rates = Vec::new();
                let mut changed = false;
                for (k, l) in lane.layers.iter_mut().enumerate() {
                    let was = l.active();
                    l.rate = grants.get(&(id, n, k)).copied();
                    let rate = LayerRate {
                        rid: l.spec.rid.clone(),
                        active: l.active(),
                        bitrate_bps: l.rate.map_or(0, |r| u32::try_from(r).unwrap_or(u32::MAX)),
                        fps: fps[k],
                    };
                    if l.active() && !was {
                        l.need_key = true;
                        resumed.push((i, n, l.spec.rid.clone()));
                    }
                    if l.told.as_ref() != Some(&rate) {
                        changed = true;
                        l.told = Some(rate.clone());
                    }
                    rates.push(rate);
                }
                if lane.exempt && lane.layers.iter().any(Layer::active) {
                    // Its lowest layer runs: the rest follow the hysteresis.
                    lane.exempt = false;
                }
                if changed {
                    tells.push((
                        i,
                        if lane.backup {
                            VideoControl::BackupRates(rates)
                        } else {
                            VideoControl::Rates(rates)
                        },
                    ));
                }
            }
        }
        for (i, c) in tells {
            self.pubs[i].tell(c);
        }
        for (i, n, rid) in resumed {
            self.ask_keyframe(i, n, &rid, cx.now, true);
        }
    }

    // -------------------------------------------------------------------------- frames

    /// One encoded frame from a producer, to the lane of its codec: normalised, encrypted,
    /// packetised and written on that lane's section.
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
        if p.state != State::Live || !cx.media_up || f.data.is_empty() {
            self.stat(not_sent);
            return;
        }
        let ts = p.rtp_ts(f.capture_us);
        let source = p.source;
        let Some(lane) = p.lanes.iter_mut().find(|l| l.codec == f.codec) else {
            self.stat(not_sent);
            return;
        };
        if lane.backup && !lane.on {
            // Not asked for (or stopped): nothing on the backup's section.
            self.stat(not_sent);
            return;
        }
        let (Some(mid), Some(pt)) = (lane.mid.clone(), lane.pt) else {
            self.stat(not_sent);
            return;
        };
        let codec = lane.codec;
        let backup = lane.backup;
        let Some(l) = lane.layers.iter_mut().find(|l| l.spec.rid == f.rid) else {
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
            l.key_sent_since_ask = true;
        }
        let key = if backup {
            format!("{}/backup/{}", source_name(source), f.rid)
        } else {
            format!("{}/{}", source_name(source), f.rid)
        };
        let backup_ms = backup.then(|| self.backup_ms(cx.now));
        if let Ok(mut s) = self.stats.lock() {
            if ok {
                *s.frames_sent.entry(key).or_default() += 1;
                *s.frames_by_codec
                    .entry(codec.as_str().to_owned())
                    .or_default() += 1;
                s.packets_sent += n;
                s.keyframes_sent += u64::from(f.keyframe);
                if f.keyframe {
                    *s.keyframes_by_codec
                        .entry(codec.as_str().to_owned())
                        .or_default() += 1;
                }
            } else {
                s.frames_not_sent += 1;
            }
            if let Some(ms) = backup_ms {
                s.backup_ms = ms;
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
            capture: (1280, 720),
            state: State::Pending,
            track_sid: None,
            lanes: vec![Lane::new(VideoCodec::Vp8, Vec::new(), "c".into(), false)],
            rtp_base: u32::MAX - 10,
            control,
            reply: None,
            replaces: Vec::new(),
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

    fn three() -> Vec<LayerSpec> {
        vec![
            LayerSpec::new("q", 320, 180, 150, 30),
            LayerSpec::new("h", 640, 360, 500, 30),
            LayerSpec::new("f", 1280, 720, 1700, 30),
        ]
    }

    /// A camera publication of H.264 q/h/f with a VP8 backup of `backup` layers.
    fn with_backup(backup: Vec<LayerSpec>) -> Publication {
        let (control, _rx) = mpsc::unbounded_channel();
        Publication {
            id: 7,
            source: VideoSource::Camera,
            capture: (1280, 720),
            state: State::Pending,
            track_sid: None,
            lanes: vec![
                Lane::new(VideoCodec::H264, three(), "cam-a".into(), false),
                Lane::new(VideoCodec::Vp8, backup, "cam-b".into(), true),
            ],
            rtp_base: 0,
            control,
            reply: None,
            replaces: Vec::new(),
            unconfirmed: None,
            muted: false,
        }
    }

    #[test]
    fn a_backup_is_one_add_track_with_two_codecs_on_their_own_cids_under_gcm() {
        let mut b = three();
        b.truncate(2);
        let p = with_backup(b);
        let mut add = p.add_track();
        check_publish(&mut add, Leases::new(true, false)).expect("leased");
        assert_eq!(add.cid, "cam-a");
        assert_eq!(
            add.encryption,
            sew_lkproto::livekit::encryption::Type::Gcm as i32,
            "one GCM request covers both codecs"
        );
        assert_eq!((add.width, add.height), (1280, 720));
        assert_eq!(add.layers.len(), 3);
        let codecs: Vec<(&str, &str, Vec<&str>)> = add
            .simulcast_codecs
            .iter()
            .map(|c| {
                (
                    c.codec.as_str(),
                    c.cid.as_str(),
                    c.layers.iter().map(|l| l.rid.as_str()).collect(),
                )
            })
            .collect();
        assert_eq!(
            codecs,
            vec![
                ("video/H264", "cam-a", vec!["q", "h", "f"]),
                ("video/VP8", "cam-b", vec!["q", "h"]),
            ]
        );
        assert_eq!(add.simulcast_codecs[1].layers[1].quality, 1);
        // The backup starts with nothing wanted; the primary with everything.
        assert!(p.lanes[1].layers.iter().all(|l| !l.wanted));
        assert!(p.lanes[0].layers.iter().all(|l| l.wanted));
        // Without the lease: refused, both codecs.
        let mut add = p.add_track();
        assert_eq!(
            check_publish(&mut add, Leases::none()),
            Err(PublishRefused::NoLease)
        );
    }

    #[test]
    #[allow(deprecated)]
    fn each_lane_reads_its_own_codecs_entry() {
        use sew_lkproto::livekit::{SubscribedCodec, SubscribedQuality};
        let q = |quality: i32, enabled: bool| SubscribedQuality { quality, enabled };
        let entry = |codec: &str, qs: Vec<SubscribedQuality>| SubscribedCodec {
            codec: codec.to_owned(),
            qualities: qs,
        };
        let update =
            |codecs: Vec<SubscribedCodec>, flat: Vec<SubscribedQuality>| SubscribedQualityUpdate {
                track_sid: "TR_x".to_owned(),
                subscribed_qualities: flat,
                subscribed_codecs: codecs,
            };
        // H.264 subscribers only: the backup stays off; the primary runs what is asked.
        let u = update(
            vec![entry("h264", vec![q(0, true), q(1, true), q(2, false)])],
            vec![],
        );
        assert_eq!(
            lane_wanted(&u, VideoCodec::H264, false, 3),
            vec![true, true, false]
        );
        assert_eq!(
            lane_wanted(&u, VideoCodec::Vp8, true, 2),
            vec![false, false]
        );
        // A VP8-only subscriber at LOW: the backup's q, named as a mime, any case.
        let u = update(
            vec![
                entry("h264", vec![q(0, false), q(1, false), q(2, false)]),
                entry("video/vp8", vec![q(0, true), q(1, false)]),
            ],
            vec![q(0, true), q(1, true), q(2, true)],
        );
        assert_eq!(
            lane_wanted(&u, VideoCodec::H264, false, 3),
            vec![false, false, false],
            "the primary never takes the backup's entry"
        );
        assert_eq!(lane_wanted(&u, VideoCodec::Vp8, true, 2), vec![true, false]);
        // A quality the backup's entry leaves out is not wanted; the primary's counts as wanted.
        let u = update(
            vec![
                entry("H264", vec![q(0, true)]),
                entry("VP8", vec![q(1, true)]),
            ],
            vec![],
        );
        assert_eq!(
            lane_wanted(&u, VideoCodec::H264, false, 3),
            vec![true, true, true]
        );
        assert_eq!(
            lane_wanted(&u, VideoCodec::Vp8, true, 3),
            vec![false, true, false]
        );
        // No entry for a codec: the primary reads the flat list; the backup ignores it.
        let u = update(vec![], vec![q(0, true), q(1, false)]);
        assert_eq!(
            lane_wanted(&u, VideoCodec::H264, false, 3),
            vec![true, false, true]
        );
        assert_eq!(
            lane_wanted(&u, VideoCodec::Vp8, true, 3),
            vec![false, false, false]
        );
        assert!(names_codec("video/H264", VideoCodec::H264));
        assert!(!names_codec("video/VP8", VideoCodec::H264));
    }

    #[test]
    fn caps_reach_both_lanes_and_heat_never_stops_the_backup() {
        let mut pb = Publisher::new(Arc::default());
        let mut b = three();
        b.truncate(2);
        let mut p = with_backup(b);
        p.state = State::Live;
        pb.pubs.push(p);
        let keys = |pb: &Publisher| -> Vec<(u64, usize, usize)> {
            pb.capped_layers().iter().map(|l| l.key).collect()
        };
        // The backup off: only the primary's layers.
        assert_eq!(keys(&pb), vec![(7, 0, 0), (7, 0, 1), (7, 0, 2)]);
        pb.pubs[0].lanes[1].on = true;
        for l in &mut pb.pubs[0].lanes[1].layers {
            l.wanted = true;
        }
        assert_eq!(pb.capped_layers().len(), 5);
        // The host's caps (heat, low power) on both lanes: one layer each at half the rate,
        // except the backup's lowest, which keeps its floor.
        pb.caps = VideoCaps::new(50, Some(1), Some(20));
        let l = pb.capped_layers();
        let got: Vec<((u64, usize, usize), u64)> = l.iter().map(|l| (l.key, l.max_bps)).collect();
        assert_eq!(got, vec![((7, 0, 0), 75_000), ((7, 1, 0), 150_000)]);
        let (prim, backup) = (&pb.pubs[0].lanes[0], &pb.pubs[0].lanes[1]);
        assert_eq!(pb.fps_of(prim, &prim.layers[0].spec), 20);
        assert_eq!(pb.fps_of(backup, &backup.layers[0].spec), 20);
        // Caps asking for no layer at all: the backup keeps one.
        pb.caps = VideoCaps::new(100, Some(0), None);
        assert_eq!(keys(&pb), vec![(7, 1, 0)]);
        // A phone's thermal steps for the backup alone: Serious one layer, Critical at 15 fps.
        pb.caps = VideoCaps::none();
        pb.backup_caps = BackupCaps {
            max_layers: Some(1),
            max_fps: Some(15),
        };
        assert_eq!(keys(&pb), vec![(7, 0, 0), (7, 0, 1), (7, 0, 2), (7, 1, 0)]);
        let (prim, backup) = (&pb.pubs[0].lanes[0], &pb.pubs[0].lanes[1]);
        assert_eq!(pb.fps_of(prim, &prim.layers[0].spec), 30);
        assert_eq!(pb.fps_of(backup, &backup.layers[0].spec), 15);
        pb.backup_caps = BackupCaps {
            max_layers: Some(0),
            max_fps: None,
        };
        assert_eq!(keys(&pb).last(), Some(&(7, 1, 0)), "never zero layers");
        // Just started: its lowest wanted layer is exempt from the hysteresis.
        pb.pubs[0].lanes[1].exempt = true;
        pb.pubs[0].lanes[1].layers[0].wanted = false;
        pb.backup_caps = BackupCaps::default();
        let ex: Vec<(u64, usize, usize)> = pb
            .capped_layers()
            .iter()
            .filter(|l| l.exempt)
            .map(|l| l.key)
            .collect();
        assert_eq!(ex, vec![(7, 1, 1)], "the lowest wanted, h here");
    }

    #[test]
    fn a_tier_change_keeps_the_backups_rungs() {
        // A phone's backup (q and h, three temporal layers) after a change to 360p hints.
        let mut old = three();
        old.truncate(2);
        for l in &mut old {
            l.temporal_layers = 3;
        }
        let p = with_backup(old);
        let old: Vec<LayerSpec> = p.lanes[1].layers.iter().map(|l| l.spec.clone()).collect();
        let plan = vec![
            LayerSpec::new("q", 320, 180, 120, 24),
            LayerSpec::new("h", 640, 360, 400, 24),
        ];
        let got = backup_layers_for(&plan, &old);
        assert_eq!(got.len(), 2);
        assert!(got.iter().all(|l| l.temporal_layers == 3 && l.fps == 24));
        // A 720p plan for a backup capped at q and h keeps q and h.
        let got = backup_layers_for(&three(), &old);
        let rids: Vec<&str> = got.iter().map(|l| l.rid.as_str()).collect();
        assert_eq!(rids, vec!["q", "h"]);
        // None of the old rungs left: the plan's lowest.
        let only_f = vec![LayerSpec::new("f", 1280, 720, 1700, 30)];
        let got = backup_layers_for(&only_f, &old);
        assert_eq!(
            (got.len(), got[0].rid.as_str(), got[0].temporal_layers),
            (1, "f", 3)
        );
    }
}
