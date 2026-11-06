//! VP8 over RTP (RFC 7741).
//!
//! Send: every packet carries the extended descriptor with a 15-bit PictureID (one per frame,
//! wrapping), TL0PICIDX (incremented on each base-layer frame) and the T field (TID and Y),
//! which is written even without temporal layers (TID 0, Y 1), so the SFU's VP8 selector
//! always sees it. N marks the top layer's frames when there are temporal layers.
//!
//! Receive: the descriptor's fields are read from the first packet of each frame; the frame is
//! the concatenation of the packets' payloads after their descriptors.

use super::dd::DdWriter;
use super::{Frame, PacketOut, RtpError, RtpIn};

/// Descriptor bytes the sender writes before each packet's part of the frame.
const SEND_DESCRIPTOR: usize = 6;

/// Where a frame sits in the temporal structure (from the encoder).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FrameLayer {
    pub(crate) keyframe: bool,
    pub(crate) temporal_id: u8,
    /// RFC 7741's Y bit: the frame depends only on the base layer, or is a keyframe.
    pub(crate) layer_sync: bool,
}

/// The first values of a stream's counters (random in production).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Vp8Starts {
    pub(crate) picture_id: u16,
    pub(crate) tl0_pic_idx: u8,
    pub(crate) frame_number: u16,
}

/// The send side of one VP8 stream (one simulcast layer), with its own counters.
#[derive(Debug)]
pub(crate) struct Vp8Sender {
    mtu: usize,
    temporal_layers: u8,
    picture_id: u16,
    tl0: u8,
    dd: DdWriter,
}

impl Vp8Sender {
    pub(crate) fn new(
        mtu: usize,
        temporal_layers: u8,
        starts: Vp8Starts,
        size: (u16, u16),
    ) -> Vp8Sender {
        let temporal_layers = temporal_layers.clamp(1, 3);
        Vp8Sender {
            mtu,
            temporal_layers,
            picture_id: starts.picture_id & 0x7fff,
            // Incremented before the first base frame is written.
            tl0: starts.tl0_pic_idx.wrapping_sub(1),
            dd: DdWriter::new(starts.frame_number, temporal_layers, size),
        }
    }

    /// A new coded size (sent in the DD structure of the next keyframe).
    pub(crate) fn set_size(&mut self, size: (u16, u16)) {
        self.dd.set_size(size);
    }

    /// The packets of one frame (in an E2EE call, the encrypted frame).
    pub(crate) fn packetize(&mut self, frame: &[u8], layer: FrameLayer) -> Vec<PacketOut> {
        let (tid, sync) = if self.temporal_layers == 1 || layer.keyframe {
            (0, true)
        } else {
            (
                layer.temporal_id.min(self.temporal_layers - 1),
                layer.layer_sync,
            )
        };
        if tid == 0 {
            self.tl0 = self.tl0.wrapping_add(1);
        }
        let non_reference = self.temporal_layers > 1 && tid == self.temporal_layers - 1;
        let room = self.mtu.saturating_sub(SEND_DESCRIPTOR).max(1);
        let chunks: Vec<&[u8]> = if frame.is_empty() {
            vec![frame]
        } else {
            frame.chunks(room).collect()
        };
        let dds = self.dd.frame(layer.keyframe, tid, chunks.len());
        let n = chunks.len();
        let out = chunks
            .into_iter()
            .zip(dds)
            .enumerate()
            .map(|(k, (chunk, dd))| {
                let mut p = Vec::with_capacity(chunk.len() + SEND_DESCRIPTOR);
                // X, N, S (first packet), PID 0; then I, L, T; the M bit and the 15-bit id.
                p.push(0x80 | (u8::from(non_reference) << 5) | (u8::from(k == 0) << 4));
                p.push(0x80 | 0x40 | 0x20);
                p.push(0x80 | (self.picture_id >> 8) as u8);
                p.push(self.picture_id as u8);
                p.push(self.tl0);
                p.push((tid << 6) | (u8::from(sync) << 5));
                p.extend_from_slice(chunk);
                PacketOut {
                    payload: p,
                    marker: k + 1 == n,
                    dd,
                }
            })
            .collect();
        self.picture_id = (self.picture_id + 1) & 0x7fff;
        out
    }
}

/// The descriptor fields of one VP8 frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Vp8Info {
    /// 7- or 15-bit PictureID, when present.
    pub(crate) picture_id: Option<u16>,
    /// TL0PICIDX, when present.
    pub(crate) tl0_pic_idx: Option<u8>,
    /// TID, when the T field is present.
    pub(crate) temporal_id: Option<u8>,
    /// The Y bit (layer sync), when the T field is present.
    pub(crate) layer_sync: bool,
    /// N: the frame is not used for reference.
    pub(crate) non_reference: bool,
}

/// One packet's payload descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Descriptor {
    /// Bytes before the VP8 payload.
    pub(crate) len: usize,
    /// S: the packet starts a partition.
    pub(crate) start: bool,
    /// PID: the partition index.
    pub(crate) partition: u8,
    pub(crate) info: Vp8Info,
}

/// Parses an RFC 7741 payload descriptor.
pub(crate) fn descriptor(p: &[u8]) -> Result<Descriptor, RtpError> {
    let short = RtpError::Malformed("a short VP8 payload descriptor");
    let b0 = *p.first().ok_or(short.clone())?;
    let mut info = Vp8Info {
        non_reference: b0 & 0x20 != 0,
        ..Vp8Info::default()
    };
    let mut pos = 1;
    if b0 & 0x80 != 0 {
        let x = *p.get(pos).ok_or(short.clone())?;
        pos += 1;
        let (i, l, t, k) = (x & 0x80 != 0, x & 0x40 != 0, x & 0x20 != 0, x & 0x10 != 0);
        if l && !t {
            return Err(RtpError::Malformed("VP8 TL0PICIDX without the T field"));
        }
        if i {
            let m = *p.get(pos).ok_or(short.clone())?;
            pos += 1;
            if m & 0x80 != 0 {
                let lo = *p.get(pos).ok_or(short.clone())?;
                pos += 1;
                info.picture_id = Some((u16::from(m & 0x7f) << 8) | u16::from(lo));
            } else {
                info.picture_id = Some(u16::from(m));
            }
        }
        if l {
            info.tl0_pic_idx = Some(*p.get(pos).ok_or(short.clone())?);
            pos += 1;
        }
        if t || k {
            let b = *p.get(pos).ok_or(short.clone())?;
            pos += 1;
            if t {
                info.temporal_id = Some(b >> 6);
                info.layer_sync = b & 0x20 != 0;
            }
        }
    }
    if pos >= p.len() {
        return Err(short);
    }
    Ok(Descriptor {
        len: pos,
        start: b0 & 0x10 != 0,
        partition: b0 & 0x0f,
        info,
    })
}

/// Whether a VP8 frame is a keyframe: the frame tag's P bit is 0. Under E2EE the first bytes
/// of every frame stay in the clear, so this reads the same before and after decryption.
pub(crate) fn is_keyframe(frame: &[u8]) -> bool {
    frame.first().is_some_and(|b| b & 0x01 == 0)
}

/// Rebuilds VP8 frames from one stream's packets, in sequence order. A frame starts at a
/// packet with S = 1 and PID = 0 and ends at the marker bit; a sequence gap inside a frame, or
/// a frame first seen on a packet that does not start it, drops the frame
/// ([`Vp8Depacketizer::dropped`]).
#[derive(Debug, Default)]
pub(crate) struct Vp8Depacketizer {
    cur: Vec<u8>,
    cur_ts: Option<u32>,
    info: Vp8Info,
    last_seq: Option<u16>,
    broken: bool,
    dropped: u64,
}

impl Vp8Depacketizer {
    pub(crate) fn new() -> Vp8Depacketizer {
        Vp8Depacketizer::default()
    }

    /// Frames dropped because a packet was missing or malformed.
    pub(crate) fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Feeds one packet; returns the frame it completed (on the marker bit).
    pub(crate) fn push(&mut self, p: RtpIn<'_>) -> Option<Frame> {
        let gap = self.last_seq.is_some_and(|l| p.seq != l.wrapping_add(1));
        self.last_seq = Some(p.seq);
        let d = descriptor(p.payload);
        let starts = d.as_ref().is_ok_and(|d| d.start && d.partition == 0);
        if starts || self.cur_ts != Some(p.timestamp) {
            if self.cur_ts.is_some() {
                // The previous frame never saw its marker.
                self.dropped += 1;
            }
            self.cur.clear();
            self.cur_ts = Some(p.timestamp);
            self.broken = !starts;
            if let Ok(d) = &d {
                self.info = d.info;
            }
        } else if gap {
            self.broken = true;
        }
        match &d {
            Ok(d) => self.cur.extend_from_slice(&p.payload[d.len..]),
            Err(_) => self.broken = true,
        }
        if !p.marker {
            return None;
        }
        let ts = self.cur_ts.take()?;
        let data = std::mem::take(&mut self.cur);
        if std::mem::take(&mut self.broken) || data.is_empty() {
            self.dropped += 1;
            return None;
        }
        Some(Frame {
            timestamp: ts,
            keyframe: is_keyframe(&data),
            data,
            vp8: Some(self.info),
        })
    }
}
