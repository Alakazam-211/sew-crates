//! H.264 over RTP (RFC 6184), packetization mode 1: a slice goes as a single NAL unit packet, or
//! as FU-A fragments above the MTU; the non-slice NAL units before a slice (SPS, PPS, SEI) go
//! together in one STAP-A. The depacketiser rebuilds the access unit with a 4-byte start code
//! before every NAL unit, whatever the sender's framing was.
//!
//! These work on the bytes as they travel, which under E2EE is the encrypted frame: its NAL
//! headers are in the clear and its ciphertext is escaped so that it holds no start code.

use super::dd::DdWriter;
use super::{Frame, PacketOut, RtpError, RtpIn};

/// The Annex B start code Sew writes.
pub(crate) const START_CODE: [u8; 4] = [0, 0, 0, 1];

const STAP_A: u8 = 24;
const FU_A: u8 = 28;
const IDR: u8 = 5;

/// The `nal_unit_type` of a NAL unit (its first byte's low 5 bits).
pub(crate) fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map_or(0, |b| b & 0x1f)
}

fn is_slice(t: u8) -> bool {
    (1..=5).contains(&t)
}

/// The NAL units of an Annex B byte stream (start codes removed; bytes before the first start
/// code ignored). A start code is `00 00 01`, or `00 00 00 01`, whose first zero is not part
/// of the NAL unit before it. A NAL unit runs to the next start code, so the last one keeps
/// every byte to the end (an E2EE trailer may end in a zero byte).
pub(crate) fn nal_units(b: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= b.len() {
        if b[i] == 0 && b[i + 1] == 0 && b[i + 2] == 1 {
            starts.push(i);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &s) in starts.iter().enumerate() {
        let begin = s + 3;
        let end = match starts.get(k + 1) {
            Some(&next) if next > begin && b[next - 1] == 0 => next - 1,
            Some(&next) => next,
            None => b.len(),
        };
        if end > begin {
            out.push(&b[begin..end]);
        }
    }
    out
}

/// Whether the access unit holds an IDR slice.
pub(crate) fn is_idr(au: &[u8]) -> bool {
    nal_units(au).iter().any(|n| nal_type(n) == IDR)
}

/// The RTP payloads of one access unit, in order (mode 1, at most `mtu` bytes each).
pub(crate) fn packetize(au: &[u8], mtu: usize) -> Result<Vec<Vec<u8>>, RtpError> {
    let nals = nal_units(au);
    if nals.is_empty() {
        return Err(RtpError::Malformed(
            "an H.264 access unit without NAL units",
        ));
    }
    let mut out = Vec::new();
    let mut agg: Vec<&[u8]> = Vec::new();
    for nal in nals {
        let t = nal_type(nal);
        if !is_slice(t) {
            // STAP-A: 1 header byte, then a 2-byte size before each NAL unit.
            if nal.len() + 3 > mtu {
                return Err(RtpError::Malformed(
                    "an H.264 parameter set larger than the MTU",
                ));
            }
            let size = 1 + agg.iter().map(|n| n.len() + 2).sum::<usize>() + nal.len() + 2;
            if size > mtu {
                flush(&mut agg, &mut out);
            }
            agg.push(nal);
            continue;
        }
        flush(&mut agg, &mut out);
        if nal.len() <= mtu {
            out.push(nal.to_vec());
            continue;
        }
        // FU-A: the indicator keeps F and NRI with type 28; the header carries S, E and the type.
        let indicator = (nal[0] & 0xe0) | FU_A;
        let body = &nal[1..];
        let room = mtu.saturating_sub(2).max(1);
        let n = body.len().div_ceil(room);
        for (k, chunk) in body.chunks(room).enumerate() {
            let header = (u8::from(k == 0) << 7) | (u8::from(k + 1 == n) << 6) | t;
            let mut p = Vec::with_capacity(chunk.len() + 2);
            p.push(indicator);
            p.push(header);
            p.extend_from_slice(chunk);
            out.push(p);
        }
    }
    flush(&mut agg, &mut out);
    Ok(out)
}

fn flush(agg: &mut Vec<&[u8]>, out: &mut Vec<Vec<u8>>) {
    match agg.len() {
        0 => {}
        1 => out.push(agg[0].to_vec()),
        _ => {
            let nri = agg.iter().map(|n| n[0] & 0x60).max().unwrap_or(0);
            let mut p = vec![nri | STAP_A];
            for n in agg.iter() {
                p.extend_from_slice(&(n.len() as u16).to_be_bytes());
                p.extend_from_slice(n);
            }
            out.push(p);
        }
    }
    agg.clear();
}

/// The send side of one H.264 stream (one simulcast layer): payloads, the marker bit on the
/// last packet of each access unit, and a Dependency Descriptor on every packet.
#[derive(Debug)]
pub(crate) struct H264Sender {
    mtu: usize,
    dd: DdWriter,
}

impl H264Sender {
    /// `first_frame_number`: the DD frame number of the first frame (random in production).
    pub(crate) fn new(mtu: usize, first_frame_number: u16, size: (u16, u16)) -> H264Sender {
        H264Sender {
            mtu,
            dd: DdWriter::new(first_frame_number, 1, size),
        }
    }

    /// A new coded size (sent in the DD structure of the next keyframe).
    pub(crate) fn set_size(&mut self, size: (u16, u16)) {
        self.dd.set_size(size);
    }

    /// The packets of one access unit (already normalised and, in an E2EE call, encrypted).
    pub(crate) fn packetize(
        &mut self,
        au: &[u8],
        keyframe: bool,
    ) -> Result<Vec<PacketOut>, RtpError> {
        let payloads = packetize(au, self.mtu)?;
        let dds = self.dd.frame(keyframe, 0, payloads.len());
        let n = payloads.len();
        Ok(payloads
            .into_iter()
            .zip(dds)
            .enumerate()
            .map(|(k, (payload, dd))| PacketOut {
                payload,
                marker: k + 1 == n,
                dd,
            })
            .collect())
    }
}

/// Rebuilds access units from one stream's packets, in sequence order (the reorder buffer is
/// in front of it). A sequence gap inside an access unit, or before one (H.264 has no
/// start-of-frame flag), drops it; [`H264Depacketizer::dropped`] counts them.
#[derive(Debug, Default)]
pub(crate) struct H264Depacketizer {
    cur: Vec<u8>,
    cur_ts: Option<u32>,
    last_seq: Option<u16>,
    broken: bool,
    in_fu: bool,
    dropped: u64,
}

impl H264Depacketizer {
    pub(crate) fn new() -> H264Depacketizer {
        H264Depacketizer::default()
    }

    /// Access units dropped because a packet was missing or malformed.
    pub(crate) fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Feeds one packet; returns the access unit it completed (on the marker bit).
    pub(crate) fn push(&mut self, p: RtpIn<'_>) -> Option<Frame> {
        let gap = self.last_seq.is_some_and(|l| p.seq != l.wrapping_add(1));
        self.last_seq = Some(p.seq);
        if self.cur_ts != Some(p.timestamp) {
            if self.cur_ts.is_some() {
                // The previous unit never saw its marker.
                self.dropped += 1;
            }
            self.cur.clear();
            self.cur_ts = Some(p.timestamp);
            self.broken = gap;
            self.in_fu = false;
        } else if gap {
            self.broken = true;
        }
        if self.append(p.payload).is_err() {
            self.broken = true;
        }
        if !p.marker {
            return None;
        }
        let ts = self.cur_ts.take()?;
        let data = std::mem::take(&mut self.cur);
        if std::mem::take(&mut self.broken) || self.in_fu || data.is_empty() {
            self.dropped += 1;
            return None;
        }
        Some(Frame {
            timestamp: ts,
            keyframe: is_idr(&data),
            data,
            vp8: None,
        })
    }

    fn append(&mut self, pl: &[u8]) -> Result<(), RtpError> {
        let t = nal_type(pl);
        if pl.is_empty() {
            return Err(RtpError::Malformed("an empty H.264 payload"));
        }
        match t {
            STAP_A => {
                let mut pos = 1;
                while pos < pl.len() {
                    let len_bytes = pl
                        .get(pos..pos + 2)
                        .ok_or(RtpError::Malformed("a truncated STAP-A"))?;
                    let n = usize::from(u16::from_be_bytes([len_bytes[0], len_bytes[1]]));
                    pos += 2;
                    let nal = pl
                        .get(pos..pos + n)
                        .filter(|nal| !nal.is_empty())
                        .ok_or(RtpError::Malformed("a truncated STAP-A"))?;
                    self.cur.extend_from_slice(&START_CODE);
                    self.cur.extend_from_slice(nal);
                    pos += n;
                }
            }
            FU_A => {
                let h = *pl.get(1).ok_or(RtpError::Malformed("a short FU-A"))?;
                let start = h & 0x80 != 0;
                let end = h & 0x40 != 0;
                if start {
                    if self.in_fu {
                        return Err(RtpError::Malformed("an FU-A start inside a fragment"));
                    }
                    self.cur.extend_from_slice(&START_CODE);
                    self.cur.push((pl[0] & 0xe0) | (h & 0x1f));
                    self.in_fu = true;
                } else if !self.in_fu {
                    return Err(RtpError::Malformed("an FU-A fragment without its start"));
                }
                self.cur.extend_from_slice(&pl[2..]);
                if end {
                    self.in_fu = false;
                }
            }
            1..=23 => {
                if self.in_fu {
                    return Err(RtpError::Malformed("a NAL unit inside an FU-A"));
                }
                self.cur.extend_from_slice(&START_CODE);
                self.cur.extend_from_slice(pl);
            }
            _ => return Err(RtpError::Malformed("an H.264 packet type outside mode 1")),
        }
        Ok(())
    }
}
