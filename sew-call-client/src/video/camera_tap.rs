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

/// What a [`CameraTap`] gives.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum CameraTapItem {
    /// The feed changed (also the first item of a new tap: the feed then).
    Feed(CameraFeed),
    /// The newest frame.
    Frame(CameraFrame),
}

struct Slot {
    feed: CameraFeed,
    /// The feed changed since the reader last took it.
    feed_new: bool,
    frame: Option<CameraFrame>,
    closed: bool,
    taken: u64,
    dropped: u64,
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
}

/// Every open tap of one [`VideoCapture`](super::VideoCapture) (shared by its clones and the
/// bridge built on it), and the feed now.
#[derive(Clone, Default)]
pub(crate) struct CameraTaps(Arc<Mutex<Hub>>);

#[derive(Default)]
struct Hub {
    taps: Vec<CameraTapSender>,
    feed: CameraFeed,
}

impl CameraTaps {
    /// A new tap; its first item is the feed now.
    pub(crate) fn open(&self) -> CameraTap {
        let (tx, rx) = CameraTap::channel();
        let mut h = lock(&self.0);
        tx.set_feed(h.feed);
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
}
