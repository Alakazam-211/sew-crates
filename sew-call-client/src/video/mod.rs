//! Video. The engine core never names kessel-video: it moves encoded frames, and [`rtp`] turns
//! them into RTP payloads and back. With `testing` and `video-core`, `FixtureBackend` is a
//! kessel-video backend that replays pre-encoded streams.

// Its users: the RTP round trips (test + testing) and FixtureBackend (testing + video-core).
#[cfg(all(feature = "testing", any(test, feature = "video-core")))]
mod container;
#[cfg(all(feature = "testing", feature = "video-core"))]
mod fixture;
pub(crate) mod rtp;

#[cfg(all(feature = "testing", feature = "video-core"))]
pub use fixture::{FixtureBackend, FixtureError};
