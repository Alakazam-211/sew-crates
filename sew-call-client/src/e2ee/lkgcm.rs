//! [`LkGcm`]: the engine's frame cipher, sew-lk-compat's `LkGcmV1` (LiveKit's per-frame AES-GCM,
//! Opus case) behind this crate's [`FrameCrypto`] interface. The format itself lives in
//! `sew-lk-compat` (Apache-2.0, LiveKit-ported); this adapter is Alakazam's own and holds no
//! frame logic: it maps the interface's types onto the cryptor's.
//!
//! - The derived key is `sew_lk_compat::e2ee::FrameKey` (HKDF-SHA256 of the IKM, salt
//!   `LKFrameEncryptionKey`); its `Debug` is redacted.
//! - [`FrameCrypto::seal`] never sees an empty frame (the send gate drops them); if one arrives
//!   it is an error, never an empty "sealed" frame.
//! - [`FrameCrypto::open`] treats anything that does not verify as a failure, an empty frame
//!   included (LkGcm passes DTX frames through only in its ring API, which this does not use).

use sew_lk_compat::e2ee::{Codec, DecryptFail, EncryptError, FrameCryptor as _, FrameIv, FrameKey};
use sew_lk_compat::e2ee::{KEY_RING_SIZE, LkGcmV1};

use super::frame::{FrameCrypto, IvParts, OpenError, SealError};

/// LiveKit's AES-GCM frame format for one encoded Opus frame (`sew-lk-compat`'s `LkGcmV1`).
#[derive(Clone, Copy, Debug, Default)]
pub struct LkGcm;

/// LkGcm's trailer: the IV length byte (12) then the key index.
const TRAILER: usize = 2;
const IV_LEN: u8 = 12;
/// The smallest sealed Opus frame: TOC, a 16-byte tag, the 12-byte IV and the trailer.
const MIN_SEALED: usize = 1 + 16 + IV_LEN as usize + TRAILER;

impl FrameCrypto for LkGcm {
    type Key = FrameKey;

    fn derive(&self, ikm: &[u8; 32]) -> FrameKey {
        FrameKey::derive(ikm)
    }

    fn seal(
        &self,
        key: &FrameKey,
        slot: u8,
        iv: IvParts,
        frame: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<(), SealError> {
        if frame.is_empty() {
            return Err(SealError::FrameTooShort);
        }
        let start = out.len();
        let iv = FrameIv {
            ssrc: iv.ssrc,
            timestamp: iv.rtp_timestamp,
            send_count: iv.send_count,
        };
        match LkGcmV1.encrypt(Codec::Opus, frame, key, slot, iv, out) {
            Ok(()) if out.len() > start => Ok(()),
            Ok(()) => Err(SealError::Cipher),
            Err(EncryptError::BadSlot(_)) => Err(SealError::BadSlot),
            Err(EncryptError::FrameTooShort { .. }) => Err(SealError::FrameTooShort),
            Err(EncryptError::UnsupportedCodec(_)) => Err(SealError::Cipher),
            // A failure sew-lk-compat adds later: the frame is not sent.
            Err(_) => Err(SealError::Cipher),
        }
    }

    fn key_index(&self, frame: &[u8]) -> Result<u8, OpenError> {
        let n = frame.len();
        if n < MIN_SEALED || frame[n - 2] != IV_LEN {
            return Err(OpenError::BadTrailer);
        }
        let slot = frame[n - 1];
        if usize::from(slot) >= KEY_RING_SIZE {
            return Err(OpenError::BadTrailer);
        }
        Ok(slot)
    }

    fn open(&self, key: &FrameKey, frame: &[u8], out: &mut Vec<u8>) -> Result<(), OpenError> {
        let slot = self.key_index(frame)?;
        let start = out.len();
        match LkGcmV1.decrypt_with_key(Codec::Opus, frame, key, out) {
            Ok(info) if !info.empty && out.len() > start => Ok(()),
            Ok(_) => {
                out.truncate(start);
                Err(OpenError::BadTrailer)
            }
            Err(e) => {
                out.truncate(start);
                Err(match e.reason {
                    DecryptFail::NoKey => OpenError::NoKey(slot),
                    DecryptFail::BadTag => OpenError::BadTag(slot),
                    DecryptFail::BadTrailer => OpenError::BadTrailer,
                    // A failure sew-lk-compat adds later: the frame did not verify.
                    _ => OpenError::BadTag(slot),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn iv() -> IvParts {
        IvParts {
            ssrc: 0x1234_5678,
            rtp_timestamp: 960,
            send_count: 7,
        }
    }

    #[test]
    fn seals_and_opens_an_opus_frame() {
        let c = LkGcm;
        let k = c.derive(&[0x42; 32]);
        let frame = [0xfcu8, 1, 2, 3, 4, 5, 6, 7, 8, 9];
        let mut wire = Vec::new();
        c.seal(&k, 5, iv(), &frame, &mut wire).unwrap();
        assert_eq!(wire[0], 0xfc, "the TOC byte stays clear");
        assert_ne!(&wire[1..frame.len()], &frame[1..], "the body is encrypted");
        assert_eq!(c.key_index(&wire).unwrap(), 5);
        let mut out = Vec::new();
        c.open(&k, &wire, &mut out).unwrap();
        assert_eq!(out, frame);
        // The same bytes LkGcmV1 itself writes (no second format).
        let mut direct = Vec::new();
        let i = iv();
        LkGcmV1
            .encrypt(
                Codec::Opus,
                &frame,
                &FrameKey::derive(&[0x42; 32]),
                5,
                FrameIv {
                    ssrc: i.ssrc,
                    timestamp: i.rtp_timestamp,
                    send_count: i.send_count,
                },
                &mut direct,
            )
            .unwrap();
        assert_eq!(direct, wire);
    }

    #[test]
    fn wrong_key_empty_and_garbage_never_open() {
        let c = LkGcm;
        let k = c.derive(&[1; 32]);
        let other = c.derive(&[2; 32]);
        let mut wire = Vec::new();
        c.seal(&k, 0, iv(), &[0x78, 9, 9, 9], &mut wire).unwrap();
        let mut out = Vec::new();
        assert_eq!(c.open(&other, &wire, &mut out), Err(OpenError::BadTag(0)));
        assert!(out.is_empty(), "nothing of a failed frame is handed out");
        assert_eq!(c.open(&k, &[], &mut out), Err(OpenError::BadTrailer));
        assert_eq!(
            c.open(&k, &[0x78, 1, 2, 3], &mut out),
            Err(OpenError::BadTrailer)
        );
        let mut tampered = wire.clone();
        tampered[2] ^= 1;
        assert_eq!(c.open(&k, &tampered, &mut out), Err(OpenError::BadTag(0)));
        assert!(out.is_empty());
        assert_eq!(
            c.seal(&k, 0, iv(), &[], &mut out),
            Err(SealError::FrameTooShort)
        );
        assert_eq!(
            c.seal(&k, 64, iv(), &[0x78, 1], &mut out),
            Err(SealError::BadSlot)
        );
        assert!(out.is_empty());
    }
}
