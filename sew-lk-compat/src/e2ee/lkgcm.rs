// SPDX-License-Identifier: Apache-2.0
// Ported from livekit/client-sdk-js src/e2ee/worker/FrameCryptor.ts, src/e2ee/worker/naluUtils.ts, src/e2ee/utils.ts, src/e2ee/constants.ts @ 53dd840 (Apache-2.0, © 2021 LiveKit, Inc.).
// Modified by Alakazam Labs, 2026.

//! `LkGcmV1`: LiveKit's per-frame AES-GCM format (the only frame format Sew uses), byte-exact
//! with client-sdk-js v2.22.3.
//!
//! ```text
//! ---------+------------------------+--------------------+---------+----
//! header   | ciphertext ‖ GCM tag   | IV (12 bytes)      |IV_LENGTH| KID
//! (clear)  |                        |                    |  (12)   |slot
//! ---------+------------------------+--------------------+---------+----
//! ```
//!
//! - The clear header is the codec's unencrypted prefix (`getUnencryptedBytes`): Opus 1 (TOC);
//!   VP8 10 on keyframes, 3 on delta frames; VP9 0; H.264 up to the first slice NAL unit + 2.
//!   It is also the GCM additional data.
//! - For H.264 everything after the header is RBSP-escaped after encryption (`writeRbsp`) and
//!   unescaped before decryption when it contains an emulation sequence (`parseRbsp`).
//! - The IV is `ssrc ‖ rtp timestamp ‖ (timestamp − (send_count mod 0xffff))`, big endian
//!   (`makeIV`); the send count starts at a random offset per SSRC and grows by one per frame.
//! - Empty frames (DTX) pass through unencrypted in both directions.
//! - The AES-128 key is HKDF-SHA256(IKM = the raw 32-byte key, salt "LKFrameEncryptionKey",
//!   info = 128 zero bytes, L = 16) (`deriveKeys` with HKDF key material).
//! - No server-injected-frame trailer (`sif_trailer` empty) and no magic plaintext bytes
//!   (`uncryptedMagicBytes` empty): a frame that does not decrypt is a failure, whatever it ends
//!   with.

#[cfg(not(feature = "rust-crypto"))]
use aws_lc_rs::aead::{AES_128_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
#[cfg(not(feature = "rust-crypto"))]
use aws_lc_rs::hkdf::{HKDF_SHA256, KeyType, Salt};

// aws-lc-rs is never built for the phones.
#[cfg(all(
    any(target_os = "ios", target_os = "android"),
    not(feature = "rust-crypto")
))]
compile_error!(
    "sew-lk-compat on iOS and Android needs the `rust-crypto` feature (sew-call-client's `mobile`)"
);
use zeroize::Zeroizing;

use super::{DecryptError, DecryptFail, KEY_RING_SIZE, PRODUCTION};

/// `IV_LENGTH` (constants.ts).
pub const IV_LEN: usize = 12;
/// The two trailer bytes after the IV: IV length and key index.
pub const TRAILER_LEN: usize = 2;
/// The AES-GCM tag length.
pub const TAG_LEN: usize = 16;
/// `UNENCRYPTED_BYTES` (constants.ts).
pub const UNENCRYPTED_KEY: usize = 10;
/// Delta VP8 frames.
pub const UNENCRYPTED_DELTA: usize = 3;
/// Audio (the Opus TOC byte).
pub const UNENCRYPTED_AUDIO: usize = 1;

/// The codec of an encoded frame (decides the clear prefix).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Codec {
    /// Opus audio.
    Opus,
    /// VP8; key/delta is read from the frame tag's P bit.
    Vp8,
    /// H.264 in Annex B.
    H264,
    /// VP9 (no clear bytes; the SFU uses the Dependency Descriptor).
    Vp9,
    /// AV1: refused under E2EE by the SDK (`getUnencryptedBytes` throws), and by us.
    Av1,
}

/// A derived AES-128-GCM frame key.
pub struct FrameKey {
    #[cfg(not(feature = "rust-crypto"))]
    key: LessSafeKey,
    #[cfg(feature = "rust-crypto")]
    key: aes_gcm::Aes128Gcm,
}

impl std::fmt::Debug for FrameKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FrameKey(<redacted>)")
    }
}

#[cfg(not(feature = "rust-crypto"))]
struct Len16;

#[cfg(not(feature = "rust-crypto"))]
impl KeyType for Len16 {
    fn len(&self) -> usize {
        16
    }
}

impl FrameKey {
    /// HKDF-SHA256 over the raw 32-byte IKM with the production salt and info (`deriveKeys`).
    pub fn derive(ikm: &[u8; 32]) -> FrameKey {
        let raw = derive_aes_key(ikm);
        FrameKey::from_aes_key(&raw)
    }

    /// A frame key from raw AES-128 key bytes (tests; retire uses random IKM instead).
    pub fn from_aes_key(raw: &[u8; 16]) -> FrameKey {
        #[cfg(not(feature = "rust-crypto"))]
        // AES_128_GCM accepts every 16-byte key, so this cannot fail.
        let key = match UnboundKey::new(&AES_128_GCM, raw) {
            Ok(k) => LessSafeKey::new(k),
            Err(_) => panic!("AES-128-GCM rejected a 16-byte key"),
        };
        #[cfg(feature = "rust-crypto")]
        let key = {
            use aes_gcm::KeyInit as _;
            aes_gcm::Aes128Gcm::new(raw.into())
        };
        FrameKey { key }
    }

    /// AES-128-GCM: seals `buf` in place under `iv` and `aad`, appending the 16-byte tag.
    #[cfg(not(feature = "rust-crypto"))]
    fn seal(&self, iv: [u8; IV_LEN], aad: &[u8], buf: &mut Vec<u8>) -> Result<(), ()> {
        self.key
            .seal_in_place_append_tag(Nonce::assume_unique_for_key(iv), Aad::from(aad), buf)
            .map_err(|_| ())
    }

    /// AES-128-GCM: seals `buf` in place under `iv` and `aad`, appending the 16-byte tag.
    #[cfg(feature = "rust-crypto")]
    fn seal(&self, iv: [u8; IV_LEN], aad: &[u8], buf: &mut Vec<u8>) -> Result<(), ()> {
        use aes_gcm::aead::AeadInPlace as _;
        self.key
            .encrypt_in_place(&iv.into(), aad, buf)
            .map_err(|_| ())
    }

    /// AES-128-GCM: opens `buf` (ciphertext and tag) in place; the plaintext on success.
    #[cfg(not(feature = "rust-crypto"))]
    // The same signature as the RustCrypto one, which needs the `Vec` (an `aead::Buffer`).
    #[allow(clippy::ptr_arg)]
    fn open<'a>(&self, iv: [u8; IV_LEN], aad: &[u8], buf: &'a mut Vec<u8>) -> Result<&'a [u8], ()> {
        self.key
            .open_in_place(Nonce::assume_unique_for_key(iv), Aad::from(aad), buf)
            .map(|p| &*p)
            .map_err(|_| ())
    }

    /// AES-128-GCM: opens `buf` (ciphertext and tag) in place; the plaintext on success.
    #[cfg(feature = "rust-crypto")]
    fn open<'a>(&self, iv: [u8; IV_LEN], aad: &[u8], buf: &'a mut Vec<u8>) -> Result<&'a [u8], ()> {
        use aes_gcm::aead::AeadInPlace as _;
        self.key
            .decrypt_in_place(&iv.into(), aad, buf)
            .map_err(|_| ())?;
        Ok(&buf[..])
    }

    /// A key derived from 32 random bytes (a retired slot is overwritten with one).
    pub fn random() -> FrameKey {
        let mut ikm = Zeroizing::new([0u8; 32]);
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), ikm.as_mut());
        FrameKey::derive(&ikm)
    }
}

/// The AES-128 key HKDF derives from `ikm` (exposed so tests can pin the KDF).
#[cfg(feature = "rust-crypto")]
pub fn derive_aes_key(ikm: &[u8; 32]) -> Zeroizing<[u8; 16]> {
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(PRODUCTION.ratchet_salt.as_bytes()), ikm);
    let info = [0u8; 128];
    let mut out = Zeroizing::new([0u8; 16]);
    let ok = hk.expand(&info, out.as_mut()).is_ok();
    assert!(ok, "HKDF-SHA256 expand to 16 bytes failed");
    out
}

/// The AES-128 key HKDF derives from `ikm` (exposed so tests can pin the KDF).
#[cfg(not(feature = "rust-crypto"))]
pub fn derive_aes_key(ikm: &[u8; 32]) -> Zeroizing<[u8; 16]> {
    let salt = Salt::new(HKDF_SHA256, PRODUCTION.ratchet_salt.as_bytes());
    let prk = salt.extract(ikm);
    let info = [0u8; 128];
    let infos = [&info[..]];
    let mut out = Zeroizing::new([0u8; 16]);
    let ok = prk
        .expand(&infos, Len16)
        .and_then(|okm| okm.fill(out.as_mut()))
        .is_ok();
    assert!(ok, "HKDF-SHA256 expand to 16 bytes failed");
    out
}

/// The IV inputs of one frame (`makeIV`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameIv {
    /// The RTP SSRC of the stream (`synchronizationSource`).
    pub ssrc: u32,
    /// The frame's RTP timestamp.
    pub timestamp: u32,
    /// The per-SSRC send count (random start, +1 per frame).
    pub send_count: u32,
}

impl FrameIv {
    /// The 12 IV bytes.
    pub fn bytes(&self) -> [u8; IV_LEN] {
        let mut iv = [0u8; IV_LEN];
        iv[..4].copy_from_slice(&self.ssrc.to_be_bytes());
        iv[4..8].copy_from_slice(&self.timestamp.to_be_bytes());
        iv[8..].copy_from_slice(
            &self
                .timestamp
                .wrapping_sub(self.send_count % 0xffff)
                .to_be_bytes(),
        );
        iv
    }
}

/// What decrypting a frame found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct DecryptInfo {
    /// The key index (slot) in the trailer.
    pub slot: u8,
    /// The clear prefix length.
    pub clear_len: usize,
    /// Whether the frame was empty (DTX) and passed through.
    pub empty: bool,
}

/// Why a frame could not be encrypted.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum EncryptError {
    /// AV1 cannot be end-to-end encrypted (the SDK throws; Sew refuses it).
    #[error("{0:?} is not supported for end-to-end encryption")]
    UnsupportedCodec(Codec),
    /// The clear prefix is longer than the frame (the SDK's typed-array view throws).
    #[error("frame of {len} bytes is shorter than its clear prefix of {clear}")]
    FrameTooShort {
        /// Frame length.
        len: usize,
        /// Clear prefix.
        clear: usize,
    },
    /// The slot is outside the 64-slot ring.
    #[error("slot {0} is outside the key ring")]
    BadSlot(u8),
}

/// A sender's 64-slot key ring (`ParticipantKeyHandler.cryptoKeyRing`, keyringSize 64).
#[derive(Debug)]
pub struct SenderRing {
    slots: Vec<Option<FrameKey>>,
}

impl Default for SenderRing {
    fn default() -> SenderRing {
        SenderRing::new()
    }
}

impl SenderRing {
    /// An empty ring of [`KEY_RING_SIZE`] slots.
    pub fn new() -> SenderRing {
        SenderRing {
            slots: (0..KEY_RING_SIZE).map(|_| None).collect(),
        }
    }

    /// Sets `slot` (taken mod the ring size, as `setKeySet` does).
    pub fn set(&mut self, slot: u8, key: FrameKey) {
        let i = usize::from(slot) % KEY_RING_SIZE;
        self.slots[i] = Some(key);
    }

    /// The key at `slot`; `None` for an empty slot or an index outside the ring.
    pub fn get(&self, slot: u8) -> Option<&FrameKey> {
        self.slots.get(usize::from(slot)).and_then(Option::as_ref)
    }

    /// Whether `slot` holds a key.
    pub fn has(&self, slot: u8) -> bool {
        self.get(slot).is_some()
    }

    /// Overwrites `slot` with a key derived from random bytes (retire).
    pub fn retire(&mut self, slot: u8) {
        self.set(slot, FrameKey::random());
    }
}

/// The frame cryptor interface (LkGcmV1 only).
pub trait FrameCryptor: Send + Sync {
    /// Encrypts `frame` with `key` under key index `slot`, appending the result to `out`.
    fn encrypt(
        &self,
        codec: Codec,
        frame: &[u8],
        key: &FrameKey,
        slot: u8,
        iv: FrameIv,
        out: &mut Vec<u8>,
    ) -> Result<(), EncryptError>;
    /// Decrypts `frame` with the sender's ring, appending the plaintext frame to `out`.
    fn decrypt(
        &self,
        codec: Codec,
        frame: &[u8],
        ring: &SenderRing,
        out: &mut Vec<u8>,
    ) -> Result<DecryptInfo, DecryptError>;
    /// The clear prefix length of `frame` (`getUnencryptedBytes`).
    fn unencrypted_prefix(&self, codec: Codec, frame: &[u8]) -> usize;
}

/// LiveKit's AES-GCM frame format (client-sdk-js v2.22.3 / rust-sdks webrtc-89d790b).
#[derive(Clone, Copy, Debug, Default)]
pub struct LkGcmV1;

/// The clear prefix and whether the codec needs NALU (RBSP) processing.
struct FrameInfo {
    unencrypted: usize,
    nalu: bool,
}

fn vp8_is_key(frame: &[u8]) -> bool {
    // RFC 6386 §9.1: frame tag bit 0 (P) is 0 for a keyframe.
    frame.first().is_some_and(|b| b & 0x01 == 0)
}

fn h264_is_key(frame: &[u8], nalus: &[usize]) -> bool {
    nalus
        .iter()
        .any(|&i| frame.get(i).is_some_and(|b| matches!(b & 0x1f, 5 | 7)))
}

fn frame_info(codec: Codec, frame: &[u8]) -> Result<FrameInfo, EncryptError> {
    Ok(match codec {
        Codec::Opus => FrameInfo {
            unencrypted: UNENCRYPTED_AUDIO,
            nalu: false,
        },
        Codec::Vp8 => FrameInfo {
            unencrypted: if vp8_is_key(frame) {
                UNENCRYPTED_KEY
            } else {
                UNENCRYPTED_DELTA
            },
            nalu: false,
        },
        Codec::Vp9 => FrameInfo {
            unencrypted: 0,
            nalu: false,
        },
        Codec::Av1 => return Err(EncryptError::UnsupportedCodec(Codec::Av1)),
        Codec::H264 => match find_nalu_indices(frame) {
            Ok(nalus) => match nalus
                .iter()
                .find(|&&i| frame.get(i).is_some_and(|b| matches!(b & 0x1f, 1 | 5)))
            {
                Some(&i) => FrameInfo {
                    unencrypted: i + 2,
                    nalu: true,
                },
                // "Could not find NALU" → the SDK falls back to VP8 handling by frame type.
                None => FrameInfo {
                    unencrypted: if h264_is_key(frame, &nalus) {
                        UNENCRYPTED_KEY
                    } else {
                        UNENCRYPTED_DELTA
                    },
                    nalu: false,
                },
            },
            // "byte stream contains leading data" → the same fallback.
            Err(LeadingData) => FrameInfo {
                unencrypted: if vp8_is_key(frame) {
                    UNENCRYPTED_KEY
                } else {
                    UNENCRYPTED_DELTA
                },
                nalu: false,
            },
        },
    })
}

/// `findNALUIndices` threw "byte stream contains leading data".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LeadingData;

/// `findNALUIndices` (naluUtils.ts): the index of each NAL unit header after its start code.
/// `Err` when the stream has leading data before the first start code.
pub fn find_nalu_indices(stream: &[u8]) -> Result<Vec<usize>, LeadingData> {
    let mut result = Vec::new();
    let len = stream.len();
    let at = |i: usize| stream.get(i).copied().unwrap_or(0xff);
    let mut start = 0usize;
    let mut pos = 0usize;
    // `searchLength = stream.length - 3` (may be negative in JS: then the loop never runs).
    let Some(search) = len.checked_sub(3) else {
        return Ok(result);
    };
    while pos < search {
        while pos < search {
            if pos + 1 < search
                && at(pos) == 0
                && at(pos + 1) == 0
                && at(pos + 2) == 0
                && at(pos + 3) == 1
            {
                break;
            }
            if at(pos) == 0 && at(pos + 1) == 0 && at(pos + 2) == 1 {
                break;
            }
            pos += 1;
        }
        if pos >= search {
            pos = len;
        }
        let mut end = pos;
        while end > start && at(end - 1) == 0 {
            end -= 1;
        }
        if start == 0 {
            if end != start {
                return Err(LeadingData);
            }
        } else {
            result.push(start);
        }
        let mut start_code = 3;
        if pos + 3 < len && at(pos) == 0 && at(pos + 1) == 0 && at(pos + 2) == 0 && at(pos + 3) == 1
        {
            start_code = 4;
        }
        pos += start_code;
        start = pos;
    }
    Ok(result)
}

/// `needsRbspUnescaping` (utils.ts).
pub fn needs_rbsp_unescaping(data: &[u8]) -> bool {
    data.len() > 3 && data.windows(3).take(data.len() - 3).any(|w| w == [0, 0, 3])
}

/// `parseRbsp` (utils.ts): removes emulation-prevention bytes.
pub fn parse_rbsp(stream: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(stream.len());
    let mut i = 0;
    while i < stream.len() {
        if stream.len() - i >= 3 && stream[i] == 0 && stream[i + 1] == 0 && stream[i + 2] == 3 {
            out.push(stream[i]);
            out.push(stream[i + 1]);
            i += 3;
        } else {
            out.push(stream[i]);
            i += 1;
        }
    }
    out
}

/// `writeRbsp` (utils.ts): inserts emulation-prevention bytes.
pub fn write_rbsp(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 64 + 1);
    let mut zeros = 0;
    for &b in data {
        if b <= 3 && zeros >= 2 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
    }
    out
}

impl FrameCryptor for LkGcmV1 {
    fn encrypt(
        &self,
        codec: Codec,
        frame: &[u8],
        key: &FrameKey,
        slot: u8,
        iv: FrameIv,
        out: &mut Vec<u8>,
    ) -> Result<(), EncryptError> {
        if frame.is_empty() {
            // `encodeFunction`: empty DTX frames are not encrypted and get no trailer.
            return Ok(());
        }
        if usize::from(slot) >= KEY_RING_SIZE {
            return Err(EncryptError::BadSlot(slot));
        }
        let info = frame_info(codec, frame)?;
        if info.unencrypted > frame.len() {
            return Err(EncryptError::FrameTooShort {
                len: frame.len(),
                clear: info.unencrypted,
            });
        }
        let ivb = iv.bytes();
        let (header, body) = frame.split_at(info.unencrypted);
        let mut sealed = Vec::with_capacity(body.len() + TAG_LEN + IV_LEN + TRAILER_LEN);
        sealed.extend_from_slice(body);
        if key.seal(ivb, header, &mut sealed).is_err() {
            // AES-GCM sealing only fails for inputs beyond 2^36 bytes.
            return Err(EncryptError::FrameTooShort {
                len: frame.len(),
                clear: info.unencrypted,
            });
        }
        sealed.extend_from_slice(&ivb);
        sealed.push(IV_LEN as u8);
        sealed.push(slot);
        out.extend_from_slice(header);
        if info.nalu {
            out.extend_from_slice(&write_rbsp(&sealed));
        } else {
            out.extend_from_slice(&sealed);
        }
        Ok(())
    }

    fn decrypt(
        &self,
        codec: Codec,
        frame: &[u8],
        ring: &SenderRing,
        out: &mut Vec<u8>,
    ) -> Result<DecryptInfo, DecryptError> {
        if frame.is_empty() {
            return Ok(DecryptInfo {
                slot: 0,
                clear_len: 0,
                empty: true,
            });
        }
        // `decodeFunction`: the key index is the last byte of the (escaped) frame.
        let slot = frame[frame.len() - 1];
        let Some(key) = ring.get(slot) else {
            return Err(DecryptError {
                slot,
                reason: DecryptFail::NoKey,
            });
        };
        self.decrypt_with_key(codec, frame, key, out)
    }

    fn unencrypted_prefix(&self, codec: Codec, frame: &[u8]) -> usize {
        frame_info(codec, frame).map_or(0, |i| i.unencrypted)
    }
}

impl LkGcmV1 {
    /// Decrypts a non-empty `frame` with `key` (the key at the frame's key index, which the
    /// caller looked up: a holder of per-slot keys outside a [`SenderRing`]), appending the
    /// plaintext frame to `out`. An empty frame is a malformed trailer here: only
    /// [`FrameCryptor::decrypt`] passes DTX frames through.
    pub fn decrypt_with_key(
        &self,
        codec: Codec,
        frame: &[u8],
        key: &FrameKey,
        out: &mut Vec<u8>,
    ) -> Result<DecryptInfo, DecryptError> {
        let Some(&slot) = frame.last() else {
            return Err(DecryptError {
                slot: 0,
                reason: DecryptFail::BadTrailer,
            });
        };
        let bad = |reason| DecryptError { slot, reason };
        let info = frame_info(codec, frame).map_err(|_| bad(DecryptFail::BadTrailer))?;
        if info.unencrypted > frame.len() {
            return Err(bad(DecryptFail::BadTrailer));
        }
        let (header, rest) = frame.split_at(info.unencrypted);
        let unescaped;
        let data: &[u8] = if info.nalu && needs_rbsp_unescaping(rest) {
            unescaped = parse_rbsp(rest);
            &unescaped
        } else {
            rest
        };
        if data.len() < TRAILER_LEN {
            return Err(bad(DecryptFail::BadTrailer));
        }
        let n = data.len();
        let iv_len = usize::from(data[n - 2]);
        if iv_len != IV_LEN || n < TRAILER_LEN + IV_LEN + TAG_LEN {
            return Err(bad(DecryptFail::BadTrailer));
        }
        let mut ivb = [0u8; IV_LEN];
        ivb.copy_from_slice(&data[n - TRAILER_LEN - IV_LEN..n - TRAILER_LEN]);
        let mut ct = data[..n - TRAILER_LEN - IV_LEN].to_vec();
        let plain = key
            .open(ivb, header, &mut ct)
            .map_err(|_| bad(DecryptFail::BadTag))?;
        out.extend_from_slice(header);
        out.extend_from_slice(plain);
        Ok(DecryptInfo {
            slot,
            clear_len: header.len(),
            empty: false,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// The same answers from the aws-lc-rs and the RustCrypto (`rust-crypto`) builds,
    /// computed independently (Python's hmac/hashlib HKDF and `cryptography`'s AES-GCM).
    #[test]
    fn known_answers_whichever_backend() {
        let seq: [u8; 32] = std::array::from_fn(|i| i as u8);
        assert_eq!(
            hex(&*derive_aes_key(&seq)),
            "6d0dd7b3d6f4efde47ab9a9e15abde76"
        );
        let key = FrameKey::derive(&[7; 32]);
        let iv = FrameIv {
            ssrc: 0x1234_5678,
            timestamp: 90_000,
            send_count: 4242,
        };
        let mut frame = vec![0xfc];
        frame.extend(1u8..40);
        let mut ct = Vec::new();
        LkGcmV1
            .encrypt(Codec::Opus, &frame, &key, 9, iv, &mut ct)
            .unwrap();
        assert_eq!(
            hex(&ct),
            "fc16beb1823135e887ad84265f847367af03460fc99487b42ddd452f8df0edd494c1aa282d2fe32bcbb92998b062a878c5ed61259658e4d71234567800015f9000014efe0c09"
        );
        let mut out = Vec::new();
        LkGcmV1
            .decrypt_with_key(Codec::Opus, &ct, &key, &mut out)
            .unwrap();
        assert_eq!(out, frame);
        let mut bad = ct.clone();
        bad[3] ^= 1;
        let e = LkGcmV1
            .decrypt_with_key(Codec::Opus, &bad, &key, &mut Vec::new())
            .unwrap_err();
        assert!(matches!(e.reason, DecryptFail::BadTag), "{e:?}");
    }
}
