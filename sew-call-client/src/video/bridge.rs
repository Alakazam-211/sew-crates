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
//! 5. the codec ([`choose_codecs`]: VP8 when the policy lists it and an encoder exists, else
//!    H.264 from the OS's hardware encoder; never H.264 on Linux, and never from a software
//!    H.264 encoder) and one encoder per layer of [`crate::plan_layers`], opened from the top
//!    down (H.264 hardware-only, each session checked to run on the hardware): a
//!    `NoResources` (or the codec's `max_instances`) drops the top layer and tries again, down
//!    to one; nothing opens: `codec_unavailable`, the camera closed and the lease released;
//! 6. the publication ([`crate::Call::publish_video`]) with the layers that opened, and, when
//!    the plan has one, the VP8 backup's layers (the same rids and sizes; a phone's stop at
//!    360p). An H.264 encoder that doesn't open hardware-only, or whose session isn't the
//!    hardware's, makes it a VP8 publication without a backup.
//!
//! **The backup** runs on demand: its encoders open on the engine's `BackupStart` (the first
//! frame a keyframe) and close on `BackupStop`; each captured frame goes to the backup's
//! active encoders too. A backup encoder that doesn't open is tried again once after a second;
//! failing again, the publication is published again in VP8 alone (the same lease), and with
//! no VP8 encoder either it ends with `encoder_lost`. A hardware H.264 encoder lost mid-call
//! (its session lost and the reopen failing) publishes again in VP8 alone too, and H.264 isn't
//! tried again for the rest of that call.
//!
//! One capture feeds both: the primary's encoders take the frame as it comes (a native buffer
//! for a hardware encoder), and the backup's take CPU planes converted once per frame, at its
//! largest running layer's size, each encoder scaling from there; nothing is converted while
//! the backup doesn't run. On a phone the backup stops at 360p, and the OS's thermal pressure
//! caps it further: `Serious` keeps only its lowest layer, `Critical` that layer at 15 fps. Heat
//! never stops the backup: its lowest layer stays while a subscriber needs it.
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
    EncodeOptions, EncoderConfig, Facing, FrameBuffer, FrameSource, ImplPreference, Implementation,
    PixelFormat, Rates, SourceEvent, VideoBackend, VideoEncoder, VideoError as KError, Wake,
};
use serde::Serialize;
use tokio::sync::{broadcast, mpsc};

use super::capture::{
    Capture, CaptureEnd, CaptureError, CaptureEvent, CaptureKind, VideoCapture, map_event,
};
use super::codec_pref::{
    H264Detection, VideoCodecPreference as CodecPreference, effective_publish_codec_under_policy,
    h264_encode_detection,
};
use super::preview::LocalFrame;
use super::publish::{
    BackupCaps, BackupSpec, EncodedVideo, LayerRate, LayerSpec, VideoControl, VideoCtl, VideoError,
    VideoPublication, VideoPublish, VideoSender, backup_layers_for, plan_layers,
};
use super::types::{VideoCodec, VideoSource};
use crate::call::Call;

/// `local_video`'s reason for a camera switch that publishes again (a new layer set).
pub(crate) const CAMERA_SWITCH: &str = "camera_switch";
/// `local_video`'s reason when the publication's encoders all failed and none reopened.
pub(crate) const ENCODER_LOST: &str = "encoder_lost";

/// How long a backup encoder that didn't open waits before its one retry.
const BACKUP_RETRY: Duration = Duration::from_secs(1);
/// How long the bridge waits for a source's first `Started`.
const START_WAIT: Duration = Duration::from_secs(3);
/// How long an opened camera has to give its first frame before it is refused with
/// `no_frames` (a camera the OS opens but feeds nothing: a lid-closed MacBook camera, a
/// Continuity Camera that went away while opening). Nothing is published before that frame.
const FIRST_FRAME_WAIT: Duration = Duration::from_millis(2500);
/// How long a pipeline gets to stop (its source and encoders dropped on its own thread); one
/// that takes longer is left to finish on its own, so no stop ever waits on a device.
const STOP_WAIT: Duration = Duration::from_secs(2);
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
///
/// Deprecated: it takes any H.264 encoder, a software one too, and never plans a backup. The
/// bridge uses [`choose_codecs`].
#[deprecated(
    note = "use choose_codecs: it takes only usable (hardware) OS H.264 and plans the VP8 backup"
)]
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

/// One codec of a [`CodecPlan`]: the codec and the backend's encoder entry for it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PlannedCodec {
    /// The codec.
    pub codec: VideoCodec,
    /// The backend's encoder entry the encoders open from.
    pub capability: CodecCapability,
}

/// What a native sender publishes in ([`choose_codecs`]).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct CodecPlan {
    /// The codec every subscriber that can decode it gets.
    pub primary: PlannedCodec,
    /// The VP8 backup negotiated beside an H.264 primary, whose encoders run only while some
    /// subscriber is on it; `None`: one codec.
    pub backup: Option<PlannedCodec>,
    /// Why this plan, for logs and the host's readback: `h264_detected`, `preferred_h264`,
    /// `preferred_vp8`, `h264_not_detected`, `backup_off` (the policy's `backup_codec` or
    /// `dynacast` is off, or it doesn't list both codecs), `no_h264_decode`, `h264_lost`.
    pub reason: &'static str,
}

/// Whether this platform may publish OS H.264 at all. Only macOS and iOS for now: Linux has no
/// OS H.264 and never publishes it; Android and Windows publish VP8 only until their backends
/// probe the hardware encoder (a static capability entry is an assumption, not a detection).
/// The `testing` fixture backend (its backend name `fixture`) stands in for an OS backend in
/// the loopback lanes.
fn platform_h264(caps: &Capabilities) -> bool {
    const OS: bool = cfg!(any(target_os = "macos", target_os = "ios"));
    OS || (cfg!(feature = "testing") && caps.backend == "fixture")
}

/// The first half of the usable-H.264 test, on the capability list: this platform may publish
/// OS H.264 ([`platform_h264`]) and the backend lists an H.264 encoder run by the hardware. A
/// software entry (the OS's software encoder) is never usable. The other two checks run when
/// the encoders open: they open hardware-only, and each opened session must report hardware
/// (an OS that can't tell, an iOS before 17.4, counts as not hardware).
fn usable_h264(caps: &Capabilities) -> Option<&CodecCapability> {
    if !platform_h264(caps) {
        return None;
    }
    caps.codecs
        .iter()
        .filter(|c| {
            c.codec == kessel_video::VideoCodec::H264
                && c.direction == CodecDirection::Encode
                && c.implementation == Implementation::Hardware
        })
        .max_by_key(|c| u64::from(c.max_width) * u64::from(c.max_height))
}

/// What a native sender publishes in, on `caps`, under the call's `policy`, with the
/// automatic codec preference, on a device that decodes H.264 where its platform does
/// ([`crate::default_video_h264`]):
/// 1. **H.264 with a VP8 backup** when the policy has `backup_codec` and `dynacast` on and
///    lists both codecs (in either order), this device has usable OS H.264 (hardware, detected
///    from the backend, never assumed; the encoders' open checks the session too), a VP8
///    encoder exists, and the device decodes H.264;
/// 2. else **one codec**: VP8 when listed and an encoder exists; else H.264 when listed and
///    usable (as above);
/// 3. else `None`: `codec_unavailable`.
///
/// Linux never publishes H.264.
pub fn choose_codecs(
    policy: &sew_proto::voice::MediaPolicy,
    caps: &Capabilities,
) -> Option<CodecPlan> {
    choose_codecs_with(
        policy,
        caps,
        crate::default_video_h264(),
        CodecPreference::default(),
        false,
    )
}

/// [`choose_codecs`] with what the call knows: `decodes_h264` (the call's media connection
/// negotiates H.264), the device's preference, and `h264_lost` (the hardware encoder was lost
/// earlier in this call: H.264 isn't tried again).
pub(crate) fn choose_codecs_with(
    policy: &sew_proto::voice::MediaPolicy,
    caps: &Capabilities,
    decodes_h264: bool,
    pref: CodecPreference,
    h264_lost: bool,
) -> Option<CodecPlan> {
    let listed = |want: VideoCodec| {
        policy
            .codecs
            .iter()
            .any(|c| super::publish::codec_of_policy(*c) == Some(want))
    };
    let vp8 = caps
        .best(kessel_video::VideoCodec::Vp8, CodecDirection::Encode)
        .cloned();
    // The run-time detection on the backend's listing (the encoders' open checks the session).
    let detected = if platform_h264(caps) {
        h264_encode_detection(caps)
    } else {
        H264Detection::NoPlatformCheck
    };
    let h264 = if h264_lost || !decodes_h264 || !detected.is_usable() {
        None
    } else {
        usable_h264(caps).cloned()
    };
    let planned = |codec, capability| PlannedCodec { codec, capability };
    let both = policy.backup_codec
        && policy.dynacast
        && listed(VideoCodec::Vp8)
        && listed(VideoCodec::H264);
    // Step 1: H.264 with the VP8 backup, as the preference allows.
    let ready = if h264.is_some() && vp8.is_some() {
        H264Detection::Usable
    } else if detected.is_usable() {
        H264Detection::NotDetected
    } else {
        detected
    };
    let (pick, _) = effective_publish_codec_under_policy(pref, ready, both);
    if pick == VideoCodec::H264
        && let (Some(h), Some(v)) = (h264.clone(), vp8.clone())
    {
        return Some(CodecPlan {
            primary: planned(VideoCodec::H264, h),
            backup: Some(planned(VideoCodec::Vp8, v)),
            reason: if pref == CodecPreference::PreferH264 {
                "preferred_h264"
            } else {
                "h264_detected"
            },
        });
    }
    let reason = if pref == CodecPreference::PreferVp8 {
        "preferred_vp8"
    } else if h264_lost {
        "h264_lost"
    } else if !both {
        "backup_off"
    } else if !decodes_h264 {
        "no_h264_decode"
    } else {
        "h264_not_detected"
    };
    // Step 2: one codec, VP8 first; H.264 only when usable.
    if listed(VideoCodec::Vp8)
        && let Some(v) = vp8
    {
        return Some(CodecPlan {
            primary: planned(VideoCodec::Vp8, v),
            backup: None,
            reason,
        });
    }
    if listed(VideoCodec::H264)
        && let Some(h) = h264
    {
        return Some(CodecPlan {
            primary: planned(VideoCodec::H264, h),
            backup: None,
            reason,
        });
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

/// Whether this device is a phone (its backup stops at 360p).
const PHONE: bool = cfg!(any(target_os = "ios", target_os = "android"));

/// The backup's layers for the primary's opened `layers`: on a desktop the same rids and
/// sizes; on a `phone` only those at most 360p on their short side (the lowest when none is).
/// Each with `temporal` VP8 temporal layers.
fn backup_ladder(layers: &[LayerSpec], phone: bool, temporal: u8) -> Vec<LayerSpec> {
    let mut out: Vec<LayerSpec> = layers
        .iter()
        .filter(|l| !phone || l.width.min(l.height) <= 360)
        .cloned()
        .collect();
    if out.is_empty()
        && let Some(l) = layers.first()
    {
        out.push(l.clone());
    }
    for l in &mut out {
        l.temporal_layers = temporal;
    }
    out
}

/// The backup's caps for a phone at thermal `level`: `Serious` only its lowest layer;
/// `Critical` that layer at 15 fps; below, none. Never fewer than one layer.
fn thermal_backup_caps(level: kessel_video::ThermalLevel) -> BackupCaps {
    use kessel_video::ThermalLevel as T;
    match level {
        T::Critical => BackupCaps {
            max_layers: Some(1),
            max_fps: Some(15),
        },
        T::Serious => BackupCaps {
            max_layers: Some(1),
            max_fps: None,
        },
        _ => BackupCaps::default(),
    }
}

/// The VP8 temporal layers for `cap` and the host's ask.
fn vp8_temporal(cap: &CodecCapability, asked: Option<u8>) -> u8 {
    let offered = cap.max_temporal_layers.clamp(1, 3);
    asked.map_or(offered, |t| t.clamp(1, offered))
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
    if codec == VideoCodec::H264 {
        // H.264 only from the OS's hardware encoder: a software session is refused at the open.
        cfg.implementation = ImplPreference::HardwareOnly;
    }
    cfg
}

/// Opens one layer's encoder. An H.264 session must report that the hardware runs it (the
/// third check of the usable-H.264 test): one that says software, or can't say, is closed and
/// refused.
fn open_encoder(
    backend: &Arc<dyn VideoBackend>,
    codec: VideoCodec,
    l: &LayerSpec,
    content: ContentHint,
    wake: &Wake,
) -> Result<Box<dyn VideoEncoder>, KError> {
    let enc = backend.encoder(encoder_config(codec, l, content), wake.clone())?;
    if codec == VideoCodec::H264 {
        let got = enc.implementation();
        if got != Implementation::Hardware {
            tracing::info!(target: "sew_call_client", rid = %l.rid, implementation = ?got, "the H.264 session is not run by the hardware: refused");
            return Err(KError::Unsupported(
                "the H.264 session is not hardware".to_owned(),
            ));
        }
    }
    Ok(enc)
}

/// One layer's encoder in the pipeline.
struct Enc {
    spec: LayerSpec,
    enc: Box<dyn VideoEncoder>,
    active: bool,
    /// The next frame submitted is a keyframe (a new source's first frame). A threaded
    /// encoder's `request_keyframe` marks whatever input it takes next, which may still be the
    /// old source's last frame; this rides on the new source's frame itself.
    key_owed: bool,
}

impl Enc {
    /// Submits `frame` with a keyframe if one is owed (cleared once the encoder took it).
    fn submit(&mut self, frame: &kessel_video::VideoFrame) -> Result<(), KError> {
        let opts = if self.key_owed {
            EncodeOptions::keyframe()
        } else {
            EncodeOptions::default()
        };
        self.enc.submit(frame, opts)?;
        self.key_owed = false;
        Ok(())
    }
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
            match open_encoder(backend, codec, l, content, wake) {
                Ok(enc) => opened.push(Enc {
                    spec: l.clone(),
                    enc,
                    active: true,
                    key_owed: false,
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

/// Waits (on the blocking pool) for an opened camera's first frame, at most
/// [`FIRST_FRAME_WAIT`], and answers the camera with that frame still to come (after the
/// events seen meanwhile, a suspension and its resume among them) and its delivered size
/// (even). None in time: [`VideoError::NoFrames`], the camera dropped (closed) there, on the
/// blocking thread. A camera that ends first is `camera_not_found`; its error is its code.
async fn wait_first_frame(
    mut src: Box<dyn FrameSource>,
) -> Result<(Box<dyn FrameSource>, (u32, u32)), VideoError> {
    tokio::task::spawn_blocking(move || {
        let until = std::time::Instant::now() + FIRST_FRAME_WAIT;
        let mut started = None;
        let mut events = std::collections::VecDeque::new();
        loop {
            while let Some(ev) = src.try_event() {
                match ev {
                    SourceEvent::Started { width, height, .. } => started = Some((width, height)),
                    SourceEvent::Ended(_) => return Err(VideoError::CameraNotFound),
                    SourceEvent::Error(e) => return Err(camera_error(&e)),
                    other => events.push_back(other),
                }
            }
            if let Some(frame) = src.try_recv() {
                let size = started.unwrap_or((frame.width, frame.height));
                let primed: Box<dyn FrameSource> = Box::new(Primed {
                    first: Some(frame),
                    events,
                    inner: src,
                });
                return Ok((primed, (size.0 & !1, size.1 & !1)));
            }
            if std::time::Instant::now() >= until {
                tracing::info!(target: "sew_call_client", waited_ms = FIRST_FRAME_WAIT.as_millis() as u64, "the camera gave no frame: no_frames");
                return Err(VideoError::NoFrames);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    })
    .await
    .map_err(|_| VideoError::PublishFailed("source thread".to_owned()))?
}

/// A camera whose first frame (and the events before it) [`wait_first_frame`] already took:
/// the events come first, then that frame, then the camera's own.
struct Primed {
    first: Option<kessel_video::VideoFrame>,
    events: std::collections::VecDeque<SourceEvent>,
    inner: Box<dyn FrameSource>,
}

impl FrameSource for Primed {
    fn try_recv(&mut self) -> Option<kessel_video::VideoFrame> {
        if !self.events.is_empty() {
            // The events seen before the first frame are read before it.
            return None;
        }
        self.first.take().or_else(|| self.inner.try_recv())
    }
    fn try_event(&mut self) -> Option<SourceEvent> {
        self.events.pop_front().or_else(|| self.inner.try_event())
    }
    fn set_constraints(&mut self, c: CaptureConstraints) -> Result<(), KError> {
        self.inner.set_constraints(c)
    }
    fn set_paused(&mut self, paused: bool) {
        self.inner.set_paused(paused);
    }
    fn stats(&self) -> kessel_video::SourceStats {
        self.inner.stats()
    }
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
    /// The call it publishes for ([`Call::local_id`]).
    call: u64,
    stop: Arc<AtomicBool>,
    /// The pipeline thread is over (the source or the publication ended).
    ended: Arc<AtomicBool>,
    bell: Arc<Bell>,
    to_pipe: std::sync::mpsc::Sender<ToPipe>,
    thread: Option<std::thread::JoinHandle<()>>,
    codec: VideoCodec,
    /// The codec the pipeline publishes in now (VP8 after a fall back).
    live: Arc<Mutex<VideoCodec>>,
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
            codec: *lock(&self.live),
            layers: self.layers.clone(),
            width: self.size.0,
            height: self.size.1,
        }
    }

    fn alive(&self) -> bool {
        !self.ended.load(Ordering::SeqCst)
    }

    /// Stops the pipeline, waiting at most [`STOP_WAIT`]: a pipeline still stopping then (its
    /// source's close waiting on the OS) finishes on its own thread.
    fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.bell.ring();
        let Some(t) = self.thread.take() else { return };
        let until = std::time::Instant::now() + STOP_WAIT;
        while !t.is_finished() {
            if std::time::Instant::now() >= until {
                tracing::warn!(target: "sew_call_client", waited_ms = STOP_WAIT.as_millis() as u64, "a video pipeline is still stopping (its source closes slowly); it finishes on its own");
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = t.join();
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
    /// The call ([`Call::local_id`]) whose hardware H.264 encoder was lost: no H.264 again in
    /// it.
    h264_lost: Arc<Mutex<Option<u64>>>,
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
                h264_lost: Arc::new(Mutex::new(None)),
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
        if feeds
            .get(&kind)
            .is_some_and(|r| !r.alive() || r.call != call.local_id())
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
        if slot
            .as_ref()
            .is_some_and(|r| !r.alive() || r.call != call.local_id())
            && let Some(r) = slot.take()
        {
            // Its source or publication ended (unpublished already), or it was an earlier
            // call's.
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
        let Some(r) = slot
            .as_ref()
            .filter(|r| r.alive() && r.call == call.local_id())
        else {
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
        // Nothing is published before the camera's first frame.
        wait_first_frame(Box::new(cam)).await
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
        let caps = backend.capabilities();
        let lost = *lock(&self.taps.h264_lost) == Some(call.local_id());
        // The device's setting (Automatic until the host sets one).
        let pref = call.video_codec_preference();
        let Some(mut plan) = choose_codecs_with(policy, &caps, call.decodes_h264(), pref, lost)
        else {
            return Err(VideoError::CodecUnavailable);
        };
        tracing::info!(target: "sew_call_client", source = ?kind, codec = plan.primary.codec.as_str(), backup = plan.backup.as_ref().map(|b| b.codec.as_str()), reason = plan.reason, "the codec plan");
        let content = content_of(kind);
        let open = |codec: VideoCodec, cap: CodecCapability| {
            let layers = VideoBridge::plan(policy, kind, codec, &cap, temporal_layers, size);
            let wake = bell.wake();
            let b2 = Arc::clone(&backend);
            async move {
                tokio::task::spawn_blocking(move || {
                    open_encoders(&b2, codec, &cap, layers, content, &wake)
                })
                .await
                .map_err(|_| VideoError::PublishFailed("encoder thread".to_owned()))
            }
        };
        let mut encs = open(plan.primary.codec, plan.primary.capability.clone()).await?;
        if encs.is_empty() && plan.primary.codec == VideoCodec::H264 {
            // The hardware-only open, or the session's own report, said no: one codec, VP8.
            tracing::info!(target: "sew_call_client", source = ?kind, "the H.264 encoder did not open on the hardware: VP8 only");
            match choose_codecs_with(policy, &caps, false, pref, true) {
                Some(p) if p.primary.codec == VideoCodec::Vp8 => {
                    plan = p;
                    plan.reason = "h264_open_failed";
                    encs = open(VideoCodec::Vp8, plan.primary.capability.clone()).await?;
                }
                _ => {}
            }
        }
        if encs.is_empty() {
            return Err(VideoError::CodecUnavailable);
        }
        let (codec, cap) = (plan.primary.codec, plan.primary.capability.clone());
        let layers: Vec<LayerSpec> = encs.iter().map(|e| e.spec.clone()).collect();
        let vp8 = match &plan.backup {
            Some(b) => Some(b.capability.clone()),
            None if codec == VideoCodec::Vp8 => Some(cap.clone()),
            None => caps
                .best(kessel_video::VideoCodec::Vp8, CodecDirection::Encode)
                .cloned(),
        };
        let backup = plan.backup.as_ref().map(|b| {
            let ladder =
                backup_ladder(&layers, PHONE, vp8_temporal(&b.capability, temporal_layers));
            BackupPipe::new(b.codec, ladder)
        });
        let mut req = VideoPublish::new(kind, codec, layers.clone(), size.0, size.1);
        if let Some(b) = &backup {
            req = req.with_backup(BackupSpec::new(b.codec, b.ladder.clone()));
        }
        let publication = call.publish_video(req).await?;
        tracing::info!(target: "sew_call_client", source = ?kind, codec = codec.as_str(), layers = layers.len(), backup = backup.as_ref().map(|b| b.ladder.len()), width = size.0, height = size.1, "video source on");
        let live = Arc::new(Mutex::new(codec));
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
            call: call.local_id(),
            backup,
            vp8,
            temporal: temporal_layers,
            live: live.clone(),
            phone: PHONE,
            machine: self.backend().subscribe(),
            thermal: kessel_video::ThermalLevel::Nominal,
            backup_converts: Arc::default(),
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
            call: call.local_id(),
            stop,
            ended,
            bell,
            to_pipe,
            thread: Some(thread),
            codec,
            live,
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

    /// Stops the pipelines of one call (its [`Call::local_id`]) without touching the call (it
    /// ended): a later call's keep running. Each stop waits a short while at most; a pipeline
    /// whose source closes slowly finishes on its own.
    pub async fn close_call(&self, call: u64) {
        let mut all: Vec<Running> = {
            let mut feeds = self.feeds.lock().await;
            let kinds: Vec<VideoSource> = feeds
                .iter()
                .filter(|(_, r)| r.call == call)
                .map(|(k, _)| *k)
                .collect();
            kinds.iter().filter_map(|k| feeds.remove(k)).collect()
        };
        {
            let mut cam = self.camera.lock().await;
            if cam.as_ref().is_some_and(|r| r.call == call) {
                all.extend(cam.take());
            }
        }
        for r in all {
            stop_running(r).await;
        }
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
    /// The call it publishes for ([`Call::local_id`]).
    call: u64,
    /// The backup's encoders (a publication with a backup codec).
    backup: Option<BackupPipe>,
    /// The VP8 encoder entry, for a fall back to VP8 alone.
    vp8: Option<CodecCapability>,
    /// The VP8 temporal layers the host asked for.
    temporal: Option<u8>,
    /// The codec published now (shared with [`Running`]).
    live: Arc<Mutex<VideoCodec>>,
    /// A phone: the backup follows the thermal steps.
    phone: bool,
    /// The backend's machine-wide events (thermal pressure).
    machine: kessel_video::EventRx,
    /// The last thermal level seen.
    thermal: kessel_video::ThermalLevel,
    /// Frames converted for the backup (once per captured frame while it runs).
    backup_converts: Arc<std::sync::atomic::AtomicU64>,
}

/// The backup's side of a pipeline: its layers, and its encoders while it runs.
struct BackupPipe {
    codec: VideoCodec,
    /// Every layer the backup may run (the engine's `BackupStart` names the ones it opens).
    ladder: Vec<LayerSpec>,
    encs: BTreeMap<String, Enc>,
    /// Between `BackupStart` and `BackupStop`.
    on: bool,
    /// Layers whose encoder didn't open, tried again at `retry_at`.
    pending: Vec<LayerSpec>,
    retry_at: Option<std::time::Instant>,
    /// The retry is spent: the next failure publishes VP8 alone.
    retried: bool,
    /// The engine's last `BackupRates` (an encoder that opens later runs at its rate).
    last_rates: Vec<LayerRate>,
}

impl BackupPipe {
    fn new(codec: VideoCodec, ladder: Vec<LayerSpec>) -> BackupPipe {
        BackupPipe {
            codec,
            ladder,
            encs: BTreeMap::new(),
            on: false,
            pending: Vec::new(),
            retry_at: None,
            retried: false,
            last_rates: Vec::new(),
        }
    }

    fn any_active(&self) -> bool {
        self.on && self.encs.values().any(|e| e.active)
    }

    /// Closes every backup encoder (on this pipeline's thread).
    fn close(&mut self) {
        self.on = false;
        self.encs.clear();
        self.pending.clear();
        self.retry_at = None;
        self.last_rates.clear();
    }
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
                        // The new camera's first frame is a keyframe on every layer: owed to
                        // that frame, not asked of the encoder now (its pending input may
                        // still be the old camera's last frame).
                        for e in self.encs.values_mut() {
                            e.key_owed = true;
                        }
                        for l in sizes {
                            if let Some(e) = self.encs.get_mut(&l.rid)
                                && (e.spec.width, e.spec.height) != (l.width, l.height)
                            {
                                let _ = e.enc.set_size(l.width, l.height);
                                e.spec.width = l.width;
                                e.spec.height = l.height;
                            }
                            if let Some(b) = self.backup.as_mut() {
                                // The backup's layers follow the camera's aspect too.
                                if let Some(r) = b.ladder.iter_mut().find(|r| r.rid == l.rid) {
                                    r.width = l.width;
                                    r.height = l.height;
                                }
                                if let Some(e) = b.encs.get_mut(&l.rid) {
                                    e.key_owed = true;
                                    if (e.spec.width, e.spec.height) != (l.width, l.height) {
                                        let _ = e.enc.set_size(l.width, l.height);
                                        e.spec.width = l.width;
                                        e.spec.height = l.height;
                                    }
                                }
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
            self.backup_retry();
            if self.end_asked {
                return;
            }
            self.machine_events();
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
                    for e in self.backup.iter_mut().flat_map(|b| b.encs.values_mut()) {
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
        let want = !self.scene_paused
            && (self_view
                || self.encs.values().any(|e| e.active)
                || self.backup.as_ref().is_some_and(BackupPipe::any_active));
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
                VideoControl::BackupStart(layers) => self.backup_start(layers),
                VideoControl::BackupRates(rates) => self.backup_rates(rates),
                VideoControl::BackupKeyframe { rid } => {
                    if let Some(e) = self.backup.as_mut().and_then(|b| b.encs.get_mut(&rid)) {
                        e.enc.request_keyframe();
                    }
                }
                VideoControl::BackupStop => {
                    if let Some(b) = self.backup.as_mut() {
                        tracing::info!(target: "sew_call_client", source = ?self.kind, encoders = b.encs.len(), "the backup stops: its encoders close");
                        b.close();
                    }
                }
            }
            if self.end_asked {
                return true;
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
            for e in self.backup.iter_mut().flat_map(|b| b.encs.values_mut()) {
                e.enc.request_keyframe();
            }
        }
        let self_view = self.self_view_open();
        self.repause(self_view);
    }

    /// A republish's layers: existing encoders resized, new ones opened, gone ones closed.
    fn relayer(&mut self, layers: Vec<LayerSpec>) {
        self.self_rid = self_view_rid(self.kind, &layers);
        if let Some(b) = self.backup.as_mut() {
            // The engine stopped the backup for the republish; its layers follow the new set.
            b.close();
            b.ladder = backup_layers_for(&layers, &b.ladder);
        }
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
                None => match open_encoder(&self.backend, self.codec, &l, self.content, &wake) {
                    Ok(enc) => {
                        next.insert(
                            l.rid.clone(),
                            Enc {
                                spec: l,
                                enc,
                                active: true,
                                key_owed: false,
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

    /// The backend's machine-wide events: on a phone with a backup, a thermal level change
    /// caps the backup ([`thermal_backup_caps`]) in the engine.
    fn machine_events(&mut self) {
        while let Some(ev) = self.machine.try_recv() {
            let kessel_video::BackendEvent::Thermal(level) = ev else {
                continue;
            };
            if !self.phone || self.backup.is_none() || level == self.thermal {
                continue;
            }
            self.thermal = level;
            let caps = thermal_backup_caps(level);
            tracing::info!(target: "sew_call_client", ?level, max_layers = ?caps.max_layers, max_fps = ?caps.max_fps, "thermal pressure: the backup's caps");
            self.ctl.backup_caps(caps);
        }
    }

    /// `BackupStart`: the backup runs; its encoders open for `layers` (each a keyframe first).
    fn backup_start(&mut self, layers: Vec<LayerSpec>) {
        let Some(b) = self.backup.as_mut() else {
            return;
        };
        b.on = true;
        for l in &layers {
            match b.ladder.iter_mut().find(|r| r.rid == l.rid) {
                Some(r) => *r = l.clone(),
                None => b.ladder.push(l.clone()),
            }
        }
        tracing::info!(target: "sew_call_client", source = ?self.kind, layers = layers.len(), "the backup starts: its encoders open");
        self.backup_open(layers);
    }

    /// `BackupRates`: which backup layers run; an active one without an encoder opens.
    fn backup_rates(&mut self, rates: Vec<LayerRate>) {
        let Some(b) = self.backup.as_mut() else {
            return;
        };
        if !b.on {
            return;
        }
        let open: Vec<LayerSpec> = rates
            .iter()
            .filter(|r| r.active && !b.encs.contains_key(&r.rid))
            .filter(|r| !b.pending.iter().any(|p| p.rid == r.rid))
            .filter_map(|r| b.ladder.iter().find(|l| l.rid == r.rid).cloned())
            .collect();
        if !open.is_empty() {
            self.backup_open(open);
        }
        let Some(b) = self.backup.as_mut() else {
            return;
        };
        b.last_rates.clone_from(&rates);
        for r in rates {
            if let Some(e) = b.encs.get_mut(&r.rid) {
                e.active = r.active;
                if r.active {
                    let _ = e
                        .enc
                        .set_rates(Rates::new(r.bitrate_bps.max(10_000), r.fps.max(1)));
                }
            }
        }
    }

    /// Opens the backup's encoders for `layers` (each starts at a keyframe, inactive until its
    /// rate comes). One that doesn't open is tried again once after [`BACKUP_RETRY`]; failing
    /// then, the publication goes to VP8 alone ([`Pipe::fall_back_to_vp8`]).
    fn backup_open(&mut self, layers: Vec<LayerSpec>) {
        let wake = self.bell.wake();
        let Some(b) = self.backup.as_mut() else {
            return;
        };
        let mut failed = Vec::new();
        for l in layers {
            if b.encs.contains_key(&l.rid) {
                continue;
            }
            match open_encoder(&self.backend, b.codec, &l, self.content, &wake) {
                Ok(mut enc) => {
                    enc.request_keyframe();
                    // Its rate if one came already (else inactive until it does).
                    let r = b.last_rates.iter().find(|r| r.rid == l.rid && r.active);
                    if let Some(r) = r {
                        let _ = enc.set_rates(Rates::new(r.bitrate_bps.max(10_000), r.fps.max(1)));
                    }
                    b.encs.insert(
                        l.rid.clone(),
                        Enc {
                            spec: l,
                            enc,
                            active: r.is_some(),
                            key_owed: false,
                        },
                    );
                }
                Err(e) => {
                    tracing::info!(target: "sew_call_client", rid = %l.rid, error = %e, "a backup encoder did not open");
                    failed.push(l);
                }
            }
        }
        if failed.is_empty() {
            if b.pending.is_empty() {
                b.retried = false;
            }
            return;
        }
        if b.retried {
            self.fall_back_to_vp8("the backup's encoder did not open again");
            return;
        }
        b.pending.extend(failed);
        b.retry_at = Some(std::time::Instant::now() + BACKUP_RETRY);
    }

    /// The backup's one retry, when due.
    fn backup_retry(&mut self) {
        let Some(b) = self.backup.as_mut() else {
            return;
        };
        if b.retry_at.is_none_or(|t| std::time::Instant::now() < t) {
            return;
        }
        b.retry_at = None;
        b.retried = true;
        let pending = std::mem::take(&mut b.pending);
        if b.on {
            tracing::info!(target: "sew_call_client", layers = pending.len(), "the backup's encoders: the one retry");
            self.backup_open(pending);
        }
    }

    /// The publication in VP8 alone (the backup can't run, or the hardware H.264 encoder is
    /// gone): the backup closes, VP8 encoders open for the primary's layers, and the engine
    /// publishes the track again in VP8 (the same lease). No VP8 encoder opens: the
    /// publication ends with `encoder_lost`.
    fn fall_back_to_vp8(&mut self, why: &str) {
        if let Some(mut b) = self.backup.take() {
            b.close();
        }
        let Some(cap) = self.vp8.clone() else {
            tracing::info!(target: "sew_call_client", source = ?self.kind, why, "no VP8 encoder to fall back to: unpublishing");
            self.end_asked = true;
            self.ctl.end(self.kind, ENCODER_LOST);
            return;
        };
        let tl = vp8_temporal(&cap, self.temporal);
        let mut plan: Vec<LayerSpec> = self
            .encs
            .values()
            .map(|e| {
                let mut l = e.spec.clone();
                l.temporal_layers = tl;
                l
            })
            .collect();
        plan.sort_by_key(|l| (l.width.min(l.height), l.width.max(l.height)));
        // The old encoders close first (a phone's hardware session among them).
        self.encs.clear();
        let wake = self.bell.wake();
        let encs = open_encoders(
            &self.backend,
            VideoCodec::Vp8,
            &cap,
            plan,
            self.content,
            &wake,
        );
        if encs.is_empty() {
            tracing::info!(target: "sew_call_client", source = ?self.kind, why, "no VP8 encoder opened: unpublishing");
            self.end_asked = true;
            self.ctl.end(self.kind, ENCODER_LOST);
            return;
        }
        let layers: Vec<LayerSpec> = encs.iter().map(|e| e.spec.clone()).collect();
        tracing::info!(target: "sew_call_client", source = ?self.kind, why, layers = layers.len(), "published again in VP8 alone");
        self.codec = VideoCodec::Vp8;
        *lock(&self.live) = VideoCodec::Vp8;
        self.self_rid = self_view_rid(self.kind, &layers);
        self.encs = encs.into_iter().map(|e| (e.spec.rid.clone(), e)).collect();
        for e in self.encs.values_mut() {
            e.enc.request_keyframe();
        }
        self.publication
            .republish_without_backup(VideoCodec::Vp8, layers);
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
            match e.submit(frame) {
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
                        let _ = e.submit(p);
                    }
                }
                // Busy: this frame is dropped for that layer (counted by the encoder).
                Err(_) => {}
            }
        }
        self.submit_backup(frame);
    }

    /// The backup's active encoders get the frame as CPU planes converted once, at the largest
    /// running backup layer's size (each encoder scales from it): a native buffer always, CPU
    /// planes when larger. Nothing is converted while the backup doesn't run.
    fn submit_backup(&mut self, frame: &kessel_video::VideoFrame) {
        let Some(b) = self.backup.as_mut().filter(|b| b.any_active()) else {
            return;
        };
        let area = |w: u32, h: u32| u64::from(w) * u64::from(h);
        let Some((tw, th)) = b
            .encs
            .values()
            .filter(|e| e.active)
            .map(|e| (e.spec.width, e.spec.height))
            .max_by_key(|(w, h)| area(*w, *h))
        else {
            return;
        };
        let native = matches!(frame.buffer, FrameBuffer::Native(_));
        let converted = if native || area(frame.width, frame.height) > area(tw, th) {
            match self.backend.convert(frame, tw, th, PixelFormat::I420) {
                Ok(f) => {
                    self.backup_converts.fetch_add(1, Ordering::Relaxed);
                    Some(f)
                }
                // A frame the backend can't convert: the backup skips it.
                Err(_) if native => return,
                Err(_) => None,
            }
        } else {
            None
        };
        let input = converted.as_ref().unwrap_or(frame);
        for e in b.encs.values_mut().filter(|e| e.active) {
            // Busy: this frame is dropped for that layer (counted by the encoder).
            let _ = e.submit(input);
        }
    }

    /// The backup's encoded frames to the engine (in the backup's codec); a lost backup
    /// encoder opens again (its retry rules).
    fn drain_backup(&mut self, sender: &VideoSender) {
        let Some(b) = self.backup.as_mut() else {
            return;
        };
        let mut dead = Vec::new();
        for (rid, e) in &mut b.encs {
            while let Some(r) = e.enc.poll() {
                match r {
                    Ok(f) => {
                        let Some(codec) = our_codec(f.codec) else {
                            continue;
                        };
                        if !e.active || !b.on {
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
                        tracing::info!(target: "sew_call_client", rid = %rid, error = %err, "a backup encoder session was lost; reopening");
                        dead.push(e.spec.clone());
                        break;
                    }
                }
            }
        }
        if dead.is_empty() {
            return;
        }
        for l in &dead {
            b.encs.remove(&l.rid);
        }
        self.backup_open(dead);
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
            match open_encoder(&self.backend, self.codec, &e.spec, self.content, &wake) {
                Ok(mut enc) => {
                    enc.request_keyframe();
                    e.enc = enc;
                }
                Err(err) if self.codec == VideoCodec::H264 => {
                    // The hardware is gone (a GPU reset, the media engine taken): VP8 alone,
                    // and no H.264 again in this call.
                    tracing::info!(target: "sew_call_client", rid = %rid, error = %err, "the hardware H.264 encoder was lost and did not reopen");
                    *lock(&self.taps.h264_lost) = Some(self.call);
                    self.fall_back_to_vp8("the hardware H.264 encoder was lost");
                    return;
                }
                Err(err) => {
                    tracing::info!(target: "sew_call_client", rid = %rid, error = %err, "a lost encoder did not reopen: its layer stops");
                    self.encs.remove(&rid);
                }
            }
        }
        self.drain_backup(sender);
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
    #[allow(deprecated)]
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

    /// Capabilities of backend `backend` with encoders `(codec, implementation)`.
    fn caps_of(
        backend: &'static str,
        encoders: &[(kessel_video::VideoCodec, Implementation)],
    ) -> Capabilities {
        let mut c = Capabilities::new(backend, "test");
        for (k, i) in encoders {
            c.codecs.push(CodecCapability::new(
                *k,
                CodecDirection::Encode,
                *i,
                "enc",
                1920,
                1080,
                30,
            ));
        }
        c
    }

    pub(super) fn media_policy(
        codecs: &[sew_proto::voice::VideoCodec],
        backup: bool,
    ) -> sew_proto::voice::MediaPolicy {
        let hint = sew_proto::voice::TrackHint {
            max_width: 1280,
            max_height: 720,
            max_fps: 30,
            layers: Vec::new(),
        };
        sew_proto::voice::MediaPolicy {
            tier: sew_proto::voice::MediaTier::Normal,
            video_enabled: true,
            e2ee: "required".to_owned(),
            audio_codecs: vec!["opus".to_owned()],
            codecs: codecs.to_vec(),
            backup_codec: backup,
            vp9: None,
            camera: hint.clone(),
            screen: hint,
            dynacast: true,
            adaptive_stream: true,
            camera_allowed: true,
            screen_allowed: true,
            relay_only: false,
        }
    }

    /// `(primary, backup, reason)` of a plan.
    fn shape(p: Option<CodecPlan>) -> Option<(VideoCodec, Option<VideoCodec>, &'static str)> {
        p.map(|p| (p.primary.codec, p.backup.map(|b| b.codec), p.reason))
    }

    #[test]
    #[cfg(feature = "testing")]
    fn h264_with_a_vp8_backup_only_when_every_rule_holds() {
        use kessel_video::VideoCodec as K;
        use sew_proto::voice::VideoCodec as P;
        let hw = caps_of(
            "fixture",
            &[
                (K::Vp8, Implementation::Software),
                (K::H264, Implementation::Hardware),
            ],
        );
        let auto = CodecPreference::Automatic;
        let plan =
            |pol: &sew_proto::voice::MediaPolicy,
             c: &Capabilities,
             dec: bool,
             pref: CodecPreference,
             lost: bool| shape(choose_codecs_with(pol, c, dec, pref, lost));
        let both = media_policy(&[P::Vp8, P::H264], true);
        // Every rule holds, in either order of the policy's list.
        let want = Some((VideoCodec::H264, Some(VideoCodec::Vp8), "h264_detected"));
        assert_eq!(plan(&both, &hw, true, auto, false), want);
        let h264_first = media_policy(&[P::H264, P::Vp8], true);
        assert_eq!(plan(&h264_first, &hw, true, auto, false), want);
        // backup_codec off, dynacast off, or one codec listed: one codec, VP8 first.
        let vp8 = Some((VideoCodec::Vp8, None, "backup_off"));
        let off = media_policy(&[P::Vp8, P::H264], false);
        assert_eq!(plan(&off, &hw, true, auto, false), vp8);
        let mut no_dyn = both.clone();
        no_dyn.dynacast = false;
        assert_eq!(plan(&no_dyn, &hw, true, auto, false), vp8);
        assert_eq!(
            plan(&media_policy(&[P::Vp8], true), &hw, true, auto, false),
            vp8
        );
        // An H.264-only policy with usable H.264: one codec.
        let only = media_policy(&[P::H264], true);
        assert_eq!(
            plan(&only, &hw, true, auto, false),
            Some((VideoCodec::H264, None, "backup_off"))
        );
        // The device doesn't decode H.264 (one codec set per PC): VP8 only.
        assert_eq!(
            plan(&both, &hw, false, auto, false),
            Some((VideoCodec::Vp8, None, "no_h264_decode"))
        );
        assert_eq!(plan(&only, &hw, false, auto, false), None);
        // The hardware was lost earlier in the call: never again in it.
        assert_eq!(
            plan(&both, &hw, true, auto, true),
            Some((VideoCodec::Vp8, None, "h264_lost"))
        );
        // The preference: VP8 always honoured; H.264 only where detected.
        assert_eq!(
            plan(&both, &hw, true, CodecPreference::PreferVp8, false),
            Some((VideoCodec::Vp8, None, "preferred_vp8"))
        );
        assert_eq!(
            plan(&both, &hw, true, CodecPreference::PreferH264, false),
            Some((VideoCodec::H264, Some(VideoCodec::Vp8), "preferred_h264"))
        );
        let sw = caps_of(
            "fixture",
            &[
                (K::Vp8, Implementation::Software),
                (K::H264, Implementation::Software),
            ],
        );
        assert_eq!(
            plan(&both, &sw, true, CodecPreference::PreferH264, false),
            Some((VideoCodec::Vp8, None, "h264_not_detected"))
        );
        // No VP8 encoder: no backup can run, so no H.264 primary with one either.
        let h264_only = caps_of("fixture", &[(K::H264, Implementation::Hardware)]);
        assert_eq!(
            plan(&both, &h264_only, true, auto, false),
            Some((VideoCodec::H264, None, "h264_not_detected"))
        );
    }

    #[test]
    fn a_software_h264_encoder_is_never_usable() {
        use kessel_video::VideoCodec as K;
        use sew_proto::voice::VideoCodec as P;
        // The OS's software encoder (macOS's software VideoToolbox entry, Microsoft's MFT,
        // Android's platform AVC) is never usable: VP8 only, and an H.264-only policy is
        // codec_unavailable (phase 1 took any H.264 encoder).
        for backend in ["fixture", "apple", "android", "windows", "linux"] {
            let sw = caps_of(
                backend,
                &[
                    (K::Vp8, Implementation::Software),
                    (K::H264, Implementation::Software),
                ],
            );
            let both = media_policy(&[P::Vp8, P::H264], true);
            let p = choose_codecs_with(&both, &sw, true, CodecPreference::Automatic, false);
            assert_eq!(
                shape(p),
                Some((VideoCodec::Vp8, None, "h264_not_detected")),
                "{backend}"
            );
            let only = media_policy(&[P::H264], true);
            let p = choose_codecs_with(&only, &sw, true, CodecPreference::Automatic, false);
            assert_eq!(shape(p), None, "{backend}: codec_unavailable");
        }
        let vp8 = caps_of("apple", &[(K::Vp8, Implementation::Software)]);
        // (Its reason follows this platform's H.264 decode: `no_h264_decode` on Linux.)
        assert_eq!(
            shape(choose_codecs(&media_policy(&[P::Vp8, P::H264], true), &vp8))
                .map(|(c, b, _)| (c, b)),
            Some((VideoCodec::Vp8, None))
        );
        assert_eq!(
            choose_codecs(&media_policy(&[], true), &caps_of("apple", &[])),
            None
        );
    }

    #[test]
    fn only_macos_and_ios_publish_os_h264() {
        use kessel_video::VideoCodec as K;
        use sew_proto::voice::VideoCodec as P;
        // A backend listing a hardware H.264 encoder: on Linux (never), Android and Windows
        // (not until their backends probe the hardware) the plan is VP8 only.
        let os_allowed = cfg!(any(target_os = "macos", target_os = "ios"));
        for backend in ["apple", "android", "windows", "linux"] {
            let hw = caps_of(
                backend,
                &[
                    (K::Vp8, Implementation::Software),
                    (K::H264, Implementation::Hardware),
                ],
            );
            let both = media_policy(&[P::Vp8, P::H264], true);
            let got = shape(choose_codecs_with(
                &both,
                &hw,
                true,
                CodecPreference::Automatic,
                false,
            ));
            if os_allowed {
                assert_eq!(got.map(|g| g.1), Some(Some(VideoCodec::Vp8)), "{backend}");
            } else {
                assert_eq!(
                    got,
                    Some((VideoCodec::Vp8, None, "h264_not_detected")),
                    "{backend}"
                );
            }
        }
        #[cfg(target_os = "linux")]
        {
            let hw = caps_of("linux", &[(K::H264, Implementation::Hardware)]);
            let only = media_policy(&[P::H264], true);
            for pref in [
                CodecPreference::Automatic,
                CodecPreference::PreferH264,
                CodecPreference::PreferVp8,
            ] {
                assert_eq!(
                    choose_codecs_with(&only, &hw, true, pref, false),
                    None,
                    "Linux never publishes H.264"
                );
            }
        }
    }

    #[test]
    #[cfg(all(feature = "testing", sew_fixtures))]
    fn an_h264_session_opens_hardware_only_and_must_report_hardware() {
        use super::super::fixture::{FixtureBackend, FixtureH264, FixtureOptions};
        let dir = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../sew-media-testclient/fixtures"
        );
        let wake: Wake = Arc::new(|| {});
        let l = LayerSpec::new("q", 320, 180, 150, 30);
        let cfg = encoder_config(VideoCodec::H264, &l, ContentHint::Camera);
        assert_eq!(cfg.implementation, ImplPreference::HardwareOnly);
        let cfg = encoder_config(VideoCodec::Vp8, &l, ContentHint::Camera);
        assert_eq!(cfg.implementation, ImplPreference::PreferHardware);
        let cap = CodecCapability::new(
            kessel_video::VideoCodec::H264,
            CodecDirection::Encode,
            Implementation::Hardware,
            "hw",
            1280,
            720,
            30,
        );
        let plan = vec![
            LayerSpec::new("q", 320, 180, 150, 30),
            LayerSpec::new("h", 640, 360, 500, 30),
        ];
        // Software (refuses hardware-only), a hardware open that fails, a session that says
        // software, a session that can't tell (an iOS before 17.4): nothing opens. Hardware:
        // every layer.
        for (mode, ok) in [
            (FixtureH264::Software, false),
            (FixtureH264::HardwareOpenFails, false),
            (FixtureH264::HardwareReportsSoftware, false),
            (FixtureH264::HardwareReportsUnknown, false),
            (FixtureH264::Hardware, true),
        ] {
            let fx = FixtureBackend::load_with(dir, FixtureOptions::new().with_h264(mode))
                .expect("the fixtures");
            let b: Arc<dyn VideoBackend> = fx.clone();
            let one = open_encoder(&b, VideoCodec::H264, &l, ContentHint::Camera, &wake);
            assert_eq!(one.is_ok(), ok, "{mode:?}");
            drop(one);
            let encs = open_encoders(
                &b,
                VideoCodec::H264,
                &cap,
                plan.clone(),
                ContentHint::Camera,
                &wake,
            );
            assert_eq!(encs.len(), if ok { 2 } else { 0 }, "{mode:?}");
            drop(encs);
            assert_eq!(
                fx.encoders_open(kessel_video::VideoCodec::H264),
                0,
                "{mode:?}"
            );
            // VP8 is kessel-vp8's software encoder, opened with the default preference.
            assert!(open_encoder(&b, VideoCodec::Vp8, &l, ContentHint::Camera, &wake).is_ok());
        }
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
        let backend: Arc<dyn VideoBackend> = HwFake::new(kessel_video::fake::FakeConfig::default());
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
                h264_lost: Arc::new(Mutex::new(None)),
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
            call: 1,
            backup: None,
            vp8: None,
            temporal: None,
            live: Arc::new(Mutex::new(VideoCodec::H264)),
            phone: false,
            machine: backend.subscribe(),
            thermal: kessel_video::ThermalLevel::Nominal,
            backup_converts: Arc::default(),
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
        let backend: Arc<dyn VideoBackend> = HwFake::new(cfg);
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
                h264_lost: Arc::new(Mutex::new(None)),
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
            call: 1,
            backup: None,
            vp8: None,
            temporal: None,
            live: Arc::new(Mutex::new(VideoCodec::H264)),
            phone: false,
            machine: backend.subscribe(),
            thermal: kessel_video::ThermalLevel::Nominal,
            backup_converts: Arc::default(),
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
        ctl: mpsc::UnboundedSender<VideoControl>,
        cmds: mpsc::UnboundedReceiver<crate::call::Cmd>,
        fallbacks: mpsc::UnboundedReceiver<super::super::publish::ProducerFallback>,
        h264_lost: Arc<Mutex<Option<u64>>>,
        converts: Arc<std::sync::atomic::AtomicU64>,
    }

    impl TestPipe {
        /// The pipeline over `src`: one H.264 q layer on `backend`.
        fn start(
            backend: &Arc<dyn VideoBackend>,
            src: Box<dyn FrameSource>,
            bell: &Arc<Bell>,
        ) -> TestPipe {
            TestPipe::start_with(backend, src, bell, None)
        }

        /// [`TestPipe::start`] with a VP8 backup of `backup` layers (and the backend's VP8 for
        /// a fall back).
        fn start_with(
            backend: &Arc<dyn VideoBackend>,
            src: Box<dyn FrameSource>,
            bell: &Arc<Bell>,
            backup: Option<Vec<LayerSpec>>,
        ) -> TestPipe {
            TestPipe::start_full(backend, src, bell, backup, false)
        }

        /// [`TestPipe::start_with`] on a `phone` (thermal steps), or not.
        fn start_full(
            backend: &Arc<dyn VideoBackend>,
            src: Box<dyn FrameSource>,
            bell: &Arc<Bell>,
            backup: Option<Vec<LayerSpec>>,
            phone: bool,
        ) -> TestPipe {
            let converts: Arc<std::sync::atomic::AtomicU64> = Arc::default();
            let vp8 = backend
                .capabilities()
                .best(kessel_video::VideoCodec::Vp8, CodecDirection::Encode)
                .cloned();
            let backup = backup.map(|l| BackupPipe::new(VideoCodec::Vp8, l));
            let h264_lost = Arc::new(Mutex::new(None));
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
            let (publication, rx, ctl, _ends, fallbacks) =
                VideoPublication::for_test_with_fallbacks(VideoSource::Camera);
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
                    h264_lost: h264_lost.clone(),
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
                call: 1,
                backup,
                vp8,
                temporal: None,
                live: Arc::new(Mutex::new(VideoCodec::H264)),
                phone,
                machine: backend.subscribe(),
                thermal: kessel_video::ThermalLevel::Nominal,
                backup_converts: converts.clone(),
            };
            let thread = std::thread::spawn(move || pipe.run());
            TestPipe {
                to_pipe,
                rx,
                notes,
                stop,
                bell: bell.clone(),
                thread: Some(thread),
                ctl,
                cmds,
                fallbacks,
                h264_lost,
                converts,
            }
        }

        /// Sends the engine's control `c`.
        fn control(&self, c: VideoControl) {
            self.ctl.send(c).expect("the pipeline runs");
            self.bell.ring();
        }

        /// The frames queued within `within`: `(rid, codec, keyframe)`.
        fn frames_for(&mut self, within: Duration) -> Vec<(String, VideoCodec, bool)> {
            let until = std::time::Instant::now() + within;
            let mut out = Vec::new();
            while std::time::Instant::now() < until {
                while let Ok(f) = self.rx.try_recv() {
                    out.push((f.frame.rid.clone(), f.frame.codec, f.frame.keyframe));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            out
        }

        /// The frames queued until `enough` holds of them (at most 20 s: a slow host only takes
        /// longer, it never starves the count): `(rid, codec, keyframe)`.
        fn frames_until(
            &mut self,
            what: &str,
            enough: impl Fn(&[(String, VideoCodec, bool)]) -> bool,
        ) -> Vec<(String, VideoCodec, bool)> {
            let until = std::time::Instant::now() + Duration::from_secs(20);
            let mut out = Vec::new();
            while !enough(&out) {
                assert!(
                    std::time::Instant::now() < until,
                    "{what} within 20 s: {out:?}"
                );
                while let Ok(f) = self.rx.try_recv() {
                    out.push((f.frame.rid.clone(), f.frame.codec, f.frame.keyframe));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            out
        }

        /// Drains the frames until `done` holds (at most 5 s).
        fn drain_until(&mut self, what: &str, done: impl Fn() -> bool) {
            let until = std::time::Instant::now() + Duration::from_secs(5);
            while !done() {
                assert!(std::time::Instant::now() < until, "{what} within 5 s");
                while self.rx.try_recv().is_ok() {}
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        /// Waits (at most 5 s) for the pipeline's fall back to one codec, then 500 ms more: the
        /// fall back and every frame queued meanwhile.
        fn until_fallback(
            &mut self,
        ) -> (
            super::super::publish::ProducerFallback,
            Vec<(String, VideoCodec, bool)>,
        ) {
            let until = std::time::Instant::now() + Duration::from_secs(5);
            let mut seen = Vec::new();
            let f = loop {
                if let Ok(f) = self.fallbacks.try_recv() {
                    break f;
                }
                assert!(
                    std::time::Instant::now() < until,
                    "no fall back to VP8 alone"
                );
                seen.extend(self.frames_for(Duration::from_millis(20)));
            };
            seen.extend(self.frames_for(Duration::from_millis(500)));
            (f, seen)
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
    /// kessel-video's fake backend with its structural H.264 as a hardware encoder (listed
    /// as one, opened hardware-only, each session reporting the hardware), with faults: VP8
    /// opens that fail, H.264 sessions lost (and their reopens refused).
    struct HwFake {
        inner: Arc<dyn VideoBackend>,
        vp8_failures: std::sync::atomic::AtomicU32,
        vp8_live: Arc<std::sync::atomic::AtomicU64>,
        h264_epoch: Arc<std::sync::atomic::AtomicU64>,
        h264_refused: AtomicBool,
        /// Encoders opened from now on lag one frame ([`Lag`]).
        lag: AtomicBool,
    }

    impl HwFake {
        fn new(cfg: kessel_video::fake::FakeConfig) -> Arc<HwFake> {
            let mut cfg = cfg;
            cfg.codec = kessel_video::fake::FakeCodec::Structural;
            Arc::new(HwFake {
                inner: kessel_video::fake::FakeBackend::new(cfg),
                vp8_failures: Default::default(),
                vp8_live: Default::default(),
                h264_epoch: Default::default(),
                h264_refused: AtomicBool::new(false),
                lag: AtomicBool::new(false),
            })
        }

        fn fail_vp8_opens(&self, n: u32) {
            self.vp8_failures.store(n, Ordering::SeqCst);
        }

        fn vp8_open(&self) -> u64 {
            self.vp8_live.load(Ordering::SeqCst)
        }

        fn lose_h264(&self) {
            self.h264_refused.store(true, Ordering::SeqCst);
            self.h264_epoch.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// A wrapped encoder: an H.264 one reports the hardware and is lost with its epoch; a VP8
    /// one is counted while open.
    struct Wrapped {
        inner: Box<dyn VideoEncoder>,
        lost: Option<(Arc<std::sync::atomic::AtomicU64>, u64)>,
        live: Option<Arc<std::sync::atomic::AtomicU64>>,
        lag: Option<Lag>,
    }

    /// An encoder one frame behind, as a threaded one whose worker hasn't taken its input yet:
    /// a submitted frame waits for the next submit, and a `request_keyframe` marks the input
    /// it takes next (the one waiting), not the next one submitted.
    #[derive(Default)]
    struct Lag {
        held: Option<(kessel_video::VideoFrame, EncodeOptions)>,
        key_asked: bool,
    }

    impl Drop for Wrapped {
        fn drop(&mut self) {
            if let Some(l) = &self.live {
                l.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }

    impl VideoEncoder for Wrapped {
        fn submit(
            &mut self,
            frame: &kessel_video::VideoFrame,
            opts: EncodeOptions,
        ) -> Result<(), KError> {
            let Some(lag) = self.lag.as_mut() else {
                return self.inner.submit(frame, opts);
            };
            if let Some((f, o)) = lag.held.replace((frame.clone(), opts)) {
                let key = o.keyframe || std::mem::take(&mut lag.key_asked);
                let o = if key {
                    EncodeOptions::keyframe()
                } else {
                    EncodeOptions::default()
                };
                self.inner.submit(&f, o)?;
            }
            Ok(())
        }
        fn poll(&mut self) -> Option<Result<kessel_video::EncodedFrame, KError>> {
            if let Some((epoch, at)) = &self.lost
                && epoch.load(Ordering::SeqCst) != *at
            {
                return Some(Err(KError::DeviceLost));
            }
            self.inner.poll()
        }
        fn request_keyframe(&mut self) {
            match self.lag.as_mut() {
                Some(lag) => lag.key_asked = true,
                None => self.inner.request_keyframe(),
            }
        }
        fn set_rates(&mut self, rates: Rates) -> Result<(), KError> {
            self.inner.set_rates(rates)
        }
        fn set_size(&mut self, width: u32, height: u32) -> Result<(), KError> {
            self.inner.set_size(width, height)
        }
        fn info(&self) -> kessel_video::EncoderInfo {
            self.inner.info()
        }
        fn stats(&self) -> kessel_video::EncoderStats {
            self.inner.stats()
        }
        fn implementation(&self) -> Implementation {
            if self.lost.is_some() {
                Implementation::Hardware
            } else {
                self.inner.implementation()
            }
        }
    }

    impl VideoBackend for HwFake {
        fn capabilities(&self) -> Capabilities {
            let mut c = self.inner.capabilities();
            for e in &mut c.codecs {
                if e.codec == kessel_video::VideoCodec::H264 {
                    e.implementation = Implementation::Hardware;
                }
            }
            c
        }
        fn now_us(&self) -> u64 {
            self.inner.now_us()
        }
        fn cameras(&self) -> Vec<kessel_video::CameraInfo> {
            self.inner.cameras()
        }
        fn camera_permission(&self) -> kessel_video::Permission {
            self.inner.camera_permission()
        }
        fn request_camera_permission(&self, done: kessel_video::Done<kessel_video::Permission>) {
            self.inner.request_camera_permission(done);
        }
        fn open_camera(
            &self,
            id: Option<&str>,
            constraints: CaptureConstraints,
            wake: Wake,
        ) -> Result<Box<dyn FrameSource>, KError> {
            self.inner.open_camera(id, constraints, wake)
        }
        fn screen_permission(&self) -> kessel_video::Permission {
            self.inner.screen_permission()
        }
        fn request_screen_permission(&self, done: kessel_video::Done<kessel_video::Permission>) {
            self.inner.request_screen_permission(done);
        }
        fn screen_sources(&self) -> Result<Vec<kessel_video::ScreenSourceInfo>, KError> {
            self.inner.screen_sources()
        }
        fn pick_screen(
            &self,
            opts: kessel_video::PickOptions,
            done: kessel_video::Done<Result<kessel_video::ScreenSelection, KError>>,
        ) {
            self.inner.pick_screen(opts, done);
        }
        fn open_screen(
            &self,
            source: &kessel_video::ScreenSelection,
            constraints: CaptureConstraints,
            wake: Wake,
        ) -> Result<Box<dyn FrameSource>, KError> {
            self.inner.open_screen(source, constraints, wake)
        }
        fn encoder(
            &self,
            mut config: EncoderConfig,
            wake: Wake,
        ) -> Result<Box<dyn VideoEncoder>, KError> {
            if config.codec.codec() == kessel_video::VideoCodec::H264 {
                if self.h264_refused.load(Ordering::SeqCst) {
                    return Err(KError::NoResources);
                }
                // The fake has no hardware: the adapter stands in for one.
                config.implementation = ImplPreference::PreferHardware;
                let at = self.h264_epoch.load(Ordering::SeqCst);
                let inner = self.inner.encoder(config, wake)?;
                return Ok(Box::new(Wrapped {
                    inner,
                    lost: Some((self.h264_epoch.clone(), at)),
                    live: None,
                    lag: self.lag.load(Ordering::SeqCst).then(Lag::default),
                }));
            }
            let fail = self
                .vp8_failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok();
            if fail {
                return Err(KError::NoResources);
            }
            let inner = self.inner.encoder(config, wake)?;
            self.vp8_live.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(Wrapped {
                inner,
                lost: None,
                live: Some(self.vp8_live.clone()),
                lag: self.lag.load(Ordering::SeqCst).then(Lag::default),
            }))
        }
        fn decoder(
            &self,
            config: kessel_video::DecoderConfig,
            wake: Wake,
        ) -> Result<Box<dyn kessel_video::VideoDecoder>, KError> {
            self.inner.decoder(config, wake)
        }
        fn convert(
            &self,
            frame: &kessel_video::VideoFrame,
            width: u32,
            height: u32,
            format: PixelFormat,
        ) -> Result<kessel_video::VideoFrame, KError> {
            self.inner.convert(frame, width, height, format)
        }
        fn subscribe(&self) -> kessel_video::EventRx {
            self.inner.subscribe()
        }
        fn shutdown(&self) {
            self.inner.shutdown();
        }
    }

    fn rate(rid: &str, active: bool) -> LayerRate {
        LayerRate {
            rid: rid.to_owned(),
            active,
            bitrate_bps: if active { 150_000 } else { 0 },
            fps: 30,
        }
    }

    #[test]
    fn the_backup_runs_between_its_start_and_stop_from_the_same_capture() {
        let fake = HwFake::new(kessel_video::fake::FakeConfig::default());
        let backend: Arc<dyn VideoBackend> = fake.clone();
        let bell = Arc::new(Bell::default());
        let src = fake_camera(&backend, &bell);
        let q = LayerSpec::new("q", 320, 180, 150, 30);
        let mut p = TestPipe::start_with(&backend, src, &bell, Some(vec![q.clone()]));
        let count = |fs: &[(String, VideoCodec, bool)], c: VideoCodec| {
            fs.iter().filter(|f| f.1 == c).count()
        };
        // Before any start: H.264 only, no VP8 encoder open.
        let before = p.frames_until("3 H.264 frames", |fs| count(fs, VideoCodec::H264) >= 3);
        assert!(before.iter().all(|f| f.1 == VideoCodec::H264), "{before:?}");
        assert_eq!(
            fake.vp8_open(),
            0,
            "the backup's encoders open only on demand"
        );
        // BackupStart, then its rates: VP8 frames of q, the first a keyframe.
        p.control(VideoControl::BackupStart(vec![q.clone()]));
        p.control(VideoControl::BackupRates(vec![rate("q", true)]));
        let on = p.frames_until("3 VP8 frames and the primary's", |fs| {
            count(fs, VideoCodec::Vp8) >= 3 && count(fs, VideoCodec::H264) >= 1
        });
        let vp8: Vec<&(String, VideoCodec, bool)> =
            on.iter().filter(|f| f.1 == VideoCodec::Vp8).collect();
        assert!(vp8[0].2, "the backup's first frame is a keyframe: {vp8:?}");
        assert!(vp8.iter().all(|f| f.0 == "q"));
        assert!(
            on.iter().any(|f| f.1 == VideoCodec::H264),
            "the primary goes on"
        );
        assert_eq!(fake.vp8_open(), 1);
        // A backup keyframe on request.
        p.control(VideoControl::BackupKeyframe { rid: "q".into() });
        let _ = p.frames_until("a backup keyframe", |fs| {
            fs.iter().any(|f| f.1 == VideoCodec::Vp8 && f.2)
        });
        // BackupStop: its encoder closed, then no more VP8.
        p.control(VideoControl::BackupStop);
        p.drain_until("the backup's encoder closed", || fake.vp8_open() == 0);
        // What the closed encoder had queued is out by now.
        let _ = p.frames_for(Duration::from_millis(200));
        let after = p.frames_until("3 H.264 frames", |fs| count(fs, VideoCodec::H264) >= 3);
        assert!(after.iter().all(|f| f.1 == VideoCodec::H264), "{after:?}");
        assert_eq!(fake.vp8_open(), 0, "the backup's encoder closed");
        assert!(p.fallbacks.try_recv().is_err(), "nothing fell back");
        p.finish();
    }

    #[test]
    fn a_backup_that_does_not_open_is_tried_once_then_the_publication_goes_vp8_alone() {
        let fake = HwFake::new(kessel_video::fake::FakeConfig::default());
        let backend: Arc<dyn VideoBackend> = fake.clone();
        let bell = Arc::new(Bell::default());
        let src = fake_camera(&backend, &bell);
        let q = LayerSpec::new("q", 320, 180, 150, 30);
        let mut p = TestPipe::start_with(&backend, src, &bell, Some(vec![q.clone()]));
        // One failure: the retry after a second opens it.
        fake.fail_vp8_opens(1);
        p.control(VideoControl::BackupStart(vec![q.clone()]));
        p.control(VideoControl::BackupRates(vec![rate("q", true)]));
        let early = p.frames_for(Duration::from_millis(600));
        assert!(early.iter().all(|f| f.1 == VideoCodec::H264), "{early:?}");
        let later = p.frames_for(Duration::from_millis(900));
        assert!(
            later.iter().any(|f| f.1 == VideoCodec::Vp8 && f.2),
            "the retry opened the backup: {later:?}"
        );
        assert!(p.fallbacks.try_recv().is_err());
        // Stopped, then started again with two failures: the retry fails too, and the
        // publication is published again in VP8 alone (its own q, a keyframe first).
        p.control(VideoControl::BackupStop);
        let _ = p.frames_for(Duration::from_millis(200));
        fake.fail_vp8_opens(2);
        p.control(VideoControl::BackupStart(vec![q.clone()]));
        p.control(VideoControl::BackupRates(vec![rate("q", true)]));
        let (f, seen) = p.until_fallback();
        assert_eq!(f.codec, VideoCodec::Vp8);
        let rids: Vec<&str> = f.layers.iter().map(|l| l.rid.as_str()).collect();
        assert_eq!(rids, vec!["q"]);
        assert_vp8_alone_from_a_keyframe(&seen);
        p.finish();
    }

    /// From the first VP8 frame on: VP8 only, the first a keyframe, and some frames.
    fn assert_vp8_alone_from_a_keyframe(seen: &[(String, VideoCodec, bool)]) {
        let at = seen
            .iter()
            .position(|f| f.1 == VideoCodec::Vp8)
            .unwrap_or_else(|| panic!("no VP8 frame after the fall back: {seen:?}"));
        assert!(seen[at].2, "a keyframe first: {seen:?}");
        let rest = &seen[at..];
        assert!(rest.len() >= 3, "{seen:?}");
        assert!(
            rest.iter().all(|f| f.1 == VideoCodec::Vp8),
            "VP8 alone: {seen:?}"
        );
    }

    #[test]
    fn a_lost_hardware_encoder_publishes_vp8_alone_and_h264_is_not_tried_again() {
        let fake = HwFake::new(kessel_video::fake::FakeConfig::default());
        let backend: Arc<dyn VideoBackend> = fake.clone();
        let bell = Arc::new(Bell::default());
        let src = fake_camera(&backend, &bell);
        let q = LayerSpec::new("q", 320, 180, 150, 30);
        let mut p = TestPipe::start_with(&backend, src, &bell, Some(vec![q]));
        let before = p.frames_for(Duration::from_millis(300));
        assert!(before.iter().any(|f| f.1 == VideoCodec::H264));
        fake.lose_h264();
        let (f, seen) = p.until_fallback();
        assert_eq!(f.codec, VideoCodec::Vp8);
        assert_eq!(*lock(&p.h264_lost), Some(1), "no H.264 again in this call");
        assert_vp8_alone_from_a_keyframe(&seen);
        assert!(
            p.cmds.try_recv().is_err(),
            "not unpublished: the lease is kept"
        );
        p.finish();
    }

    #[test]
    fn the_backups_ladder_is_the_primarys_and_a_phones_stops_at_360p() {
        let layers = vec![
            LayerSpec::new("q", 320, 180, 150, 30),
            LayerSpec::new("h", 640, 360, 500, 30),
            LayerSpec::new("f", 1280, 720, 1700, 30),
        ];
        let rids = |l: &[LayerSpec]| l.iter().map(|x| x.rid.clone()).collect::<Vec<_>>();
        let desk = backup_ladder(&layers, false, 3);
        assert_eq!(rids(&desk), vec!["q", "h", "f"]);
        assert!(desk.iter().all(|l| l.temporal_layers == 3));
        assert_eq!(rids(&backup_ladder(&layers, true, 2)), vec!["q", "h"]);
        // A portrait phone camera: 360 × 640 is 360p.
        let portrait = vec![
            LayerSpec::new("q", 180, 320, 150, 30),
            LayerSpec::new("h", 360, 640, 500, 30),
            LayerSpec::new("f", 720, 1280, 1700, 30),
        ];
        assert_eq!(rids(&backup_ladder(&portrait, true, 1)), vec!["q", "h"]);
        // Only a 720p layer: the phone keeps it (a backup always has a layer).
        let big = vec![LayerSpec::new("f", 1280, 720, 1700, 30)];
        assert_eq!(rids(&backup_ladder(&big, true, 1)), vec!["f"]);
    }

    #[test]
    fn one_capture_feeds_both_codecs_and_the_backup_converts_once_per_frame() {
        let fake = HwFake::new(kessel_video::fake::FakeConfig::default());
        let backend: Arc<dyn VideoBackend> = fake.clone();
        let bell = Arc::new(Bell::default());
        // A 1280 × 720 camera; the backup runs q and h (640 × 360 the largest).
        let src = backend
            .open_camera(None, CaptureConstraints::new(1280, 720, 30), bell.wake())
            .expect("the fake camera opens");
        let q = LayerSpec::new("q", 320, 180, 150, 30);
        let h = LayerSpec::new("h", 640, 360, 500, 30);
        let mut p = TestPipe::start_with(&backend, src, &bell, Some(vec![q.clone(), h.clone()]));
        let _ = p.frames_until("3 frames", |fs| fs.len() >= 3);
        assert_eq!(
            p.converts.load(Ordering::SeqCst),
            0,
            "nothing is converted while the backup doesn't run"
        );
        p.control(VideoControl::BackupStart(vec![q, h]));
        p.control(VideoControl::BackupRates(vec![
            rate("q", true),
            rate("h", true),
        ]));
        let n = |fs: &[(String, VideoCodec, bool)], c: VideoCodec, rid: Option<&str>| {
            fs.iter()
                .filter(|f| f.1 == c && rid.is_none_or(|r| f.0 == r))
                .count() as u64
        };
        // Both codecs from the one capture: 20 frames of the primary, 5 of each backup layer.
        let on = p.frames_until("both codecs", |fs| {
            n(fs, VideoCodec::Vp8, Some("q")) >= 5
                && n(fs, VideoCodec::Vp8, Some("h")) >= 5
                && n(fs, VideoCodec::H264, None) >= 20
        });
        let converts = p.converts.load(Ordering::SeqCst);
        let (nq, nh) = (
            n(&on, VideoCodec::Vp8, Some("q")),
            n(&on, VideoCodec::Vp8, Some("h")),
        );
        let primary = n(&on, VideoCodec::H264, None);
        // Once per captured frame (the primary's H.264 makes one frame per capture), not once
        // per backup layer (q and h come from one conversion: once per layer would be about
        // twice the primary's frames).
        assert!(
            converts >= primary / 2 && converts <= primary + 5,
            "{converts} conversions for {primary} captured frames ({nq} q, {nh} h)"
        );
        p.control(VideoControl::BackupStop);
        p.drain_until("the backup's encoders closed", || fake.vp8_open() == 0);
        let _ = p.frames_for(Duration::from_millis(200));
        let c0 = p.converts.load(Ordering::SeqCst);
        let _ = p.frames_for(Duration::from_millis(400));
        assert_eq!(
            p.converts.load(Ordering::SeqCst),
            c0,
            "no conversion after the stop"
        );
        p.finish();
    }

    #[test]
    fn heat_caps_a_phones_backup_but_never_stops_it() {
        use kessel_video::ThermalLevel as T;
        assert_eq!(thermal_backup_caps(T::Nominal), BackupCaps::default());
        assert_eq!(thermal_backup_caps(T::Fair), BackupCaps::default());
        assert_eq!(
            thermal_backup_caps(T::Serious),
            BackupCaps {
                max_layers: Some(1),
                max_fps: None
            }
        );
        assert_eq!(
            thermal_backup_caps(T::Critical),
            BackupCaps {
                max_layers: Some(1),
                max_fps: Some(15)
            }
        );
        // The pipeline on a phone sends the engine each new level's caps.
        let mut cfg = kessel_video::fake::FakeConfig::default();
        cfg.backend_events = vec![
            (
                Duration::from_millis(400),
                kessel_video::BackendEvent::Thermal(T::Serious),
            ),
            (
                Duration::from_millis(700),
                kessel_video::BackendEvent::Thermal(T::Critical),
            ),
            (
                Duration::from_millis(1000),
                kessel_video::BackendEvent::Thermal(T::Nominal),
            ),
        ];
        let fake = HwFake::new(cfg);
        let backend: Arc<dyn VideoBackend> = fake.clone();
        let bell = Arc::new(Bell::default());
        let src = fake_camera(&backend, &bell);
        let q = LayerSpec::new("q", 320, 180, 150, 30);
        let mut p = TestPipe::start_full(&backend, src, &bell, Some(vec![q]), true);
        let until = std::time::Instant::now() + Duration::from_secs(5);
        let mut got = Vec::new();
        while got.len() < 3 {
            assert!(std::time::Instant::now() < until, "caps so far: {got:?}");
            while let Ok(c) = p.cmds.try_recv() {
                if let crate::call::Cmd::Publish(super::super::publish::VideoCmd::BackupCaps(c)) = c
                {
                    got.push(c);
                }
            }
            let _ = p.frames_for(Duration::from_millis(20));
        }
        assert_eq!(
            got,
            vec![
                thermal_backup_caps(T::Serious),
                thermal_backup_caps(T::Critical),
                BackupCaps::default()
            ]
        );
        p.finish();
    }

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
        let backend: Arc<dyn VideoBackend> = HwFake::new(cfg);
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
        let old_backend: Arc<dyn VideoBackend> = HwFake::new(cfg);
        let backend: Arc<dyn VideoBackend> = HwFake::new(kessel_video::fake::FakeConfig::default());
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
        let backend: Arc<dyn VideoBackend> = HwFake::new(kessel_video::fake::FakeConfig::default());
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

    /// A switch while the encoder still holds the old camera's last frame (a threaded encoder's
    /// pending input): the keyframe goes on the new camera's first frame, not on that old
    /// frame (which would leave the new camera's frames as deltas on another picture).
    #[test]
    fn a_switch_keys_the_new_cameras_first_frame_not_a_pending_old_one() {
        let fake = HwFake::new(kessel_video::fake::FakeConfig::default());
        fake.lag.store(true, Ordering::SeqCst);
        let backend: Arc<dyn VideoBackend> = fake.clone();
        let bell = Arc::new(Bell::default());
        let src = fake_camera(&backend, &bell);
        let mut p = TestPipe::start(&backend, src, &bell);
        let _ = timed_until(&mut p.rx, "the first frames", 4);
        let (tx, done) = std::sync::mpsc::channel();
        p.send(ToPipe::Release(tx));
        done.recv_timeout(Duration::from_secs(5)).expect("released");
        let released = backend.now_us();
        let next = fake_camera(&backend, &bell);
        p.send(ToPipe::Source(
            next,
            vec![LayerSpec::new("q", 320, 180, 150, 30)],
            Facing::Back,
        ));
        let mut got = Vec::new();
        let until = std::time::Instant::now() + Duration::from_secs(10);
        while got.iter().filter(|(t, _)| *t >= released).count() < 3 {
            assert!(
                std::time::Instant::now() < until,
                "the new camera's frames: {got:?}"
            );
            got.extend(timed(&mut p.rx));
            std::thread::sleep(Duration::from_millis(5));
        }
        let old: Vec<bool> = got
            .iter()
            .filter(|(t, _)| *t < released)
            .map(|f| f.1)
            .collect();
        let new: Vec<bool> = got
            .iter()
            .filter(|(t, _)| *t >= released)
            .map(|f| f.1)
            .collect();
        assert!(
            !old.is_empty(),
            "the old camera's last frame came out after the switch (the lag): {got:?}"
        );
        assert!(
            new[0],
            "the new camera's first frame is a keyframe: {got:?}"
        );
        assert!(new[1..].iter().all(|k| !k), "one keyframe: {got:?}");
        assert!(
            !old.iter().any(|k| *k),
            "no keyframe on the old camera's last frame: {got:?}"
        );
        p.finish();
    }

    /// The frames queued for the engine: `(capture_us, keyframe)`.
    fn timed(rx: &mut mpsc::Receiver<super::super::publish::VideoIn>) -> Vec<(u64, bool)> {
        let mut out = Vec::new();
        while let Ok(f) = rx.try_recv() {
            out.push((f.frame.capture_us, f.frame.keyframe));
        }
        out
    }

    fn timed_until(
        rx: &mut mpsc::Receiver<super::super::publish::VideoIn>,
        what: &str,
        n: usize,
    ) -> Vec<(u64, bool)> {
        let until = std::time::Instant::now() + Duration::from_secs(10);
        let mut got = Vec::new();
        while got.len() < n {
            assert!(
                std::time::Instant::now() < until,
                "timed out waiting for {what}"
            );
            got.extend(timed(rx));
            std::thread::sleep(Duration::from_millis(5));
        }
        got
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
        let backend: Arc<dyn VideoBackend> = HwFake::new(cfg);
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
        assert_eq!(VideoError::NoFrames.code(), "no_frames");
    }

    /// A test camera: its scripted events, frames from `frames_from` on (never with `None`),
    /// and a drop that takes `drop_takes` (an OS slow to close the device).
    struct Scripted {
        events: std::collections::VecDeque<SourceEvent>,
        frames_from: Option<std::time::Instant>,
        drop_takes: Duration,
    }

    impl Scripted {
        fn new(events: Vec<SourceEvent>, frames_after: Option<Duration>) -> Scripted {
            Scripted {
                events: events.into(),
                frames_from: frames_after.map(|d| std::time::Instant::now() + d),
                drop_takes: Duration::ZERO,
            }
        }
    }

    impl Drop for Scripted {
        fn drop(&mut self) {
            std::thread::sleep(self.drop_takes);
        }
    }

    impl FrameSource for Scripted {
        fn try_recv(&mut self) -> Option<kessel_video::VideoFrame> {
            if self
                .frames_from
                .is_some_and(|t| std::time::Instant::now() >= t)
            {
                kessel_video::VideoFrame::filled(PixelFormat::I420, 64, 36, [16, 128, 128], 0).ok()
            } else {
                None
            }
        }
        fn try_event(&mut self) -> Option<SourceEvent> {
            self.events.pop_front()
        }
        fn set_constraints(&mut self, _c: CaptureConstraints) -> Result<(), KError> {
            Ok(())
        }
        fn set_paused(&mut self, _paused: bool) {}
        fn stats(&self) -> kessel_video::SourceStats {
            kessel_video::SourceStats::default()
        }
    }

    /// A camera the OS opens but feeds nothing (a lid-closed MacBook's, suspended): refused
    /// with `no_frames` after the watchdog's time, never later.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_camera_without_frames_is_no_frames_after_the_watchdog() {
        let began = std::time::Instant::now();
        let src = Scripted::new(
            vec![SourceEvent::Interrupted(
                kessel_video::Interruption::Suspended,
            )],
            None,
        );
        let r = wait_first_frame(Box::new(src)).await;
        assert!(
            matches!(r, Err(VideoError::NoFrames)),
            "{:?}",
            r.as_ref().map(|(_, s)| s)
        );
        let took = began.elapsed();
        assert!(
            took >= FIRST_FRAME_WAIT && took < FIRST_FRAME_WAIT + Duration::from_secs(2),
            "the watchdog's time: {took:?}"
        );
    }

    /// The first frame is kept for the pipeline, after the events seen before it (a suspension
    /// and its resume), and the size is the frame's when no `Started` came.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_first_frame_comes_after_the_events_seen_before_it() {
        let src = Scripted::new(
            vec![
                SourceEvent::Interrupted(kessel_video::Interruption::Suspended),
                SourceEvent::Resumed,
            ],
            Some(Duration::from_millis(300)),
        );
        let (mut primed, size) = wait_first_frame(Box::new(src)).await.expect("a frame");
        assert_eq!(size, (64, 36));
        assert!(primed.try_recv().is_none(), "the events first");
        assert_eq!(
            primed.try_event(),
            Some(SourceEvent::Interrupted(
                kessel_video::Interruption::Suspended
            ))
        );
        assert_eq!(primed.try_event(), Some(SourceEvent::Resumed));
        assert_eq!(primed.try_event(), None);
        let f = primed.try_recv().expect("the kept first frame");
        assert_eq!((f.width, f.height), (64, 36));
        // A source that ends before its first frame is `camera_not_found`.
        let ended = Scripted::new(
            vec![SourceEvent::Ended(kessel_video::EndReason::DeviceLost)],
            None,
        );
        assert!(matches!(
            wait_first_frame(Box::new(ended)).await,
            Err(VideoError::CameraNotFound)
        ));
    }

    /// A pipeline whose camera closes slowly (its drop waits on the OS) stops within
    /// [`STOP_WAIT`]: the stop never waits on the device; the close finishes on its own thread.
    #[test]
    fn a_pipeline_whose_camera_closes_slowly_stops_within_the_bound() {
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            let mut src = Scripted::new(Vec::new(), None);
            src.drop_takes = Duration::from_secs(8);
            while !s2.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
            drop(src);
        });
        let (to_pipe, _from) = std::sync::mpsc::channel();
        let cap = caps(&[kessel_video::VideoCodec::Vp8]).codecs[0].clone();
        let r = Running {
            call: 1,
            stop,
            ended: Arc::new(AtomicBool::new(false)),
            bell: Arc::new(Bell::default()),
            to_pipe,
            thread: Some(thread),
            codec: VideoCodec::Vp8,
            live: Arc::new(Mutex::new(VideoCodec::Vp8)),
            cap,
            layers: Vec::new(),
            size: (640, 360),
            device: None,
            temporal: None,
        };
        let began = std::time::Instant::now();
        r.stop();
        let took = began.elapsed();
        assert!(
            took >= STOP_WAIT && took < STOP_WAIT + Duration::from_secs(1),
            "the stop waited {took:?}"
        );
    }
}
