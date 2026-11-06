//! The H.264 access unit as Sew encrypts and sends it: a 4-byte start code before every NAL
//! unit, no access unit delimiters. Under E2EE the clear header (the NAL units up to the first
//! slice, plus 2 bytes) is also the cipher's additional data, so sender and receiver must frame
//! the start codes the same way; the receiver's depacketiser always writes 4-byte codes, and
//! this makes the sender's frames match whatever the encoder wrote.

use super::RtpError;
use super::h264::{START_CODE, nal_type, nal_units};

const AUD: u8 = 9;
const SPS: u8 = 7;
const PPS: u8 = 8;
const IDR: u8 = 5;

/// `au` with 4-byte start codes and without AUD NAL units.
pub(crate) fn normalise(au: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(au.len() + 8);
    for nal in nal_units(au) {
        if nal_type(nal) == AUD {
            continue;
        }
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(nal);
    }
    out
}

/// Checks a normalised access unit before it is encrypted: it holds a slice, and an IDR
/// access unit has an SPS and a PPS before its first IDR slice.
pub(crate) fn check(au: &[u8]) -> Result<(), RtpError> {
    let nals = nal_units(au);
    let first_slice = nals
        .iter()
        .position(|n| (1..=5).contains(&nal_type(n)))
        .ok_or(RtpError::Malformed("an H.264 access unit without a slice"))?;
    if nals.iter().any(|n| nal_type(n) == IDR) {
        let before = &nals[..first_slice];
        let has = |t| before.iter().any(|n| nal_type(n) == t);
        if !has(SPS) || !has(PPS) {
            return Err(RtpError::Malformed(
                "an IDR access unit without SPS and PPS before it",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_byte_codes_become_four_and_aud_goes() {
        let au = [
            0, 0, 1, 0x09, 0xf0, // AUD
            0, 0, 1, 0x67, 0x42, 0xe0, 0x1f, // SPS
            0, 0, 0, 1, 0x68, 0xce, // PPS
            0, 0, 1, 0x65, 0x88, 0x84, 0x00, // IDR (ends in a zero byte)
        ];
        let want = [
            0, 0, 0, 1, 0x67, 0x42, 0xe0, 0x1f, 0, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65, 0x88,
            0x84, 0x00,
        ];
        assert_eq!(normalise(&au), want);
        assert_eq!(normalise(&want), want, "idempotent");
        check(&want).expect("SPS and PPS before the IDR");
    }

    #[test]
    fn check_refuses_an_idr_without_parameter_sets() {
        let au = [0, 0, 0, 1, 0x65, 0x88, 0, 0, 0, 1, 0x68, 0xce];
        assert!(check(&au).is_err());
        assert!(check(&[0, 0, 0, 1, 0x67, 0x42]).is_err(), "no slice");
        check(&[0, 0, 0, 1, 0x41, 0x9a]).expect("a non-IDR needs none");
    }
}
