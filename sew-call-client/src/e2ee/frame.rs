//! The frame-crypto interface.
//!
//! The frame format is LiveKit's per-frame AES-GCM (`LkGcm`), which is LiveKit-derived and so
//! lives in `sew-lk-compat` (Apache-2.0). This crate never re-implements it: it holds the keys
//! and runs the checks, and asks a [`FrameCrypto`] to seal and open one encoded Opus frame.
//!
//! There is deliberately no "identity" or pass-through implementation outside `#[cfg(test)]`:
//! the only way an outgoing frame leaves [`super::SendGate`] is through [`FrameCrypto::seal`].

use std::fmt;

/// What the IV of one frame is built from (LkGcm: `ssrc ‖ rtp timestamp ‖ (timestamp −
/// (send_count mod 0xffff))`). The send gate owns the per-SSRC send count, which starts at a
/// random offset.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct IvParts {
    /// The sender's SSRC.
    pub ssrc: u32,
    /// The frame's RTP timestamp.
    pub rtp_timestamp: u32,
    /// Frames sent on this SSRC so far (from a random start).
    pub send_count: u32,
}

impl fmt::Debug for IvParts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Not secret, but kept out of logs anyway: nothing per frame belongs in a log.
        f.write_str("IvParts(..)")
    }
}

/// Why sealing a frame failed. Carries no key bytes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SealError {
    /// The frame is shorter than its clear prefix (an Opus frame needs its TOC byte).
    #[error("frame too short")]
    FrameTooShort,
    /// The slot is outside the 64-slot ring.
    #[error("slot outside the key ring")]
    BadSlot,
    /// The cipher failed.
    #[error("cipher failure")]
    Cipher,
}

/// Why opening a frame failed (the receiver check). Carries no key bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum OpenError {
    /// The trailer is malformed (IV length, frame too short).
    #[error("malformed trailer")]
    BadTrailer,
    /// No key at the frame's key index.
    #[error("no key at index {0}")]
    NoKey(u8),
    /// The GCM tag did not verify (wrong or retired key, a forged or plaintext frame).
    #[error("tag did not verify at index {0}")]
    BadTag(u8),
}

/// One audio frame cipher: LkGcm's Opus case (clear prefix = the TOC byte, also the AAD).
///
/// `Key` is the derived AES key (HKDF-SHA256 of the 32-byte IKM, salt `LKFrameEncryptionKey`);
/// it must redact `Debug` and zeroize on drop.
pub trait FrameCrypto: Send + Sync + 'static {
    /// The derived per-slot key.
    type Key: Send + Sync + fmt::Debug;

    /// Derives the frame key from a 32-byte IKM (`KeyEvent::{Install, Activate, Retire}`).
    fn derive(&self, ikm: &[u8; 32]) -> Self::Key;

    /// Seals one encoded Opus frame (never empty) under key index `slot`, appending the wire
    /// frame (clear TOC ‖ ciphertext ‖ tag ‖ IV ‖ IV length ‖ slot) to `out`.
    fn seal(
        &self,
        key: &Self::Key,
        slot: u8,
        iv: IvParts,
        frame: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<(), SealError>;

    /// The key index in a received frame's trailer, without opening it.
    fn key_index(&self, frame: &[u8]) -> Result<u8, OpenError>;

    /// Opens one received frame with `key` (the key at [`FrameCrypto::key_index`]), appending
    /// the Opus frame to `out`. A frame that does not verify is an error, whatever it ends with
    /// (no server-injected trailer, no magic plaintext bytes).
    fn open(&self, key: &Self::Key, frame: &[u8], out: &mut Vec<u8>) -> Result<(), OpenError>;
}
