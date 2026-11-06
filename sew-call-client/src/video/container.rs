//! Readers for the pre-encoded fixture files the tests and `FixtureBackend` replay: IVF (VP8)
//! and Annex B (H.264).

use super::rtp::h264::{START_CODE, nal_type, nal_units};

/// The frames of an IVF file (32-byte file header, then a 12-byte header before each frame:
/// the size, little-endian, and the timestamp).
pub(crate) fn ivf_frames(b: &[u8]) -> Result<Vec<Vec<u8>>, String> {
    if b.len() < 32 || &b[..4] != b"DKIF" {
        return Err("not an IVF file".into());
    }
    let header = usize::from(u16::from_le_bytes([b[6], b[7]]));
    let mut pos = header.max(32);
    let mut out = Vec::new();
    while pos < b.len() {
        let h = b
            .get(pos..pos + 12)
            .ok_or_else(|| format!("a truncated IVF frame header at {pos}"))?;
        let size = u32::from_le_bytes([h[0], h[1], h[2], h[3]]) as usize;
        pos += 12;
        let frame = b
            .get(pos..pos + size)
            .ok_or_else(|| format!("a truncated IVF frame at {pos}"))?;
        out.push(frame.to_vec());
        pos += size;
    }
    Ok(out)
}

/// The access units of an H.264 Annex B stream with one slice per picture (as the fixtures
/// are encoded): each slice closes an access unit, AUD NAL units are dropped, and every NAL
/// unit gets a 4-byte start code. Each unit comes with whether it holds an IDR slice.
pub(crate) fn h264_access_units(b: &[u8]) -> Vec<(bool, Vec<u8>)> {
    let mut out = Vec::new();
    let mut cur = Vec::new();
    let mut idr = false;
    for nal in nal_units(b) {
        let t = nal_type(nal);
        if t == 9 {
            continue;
        }
        cur.extend_from_slice(&START_CODE);
        cur.extend_from_slice(nal);
        idr |= t == 5;
        if t == 1 || t == 5 {
            out.push((idr, std::mem::take(&mut cur)));
            idr = false;
        }
    }
    out
}
