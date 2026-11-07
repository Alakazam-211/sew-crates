//! The coded size of a received frame: a VP8 keyframe's header (RFC 6386 §9.1) or an H.264
//! access unit's sequence parameter set (ITU-T H.264 §7.3.2.1.1, with the frame cropping).
//! Other frames carry no size.

use super::rtp::h264::{nal_type, nal_units};

/// The size of a VP8 keyframe (`None` for a delta frame or a malformed header).
pub(crate) fn vp8(frame: &[u8]) -> Option<(u32, u32)> {
    if frame.len() < 10 || frame[0] & 0x01 != 0 || frame[3..6] != [0x9d, 0x01, 0x2a] {
        return None;
    }
    let w = u32::from(u16::from_le_bytes([frame[6], frame[7]]) & 0x3fff);
    let h = u32::from(u16::from_le_bytes([frame[8], frame[9]]) & 0x3fff);
    (w > 0 && h > 0).then_some((w, h))
}

/// The size in the first SPS of an H.264 access unit (`None` without one, or a malformed one).
pub(crate) fn h264(au: &[u8]) -> Option<(u32, u32)> {
    let sps = nal_units(au).into_iter().find(|n| nal_type(n) == 7)?;
    sps_size(&unescape(sps.get(1..)?))
}

/// The RBSP of a NAL unit's payload: emulation prevention bytes (`00 00 03`) removed.
fn unescape(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    let mut zeros = 0;
    for &x in b {
        if zeros >= 2 && x == 3 {
            zeros = 0;
            continue;
        }
        zeros = if x == 0 { zeros + 1 } else { 0 };
        out.push(x);
    }
    out
}

/// An Exp-Golomb bit reader.
struct Bits<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> Option<u32> {
        let byte = *self.b.get(self.pos / 8)?;
        let v = (byte >> (7 - (self.pos % 8))) & 1;
        self.pos += 1;
        Some(u32::from(v))
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let rest = self.bits(zeros)?;
        Some(((1u64 << zeros) - 1 + u64::from(rest)).min(u64::from(u32::MAX)) as u32)
    }

    fn se(&mut self) -> Option<i64> {
        let k = i64::from(self.ue()?);
        Some(if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) })
    }
}

/// `scaling_list()` (§7.3.2.1.1.1): only read past.
fn skip_scaling_list(r: &mut Bits<'_>, size: usize) -> Option<()> {
    let (mut last, mut next) = (8i64, 8i64);
    for _ in 0..size {
        if next != 0 {
            next = (last + r.se()? + 256) % 256;
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

fn sps_size(rbsp: &[u8]) -> Option<(u32, u32)> {
    let mut r = Bits { b: rbsp, pos: 0 };
    let profile_idc = r.bits(8)?;
    r.bits(16)?; // constraint flags, reserved bits, level_idc
    r.ue()?; // seq_parameter_set_id
    let mut chroma_format_idc = 1;
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format_idc = r.ue()?;
        if chroma_format_idc == 3 {
            r.bit()?; // separate_colour_plane_flag
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.bit()?; // qpprime_y_zero_transform_bypass_flag
        if r.bit()? == 1 {
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for i in 0..lists {
                if r.bit()? == 1 {
                    skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    match r.ue()? {
        0 => {
            r.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            r.bit()?; // delta_pic_order_always_zero_flag
            r.se()?; // offset_for_non_ref_pic
            r.se()?; // offset_for_top_to_bottom_field
            let n = r.ue()?;
            if n > 255 {
                return None;
            }
            for _ in 0..n {
                r.se()?;
            }
        }
        _ => {}
    }
    r.ue()?; // max_num_ref_frames
    r.bit()?; // gaps_in_frame_num_value_allowed_flag
    let w_mbs = u64::from(r.ue()?) + 1;
    let h_units = u64::from(r.ue()?) + 1;
    let frame_mbs_only = u64::from(r.bit()?);
    if frame_mbs_only == 0 {
        r.bit()?; // mb_adaptive_frame_field_flag
    }
    r.bit()?; // direct_8x8_inference_flag
    let (mut l, mut rt, mut t, mut b) = (0u64, 0u64, 0u64, 0u64);
    if r.bit()? == 1 {
        l = u64::from(r.ue()?);
        rt = u64::from(r.ue()?);
        t = u64::from(r.ue()?);
        b = u64::from(r.ue()?);
    }
    let field = 2 - frame_mbs_only;
    let (cx, cy) = match chroma_format_idc {
        0 => (1, field),
        1 => (2, 2 * field),
        2 => (2, field),
        _ => (1, field),
    };
    let w = (w_mbs * 16).checked_sub(cx * (l + rt))?;
    let h = (field * h_units * 16).checked_sub(cy * (t + b))?;
    let (w, h) = (u32::try_from(w).ok()?, u32::try_from(h).ok()?);
    (w > 0 && h > 0).then_some((w, h))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    // The fixture bitstreams live in Sew's source tree, not in the published package (several
    // megabytes of video), so these run only with `testing` in Sew's tree (`sew_fixtures`,
    // build.rs); the package's own `cargo test` skips them.
    #[cfg(feature = "testing")]
    #[cfg(sew_fixtures)]
    mod on_the_fixtures {
        use std::path::PathBuf;

        use super::*;
        use crate::video::container::{h264_access_units, ivf_frames};

        fn fixture(name: &str) -> Vec<u8> {
            let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../sew-media-testclient/fixtures")
                .join(name);
            std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
        }

        const SIZES: [(&str, (u32, u32)); 3] =
            [("q", (320, 180)), ("h", (640, 360)), ("f", (1280, 720))];

        #[test]
        fn vp8_keyframes_carry_their_size_and_delta_frames_none() {
            for (rid, size) in SIZES {
                let frames = ivf_frames(&fixture(&format!("vp8_sim_{rid}.ivf"))).unwrap();
                let mut keys = 0;
                for f in &frames {
                    match vp8(f) {
                        Some(s) => {
                            keys += 1;
                            assert_eq!(s, size, "vp8 {rid}");
                        }
                        None => assert_eq!(f[0] & 1, 1, "only delta frames have no size"),
                    }
                }
                assert!(keys >= 10, "vp8 {rid}: {keys} keyframes");
            }
        }

        #[test]
        fn h264_idrs_carry_the_sps_size() {
            for (rid, size) in SIZES {
                let aus = h264_access_units(&fixture(&format!("h264_sim_{rid}.h264")));
                let mut with_sps = 0;
                for (idr, au) in &aus {
                    if let Some(s) = h264(au) {
                        with_sps += 1;
                        assert_eq!(s, size, "h264 {rid}");
                    } else {
                        assert!(!idr, "an IDR without an SPS size");
                    }
                }
                assert!(with_sps >= 10, "h264 {rid}: {with_sps}");
            }
        }
    }

    #[test]
    fn emulation_prevention_bytes_are_removed() {
        assert_eq!(
            unescape(&[0, 0, 3, 1, 0, 0, 3, 0, 3]),
            vec![0, 0, 1, 0, 0, 0, 3]
        );
    }

    #[test]
    fn garbage_has_no_size() {
        assert_eq!(vp8(&[0x10, 0, 0, 1, 2, 3, 4, 5, 6, 7]), None);
        assert_eq!(vp8(&[0x00; 4]), None);
        assert_eq!(h264(&[0, 0, 0, 1, 0x67]), None);
        assert_eq!(h264(&[0, 0, 0, 1, 0x65, 0x88]), None);
    }
}
