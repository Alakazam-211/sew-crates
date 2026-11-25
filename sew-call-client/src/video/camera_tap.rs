//! Raw camera frames for a native self-view (feature `video-core`):
//! [`VideoCapture::camera_tap`](super::VideoCapture::camera_tap).
//!
//! A tap gets the camera's own frames as kessel-video captured them, before any encoder: no
//! encode, no decode and no copy (each frame is the capture's, shared). What a frame holds is
//! what the backend gives:
//! - **macOS and iOS**: the OS's buffer (`FrameBuffer::Native`, a `CVPixelBuffer`, NV12) while
//!   a call publishes the camera and during the pre-join preview;
//! - **Android**: CPU planes (I420), which the backend already turned upright;
//! - **Linux and Windows**: CPU planes as the camera delivers them.
//!
//! Frames come upright (the backend turns them; [`CameraFrame::rotation`] says what turn is
//! still left to the host, `0` with every kessel-video backend) and never mirrored: a host
//! mirrors a front camera's frames itself ([`CameraFrame::facing`], [`CameraFrame::mirror`]),
//! frame by frame, so a camera switch lands on the new camera's first frame.
//!
//! **No queue.** A tap holds at most one frame: a newer frame replaces one not yet taken (it
//! counts in [`CameraTap::dropped`]), so a slow reader always gets the newest frame, and the
//! camera's thread never waits for a reader. Keep a frame no longer than it is shown: on Apple
//! it is one of the camera's own pool of buffers.
//!
//! **The feed** ([`CameraFeed`]): `Live` while frames come, `Paused` while the camera is
//! paused (the app in the background, another app holding the camera, a hidden window), `Off`
//! while no camera is open. A change comes before any frame after it; a frame waiting when the
//! feed pauses or goes off is dropped.
//!
//! A tap never opens the camera: frames come while a camera is open through the same
//! [`VideoCapture`](super::VideoCapture) (the pre-join preview, or the
//! [`VideoBridge`](super::VideoBridge)'s camera publish). While a tap is open, a published
//! camera keeps capturing even when no subscriber wants a layer (as with the self-view), and
//! nothing more is encoded for it.
//!
//! **The zero-hop preview** (macOS and iOS, feature `video`): each camera source opened through
//! the [`VideoCapture`](super::VideoCapture) (the pre-join preview, the bridge's camera publish,
//! a camera switch, a reopen after the background) is a new *generation*. When the source has
//! kessel-video's `ApplePreview` (a handle on the camera's own `AVCaptureSession`), every open
//! tap gets `CameraTapItem::PreviewReady` with it, for an `AVCaptureVideoPreviewLayer` the
//! system draws straight from the camera, and `CameraTapItem::PreviewGone` when that source
//! goes: always before the next generation's `PreviewReady`, and on the final close. A source
//! without a preview still takes a generation, and sends neither. A tap opened while a source
//! with a preview is live gets that source's `PreviewReady` at once. Preview items are never
//! dropped or coalesced: unlike frames they queue, in order, until taken (a feed change still
//! comes first). The frames keep coming as before, as the fallback and for picture in picture.
//! On other targets there are no preview items.

#[cfg(any(sew_preview_events, test))]
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use kessel_video::{Facing, VideoFrame};
use serde::Serialize;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One raw camera frame for a native self-view.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct CameraFrame {
    /// The frame as the backend captured it (shared, not copied): on macOS and iOS an OS
    /// buffer (`FrameBuffer::Native`, a `CVPixelBuffer`), elsewhere CPU planes. Its `width` and
    /// `height` are as shown (portrait or landscape), its `capture_us` the backend's clock.
    pub frame: VideoFrame,
    /// Which way the camera that captured it faces (`Unknown` when the OS doesn't say).
    pub facing: Facing,
    /// Degrees clockwise (0, 90, 180 or 270) the host still has to turn the frame to show it
    /// upright: `0` with every kessel-video backend, which turns frames upright itself.
    pub rotation: u32,
}

impl CameraFrame {
    /// `frame` from a camera facing `facing`, upright.
    pub fn new(frame: VideoFrame, facing: Facing) -> CameraFrame {
        CameraFrame {
            frame,
            facing,
            rotation: 0,
        }
    }

    /// Whether a self-view shows this frame mirrored: a front camera's (as a mirror does). The
    /// pixels are never mirrored; the host mirrors its view.
    pub fn mirror(&self) -> bool {
        self.facing == Facing::Front
    }
}

/// Whether a tap's camera gives frames ([`CameraTap`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CameraFeed {
    /// No camera is open (or it closed).
    #[default]
    Off,
    /// Frames come.
    Live,
    /// The camera is open but paused (the app in the background, another app holding the
    /// camera, a hidden window): no frames until it runs again.
    Paused,
}

/// The handle a `CameraTapItem::PreviewReady` carries: on macOS and iOS (feature `video`)
/// kessel-video's `ApplePreview`, a handle on the camera source's own `AVCaptureSession`.
/// Holding, cloning, sending and dropping it needs no `unsafe`; only its two pointer methods
/// (`capture_session`, `new_preview_layer`) are `unsafe`, and the host calls them.
#[cfg(sew_apple_preview)]
pub type PreviewHandle = kessel_video::ApplePreview;

/// TEST ONLY (feature `testing`, off Apple): what a `CameraTapItem::PreviewReady` carries where
/// there is no `ApplePreview`, so the preview items can be tested on any target.
#[cfg(all(not(sew_apple_preview), any(test, feature = "testing")))]
pub type PreviewHandle = FakePreview;

/// TEST ONLY (feature `testing`, off Apple): a stand-in for kessel-video's `ApplePreview`.
#[cfg(all(not(sew_apple_preview), any(test, feature = "testing")))]
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FakePreview(pub u64);

/// What a [`CameraTap`] gives.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum CameraTapItem {
    /// The feed changed (also the first item of a new tap: the feed then).
    Feed(CameraFeed),
    /// The newest frame.
    Frame(CameraFrame),
    /// macOS and iOS (feature `video`): a camera source with a zero-hop preview is live (see the
    /// module docs). The host makes its self-view layer from `preview` with
    /// `new_preview_layer`, **on the main thread** (the layer is the host's to release), and
    /// mirrors it for [`Facing::Front`] (`new_preview_layer(None)` does). It takes the place of
    /// any earlier generation's layer. The frames keep coming, as the fallback and for picture
    /// in picture. Never dropped or coalesced.
    #[cfg(any(sew_preview_events, test))]
    PreviewReady {
        /// The handle on the source's capture session.
        preview: PreviewHandle,
        /// Which way the source's camera faces.
        facing: Facing,
        /// The source's generation: one more for each camera source opened.
        generation: u64,
    },
    /// macOS and iOS (feature `video`): the camera source of `generation` went (closed,
    /// switched, reopened, or the camera turned off). Remove or hide that generation's layer
    /// now: whether a dropped source's layer goes black or keeps its last frame is not
    /// promised. Always before the next generation's `PreviewReady`; never dropped or
    /// coalesced.
    #[cfg(any(sew_preview_events, test))]
    PreviewGone {
        /// The generation that went.
        generation: u64,
    },
}

struct Slot {
    feed: CameraFeed,
    /// The feed changed since the reader last took it.
    feed_new: bool,
    frame: Option<CameraFrame>,
    closed: bool,
    taken: u64,
    dropped: u64,
    /// The preview items not yet taken, in order (never dropped).
    #[cfg(any(sew_preview_events, test))]
    previews: VecDeque<CameraTapItem>,
}

struct Shared {
    slot: Mutex<Slot>,
    ready: Condvar,
}

/// The reading end of a camera tap ([`VideoCapture::camera_tap`](super::VideoCapture::camera_tap)
/// or [`CameraTap::channel`]): the feed, then the newest frame each time one is taken. Dropping
/// it (or [`CameraTap::close`]) closes it; the camera side then forgets it.
pub struct CameraTap {
    shared: Arc<Shared>,
}

/// The feeding end of a [`CameraTap`]. Never blocks: a frame replaces one not yet taken.
#[derive(Clone)]
pub struct CameraTapSender {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for CameraTap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = lock(&self.shared.slot);
        f.debug_struct("CameraTap")
            .field("feed", &s.feed)
            .field("closed", &s.closed)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for CameraTapSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CameraTapSender").finish_non_exhaustive()
    }
}

impl CameraTap {
    /// A tap fed by the caller (a host's own camera, tests): its feed starts `Off`.
    pub fn channel() -> (CameraTapSender, CameraTap) {
        let shared = Arc::new(Shared {
            slot: Mutex::new(Slot {
                feed: CameraFeed::Off,
                feed_new: true,
                frame: None,
                closed: false,
                taken: 0,
                dropped: 0,
                #[cfg(any(sew_preview_events, test))]
                previews: VecDeque::new(),
            }),
            ready: Condvar::new(),
        });
        (
            CameraTapSender {
                shared: Arc::clone(&shared),
            },
            CameraTap { shared },
        )
    }

    fn take(s: &mut Slot) -> Option<CameraTapItem> {
        if s.closed {
            return None;
        }
        if std::mem::take(&mut s.feed_new) {
            return Some(CameraTapItem::Feed(s.feed));
        }
        #[cfg(any(sew_preview_events, test))]
        if let Some(p) = s.previews.pop_front() {
            return Some(p);
        }
        let f = s.frame.take()?;
        s.taken += 1;
        Some(CameraTapItem::Frame(f))
    }

    /// The feed change or the frame waiting now, if any (`None` once closed).
    pub fn try_recv(&self) -> Option<CameraTapItem> {
        Self::take(&mut lock(&self.shared.slot))
    }

    /// The next feed change or frame, waiting at most `wait` (`None`: nothing came, or the
    /// tap is closed).
    pub fn recv_timeout(&self, wait: Duration) -> Option<CameraTapItem> {
        let until = Instant::now() + wait;
        let mut s = lock(&self.shared.slot);
        loop {
            if let Some(item) = Self::take(&mut s) {
                return Some(item);
            }
            if s.closed {
                return None;
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            s = self
                .shared
                .ready
                .wait_timeout(s, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// The feed now.
    pub fn feed(&self) -> CameraFeed {
        lock(&self.shared.slot).feed
    }

    /// Frames taken from the tap so far.
    pub fn taken(&self) -> u64 {
        lock(&self.shared.slot).taken
    }

    /// Frames a newer frame replaced before they were taken (the reader was slower than the
    /// camera).
    pub fn dropped(&self) -> u64 {
        lock(&self.shared.slot).dropped
    }

    /// Closes the tap: a waiting [`CameraTap::recv_timeout`] returns `None` at once, and no
    /// frame comes any more.
    pub fn close(&self) {
        let stale = {
            let mut s = lock(&self.shared.slot);
            s.closed = true;
            s.frame.take()
        };
        drop(stale);
        #[cfg(any(sew_preview_events, test))]
        {
            // The handles (retains on capture sessions) go too, outside the lock.
            let previews = std::mem::take(&mut lock(&self.shared.slot).previews);
            drop(previews);
        }
        self.shared.ready.notify_all();
    }

    /// The tap is closed.
    pub fn is_closed(&self) -> bool {
        lock(&self.shared.slot).closed
    }
}

impl Drop for CameraTap {
    fn drop(&mut self) {
        self.close();
    }
}

impl CameraTapSender {
    /// Hands `frame` over (the feed turns `Live`), replacing a frame not yet taken. Never
    /// blocks; `false` once the tap is closed.
    pub fn send(&self, frame: CameraFrame) -> bool {
        let stale = {
            let mut s = lock(&self.shared.slot);
            if s.closed {
                return false;
            }
            if s.feed != CameraFeed::Live {
                s.feed = CameraFeed::Live;
                s.feed_new = true;
            }
            let stale = s.frame.replace(frame);
            if stale.is_some() {
                s.dropped += 1;
            }
            stale
        };
        // The replaced frame (an OS buffer on Apple) goes back outside the lock.
        drop(stale);
        self.shared.ready.notify_one();
        true
    }

    /// The feed is now `feed` (a frame waiting is dropped unless it is `Live`). Never blocks;
    /// `false` once the tap is closed.
    pub fn set_feed(&self, feed: CameraFeed) -> bool {
        let stale = {
            let mut s = lock(&self.shared.slot);
            if s.closed {
                return false;
            }
            if s.feed == feed {
                return true;
            }
            s.feed = feed;
            s.feed_new = true;
            if feed == CameraFeed::Live {
                None
            } else {
                s.frame.take()
            }
        };
        drop(stale);
        self.shared.ready.notify_one();
        true
    }

    /// The tap's reader closed it.
    pub fn is_closed(&self) -> bool {
        lock(&self.shared.slot).closed
    }

    /// macOS and iOS (feature `video`): queues `CameraTapItem::PreviewReady`, after the preview
    /// items before it; never dropped or coalesced. Never blocks; `false` once the tap is
    /// closed.
    #[cfg(any(sew_preview_events, test))]
    pub fn preview_ready(&self, preview: PreviewHandle, facing: Facing, generation: u64) -> bool {
        self.preview(CameraTapItem::PreviewReady {
            preview,
            facing,
            generation,
        })
    }

    /// macOS and iOS (feature `video`): queues `CameraTapItem::PreviewGone`, after the preview
    /// items before it; never dropped or coalesced. Never blocks; `false` once the tap is
    /// closed.
    #[cfg(any(sew_preview_events, test))]
    pub fn preview_gone(&self, generation: u64) -> bool {
        self.preview(CameraTapItem::PreviewGone { generation })
    }

    #[cfg(any(sew_preview_events, test))]
    fn preview(&self, item: CameraTapItem) -> bool {
        {
            let mut s = lock(&self.shared.slot);
            if s.closed {
                return false;
            }
            s.previews.push_back(item);
        }
        self.shared.ready.notify_one();
        true
    }
}

/// Every open tap of one [`VideoCapture`](super::VideoCapture) (shared by its clones and the
/// bridge built on it), and the feed now.
#[derive(Clone, Default)]
pub(crate) struct CameraTaps(Arc<Mutex<Hub>>);

#[derive(Default)]
struct Hub {
    taps: Vec<CameraTapSender>,
    feed: CameraFeed,
    /// The last camera source's generation (one more for each source opened).
    #[cfg(any(sew_preview_events, test))]
    generation: u64,
    /// The live source's preview: its `PreviewReady` went out, its `PreviewGone` not yet.
    #[cfg(any(sew_preview_events, test))]
    live: Option<(PreviewHandle, Facing, u64)>,
}

#[cfg(any(sew_preview_events, test))]
impl Hub {
    /// `PreviewGone` for the live preview, if any, to every open tap. Answers its handle, to
    /// drop outside the lock.
    fn gone(&mut self) -> Option<PreviewHandle> {
        let (preview, _, generation) = self.live.take()?;
        self.taps.retain(|t| t.preview_gone(generation));
        Some(preview)
    }
}

/// A camera source's generation ([`CameraTaps::preview_open`]), held beside the source:
/// dropped with it, it sends that generation's `PreviewGone` (when its `PreviewReady` went out
/// and a later source hasn't already sent it).
#[cfg(any(sew_preview_events, test))]
pub(crate) struct PreviewLease {
    taps: CameraTaps,
    generation: u64,
}

#[cfg(any(sew_preview_events, test))]
impl PreviewLease {
    /// Its generation.
    #[cfg(all(test, not(sew_apple_preview)))]
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
}

#[cfg(any(sew_preview_events, test))]
impl Drop for PreviewLease {
    fn drop(&mut self) {
        // The handle (a retain on the capture session on Apple) goes at the end, outside the
        // lock.
        let _stale = {
            let mut h = lock(&self.taps.0);
            if h.live.as_ref().is_some_and(|l| l.2 == self.generation) {
                h.gone()
            } else {
                None
            }
        };
    }
}

impl CameraTaps {
    /// A new tap; its first item is the feed now, then the live source's `PreviewReady` (if a
    /// source with a preview is live).
    pub(crate) fn open(&self) -> CameraTap {
        let (tx, rx) = CameraTap::channel();
        let mut h = lock(&self.0);
        tx.set_feed(h.feed);
        #[cfg(any(sew_preview_events, test))]
        if let Some((preview, facing, generation)) = &h.live {
            tx.preview_ready(preview.clone(), *facing, *generation);
        }
        h.taps.retain(|t| !t.is_closed());
        h.taps.push(tx);
        rx
    }

    /// A tap is open.
    pub(crate) fn any(&self) -> bool {
        let mut h = lock(&self.0);
        h.taps.retain(|t| !t.is_closed());
        !h.taps.is_empty()
    }

    /// A captured frame to every open tap (the feed turns `Live`). Never blocks on a reader.
    pub(crate) fn frame(&self, frame: &VideoFrame, facing: Facing) {
        let mut h = lock(&self.0);
        h.feed = CameraFeed::Live;
        if h.taps.is_empty() {
            return;
        }
        h.taps
            .retain(|t| t.send(CameraFrame::new(frame.clone(), facing)));
    }

    /// The feed is now `feed`, for every open tap.
    pub(crate) fn feed(&self, feed: CameraFeed) {
        let mut h = lock(&self.0);
        h.feed = feed;
        h.taps.retain(|t| t.set_feed(feed));
    }

    /// A camera source opened: the next generation. A live preview of an earlier source gets
    /// its `PreviewGone` first (should that source not have gone yet); then, when the new
    /// source has a preview, `PreviewReady` to every open tap. Without one the generation
    /// still moves on and nothing is sent. Dropping the lease (with the source) sends its
    /// `PreviewGone`.
    #[cfg(any(sew_preview_events, test))]
    pub(crate) fn preview_open(
        &self,
        preview: Option<PreviewHandle>,
        facing: Facing,
    ) -> PreviewLease {
        // The earlier source's handle goes at the end, outside the lock.
        let (_stale, generation) = {
            let mut h = lock(&self.0);
            h.generation += 1;
            let generation = h.generation;
            let stale = h.gone();
            if let Some(preview) = preview {
                h.taps
                    .retain(|t| t.preview_ready(preview.clone(), facing, generation));
                h.live = Some((preview, facing, generation));
            }
            (stale, generation)
        };
        PreviewLease {
            taps: self.clone(),
            generation,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;

    fn frame(n: u32) -> VideoFrame {
        kessel_video::fake::pattern_frame(16, 16, n, u64::from(n)).expect("a frame")
    }

    fn capture_us(item: Option<CameraTapItem>) -> u64 {
        match item {
            Some(CameraTapItem::Frame(f)) => f.frame.capture_us,
            other => panic!("a frame, got {other:?}"),
        }
    }

    #[test]
    fn a_slow_reader_gets_the_newest_frame_and_the_rest_are_counted_dropped() {
        let taps = CameraTaps::default();
        let tap = taps.open();
        assert!(matches!(
            tap.try_recv(),
            Some(CameraTapItem::Feed(CameraFeed::Off))
        ));
        assert!(tap.try_recv().is_none());
        for n in 1..=5 {
            taps.frame(&frame(n), Facing::Front);
        }
        assert!(matches!(
            tap.try_recv(),
            Some(CameraTapItem::Feed(CameraFeed::Live))
        ));
        assert_eq!(capture_us(tap.try_recv()), 5, "the newest frame");
        assert!(tap.try_recv().is_none(), "never a queue");
        assert_eq!(tap.dropped(), 4);
        assert_eq!(tap.taken(), 1);
    }

    #[test]
    fn a_pause_drops_the_waiting_frame_and_comes_first() {
        let taps = CameraTaps::default();
        taps.frame(&frame(1), Facing::Back);
        let tap = taps.open();
        assert!(matches!(
            tap.try_recv(),
            Some(CameraTapItem::Feed(CameraFeed::Live))
        ));
        taps.frame(&frame(2), Facing::Back);
        taps.feed(CameraFeed::Paused);
        assert!(matches!(
            tap.try_recv(),
            Some(CameraTapItem::Feed(CameraFeed::Paused))
        ));
        assert!(
            tap.try_recv().is_none(),
            "the frame before the pause is gone"
        );
        taps.frame(&frame(3), Facing::Back);
        assert!(matches!(
            tap.try_recv(),
            Some(CameraTapItem::Feed(CameraFeed::Live))
        ));
        match tap.try_recv() {
            Some(CameraTapItem::Frame(f)) => {
                assert_eq!(f.facing, Facing::Back);
                assert_eq!(f.rotation, 0);
                assert!(!f.mirror());
            }
            other => panic!("a frame, got {other:?}"),
        }
    }

    #[test]
    fn a_closed_tap_is_forgotten_and_wakes_its_reader() {
        let taps = CameraTaps::default();
        let tap = Arc::new(taps.open());
        assert!(taps.any());
        let _ = tap.try_recv();
        let reader = {
            let tap = Arc::clone(&tap);
            std::thread::spawn(move || tap.recv_timeout(Duration::from_secs(30)))
        };
        std::thread::sleep(Duration::from_millis(50));
        tap.close();
        assert!(
            reader.join().expect("the reader").is_none(),
            "closed: nothing"
        );
        assert!(!taps.any(), "forgotten");
        taps.frame(&frame(1), Facing::Front);
        assert!(tap.try_recv().is_none());
    }

    #[test]
    fn a_waiting_reader_wakes_on_a_frame() {
        let (tx, tap) = CameraTap::channel();
        assert!(matches!(
            tap.recv_timeout(Duration::from_millis(1)),
            Some(CameraTapItem::Feed(CameraFeed::Off))
        ));
        let sender = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            tx.send(CameraFrame::new(frame(7), Facing::Front))
        });
        assert!(matches!(
            tap.recv_timeout(Duration::from_secs(10)),
            Some(CameraTapItem::Feed(CameraFeed::Live))
        ));
        assert_eq!(capture_us(tap.recv_timeout(Duration::from_secs(10))), 7);
        assert!(sender.join().expect("the sender"));
        drop(tap);
    }

    // ── the preview items (off Apple: the stand-in handle) ──

    /// Every item queued now, frames as `Frame(capture_us)`.
    #[cfg(not(sew_apple_preview))]
    fn drain(tap: &CameraTap) -> Vec<Seen> {
        let mut out = Vec::new();
        while let Some(item) = tap.try_recv() {
            out.push(seen(item));
        }
        out
    }

    #[cfg(not(sew_apple_preview))]
    #[derive(Debug, PartialEq, Eq)]
    enum Seen {
        Feed(CameraFeed),
        Frame(u64),
        Ready(u64, Facing, u64),
        Gone(u64),
    }

    #[cfg(not(sew_apple_preview))]
    fn seen(item: CameraTapItem) -> Seen {
        match item {
            CameraTapItem::Feed(f) => Seen::Feed(f),
            CameraTapItem::Frame(f) => Seen::Frame(f.frame.capture_us),
            CameraTapItem::PreviewReady {
                preview,
                facing,
                generation,
            } => Seen::Ready(preview.0, facing, generation),
            CameraTapItem::PreviewGone { generation } => Seen::Gone(generation),
        }
    }

    #[cfg(not(sew_apple_preview))]
    #[test]
    fn each_source_is_a_generation_and_its_gone_comes_before_the_next_ready() {
        let taps = CameraTaps::default();
        let tap = taps.open();
        assert_eq!(drain(&tap), vec![Seen::Feed(CameraFeed::Off)]);

        let first = taps.preview_open(Some(FakePreview(10)), Facing::Front);
        assert_eq!(first.generation(), 1);
        assert_eq!(drain(&tap), vec![Seen::Ready(10, Facing::Front, 1)]);
        // A flip: the old source goes, then the new one opens.
        drop(first);
        assert_eq!(drain(&tap), vec![Seen::Gone(1)]);
        let second = taps.preview_open(Some(FakePreview(11)), Facing::Back);
        assert_eq!(drain(&tap), vec![Seen::Ready(11, Facing::Back, 2)]);

        // A new source while the old one hasn't gone yet (a pipeline still stopping): the
        // old one's Gone first, and nothing more when it goes later.
        let third = taps.preview_open(Some(FakePreview(12)), Facing::Front);
        assert_eq!(
            drain(&tap),
            vec![Seen::Gone(2), Seen::Ready(12, Facing::Front, 3)]
        );
        drop(second);
        assert_eq!(drain(&tap), vec![], "its Gone went out already");

        // A source without a preview: its own generation, no item; the one before still goes
        // first.
        let bare = taps.preview_open(None, Facing::Unknown);
        assert_eq!(bare.generation(), 4);
        assert_eq!(drain(&tap), vec![Seen::Gone(3)]);
        drop(third);
        drop(bare);
        assert_eq!(
            drain(&tap),
            vec![],
            "nothing for a source without a preview"
        );

        // The final close: Gone.
        let last = taps.preview_open(Some(FakePreview(13)), Facing::Front);
        assert_eq!(drain(&tap), vec![Seen::Ready(13, Facing::Front, 5)]);
        drop(last);
        assert_eq!(drain(&tap), vec![Seen::Gone(5)]);
    }

    #[cfg(not(sew_apple_preview))]
    #[test]
    fn a_late_tap_gets_the_live_preview_at_once_after_the_feed() {
        let taps = CameraTaps::default();
        let lease = taps.preview_open(Some(FakePreview(7)), Facing::Front);
        taps.frame(&frame(1), Facing::Front);
        let late = taps.open();
        assert_eq!(
            drain(&late),
            vec![
                Seen::Feed(CameraFeed::Live),
                Seen::Ready(7, Facing::Front, 1)
            ]
        );
        drop(lease);
        assert_eq!(drain(&late), vec![Seen::Gone(1)]);
        let after = taps.open();
        assert_eq!(
            drain(&after),
            vec![Seen::Feed(CameraFeed::Live)],
            "no source with a preview is live: no Ready"
        );
    }

    #[cfg(not(sew_apple_preview))]
    #[test]
    fn a_slow_reader_gets_every_preview_item_in_order_while_frames_are_skipped() {
        let taps = CameraTaps::default();
        let tap = taps.open();
        assert_eq!(drain(&tap), vec![Seen::Feed(CameraFeed::Off)]);
        let mut n = 0;
        let mut lease = None;
        for g in 1..=50u64 {
            // A flip after every few frames, none of it read.
            drop(lease.take());
            lease = Some(taps.preview_open(Some(FakePreview(g)), Facing::Front));
            for _ in 0..4 {
                n += 1;
                taps.frame(&frame(n), Facing::Front);
            }
        }
        drop(lease);
        let got = drain(&tap);
        let previews: Vec<&Seen> = got
            .iter()
            .filter(|s| matches!(s, Seen::Ready(..) | Seen::Gone(_)))
            .collect();
        let mut want = Vec::new();
        for g in 1..=50u64 {
            want.push(Seen::Ready(g, Facing::Front, g));
            want.push(Seen::Gone(g));
        }
        assert_eq!(
            previews,
            want.iter().collect::<Vec<_>>(),
            "every one, in order"
        );
        let frames: Vec<&Seen> = got.iter().filter(|s| matches!(s, Seen::Frame(_))).collect();
        assert_eq!(
            frames,
            vec![&Seen::Frame(u64::from(n))],
            "the newest frame only"
        );
        assert_eq!(tap.dropped(), u64::from(n) - 1, "the other frames skipped");
        assert_eq!(
            got[0],
            Seen::Feed(CameraFeed::Live),
            "the feed change first"
        );
    }

    #[cfg(not(sew_apple_preview))]
    #[test]
    fn a_closed_tap_takes_no_preview_item_and_is_forgotten() {
        let taps = CameraTaps::default();
        let tap = taps.open();
        let lease = taps.preview_open(Some(FakePreview(1)), Facing::Front);
        tap.close();
        assert!(tap.try_recv().is_none());
        drop(lease);
        assert!(!taps.any(), "forgotten");
        let (tx, rx) = CameraTap::channel();
        drop(rx);
        assert!(!tx.preview_ready(FakePreview(2), Facing::Back, 9));
        assert!(!tx.preview_gone(9));
    }
}
