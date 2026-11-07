//! [`LkGcm`]: the engine's frame cipher, sew-lk-compat's `LkGcmV1` (LiveKit's per-frame AES-GCM:
//! the Opus case, and the VP8 and H.264 cases for received video) behind this crate's
//! [`FrameCrypto`] interface. The format itself lives in
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

use super::frame::{FrameCrypto, IvParts, OpenError, SealError, VideoFrameCodec};

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

    fn seal_video(
        &self,
        key: &FrameKey,
        slot: u8,
        codec: VideoFrameCodec,
        iv: IvParts,
        frame: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<(), SealError> {
        if frame.is_empty() {
            return Err(SealError::FrameTooShort);
        }
        let codec = match codec {
            VideoFrameCodec::Vp8 => Codec::Vp8,
            VideoFrameCodec::H264 => Codec::H264,
        };
        let start = out.len();
        let iv = FrameIv {
            ssrc: iv.ssrc,
            timestamp: iv.rtp_timestamp,
            send_count: iv.send_count,
        };
        match LkGcmV1.encrypt(codec, frame, key, slot, iv, out) {
            Ok(()) if out.len() > start => Ok(()),
            Ok(()) => Err(SealError::Cipher),
            Err(EncryptError::BadSlot(_)) => {
                out.truncate(start);
                Err(SealError::BadSlot)
            }
            Err(EncryptError::FrameTooShort { .. }) => {
                out.truncate(start);
                Err(SealError::FrameTooShort)
            }
            Err(_) => {
                out.truncate(start);
                Err(SealError::Cipher)
            }
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
        open_codec(Codec::Opus, key, frame, out)
    }

    fn open_video(
        &self,
        key: &FrameKey,
        codec: crate::video::VideoCodec,
        frame: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<(), OpenError> {
        let codec = match codec {
            crate::video::VideoCodec::Vp8 => Codec::Vp8,
            crate::video::VideoCodec::H264 => Codec::H264,
        };
        open_codec(codec, key, frame, out)
    }
}

/// Opens one frame of `codec`: anything that does not verify is a failure, an empty frame
/// included, and nothing of a failed frame stays in `out`.
fn open_codec(
    codec: Codec,
    key: &FrameKey,
    frame: &[u8],
    out: &mut Vec<u8>,
) -> Result<(), OpenError> {
    let slot = LkGcm.key_index(frame)?;
    let start = out.len();
    match LkGcmV1.decrypt_with_key(codec, frame, key, out) {
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
    fn seals_video_frames_with_the_codecs_clear_prefix() {
        let c = LkGcm;
        let k = c.derive(&[0x42; 32]);
        // A VP8 keyframe (P bit clear, start code 9d 01 2a): 10 bytes stay clear.
        let mut key = vec![0x10, 0x02, 0x00, 0x9d, 0x01, 0x2a, 0x40, 0x01, 0xf0, 0x00];
        key.extend_from_slice(&[0x77; 40]);
        let mut wire = Vec::new();
        c.seal_video(&k, 3, VideoFrameCodec::Vp8, iv(), &key, &mut wire)
            .unwrap();
        assert_eq!(&wire[..10], &key[..10]);
        assert_ne!(&wire[10..50], &key[10..]);
        assert_eq!(c.key_index(&wire).unwrap(), 3);
        let mut direct = Vec::new();
        LkGcmV1
            .encrypt(
                Codec::Vp8,
                &key,
                &FrameKey::derive(&[0x42; 32]),
                3,
                FrameIv {
                    ssrc: iv().ssrc,
                    timestamp: iv().rtp_timestamp,
                    send_count: iv().send_count,
                },
                &mut direct,
            )
            .unwrap();
        assert_eq!(direct, wire, "the bytes LkGcmV1 writes");
        // An H.264 IDR access unit: the parameter sets and the slice header stay clear.
        let au = [
            0, 0, 0, 1, 0x67, 0x42, 0xe0, 0x1f, 0, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80, 0, 0, 0, 1,
            0x65, 0x88, 0x84, 0x21, 0x43, 0x55, 0x66, 0x77, 0x88, 0x99,
        ];
        let mut wire = Vec::new();
        c.seal_video(&k, 0, VideoFrameCodec::H264, iv(), &au, &mut wire)
            .unwrap();
        assert_eq!(&wire[..22], &au[..22], "up to the first slice, plus 2");
        let mut out = Vec::new();
        assert_eq!(
            c.seal_video(&k, 0, VideoFrameCodec::Vp8, iv(), &[], &mut out),
            Err(SealError::FrameTooShort)
        );
        assert!(out.is_empty());
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

    fn sealed(codec: Codec, frame: &[u8], ikm: &[u8; 32]) -> Vec<u8> {
        let mut wire = Vec::new();
        let i = iv();
        LkGcmV1
            .encrypt(
                codec,
                frame,
                &FrameKey::derive(ikm),
                3,
                FrameIv {
                    ssrc: i.ssrc,
                    timestamp: i.rtp_timestamp,
                    send_count: i.send_count,
                },
                &mut wire,
            )
            .unwrap();
        wire
    }

    #[test]
    fn opens_vp8_and_h264_frames_and_nothing_else() {
        use crate::video::VideoCodec;
        let c = LkGcm;
        let k = c.derive(&[7; 32]);
        // A VP8 keyframe (P bit clear, the start code, 640x360) and an H.264 IDR access unit.
        let mut vp8 = vec![0x10, 0x02, 0x00, 0x9d, 0x01, 0x2a, 0x80, 0x02, 0x68, 0x01];
        vp8.extend_from_slice(&[0x5a; 40]);
        let mut h264 = vec![0, 0, 0, 1, 0x67, 0x42, 0xe0, 0x1f, 0, 0, 0, 1, 0x68, 0xce];
        h264.extend_from_slice(&[0, 0, 0, 1, 0x65, 0x88, 0x84]);
        h264.extend_from_slice(&[0x33; 40]);
        for (codec, lk, frame) in [
            (VideoCodec::Vp8, Codec::Vp8, &vp8),
            (VideoCodec::H264, Codec::H264, &h264),
        ] {
            let wire = sealed(lk, frame, &[7; 32]);
            assert_eq!(c.key_index(&wire).unwrap(), 3);
            let mut out = Vec::new();
            c.open_video(&k, codec, &wire, &mut out).unwrap();
            assert_eq!(&out, frame, "{codec:?}");
            // Another key, a flipped bit: never opened.
            let mut out = Vec::new();
            let wrong = c.derive(&[8; 32]);
            assert_eq!(
                c.open_video(&wrong, codec, &wire, &mut out),
                Err(OpenError::BadTag(3))
            );
            let mut tampered = wire.clone();
            tampered[20] ^= 1;
            assert!(c.open_video(&k, codec, &tampered, &mut out).is_err());
            // The audio path refuses a video frame (its clear prefix is one byte).
            assert!(c.open(&k, &wire, &mut out).is_err());
            assert!(out.is_empty());
        }
    }
}
