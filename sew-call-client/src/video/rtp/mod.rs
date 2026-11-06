//! RTP payload formats for video: VP8 (RFC 7741) and H.264 (RFC 6184, packetization mode 1)
//! packetisers and depacketisers, the H.264 normaliser, and the Dependency Descriptor writer.
//!
//! The order on the wire. Send: encode → normalise and check (H.264) → encrypt → packetise.
//! Receive: reorder → depacketise into one frame → decrypt and verify → decode. These modules
//! never see a key: they work on whatever bytes travel, which in an E2EE call is the encrypted
//! frame (its codec headers stay in the clear).

// The engine's video path takes these up; until then only the tests call them.
#![cfg_attr(not(all(test, feature = "testing")), allow(dead_code))]

pub(crate) mod dd;
pub(crate) mod h264;
pub(crate) mod normalise;
pub(crate) mod vp8;

// The round trips need Sew's tree (the fixture bitstreams in crates/sew-media-testclient) and
// dev-dependencies the published package drops (sha2, str0m's `unversioned`), so they run only
// with `testing`, as scripts/test-call-client.sh does; the package's own `cargo test` skips them.
// Two attributes, not cfg(all(..)): clippy treats only a plain `#[cfg(test)]` module as test code
// (allow-expect-in-tests).
#[cfg(test)]
#[cfg(feature = "testing")]
mod tests;

/// The largest RTP payload Sew sends; the RTP header, its extensions and SRTP's tag fit
/// beside it in a 1280-byte path.
pub(crate) const MTU: usize = 1200;

/// Why a frame or packet could not be built or read.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum RtpError {
    #[error("malformed: {0}")]
    Malformed(&'static str),
}

/// One outgoing packet's payload (the caller adds the RTP header: SSRC, sequence number,
/// payload type, the 90 kHz timestamp and the extensions).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PacketOut {
    pub(crate) payload: Vec<u8>,
    /// The last packet of the frame.
    pub(crate) marker: bool,
    /// The Dependency Descriptor element for this packet.
    pub(crate) dd: Vec<u8>,
}

/// One received packet, in sequence order.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RtpIn<'a> {
    pub(crate) seq: u16,
    pub(crate) timestamp: u32,
    pub(crate) marker: bool,
    pub(crate) payload: &'a [u8],
}

/// One reassembled frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Frame {
    pub(crate) timestamp: u32,
    /// VP8: the frame. H.264: the access unit with 4-byte start codes.
    pub(crate) data: Vec<u8>,
    /// VP8: the P bit; H.264: an IDR slice is present.
    pub(crate) keyframe: bool,
    /// VP8 only: the descriptor fields of the frame's first packet.
    pub(crate) vp8: Option<vp8::Vp8Info>,
}
