//! The Dependency Descriptor RTP header extension (the AV1 RTP specification, appendix A),
//! written for one simulcast layer: one spatial layer and 1 to 3 temporal layers (L1T1, L1T2,
//! L1T3), one decode target per temporal layer, one chain over the base-layer frames.
//!
//! The structure is attached to the first packet of every keyframe. Each frame names the
//! template of its temporal layer; when its real distances (to the frame it depends on, and to
//! the last base-layer frame) differ from the template's, as after a frame the encoder skipped,
//! the packet carries them as custom frame diffs and chain diffs.

/// Decode-target indications (2 bits).
mod dti {
    pub(super) const NOT_PRESENT: u8 = 0;
    pub(super) const DISCARDABLE: u8 = 1;
    pub(super) const SWITCH: u8 = 2;
    pub(super) const REQUIRED: u8 = 3;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Template {
    pub(crate) temporal: u8,
    pub(crate) key: bool,
    pub(crate) dtis: Vec<u8>,
    pub(crate) fdiffs: Vec<u16>,
    pub(crate) chain_diff: u8,
}

fn dti_of(key: bool, t_frame: u8, t_dt: u8) -> u8 {
    if t_frame > t_dt {
        dti::NOT_PRESENT
    } else if key || t_frame == 0 {
        dti::SWITCH
    } else if t_frame == t_dt {
        dti::DISCARDABLE
    } else {
        dti::REQUIRED
    }
}

fn template(layers: u8, key: bool, temporal: u8, fdiff: u16, chain_diff: u8) -> Template {
    Template {
        temporal,
        key,
        dtis: (0..layers).map(|td| dti_of(key, temporal, td)).collect(),
        fdiffs: if key { Vec::new() } else { vec![fdiff] },
        chain_diff,
    }
}

/// The templates of an L1Tn structure, in temporal order: the keyframe, then each layer in the
/// usual WebRTC pattern (L1T2: 0 1 0 1; L1T3: 0 2 1 2), with the T2 frames after a T0 and after
/// a T1 as two templates (their chain diffs differ).
pub(crate) fn templates(layers: u8) -> Vec<Template> {
    let n = layers.clamp(1, 3);
    let mut t = vec![template(n, true, 0, 0, 0)];
    match n {
        1 => t.push(template(n, false, 0, 1, 1)),
        2 => {
            t.push(template(n, false, 0, 2, 2));
            t.push(template(n, false, 1, 1, 1));
        }
        _ => {
            t.push(template(n, false, 0, 4, 4));
            t.push(template(n, false, 1, 2, 2));
            t.push(template(n, false, 2, 1, 1));
            t.push(template(n, false, 2, 1, 3));
        }
    }
    t
}

/// The DD writer of one stream.
#[derive(Debug)]
pub(crate) struct DdWriter {
    layers: u8,
    templates: Vec<Template>,
    size: (u16, u16),
    frame_number: u16,
    /// The frame number of the latest frame of each temporal layer.
    last: [Option<u16>; 3],
}

impl DdWriter {
    /// One stream with `temporal_layers` (1 to 3) at `size`.
    pub(crate) fn new(first_frame_number: u16, temporal_layers: u8, size: (u16, u16)) -> DdWriter {
        let layers = temporal_layers.clamp(1, 3);
        DdWriter {
            layers,
            templates: templates(layers),
            size,
            frame_number: first_frame_number,
            last: [None; 3],
        }
    }

    pub(crate) fn set_size(&mut self, size: (u16, u16)) {
        self.size = size;
    }

    /// The DD elements of one frame's `packets` packets. Advances the frame number.
    pub(crate) fn frame(
        &mut self,
        keyframe: bool,
        temporal_id: u8,
        packets: usize,
    ) -> Vec<Vec<u8>> {
        let fnum = self.frame_number;
        self.frame_number = fnum.wrapping_add(1);
        let t = if keyframe {
            0
        } else {
            temporal_id.min(self.layers - 1)
        };
        if keyframe {
            self.last = [None; 3];
        }
        // What it depends on: the latest frame of a lower layer, or the last base frame.
        let reference = if t == 0 {
            self.last[0]
        } else {
            self.last[..usize::from(t)]
                .iter()
                .flatten()
                .copied()
                .max_by_key(|&f| {
                    // The most recent one: the smallest distance back.
                    std::cmp::Reverse(fnum.wrapping_sub(f))
                })
        };
        let fdiffs: Vec<u16> = match (keyframe, reference) {
            (true, _) | (false, None) => Vec::new(),
            (false, Some(r)) => vec![fnum.wrapping_sub(r)],
        };
        let chain = if keyframe {
            0
        } else {
            self.last[0].map_or(0, |b| fnum.wrapping_sub(b).min(255) as u8)
        };
        self.last[usize::from(t)] = Some(fnum);
        let pick = self
            .templates
            .iter()
            .position(|tp| {
                tp.key == keyframe
                    && tp.temporal == t
                    && tp.fdiffs == fdiffs
                    && tp.chain_diff == chain
            })
            .or_else(|| {
                self.templates
                    .iter()
                    .position(|tp| tp.key == keyframe && tp.temporal == t)
            })
            .unwrap_or(0);
        let tp = &self.templates[pick];
        let custom_fdiffs = tp.fdiffs != fdiffs;
        let custom_chain = tp.chain_diff != chain;
        (0..packets)
            .map(|k| {
                let element = Element {
                    first: k == 0,
                    last: k + 1 == packets,
                    template: pick as u8,
                    frame_number: fnum,
                    structure: (keyframe && k == 0).then_some((
                        self.layers,
                        &self.templates[..],
                        self.size,
                    )),
                    fdiffs: custom_fdiffs.then_some(&fdiffs[..]),
                    chain: custom_chain.then_some(chain),
                };
                element.write()
            })
            .collect()
    }
}

struct Element<'a> {
    first: bool,
    last: bool,
    template: u8,
    frame_number: u16,
    structure: Option<(u8, &'a [Template], (u16, u16))>,
    fdiffs: Option<&'a [u16]>,
    chain: Option<u8>,
}

impl Element<'_> {
    fn write(&self) -> Vec<u8> {
        let mut b = Bits::default();
        b.put(u64::from(self.first), 1);
        b.put(u64::from(self.last), 1);
        b.put(u64::from(self.template), 6); // template_id_offset is 0
        b.put(u64::from(self.frame_number), 16);
        if self.structure.is_none() && self.fdiffs.is_none() && self.chain.is_none() {
            return b.out;
        }
        b.put(u64::from(self.structure.is_some()), 1);
        b.put(0, 1); // active_decode_targets_present_flag
        b.put(0, 1); // custom_dtis_flag
        b.put(u64::from(self.fdiffs.is_some()), 1);
        b.put(u64::from(self.chain.is_some()), 1);
        if let Some((layers, templates, size)) = self.structure {
            write_structure(&mut b, layers, templates, size);
        }
        if let Some(fdiffs) = self.fdiffs {
            for &f in fdiffs {
                let minus_one = u64::from(f.saturating_sub(1));
                let size = if minus_one < 16 {
                    1
                } else if minus_one < 256 {
                    2
                } else {
                    3
                };
                b.put(size, 2);
                b.put(minus_one, 4 * size as usize);
            }
            b.put(0, 2);
        }
        if let Some(c) = self.chain {
            b.put(u64::from(c), 8);
        }
        b.out
    }
}

fn write_structure(b: &mut Bits, layers: u8, templates: &[Template], size: (u16, u16)) {
    b.put(0, 6); // template_id_offset
    b.put(u64::from(layers - 1), 5); // dt_cnt_minus_one
    for (i, t) in templates.iter().enumerate() {
        let idc = match templates.get(i + 1) {
            None => 3,
            Some(n) if n.temporal == t.temporal => 0,
            Some(_) => 1,
        };
        b.put(idc, 2);
    }
    for t in templates {
        for &d in &t.dtis {
            b.put(u64::from(d), 2);
        }
    }
    for t in templates {
        for &f in &t.fdiffs {
            b.put(1, 1);
            b.put(u64::from(f - 1), 4);
        }
        b.put(0, 1);
    }
    // One chain (ns(DtCnt + 1)); every decode target protected by it (ns(1) writes nothing).
    b.ns(1, u32::from(layers) + 1);
    for t in templates {
        b.put(u64::from(t.chain_diff), 4);
    }
    b.put(1, 1); // resolutions_present_flag
    b.put(u64::from(size.0.max(1) - 1), 16);
    b.put(u64::from(size.1.max(1) - 1), 16);
}

/// A bit writer, most significant bit first.
#[derive(Debug, Default)]
struct Bits {
    out: Vec<u8>,
    n: usize,
}

impl Bits {
    fn put(&mut self, v: u64, bits: usize) {
        for i in (0..bits).rev() {
            if self.n.is_multiple_of(8) {
                self.out.push(0);
            }
            if (v >> i) & 1 == 1 {
                let last = self.out.len() - 1;
                self.out[last] |= 0x80 >> (self.n % 8);
            }
            self.n += 1;
        }
    }

    /// `ns(n)`: the non-symmetric unsigned encoding of `v` in `0..n`.
    fn ns(&mut self, v: u32, n: u32) {
        if n <= 1 {
            return;
        }
        let w = 32 - n.leading_zeros();
        let m = (1u32 << w) - n;
        if v < m {
            self.put(u64::from(v), (w - 1) as usize);
        } else {
            let x = v + m;
            self.put(u64::from(x >> 1), (w - 1) as usize);
            self.put(u64::from(x & 1), 1);
        }
    }
}

#[cfg(test)]
pub(crate) mod reader {
    //! A reader of the descriptor as the specification defines it, for the tests.

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct Structure {
        pub(crate) dt_cnt: u8,
        /// (temporal id, dtis, fdiffs, chain diff) per template.
        pub(crate) templates: Vec<(u8, Vec<u8>, Vec<u16>, u8)>,
        pub(crate) chains: u32,
        pub(crate) resolution: Option<(u16, u16)>,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct Parsed {
        pub(crate) first: bool,
        pub(crate) last: bool,
        pub(crate) template: u8,
        pub(crate) frame_number: u16,
        pub(crate) structure: Option<Structure>,
        /// The frame's own fdiffs and chain diff (the template's unless custom).
        pub(crate) fdiffs: Vec<u16>,
        pub(crate) chain: u8,
        pub(crate) temporal: u8,
    }

    struct R<'a> {
        b: &'a [u8],
        pos: usize,
    }

    impl R<'_> {
        fn f(&mut self, n: usize) -> u64 {
            let mut v = 0;
            for _ in 0..n {
                let byte = self.b.get(self.pos / 8).copied().unwrap_or(0);
                v = (v << 1) | u64::from((byte >> (7 - self.pos % 8)) & 1);
                self.pos += 1;
            }
            v
        }
        fn ns(&mut self, n: u32) -> u32 {
            if n <= 1 {
                return 0;
            }
            let w = 32 - n.leading_zeros();
            let m = (1u32 << w) - n;
            let v = self.f((w - 1) as usize) as u32;
            if v < m {
                return v;
            }
            let extra = self.f(1) as u32;
            (v << 1) - m + extra
        }
    }

    /// Parses one element; `known` is the structure from an earlier keyframe.
    pub(crate) fn parse(b: &[u8], known: Option<&Structure>) -> Parsed {
        let mut r = R { b, pos: 0 };
        let first = r.f(1) == 1;
        let last = r.f(1) == 1;
        let template = r.f(6) as u8;
        let frame_number = r.f(16) as u16;
        let (mut has_structure, mut custom_fdiffs, mut custom_chains) = (false, false, false);
        if b.len() > 3 {
            has_structure = r.f(1) == 1;
            assert_eq!(r.f(1), 0, "no active decode targets bitmask");
            assert_eq!(r.f(1), 0, "no custom dtis");
            custom_fdiffs = r.f(1) == 1;
            custom_chains = r.f(1) == 1;
        }
        let mut structure = None;
        if has_structure {
            assert_eq!(r.f(6), 0, "template_id_offset");
            let dt_cnt = r.f(5) as u8 + 1;
            let mut temporals = vec![0u8];
            loop {
                let idc = r.f(2);
                let t = *temporals.last().unwrap_or(&0);
                match idc {
                    0 => temporals.push(t),
                    1 => temporals.push(t + 1),
                    3 => break,
                    _ => panic!("a second spatial layer"),
                }
            }
            let mut templates: Vec<(u8, Vec<u8>, Vec<u16>, u8)> = temporals
                .iter()
                .map(|&t| (t, Vec::new(), Vec::new(), 0))
                .collect();
            for t in templates.iter_mut() {
                for _ in 0..dt_cnt {
                    t.1.push(r.f(2) as u8);
                }
            }
            for t in templates.iter_mut() {
                while r.f(1) == 1 {
                    t.2.push(r.f(4) as u16 + 1);
                }
            }
            let chains = r.ns(u32::from(dt_cnt) + 1);
            if chains > 0 {
                for _ in 0..dt_cnt {
                    assert_eq!(r.ns(chains), 0, "protected by chain 0");
                }
                for t in templates.iter_mut() {
                    t.3 = r.f(4) as u8;
                }
            }
            let resolution = (r.f(1) == 1).then(|| (r.f(16) as u16 + 1, r.f(16) as u16 + 1));
            structure = Some(Structure {
                dt_cnt,
                templates,
                chains,
                resolution,
            });
        }
        let s = structure.as_ref().or(known).expect("a structure");
        let tp = &s.templates[usize::from(template)];
        let mut fdiffs = tp.2.clone();
        let mut chain = tp.3;
        let temporal = tp.0;
        if custom_fdiffs {
            fdiffs.clear();
            loop {
                let size = r.f(2) as usize;
                if size == 0 {
                    break;
                }
                fdiffs.push(r.f(4 * size) as u16 + 1);
            }
        }
        if custom_chains {
            chain = r.f(8) as u8;
        }
        assert!(
            r.pos <= b.len() * 8 && b.len() * 8 - r.pos < 8,
            "padding under a byte"
        );
        Parsed {
            first,
            last,
            template,
            frame_number,
            temporal,
            structure,
            fdiffs,
            chain,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::reader::parse;
    use super::*;

    #[test]
    fn mandatory_fields_are_three_bytes() {
        let mut w = DdWriter::new(0x1234, 1, (640, 360));
        let key = w.frame(true, 0, 2);
        assert!(
            key[0].len() > 3,
            "the keyframe's first packet carries the structure"
        );
        // First packet of the next (delta) frame: S=1 E=0, template 1, frame 0x1235.
        let d = w.frame(false, 0, 2);
        assert_eq!(d[0], vec![0x80 | 1, 0x12, 0x35]);
        assert_eq!(d[1], vec![0x40 | 1, 0x12, 0x35]);
        // The keyframe's second packet: E=1, template 0, no structure.
        assert_eq!(key[1], vec![0x40, 0x12, 0x34]);
    }

    #[test]
    fn structure_parses_per_the_specification() {
        for layers in 1..=3u8 {
            let mut w = DdWriter::new(7, layers, (1280, 720));
            let e = w.frame(true, 0, 1);
            let p = parse(&e[0], None);
            let s = p.structure.expect("structure");
            assert_eq!(s.dt_cnt, layers);
            assert_eq!(s.chains, 1);
            assert_eq!(s.resolution, Some((1280, 720)));
            let want: Vec<_> = templates(layers)
                .into_iter()
                .map(|t| (t.temporal, t.dtis, t.fdiffs, t.chain_diff))
                .collect();
            assert_eq!(s.templates, want);
            assert!(p.first && p.last);
            assert_eq!(p.frame_number, 7);
        }
    }

    #[test]
    fn l1t3_pattern_uses_the_templates_without_custom_fields() {
        let mut w = DdWriter::new(65534, 3, (320, 180));
        let s = parse(&w.frame(true, 0, 1)[0], None)
            .structure
            .expect("structure");
        // 0 2 1 2 0 2 1 2 after the keyframe (which is the first T0).
        let pattern = [2u8, 1, 2, 0, 2, 1, 2, 0, 2];
        let want = [
            (1, 1),
            (2, 2),
            (1, 3),
            (4, 4),
            (1, 1),
            (2, 2),
            (1, 3),
            (4, 4),
            (1, 1),
        ];
        for (&tid, &(fdiff, chain)) in pattern.iter().zip(&want) {
            let e = w.frame(false, tid, 1);
            assert_eq!(e[0].len(), 3, "tid {tid}: no custom fields");
            let p = parse(&e[0], Some(&s));
            assert_eq!(p.temporal, tid);
            assert_eq!(
                (p.fdiffs.clone(), p.chain),
                (vec![fdiff], chain),
                "tid {tid}"
            );
        }
    }

    #[test]
    fn a_skipped_frame_gives_custom_diffs() {
        let mut w = DdWriter::new(0, 3, (320, 180));
        let s = parse(&w.frame(true, 0, 1)[0], None)
            .structure
            .expect("structure");
        // The encoder skipped the T2 and T1: the next T0 is 1 frame after the keyframe.
        let e = w.frame(false, 0, 1);
        let p = parse(&e[0], Some(&s));
        assert_eq!((p.fdiffs, p.chain, p.temporal), (vec![1], 1, 0));
        // An L1T1 stream never needs them.
        let mut w = DdWriter::new(0, 1, (320, 180));
        let s = parse(&w.frame(true, 0, 1)[0], None)
            .structure
            .expect("structure");
        for _ in 0..5 {
            let e = w.frame(false, 0, 1);
            assert_eq!(e[0].len(), 3);
            assert_eq!(parse(&e[0], Some(&s)).fdiffs, vec![1]);
        }
    }
}
