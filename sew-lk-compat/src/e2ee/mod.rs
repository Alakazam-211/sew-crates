//! The LiveKit frame format's configuration and errors, shared by [`lkgcm`].
//!
//! - [`lkgcm`]: LiveKit's AES-GCM frame format (`LkGcmV1`) and the 64-slot per-sender key ring
//!   (ported from client-sdk-js; see that file's header).
//! - [`PRODUCTION`]: the one configuration Sew runs.
//!
//! The items here are Alakazam Labs' own (taken from Sew's media test client);
//! only `lkgcm.rs` is ported from LiveKit.

pub mod lkgcm;

use serde::Serialize;

pub use lkgcm::{
    Codec, DecryptInfo, EncryptError, FrameCryptor, FrameIv, FrameKey, LkGcmV1, SenderRing,
};

/// LiveKit `keyRingSize` on every client (the server's `e2ee.key_ring_size`).
pub const KEY_RING_SIZE: usize = 64;

/// The production E2EE configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProductionConfig {
    /// The frame format: LiveKit AES-GCM.
    pub frame_format: &'static str,
    /// `keyRingSize`.
    pub key_ring_size: usize,
    /// The key derivation: HKDF-SHA256 over the raw 32-byte IKM.
    pub kdf: &'static str,
    /// `ratchetSalt`, the HKDF salt.
    pub ratchet_salt: &'static str,
    /// HKDF info length (zero bytes), as the SDKs use.
    pub kdf_info_len: usize,
    /// AES key size in bits (`keySize`).
    pub key_size_bits: u32,
    /// `ratchetWindowSize`: never ratchet.
    pub ratchet_window_size: u32,
    /// `failureTolerance`: never give up on a sender.
    pub failure_tolerance: i32,
    /// `sharedKey`: per-sender keys.
    pub shared_key: bool,
    /// `uncryptedMagicBytes`: none (no plaintext bypass).
    pub uncrypted_magic_bytes: &'static [u8],
    /// The server-injected-frame trailer: none.
    pub sif_trailer: &'static [u8],
    /// The server's `e2ee.activate_timeout_ms`.
    pub activate_timeout_ms: u64,
    /// The server's `e2ee.retire_after_ms`.
    pub retire_after_ms: u64,
    /// The server's `e2ee.min_activation_interval_ms`.
    pub min_activation_interval_ms: u64,
    /// The server's `e2ee.client_activate_fallback_ms`: a client that acked but got no
    /// `activate` switches by itself.
    pub client_activate_fallback_ms: u64,
}

/// The one production configuration.
pub const PRODUCTION: ProductionConfig = ProductionConfig {
    frame_format: "lk_gcm_v1",
    key_ring_size: KEY_RING_SIZE,
    kdf: "hkdf",
    ratchet_salt: "LKFrameEncryptionKey",
    kdf_info_len: 128,
    key_size_bits: 128,
    ratchet_window_size: 0,
    failure_tolerance: -1,
    shared_key: false,
    uncrypted_magic_bytes: b"",
    sif_trailer: b"",
    activate_timeout_ms: 1_000,
    retire_after_ms: 10_000,
    min_activation_interval_ms: 200,
    client_activate_fallback_ms: 2_500,
};

/// The failure classes of `decrypt_fail`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DecryptFail {
    /// No key at the frame's key index (never installed, or outside the ring).
    NoKey,
    /// The GCM tag did not verify (wrong or retired key, corrupted frame).
    BadTag,
    /// The trailer is malformed (IV length, frame too short).
    BadTrailer,
}

/// A frame that did not decrypt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("decrypt failed ({reason:?}) at key index {slot}")]
#[non_exhaustive]
pub struct DecryptError {
    /// The key index read from the trailer.
    pub slot: u8,
    /// Why.
    pub reason: DecryptFail,
}
