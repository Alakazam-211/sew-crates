//! Round trips on the real fixture bitstreams, with str0m's own VP8 and H.264 depacketisers
//! (and packetisers) as an independent implementation on the other side.

use std::path::PathBuf;

use sha2::{Digest, Sha256};
use str0m::format::CodecExtra;
use str0m::unversioned as s0;
use str0m::unversioned::Packetizer as _;

use super::dd::reader;
use super::h264::{self, H264Depacketizer, H264Sender, nal_type, nal_units};
use super::normalise::{check, normalise};
use super::vp8::{self, FrameLayer, Vp8Depacketizer, Vp8Sender, Vp8Starts};
use super::{MTU, PacketOut, RtpIn};
use crate::video::container::{h264_access_units, ivf_frames};

fn fixture(name: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../sew-media-testclient/fixtures")
        .join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// The files and their sha256 (the testclient's MANIFEST.toml).
const FILES: [(&str, &str); 6] = [
    (
        "vp8_sim_q.ivf",
        "76bfce1d77724305e9dca2747d237974e64be5d6f2dde43c9734fffe976a0414",
    ),
    (
        "vp8_sim_h.ivf",
        "64f288d4a5304e67e0f2b774010b7a5c45f902a2031e5018120e8279b0a7b83e",
    ),
    (
        "vp8_sim_f.ivf",
        "dc40cdc26eddc2f094c6509fb28630ac7939049ef189e1739dce210ddca03fd4",
    ),
    (
        "h264_sim_q.h264",
        "a10b975b0e65972f88fd145f7a0e246a4de01b90af396d6fc646b7fa34b0160d",
    ),
    (
        "h264_sim_h.h264",
        "0986093ecdd94a0e0cb6633bd1daa5094689bc35c1d5a90b5fdee8d9b5bd5563",
    ),
    (
        "h264_sim_f.h264",
        "b8e9b9065d443e7ce20754e73c3baafb93b3cf18389cf196719d1eb169357c19",
    ),
];

#[test]
fn the_fixtures_are_the_manifest_ones() {
    for (name, sha) in FILES {
        assert_eq!(hex::encode(Sha256::digest(fixture(name))), sha, "{name}");
    }
}

fn vp8_frames(rid: &str) -> Vec<Vec<u8>> {
    let f = ivf_frames(&fixture(&format!("vp8_sim_{rid}.ivf"))).expect("ivf");
    assert_eq!(f.len(), 300, "vp8_sim_{rid}");
    f
}

fn h264_aus(rid: &str) -> Vec<(bool, Vec<u8>)> {
    let a = h264_access_units(&fixture(&format!("h264_sim_{rid}.h264")));
    assert_eq!(a.len(), 300, "h264_sim_{rid}");
    a
}

/// The fixtures' 0-2-1-2 pattern: (temporal id, Y).
fn vp8_layer(unit: usize, keyframe: bool) -> FrameLayer {
    let tid = [0u8, 2, 1, 2][unit % 4];
    FrameLayer {
        keyframe,
        temporal_id: tid,
        layer_sync: keyframe || (tid > 0 && matches!(unit % 4, 1 | 2)),
    }
}

/// Feeds packets with consecutive sequence numbers from `seq`.
fn rtp_in(packets: &[PacketOut], seq: u16, ts: u32) -> Vec<RtpIn<'_>> {
    packets
        .iter()
        .enumerate()
        .map(|(k, p)| RtpIn {
            seq: seq.wrapping_add(k as u16),
            timestamp: ts,
            marker: p.marker,
            payload: &p.payload,
        })
        .collect()
}

fn str0m_depacketize(d: &mut dyn s0::Depacketizer, payloads: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut extra = CodecExtra::None;
    for p in payloads {
        d.depacketize(p, &mut out, &mut extra)
            .expect("str0m depacketizes Sew's packet");
    }
    out
}

#[test]
fn vp8_round_trips_through_sew_and_str0m() {
    for rid in ["q", "h", "f"] {
        let frames = vp8_frames(rid);
        let mut tx = Vp8Sender::new(MTU, 3, Vp8Starts::default(), (320, 180));
        let mut rx = Vp8Depacketizer::new();
        let mut seq = 65_000u16;
        let mut keys = 0;
        for (unit, frame) in frames.iter().enumerate() {
            let key = vp8::is_keyframe(frame);
            keys += usize::from(key);
            let layer = vp8_layer(unit, key);
            let packets = tx.packetize(frame, layer);
            assert!(
                packets.iter().all(|p| p.payload.len() <= MTU),
                "{rid} {unit}: MTU"
            );
            assert!(packets.last().is_some_and(|p| p.marker));
            assert!(packets.iter().rev().skip(1).all(|p| !p.marker));
            let ts = unit as u32 * 3000;
            let mut got = None;
            for p in rtp_in(&packets, seq, ts) {
                got = rx.push(p).or(got);
            }
            seq = seq.wrapping_add(packets.len() as u16);
            let got = got.unwrap_or_else(|| panic!("{rid} {unit}: no frame"));
            assert_eq!(got.data, *frame, "{rid} {unit}");
            assert_eq!(got.keyframe, key);
            let info = got.vp8.expect("vp8 info");
            assert_eq!(info.temporal_id, Some(layer.temporal_id));
            assert_eq!(info.layer_sync, layer.layer_sync);
            assert_eq!(info.non_reference, layer.temporal_id == 2);
            // str0m's depacketiser rebuilds the same frame from the same packets.
            let mut d = s0::Vp8Depacketizer::default();
            let payloads: Vec<&[u8]> = packets.iter().map(|p| &p.payload[..]).collect();
            assert_eq!(
                str0m_depacketize(&mut d, &payloads),
                *frame,
                "{rid} {unit}: str0m"
            );
            assert_eq!(d.t, 1, "the T field");
            assert_eq!(d.tid, layer.temporal_id);
        }
        assert!(keys >= 10, "{rid}: keyframes every 28 frames");
        assert_eq!(rx.dropped(), 0);
    }
}

#[test]
fn str0m_vp8_packets_depacketize_in_sew() {
    let frames = vp8_frames("f");
    let mut p = s0::Vp8Packetizer::default();
    let mut rx = Vp8Depacketizer::new();
    let mut seq = 1u16;
    for (unit, frame) in frames.iter().enumerate() {
        let payloads = p.packetize(MTU, frame).expect("str0m packetizes");
        let n = payloads.len();
        let mut got = None;
        for (k, pl) in payloads.iter().enumerate() {
            got = rx
                .push(RtpIn {
                    seq,
                    timestamp: unit as u32,
                    marker: k + 1 == n,
                    payload: pl,
                })
                .or(got);
            seq = seq.wrapping_add(1);
        }
        assert_eq!(got.expect("frame").data, *frame, "{unit}");
    }
}

#[test]
fn vp8_descriptor_fields() {
    let starts = Vp8Starts {
        picture_id: 0x7ffe,
        tl0_pic_idx: 255,
        frame_number: 9,
    };
    let mut tx = Vp8Sender::new(MTU, 3, starts, (320, 180));
    let frame = [0x10u8, 0x02, 0x00, 0x9d, 0x01, 0x2a, 0, 0, 0, 0];
    let mut seen = Vec::new();
    for unit in 0..6 {
        let layer = vp8_layer(unit, unit == 0);
        let p = tx.packetize(&frame, layer);
        let d = vp8::descriptor(&p[0].payload).expect("descriptor");
        assert!(d.start && d.partition == 0);
        seen.push((d.info.picture_id, d.info.tl0_pic_idx, d.info.temporal_id));
    }
    // PictureID wraps at 15 bits; TL0PICIDX moves on each TID 0 frame only (and wraps).
    assert_eq!(
        seen,
        [
            (Some(0x7ffe), Some(255), Some(0)),
            (Some(0x7fff), Some(255), Some(2)),
            (Some(0), Some(255), Some(1)),
            (Some(1), Some(255), Some(2)),
            (Some(2), Some(0), Some(0)),
            (Some(3), Some(0), Some(2)),
        ]
    );
    // Without temporal layers the T field is still there: TID 0, Y 1, never N.
    let mut tx = Vp8Sender::new(MTU, 1, Vp8Starts::default(), (320, 180));
    for unit in 0..4 {
        let p = tx.packetize(&frame, vp8_layer(unit, false));
        let d = vp8::descriptor(&p[0].payload).expect("descriptor");
        assert_eq!(d.info.temporal_id, Some(0));
        assert!(d.info.layer_sync && !d.info.non_reference);
        assert_eq!(p[0].payload[1], 0xe0, "I L T");
        assert_eq!(
            d.info.tl0_pic_idx,
            Some(unit as u8),
            "a TL0PICIDX per frame"
        );
    }
    // Every packet of a frame carries the descriptor; only the first has S.
    let big = vec![0x11u8; 3000];
    let p = tx.packetize(&big, vp8_layer(1, false));
    assert_eq!(p.len(), 3);
    for (k, pk) in p.iter().enumerate() {
        let d = vp8::descriptor(&pk.payload).expect("descriptor");
        assert_eq!(d.start, k == 0);
        assert_eq!(d.len, 6);
    }
}

#[test]
fn vp8_dd_follows_the_temporal_layers() {
    let frames = vp8_frames("q");
    let mut tx = Vp8Sender::new(MTU, 3, Vp8Starts::default(), (320, 180));
    let mut structure = None;
    for (unit, frame) in frames.iter().enumerate().take(60) {
        if unit == 30 {
            // A new size goes out with the next keyframe's structure (frame 56).
            tx.set_size((640, 360));
        }
        let key = vp8::is_keyframe(frame);
        let packets = tx.packetize(frame, vp8_layer(unit, key));
        for (k, p) in packets.iter().enumerate() {
            let parsed = reader::parse(&p.dd, structure.as_ref());
            assert_eq!(parsed.first, k == 0);
            assert_eq!(parsed.last, k + 1 == packets.len());
            assert_eq!(parsed.temporal, vp8_layer(unit, key).temporal_id);
            assert_eq!(parsed.structure.is_some(), key && k == 0);
            if let Some(s) = parsed.structure {
                let want = if unit < 30 { (320, 180) } else { (640, 360) };
                assert_eq!(s.resolution, Some(want), "{unit}");
                structure = Some(s);
            }
        }
    }
}

#[test]
fn h264_round_trips_through_sew_and_str0m() {
    for rid in ["q", "h", "f"] {
        let aus = h264_aus(rid);
        let mut tx = H264Sender::new(MTU, 100, (2, 2));
        let size = match rid {
            "q" => (320, 180),
            "h" => (640, 360),
            _ => (1280, 720),
        };
        tx.set_size(size);
        let mut rx = H264Depacketizer::new();
        let mut seq = 65_500u16;
        let (mut fua, mut stap) = (0, 0);
        let mut structure = None;
        for (unit, (key, au)) in aus.iter().enumerate() {
            assert_eq!(normalise(au), *au, "the fixture is already normal");
            check(au).expect("SPS and PPS before each IDR");
            assert_eq!(h264::is_idr(au), *key);
            let packets = tx.packetize(au, *key).expect("packetize");
            assert!(packets.iter().all(|p| p.payload.len() <= MTU));
            for (k, p) in packets.iter().enumerate() {
                match nal_type(&p.payload) {
                    28 => fua += 1,
                    24 => {
                        stap += 1;
                        assert!(*key && k == 0, "STAP-A: the keyframe's parameter sets");
                    }
                    t => assert!((1..=23).contains(&t)),
                }
                let parsed = reader::parse(&p.dd, structure.as_ref());
                assert_eq!(parsed.structure.is_some(), *key && k == 0);
                if let Some(s) = parsed.structure {
                    assert_eq!(s.resolution, Some(size));
                    structure = Some(s);
                }
            }
            let mut got = None;
            for p in rtp_in(&packets, seq, unit as u32) {
                got = rx.push(p).or(got);
            }
            seq = seq.wrapping_add(packets.len() as u16);
            let got = got.unwrap_or_else(|| panic!("{rid} {unit}: no frame"));
            assert_eq!(got.data, *au, "{rid} {unit}");
            assert_eq!(got.keyframe, *key);
            let mut d = s0::H264Depacketizer::default();
            let payloads: Vec<&[u8]> = packets.iter().map(|p| &p.payload[..]).collect();
            assert_eq!(
                str0m_depacketize(&mut d, &payloads),
                *au,
                "{rid} {unit}: str0m"
            );
        }
        assert_eq!(stap, 10, "{rid}: one STAP-A per keyframe (every 30)");
        if rid == "f" {
            assert!(fua > 0, "the 720p layer needs FU-A");
        }
        assert_eq!(rx.dropped(), 0);
    }
}

#[test]
fn str0m_h264_packets_depacketize_in_sew() {
    let aus = h264_aus("f");
    let mut p = s0::H264Packetizer::default();
    let mut rx = H264Depacketizer::new();
    let mut seq = 7u16;
    for (unit, (_, au)) in aus.iter().enumerate() {
        let payloads = p.packetize(MTU, au).expect("str0m packetizes");
        let n = payloads.len();
        let mut got = None;
        for (k, pl) in payloads.iter().enumerate() {
            got = rx
                .push(RtpIn {
                    seq,
                    timestamp: unit as u32,
                    marker: k + 1 == n,
                    payload: pl,
                })
                .or(got);
            seq = seq.wrapping_add(1);
        }
        assert_eq!(got.expect("access unit").data, *au, "{unit}");
    }
}

#[test]
fn h264_depacketizer_writes_four_byte_start_codes() {
    // An access unit with 3-byte start codes goes out as is (not normalised) and comes back
    // with 4-byte codes: what the normaliser would have made of it.
    let au = [
        0, 0, 1, 0x67, 0x42, 0xe0, 0x1f, 0, 0, 1, 0x68, 0xce, 0x3c, 0x80, 0, 0, 1, 0x65, 0x88,
        0x84, 0x21,
    ];
    let packets = H264Sender::new(MTU, 0, (2, 2))
        .packetize(&au, true)
        .expect("packetize");
    let mut rx = H264Depacketizer::new();
    let got = rtp_in(&packets, 0, 0)
        .into_iter()
        .find_map(|p| rx.push(p))
        .expect("access unit");
    assert_eq!(got.data, normalise(&au));
    assert_ne!(got.data, au);
    assert_eq!(nal_units(&got.data).len(), 3);
}

#[test]
fn a_lost_packet_drops_only_its_frame() {
    let aus = h264_aus("f");
    let (key, au) = &aus[0];
    let mut tx = H264Sender::new(MTU, 0, (1280, 720));
    let first = tx.packetize(au, *key).expect("packetize");
    assert!(first.len() > 3);
    let mut rx = H264Depacketizer::new();
    let mut ins = rtp_in(&first, 10, 0);
    ins.remove(2);
    assert!(ins.into_iter().all(|p| rx.push(p).is_none()));
    assert_eq!(rx.dropped(), 1);
    let (_, next) = &aus[1];
    let second = tx.packetize(next, false).expect("packetize");
    let got = rtp_in(&second, 10 + first.len() as u16, 1)
        .into_iter()
        .find_map(|p| rx.push(p))
        .expect("the next unit");
    assert_eq!(got.data, *next);

    let frames = vp8_frames("f");
    let mut tx = Vp8Sender::new(MTU, 3, Vp8Starts::default(), (1280, 720));
    let a = tx.packetize(&frames[0], vp8_layer(0, true));
    let b = tx.packetize(&frames[1], vp8_layer(1, false));
    assert!(a.len() > 2);
    let mut rx = Vp8Depacketizer::new();
    let mut ins = rtp_in(&a, 0, 0);
    ins.remove(1);
    assert!(ins.into_iter().all(|p| rx.push(p).is_none()));
    let got = rtp_in(&b, a.len() as u16, 1)
        .into_iter()
        .find_map(|p| rx.push(p));
    assert_eq!(got.expect("next frame").data, frames[1]);
    assert_eq!(rx.dropped(), 1);
    // A frame first seen after its first packet is lost is dropped too.
    let c = tx.packetize(&frames[2], vp8_layer(2, false));
    if c.len() > 1 {
        let start = a.len() as u16 + b.len() as u16 + 1;
        assert!(
            rtp_in(&c[1..], start, 2)
                .into_iter()
                .all(|p| rx.push(p).is_none())
        );
    }
}
