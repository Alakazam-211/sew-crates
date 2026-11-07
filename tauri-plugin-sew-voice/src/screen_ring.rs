//! The iPhone screen share's consumer side (feature `ios-screen-ring`, with `video-core`).
//!
//! iOS captures the screen only in a broadcast upload extension: a separate process that the
//! person starts from the system's broadcast picker, outside the app. The extension writes each
//! screen frame (raw NV12, no keys) into a [`kessel_frame_ring`] ring, a single-producer,
//! single-consumer shared-memory file in the App Group container that the app and the extension
//! share. This module is the app's side:
//!
//! 1. [`RingPump::create`] makes the ring at [`ring_path`] (`<container>/sew-screen.ring`);
//! 2. [`RingPump::step`], about every 10 ms ([`POLL`]), reads the newest frame (older ones are
//!    skipped; it never blocks) and pushes it into a kessel-video frame-in source;
//! 3. the first frame of a share hands that source out ([`PumpStep::Start`]), and the driver
//!    ([`drive`]) publishes it as the call's `screen` track through the engine's
//!    `VideoBridge::publish_source` (the same codec choice, encoders and layers as a desktop
//!    screen);
//! 4. the share ends ([`PumpStep::Ended`]) when the extension says it finished, dies, goes quiet
//!    without saying it runs, or the ring is lost; the driver unpublishes the track. Each end
//!    makes a fresh ring, so the next broadcast starts a new share.
//!
//! **The ring is hostile input.** The extension is another process with the file open read and
//! write. kessel-frame-ring's consumer checks every slot against the app's own layout before
//! copying (sizes, strides, rotation, colour, length, the seqlock); this module checks each
//! frame again before it builds a kessel-video frame, maps the extension's timestamp with
//! saturating maths, and skips (counts) anything that doesn't fit instead of panicking.
//!
//! **iOS file locks.** iOS ends a suspended app that holds a file lock in a shared container
//! (`0xdead10cc`). The consumer takes the ring's lock for a moment in `Consumer::producer`, so
//! the pump asks only while a share runs (the app is then publishing, with the call's audio
//! keeping it awake); between shares it only reads frames, which takes no lock.
//!
//! The module builds on iOS and Linux (where Sew's tests run it off-device against a producer
//! written in the same process).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kessel_frame_ring::{Color, Consumer, EndReason as RingEnd, FrameInfo, Layout, ProducerState};
use sew_call_client::kessel_video::{
    self, Bytes, ColorSpace, EndReason, FrameInOptions, FrameInSender, PixelFormat, Planes,
    Rotation, VideoFrame,
};
use sew_call_client::{SourceFeed, VideoError};

use crate::engine::BoxFuture;

/// The ring's re-export, for the app's side (the extension links it as a static library).
pub use kessel_frame_ring;

/// The ring's file name in the App Group container. The extension attaches to
/// `<container>/sew-screen.ring` (`kfr_producer_attach`).
pub const RING_FILE: &str = "sew-screen.ring";

/// How often the driver reads the ring.
pub const POLL: Duration = Duration::from_millis(10);

/// A share whose extension has not said it runs ends after this long without a frame.
pub const IDLE_END: Duration = Duration::from_secs(2);

/// While the extension runs but sends nothing (iOS delivers screen frames only when the screen
/// changes), the newest frame is sent again this often, so a keyframe request is answered.
pub const REPEAT: Duration = Duration::from_secs(1);

/// How often the pump checks, between shares, that its file is still the one at the path.
const CHECK: Duration = Duration::from_secs(1);

/// The largest age of a frame's timestamp that is believed (older: stamped now).
const MAX_AGE_US: u64 = 1_000_000;

/// A frame side below this is skipped (the ring's own box starts at 16).
const MIN_SIDE: u32 = 16;

/// Phones cap a screen share at 720p.
const PHONE_BOX: (u32, u32) = (1280, 720);

/// The ring's path in the App Group container.
pub fn ring_path(container: &Path) -> PathBuf {
    container.join(RING_FILE)
}

/// The ring's layout for a call's policy: the screen hint's box (either way round), at most
/// 1280 × 720, at the hint's rate (1 to 30 fps; 15 without a policy).
pub fn layout_for(policy: Option<&sew_proto::voice::MediaPolicy>) -> Layout {
    let (mut long, mut short, mut fps) = (PHONE_BOX.0, PHONE_BOX.1, 15);
    if let Some(p) = policy {
        let (w, h) = (p.screen.max_width, p.screen.max_height);
        if w > 0 && h > 0 {
            long = long.min(w.max(h));
            short = short.min(w.min(h));
        }
        if p.screen.max_fps > 0 {
            fps = p.screen.max_fps.min(30);
        }
    }
    let even = |v: u32| (v.clamp(MIN_SIDE, kessel_frame_ring::MAX_SIDE)) & !1;
    let mut l = Layout::new(even(long), even(short));
    l.max_fps = fps;
    l
}

/// Why a share ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ShareEnd {
    /// The person stopped the broadcast (`kfr_producer_finish(p, 1)`).
    UserStopped,
    /// The extension closed the ring (`kfr_producer_finish(p, 2)`, or a producer dropped).
    Closed,
    /// The extension failed (`kfr_producer_finish(p, 3)`).
    Failed,
    /// The extension died without a word (its lock is gone).
    ExtensionGone,
    /// No frame for [`IDLE_END`] from an extension that never said it runs.
    Idle,
    /// The ring's file shrank, was replaced or the OS failed.
    RingLost,
    /// The app stopped it (`screen {on: false}`, or the publication ended).
    AppStopped,
}

impl ShareEnd {
    /// The frame-in source's end reason.
    fn source_end(self) -> EndReason {
        match self {
            ShareEnd::UserStopped => EndReason::UserStopped,
            ShareEnd::AppStopped => EndReason::Closed,
            _ => EndReason::SourceClosed,
        }
    }

    /// The ring's end said by the extension, which needs no stop request.
    fn by_extension(self) -> bool {
        matches!(
            self,
            ShareEnd::UserStopped | ShareEnd::Closed | ShareEnd::Failed
        )
    }
}

/// What one [`RingPump::step`] did.
#[derive(Debug)]
#[non_exhaustive]
pub enum PumpStep {
    /// Nothing new.
    Idle,
    /// A share's first frame: publish this source as the call's `screen` track (or call
    /// [`RingPump::refused`]).
    Start(SourceFeed),
    /// A frame went to the share's source.
    Frame,
    /// The share ended: unpublish the track. A fresh ring is armed for the next broadcast.
    Ended(ShareEnd),
    /// The ring is gone and could not be made again: stop driving (the reason).
    Lost(String),
}

/// The pump's counters (the ring's, and its own).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct PumpStats {
    /// The ring's counters (of the current ring file).
    pub ring: kessel_frame_ring::RingStats,
    /// Frames pushed to a share's source (repeats included).
    pub pushed: u64,
    /// Frames the pump itself skipped (too small, a size or length that doesn't add up, or one
    /// kessel-video refused).
    pub skipped: u64,
    /// Shares started.
    pub shares: u64,
    /// Rings made (the first, and one after each end or loss).
    pub rings: u64,
}

struct Share {
    sender: FrameInSender,
    /// The last new frame (not a repeat).
    last_frame: Instant,
    /// The last push (a repeat too).
    last_push: Instant,
    /// The extension said it runs (at the last check).
    running: bool,
}

/// The app's side of one ring: see the [module docs](self). `Send`, not `Sync`.
pub struct RingPump {
    consumer: Consumer,
    path: PathBuf,
    layout: Layout,
    /// The ring file's device and inode, to notice it being replaced or removed.
    id: Option<(u64, u64)>,
    buf: Vec<u8>,
    share: Option<Share>,
    last_capture_us: u64,
    next_check: Option<Instant>,
    lost: Option<String>,
    pushed: u64,
    skipped: u64,
    shares: u64,
    rings: u64,
}

impl std::fmt::Debug for RingPump {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RingPump")
            .field("path", &self.path)
            .field("layout", &self.layout)
            .field("sharing", &self.share.is_some())
            .finish_non_exhaustive()
    }
}

fn file_id(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt as _;
    let m = std::fs::symlink_metadata(path).ok()?;
    m.file_type().is_file().then(|| (m.dev(), m.ino()))
}

impl RingPump {
    /// Makes the ring at `path` (replacing any file there) with `layout`.
    pub fn create(
        path: impl Into<PathBuf>,
        layout: Layout,
    ) -> Result<RingPump, kessel_frame_ring::RingError> {
        let path = path.into();
        let consumer = Consumer::create(&path, layout)?;
        Ok(RingPump {
            id: file_id(&path),
            consumer,
            path,
            layout,
            buf: Vec::new(),
            share: None,
            last_capture_us: 0,
            next_check: None,
            lost: None,
            pushed: 0,
            skipped: 0,
            shares: 0,
            rings: 1,
        })
    }

    /// The ring's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The ring's layout.
    pub fn layout(&self) -> Layout {
        self.layout
    }

    /// A share is running.
    pub fn is_sharing(&self) -> bool {
        self.share.is_some()
    }

    /// The counters.
    pub fn stats(&self) -> PumpStats {
        PumpStats {
            ring: self.consumer.stats(),
            pushed: self.pushed,
            skipped: self.skipped,
            shares: self.shares,
            rings: self.rings,
        }
    }

    /// One look at the ring, at `now`. Never blocks.
    pub fn step(&mut self, now: Instant) -> PumpStep {
        if let Some(why) = &self.lost {
            return PumpStep::Lost(why.clone());
        }
        if self.share.is_some() {
            if let Some(end) = self.check_share() {
                return self.end(end);
            }
        } else if self.next_check.is_none_or(|t| now >= t) {
            self.next_check = Some(now + CHECK);
            if !self.healthy() {
                tracing::info!(target: "sew_voice", path = %self.path.display(), "the screen ring's file went away or shrank; making it again");
                if let Some(lost) = self.remake() {
                    return lost;
                }
            }
        }
        match self.consumer.try_read(&mut self.buf) {
            Some(info) => self.frame(info, now, false),
            None => self.quiet(now),
        }
    }

    /// The file at the path is still this ring's, whole.
    fn healthy(&self) -> bool {
        use std::os::unix::fs::MetadataExt as _;
        let Some(id) = self.id else { return false };
        std::fs::symlink_metadata(&self.path).is_ok_and(|m| {
            m.file_type().is_file()
                && (m.dev(), m.ino()) == id
                && m.len() >= self.layout.file_len() as u64
        })
    }

    /// The share's publication was refused: its source is gone; the extension is asked to stop
    /// and a fresh ring is armed.
    pub fn refused(&mut self) {
        if self.share.take().is_some() {
            self.consumer.request_stop();
            let _ = self.remake();
        }
    }

    /// The extension's state while a share runs; `Some` ends it.
    fn check_share(&mut self) -> Option<ShareEnd> {
        let share = self.share.as_mut()?;
        if !share.sender.is_open() {
            return Some(ShareEnd::AppStopped);
        }
        match self.consumer.producer() {
            Err(_) => Some(ShareEnd::RingLost),
            Ok(ProducerState::Ended(r)) => Some(match r {
                RingEnd::UserStopped => ShareEnd::UserStopped,
                RingEnd::Failed => ShareEnd::Failed,
                _ => ShareEnd::Closed,
            }),
            Ok(ProducerState::Gone) => Some(ShareEnd::ExtensionGone),
            Ok(ProducerState::Running) => {
                share.running = true;
                None
            }
            Ok(_) => {
                share.running = false;
                None
            }
        }
    }

    /// No new frame.
    fn quiet(&mut self, now: Instant) -> PumpStep {
        let Some(share) = self.share.as_ref() else {
            return PumpStep::Idle;
        };
        if !share.running {
            if now.saturating_duration_since(share.last_frame) >= IDLE_END {
                return self.end(ShareEnd::Idle);
            }
            return PumpStep::Idle;
        }
        if now.saturating_duration_since(share.last_push) >= REPEAT {
            if let Some(info) = self.consumer.read_latest(&mut self.buf) {
                return self.frame(info, now, true);
            }
            // Nothing valid to repeat: try again after the next interval.
            if let Some(s) = self.share.as_mut() {
                s.last_push = now;
            }
        }
        PumpStep::Idle
    }

    /// A frame read out of the ring (`repeat`: the newest one again).
    fn frame(&mut self, info: FrameInfo, now: Instant, repeat: bool) -> PumpStep {
        let Some(frame) = self.video_frame(&info, repeat) else {
            self.skipped = self.skipped.saturating_add(1);
            return PumpStep::Idle;
        };
        let Ok(rotation) = Rotation::from_degrees(info.rotation) else {
            self.skipped = self.skipped.saturating_add(1);
            return PumpStep::Idle;
        };
        if let Some(share) = self.share.as_mut() {
            return match share.sender.push(frame, rotation) {
                Ok(()) => {
                    share.last_push = now;
                    if !repeat {
                        share.last_frame = now;
                    }
                    self.pushed = self.pushed.saturating_add(1);
                    PumpStep::Frame
                }
                Err(kessel_video::VideoError::ShutDown) => self.end(ShareEnd::AppStopped),
                Err(e) => {
                    tracing::debug!(target: "sew_voice", error = %e, "a screen ring frame was refused");
                    self.skipped = self.skipped.saturating_add(1);
                    PumpStep::Idle
                }
            };
        }
        if repeat {
            return PumpStep::Idle;
        }
        // The first frame of a share: a frame-in source the size of the ring's box.
        let (w, h) = if matches!(info.rotation, 90 | 270) {
            (info.height, info.width)
        } else {
            (info.width, info.height)
        };
        // The ring's rate (0: no limit, then frame-in's highest).
        let fps = match self.layout.max_fps {
            0 => 240,
            f => f.min(240),
        };
        let opts = FrameInOptions::new(self.layout.max_width, self.layout.max_height, fps);
        let (sender, source) = match kessel_video::frame_in(opts, kessel_video::no_wake()) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(target: "sew_voice", error = %e, "no frame-in source for the screen ring");
                self.skipped = self.skipped.saturating_add(1);
                return PumpStep::Idle;
            }
        };
        if let Err(e) = sender.push(frame, rotation) {
            tracing::debug!(target: "sew_voice", error = %e, "the screen ring's first frame was refused");
            self.skipped = self.skipped.saturating_add(1);
            return PumpStep::Idle;
        }
        self.pushed = self.pushed.saturating_add(1);
        self.shares = self.shares.saturating_add(1);
        self.share = Some(Share {
            sender,
            last_frame: now,
            last_push: now,
            running: false,
        });
        tracing::info!(target: "sew_voice", width = w, height = h, "the screen ring's share starts");
        PumpStep::Start(SourceFeed::new(Box::new(source), w & !1, h & !1))
    }

    /// The bytes read out as a kessel-video frame, checked once more: `None` skips it.
    fn video_frame(&mut self, info: &FrameInfo, repeat: bool) -> Option<VideoFrame> {
        let side = MIN_SIDE..=kessel_frame_ring::MAX_SIDE;
        let fits = side.contains(&info.width)
            && side.contains(&info.height)
            && matches!(info.rotation, 0 | 90 | 180 | 270)
            && info.len() == self.buf.len()
            && kessel_frame_ring::nv12_len(info.width, info.height) == self.buf.len()
            && self.buf.len() <= self.layout.slot_bytes();
        if !fits {
            return None;
        }
        let capture_us = self.capture_us(info.timestamp_ns, repeat);
        let data = Bytes::from(std::mem::take(&mut self.buf));
        let planes = Planes::tight(PixelFormat::Nv12, info.width, info.height, data).ok()?;
        let mut frame =
            VideoFrame::from_planes(planes, info.width, info.height, capture_us).ok()?;
        frame.color = match info.color {
            Color::Bt709Limited => ColorSpace::Bt709Limited,
            Color::Bt709Full => ColorSpace::Bt709Full,
            _ => ColorSpace::Bt601Limited,
        };
        Some(frame)
    }

    /// The extension's timestamp (ns on the ring's clock) on kessel-video's clock, never
    /// backwards and never more than [`MAX_AGE_US`] old; a repeat is stamped now.
    fn capture_us(&mut self, timestamp_ns: u64, repeat: bool) -> u64 {
        let now_us = kessel_video::now_us();
        let age_us = if repeat || timestamp_ns == 0 {
            0
        } else {
            (kessel_frame_ring::monotonic_ns().saturating_sub(timestamp_ns) / 1_000).min(MAX_AGE_US)
        };
        let t = now_us
            .saturating_sub(age_us)
            .max(self.last_capture_us.saturating_add(1));
        self.last_capture_us = t;
        t
    }

    /// Ends the share and arms a fresh ring.
    fn end(&mut self, why: ShareEnd) -> PumpStep {
        if let Some(share) = self.share.take() {
            share.sender.end(why.source_end());
        }
        if !why.by_extension() {
            self.consumer.request_stop();
        }
        tracing::info!(target: "sew_voice", ?why, "the screen ring's share ends");
        if let Some(lost) = self.remake() {
            return lost;
        }
        PumpStep::Ended(why)
    }

    /// A fresh ring at the path (the old one is dropped after the new one is in place, so its
    /// removal never takes the new file). `Some` when it can't be made.
    fn remake(&mut self) -> Option<PumpStep> {
        match Consumer::create(&self.path, self.layout) {
            Ok(c) => {
                self.consumer.request_stop();
                self.consumer = c;
                self.id = file_id(&self.path);
                self.rings = self.rings.saturating_add(1);
                self.next_check = None;
                None
            }
            Err(e) => {
                let why = format!("the screen ring can't be made again: {e}");
                tracing::warn!(target: "sew_voice", path = %self.path.display(), error = %e, "the screen ring is lost");
                self.lost = Some(why.clone());
                Some(PumpStep::Lost(why))
            }
        }
    }
}

impl Drop for RingPump {
    fn drop(&mut self) {
        if let Some(share) = self.share.take() {
            share.sender.end(EndReason::Closed);
        }
        // The extension finishes its broadcast; the consumer's own drop marks it gone and
        // removes the file.
        self.consumer.request_stop();
    }
}

/// Where a ring's share goes: the call's `screen` publication.
pub trait ScreenShareSink: Send + Sync + 'static {
    /// Publish `feed` as the call's screen (`VideoBridge::publish_source`).
    fn publish(&self, feed: SourceFeed) -> BoxFuture<'_, Result<(), VideoError>>;
    /// Unpublish the screen (`VideoBridge::stop_source`); safe to repeat.
    fn unpublish(&self) -> BoxFuture<'_, ()>;
    /// A share's publication was refused (the policy, the lease, the codec).
    fn refused(&self, error: &VideoError) {
        let _ = error;
    }
    /// `false` once the call is gone: [`drive`] then stops (and the ring with it).
    fn alive(&self) -> bool {
        true
    }
}

/// Runs `pump` every [`POLL`] into `sink` until the ring is lost or the sink's call is gone
/// (or the task is dropped); the pump's drop then asks the extension to finish and removes the
/// ring.
pub async fn drive(mut pump: RingPump, sink: Arc<dyn ScreenShareSink>) {
    let mut tick = tokio::time::interval(POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if !sink.alive() {
            return;
        }
        match pump.step(Instant::now()) {
            PumpStep::Start(feed) => {
                if let Err(e) = sink.publish(feed).await {
                    tracing::info!(target: "sew_voice", error = %e, "the screen ring's share was refused");
                    pump.refused();
                    sink.refused(&e);
                }
            }
            PumpStep::Ended(_) => sink.unpublish().await,
            PumpStep::Lost(_) => {
                sink.unpublish().await;
                return;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr
)]
mod tests;
