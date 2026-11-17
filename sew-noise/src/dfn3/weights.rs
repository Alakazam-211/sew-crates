//! Reading the weights archive: a POSIX tar of NNEF tensor files (one `<label>.dat` per weight,
//! float32, little-endian) beside an NNEF graph that declares them and a README. Pure Rust and
//! push-driven: [`Reader`] takes the archive's bytes in pieces of any size, hashes them as they
//! go and decodes each tensor straight into its own buffer, so loading holds the weights and
//! one piece, never the whole archive twice. A file is fed in 64 KiB chunks; bytes in memory are
//! fed as they are.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

use super::LoadError;

/// The archive's sha256, hex: the only weights this version loads.
pub(crate) const PINNED_SHA256: &str =
    "b6e52c0bc66b82e42e3403d22cdd3b847143026d6a41cc4380a97b044f7766af";
/// The archive's size in bytes.
pub(crate) const PINNED_BYTES: u64 = 39_034_880;

const BLOCK: usize = 512;
/// An NNEF tensor file's header.
const TENSOR_HEADER: usize = 128;

pub(crate) fn hex(d: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(d.len() * 2);
    for b in d {
        let _ = write!(s, "{b:02x}");
    }
    s
}
const CHUNK: usize = 64 * 1024;

/// Feeds the file at `path` to `sink` in chunks.
fn each_chunk(
    path: &Path,
    mut sink: impl FnMut(&[u8]) -> Result<(), LoadError>,
) -> Result<(), LoadError> {
    let mut f = File::open(path).map_err(|e| LoadError::Io(e.to_string()))?;
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = f.read(&mut buf).map_err(|e| LoadError::Io(e.to_string()))?;
        if n == 0 {
            return Ok(());
        }
        sink(&buf[..n])?;
    }
}

/// The sha256 of the file at `path`, read in chunks.
pub(crate) fn sha256_file(path: &Path) -> Result<String, LoadError> {
    let mut h = Sha256::new();
    each_chunk(path, |c| {
        h.update(c);
        Ok(())
    })?;
    Ok(hex(&h.finalize()))
}

/// One tensor: its shape and values.
#[derive(Debug)]
pub(crate) struct Tensor {
    pub(crate) shape: Vec<usize>,
    pub(crate) data: Vec<f32>,
}

fn octal(field: &[u8]) -> Result<u64, LoadError> {
    let s: String = field
        .iter()
        .take_while(|b| **b != 0 && **b != b' ')
        .map(|b| *b as char)
        .collect();
    let s = s.trim();
    if s.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(s, 8).map_err(|_| LoadError::Format(format!("bad tar number {s:?}")))
}

fn cstr(field: &[u8]) -> String {
    field
        .iter()
        .take_while(|b| **b != 0)
        .map(|b| *b as char)
        .collect()
}

fn u32le(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Where the reader is in the archive.
#[derive(Debug)]
enum State {
    /// Collecting a 512-byte tar header in `head`.
    Header,
    /// Collecting a tensor file's 128-byte header in `head`; `size` is the member's size.
    TensorHeader { label: String, size: u64 },
    /// Decoding `left` more bytes of the tensor in `tensor`.
    TensorData { left: u64 },
    /// Skipping `left` bytes (a member that is not a tensor, or a member's padding).
    Skip { left: u64 },
    /// After the end-of-archive block: the rest is padding (hashed, not read).
    End,
}

/// The archive, read from bytes pushed in pieces of any size ([`Self::feed`]), then
/// [`Self::finish`]. Every byte fed is hashed; `finish` refuses unless the bytes were exactly the
/// pinned archive.
#[derive(Debug)]
pub(crate) struct Reader {
    hash: Sha256,
    read: u64,
    state: State,
    /// The header being collected (a tar header or a tensor header).
    head: Vec<u8>,
    /// The tensor being decoded: its label, shape and values so far.
    tensor: Option<(String, Tensor)>,
    /// A value's bytes split between two pieces.
    carry: [u8; 4],
    carry_len: usize,
    /// The padding after the tensor being decoded.
    pad: u64,
    out: HashMap<String, Tensor>,
    seen_graph: bool,
}

impl Reader {
    pub(crate) fn new() -> Self {
        Self {
            hash: Sha256::new(),
            read: 0,
            state: State::Header,
            head: Vec::with_capacity(BLOCK),
            tensor: None,
            carry: [0; 4],
            carry_len: 0,
            pad: 0,
            out: HashMap::new(),
            seen_graph: false,
        }
    }

    /// The next piece of the archive. After an error the reader must not be fed again.
    pub(crate) fn feed(&mut self, piece: &[u8]) -> Result<(), LoadError> {
        self.hash.update(piece);
        self.read += piece.len() as u64;
        if self.read > PINNED_BYTES {
            // Longer than the pinned archive: it cannot be it, and reading on would only spend
            // memory on what is refused anyway.
            return Err(LoadError::Format("longer than the pinned archive".into()));
        }
        let mut rest = piece;
        while !rest.is_empty() {
            rest = self.step(rest)?;
        }
        Ok(())
    }

    /// Consumes what it can of `input` in the current state; answers the rest.
    fn step<'a>(&mut self, input: &'a [u8]) -> Result<&'a [u8], LoadError> {
        match &mut self.state {
            State::End => Ok(&[]),
            State::Skip { left } => {
                let take = (*left).min(input.len() as u64) as usize;
                *left -= take as u64;
                if *left == 0 {
                    self.state = State::Header;
                }
                Ok(&input[take..])
            }
            State::Header => {
                let rest = collect(&mut self.head, BLOCK, input);
                if self.head.len() == BLOCK {
                    self.header()?;
                }
                Ok(rest)
            }
            State::TensorHeader { .. } => {
                let rest = collect(&mut self.head, TENSOR_HEADER, input);
                if self.head.len() == TENSOR_HEADER {
                    self.tensor_header()?;
                }
                Ok(rest)
            }
            State::TensorData { left } => {
                let take = (*left).min(input.len() as u64) as usize;
                *left -= take as u64;
                let done = *left == 0;
                let (bytes, rest) = input.split_at(take);
                self.decode(bytes);
                if done {
                    self.tensor_done()?;
                }
                Ok(rest)
            }
        }
    }

    /// A whole tar header is in `head`.
    fn header(&mut self) -> Result<(), LoadError> {
        let h = &self.head[..];
        if h.iter().all(|b| *b == 0) {
            // The end: a second zero block, then the record padding (the pin covers it all).
            self.head.clear();
            self.state = State::End;
            return Ok(());
        }
        let name = {
            let prefix = cstr(&h[345..500]);
            let base = cstr(&h[0..100]);
            if prefix.is_empty() {
                base
            } else {
                format!("{prefix}/{base}")
            }
        };
        let size = octal(&h[124..136])?;
        let kind = h[156];
        self.head.clear();
        if kind != b'0' && kind != 0 {
            return Err(LoadError::Format(format!("{name}: not a plain file")));
        }
        let padded = size.div_ceil(BLOCK as u64) * BLOCK as u64;
        if let Some(label) = name.strip_suffix(".dat") {
            if self.out.contains_key(label) {
                return Err(LoadError::Format(format!("{name}: twice in the archive")));
            }
            if size < TENSOR_HEADER as u64 {
                return Err(LoadError::Format(format!(
                    "{name}: shorter than an NNEF tensor header"
                )));
            }
            self.pad = padded - size;
            self.state = State::TensorHeader {
                label: label.to_owned(),
                size,
            };
        } else if name == "README.txt" || name == "graph.nnef" {
            self.seen_graph |= name == "graph.nnef";
            self.skip(padded);
        } else {
            return Err(LoadError::Format(format!(
                "{name}: not expected in the archive"
            )));
        }
        Ok(())
    }

    fn skip(&mut self, left: u64) {
        self.state = if left == 0 {
            State::Header
        } else {
            State::Skip { left }
        };
    }

    /// A whole tensor header is in `head`.
    fn tensor_header(&mut self) -> Result<(), LoadError> {
        let State::TensorHeader { label, size } = std::mem::replace(&mut self.state, State::Header)
        else {
            return Err(LoadError::Format("reader out of step".into()));
        };
        let name = format!("{label}.dat");
        let bad = |what: &str| LoadError::Format(format!("{name}: {what}"));
        let h = &self.head[..];
        if h[0] != 0x4E || h[1] != 0xEF || h[2] != 1 {
            return Err(bad("not an NNEF 1.x tensor file"));
        }
        let data_len = u32le(h, 4) as u64;
        let rank = u32le(h, 8) as usize;
        if rank == 0 || rank > 8 {
            return Err(bad("rank out of range"));
        }
        let shape: Vec<usize> = (0..rank).map(|i| u32le(h, 12 + 4 * i) as usize).collect();
        let bits = u32le(h, 44);
        let item = u32le(h, 48);
        if bits != 32 || item != 0 {
            return Err(bad("not float32"));
        }
        // No tensor of the model is near this; a header claiming more is malformed (and must not
        // make the reader reserve it).
        const MAX_VALUES: u64 = 4 << 20;
        let count = shape
            .iter()
            .try_fold(1u64, |a, d| a.checked_mul(*d as u64))
            .filter(|c| *c <= MAX_VALUES)
            .ok_or_else(|| bad("shape too large"))? as usize;
        if data_len != 4 * count as u64 || size != TENSOR_HEADER as u64 + data_len {
            return Err(bad("data length does not match the shape"));
        }
        self.head.clear();
        self.carry_len = 0;
        self.tensor = Some((
            label,
            Tensor {
                shape,
                data: Vec::with_capacity(count),
            },
        ));
        if data_len == 0 {
            self.tensor_done()?;
        } else {
            self.state = State::TensorData { left: data_len };
        }
        Ok(())
    }

    /// Decodes `bytes` of the current tensor's values (a value may straddle two pieces).
    fn decode(&mut self, mut bytes: &[u8]) {
        let Some((_, t)) = self.tensor.as_mut() else {
            return;
        };
        if self.carry_len > 0 {
            let take = (4 - self.carry_len).min(bytes.len());
            self.carry[self.carry_len..self.carry_len + take].copy_from_slice(&bytes[..take]);
            self.carry_len += take;
            bytes = &bytes[take..];
            if self.carry_len < 4 {
                return;
            }
            t.data.push(f32::from_le_bytes(self.carry));
            self.carry_len = 0;
        }
        let (whole, tail) = bytes.as_chunks::<4>();
        t.data.extend(whole.iter().map(|b| f32::from_le_bytes(*b)));
        self.carry[..tail.len()].copy_from_slice(tail);
        self.carry_len = tail.len();
    }

    /// The current tensor's last byte is in.
    fn tensor_done(&mut self) -> Result<(), LoadError> {
        let Some((label, t)) = self.tensor.take() else {
            return Err(LoadError::Format("reader out of step".into()));
        };
        if t.data.iter().any(|v| !v.is_finite()) {
            return Err(LoadError::Format(format!(
                "{label}.dat: a value is not finite"
            )));
        }
        self.out.insert(label, t);
        let pad = self.pad;
        self.skip(pad);
        Ok(())
    }

    /// After the last piece: the tensors, if the bytes were exactly the pinned archive.
    pub(crate) fn finish(self) -> Result<HashMap<String, Tensor>, LoadError> {
        if !matches!(self.state, State::End) {
            return Err(LoadError::Format("truncated archive".into()));
        }
        let got = hex(&self.hash.finalize());
        if got != PINNED_SHA256 || self.read != PINNED_BYTES {
            return Err(LoadError::Mismatch { sha256: got });
        }
        if !self.seen_graph {
            return Err(LoadError::Format("no graph.nnef in the archive".into()));
        }
        Ok(self.out)
    }
}

/// Appends to `head` up to `want` bytes from `input`; answers the rest of `input`.
fn collect<'a>(head: &mut Vec<u8>, want: usize, input: &'a [u8]) -> &'a [u8] {
    let take = (want - head.len()).min(input.len());
    head.extend_from_slice(&input[..take]);
    &input[take..]
}

/// A tiny archive in the weights' format (an NNEF graph and one 4 x 4 tensor), which is not
/// the pinned archive: for running the reader's code once ([`super::warm_up_loader`]).
pub(crate) fn synthetic_archive() -> Vec<u8> {
    fn member(a: &mut Vec<u8>, name: &str, body: &[u8]) {
        let mut h = [0u8; BLOCK];
        h[..name.len()].copy_from_slice(name.as_bytes());
        let size = format!("{:011o}", body.len());
        h[124..135].copy_from_slice(size.as_bytes());
        h[156] = b'0';
        a.extend_from_slice(&h);
        a.extend_from_slice(body);
        a.resize(a.len().div_ceil(BLOCK) * BLOCK, 0);
    }
    let mut tensor = vec![0x4E, 0xEF, 1, 0];
    tensor.extend_from_slice(&64u32.to_le_bytes());
    tensor.extend_from_slice(&2u32.to_le_bytes());
    for d in [4u32, 4, 0, 0, 0, 0, 0, 0] {
        tensor.extend_from_slice(&d.to_le_bytes());
    }
    tensor.extend_from_slice(&32u32.to_le_bytes());
    tensor.extend_from_slice(&0u32.to_le_bytes());
    tensor.resize(TENSOR_HEADER, 0);
    for i in 0..16u8 {
        tensor.extend_from_slice(&(f32::from(i) * 0.25).to_le_bytes());
    }
    let mut a = Vec::with_capacity(4 * BLOCK);
    member(&mut a, "graph.nnef", b"version 1.0;");
    member(&mut a, "w.dat", &tensor);
    a.extend_from_slice(&[0u8; 2 * BLOCK]);
    a
}

/// Reads every tensor of the archive at `path`, checking the bytes against the pin as they are
/// read (the caller has checked the whole file once already; this catches a file that changes
/// in between).
pub(crate) fn read_archive(path: &Path) -> Result<HashMap<String, Tensor>, LoadError> {
    let mut r = Reader::new();
    each_chunk(path, |c| r.feed(c))?;
    r.finish()
}

/// Reads every tensor of the archive in `bytes`, checking them against the pin as they are read.
#[cfg(test)]
pub(crate) fn read_bytes(bytes: &[u8]) -> Result<HashMap<String, Tensor>, LoadError> {
    let mut r = Reader::new();
    r.feed(bytes)?;
    r.finish()
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::print_stderr
    )]
    use super::*;

    fn header(name: &str, size: u64) -> [u8; BLOCK] {
        let mut h = [0u8; BLOCK];
        h[..name.len()].copy_from_slice(name.as_bytes());
        let s = format!("{size:011o}");
        h[124..135].copy_from_slice(s.as_bytes());
        h[156] = b'0';
        h
    }

    fn tensor_file(magic: [u8; 4], shape: &[u32], data_len: u32, values: &[f32]) -> Vec<u8> {
        let mut t = magic.to_vec();
        t.extend_from_slice(&data_len.to_le_bytes());
        t.extend_from_slice(&(shape.len() as u32).to_le_bytes());
        for i in 0..8 {
            t.extend_from_slice(&shape.get(i).copied().unwrap_or(0).to_le_bytes());
        }
        t.extend_from_slice(&32u32.to_le_bytes());
        t.extend_from_slice(&0u32.to_le_bytes());
        t.resize(128, 0);
        for v in values {
            t.extend_from_slice(&v.to_le_bytes());
        }
        t
    }

    fn archive(members: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut a = Vec::new();
        for (name, body) in members {
            a.extend_from_slice(&header(name, body.len() as u64));
            a.extend_from_slice(body);
            a.resize(a.len().div_ceil(BLOCK) * BLOCK, 0);
        }
        a.extend_from_slice(&[0u8; 2 * BLOCK]);
        a
    }

    fn read_file(tag: &str, bytes: &[u8]) -> Result<HashMap<String, Tensor>, LoadError> {
        let base = std::env::var_os("SEW_TEST_TMP")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = base.join(format!("noise-dfn3-weights-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("w.tar");
        std::fs::write(&p, bytes).unwrap();
        let r = read_archive(&p);
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_dir(&dir);
        r
    }

    /// The bytes fed to a [`Reader`] in pieces of `piece` bytes.
    fn read_pieces(bytes: &[u8], piece: usize) -> Result<HashMap<String, Tensor>, LoadError> {
        let mut r = Reader::new();
        for p in bytes.chunks(piece) {
            r.feed(p)?;
        }
        r.finish()
    }

    /// Every way in: a file, the bytes at once, and the bytes in small and odd-sized pieces
    /// (values and headers split across pieces). Each must refuse.
    fn refused_every_way(tag: &str, bytes: &[u8]) {
        let ways = [
            ("file", read_file(tag, bytes)),
            ("bytes", read_bytes(bytes)),
            ("1-byte pieces", read_pieces(bytes, 1)),
            ("7-byte pieces", read_pieces(bytes, 7)),
            ("513-byte pieces", read_pieces(bytes, 513)),
        ];
        for (how, r) in ways {
            match r {
                Err(LoadError::Format(_) | LoadError::Mismatch { .. }) => {}
                other => panic!("{tag} ({how}): {:?}", other.map(|t| t.len())),
            }
        }
    }

    const NNEF: [u8; 4] = [0x4E, 0xEF, 1, 0];

    fn with(mut bytes: Vec<u8>, at: usize, value: &[u8]) -> Vec<u8> {
        bytes[at..at + value.len()].copy_from_slice(value);
        bytes
    }

    /// Malformed and truncated archives are refused with an error, never a panic, and a header
    /// claiming a huge tensor never makes the reader reserve it.
    #[test]
    fn malformed_archives_are_refused() {
        let good = tensor_file(NNEF, &[2, 2], 16, &[0.5, -0.5, 1.0, 2.0]);
        let one = |t: Vec<u8>| archive(&[("a.dat", t)]);
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("empty", Vec::new()),
            (
                "noise",
                (0..1_500u32)
                    .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
                    .collect(),
            ),
            ("short-tensor", one(vec![0u8; 64])),
            (
                "bad-magic",
                one(tensor_file([0x50, 0x4B, 3, 4], &[2], 8, &[1.0, 2.0])),
            ),
            ("rank-0", one(tensor_file(NNEF, &[], 0, &[]))),
            (
                "rank-9",
                with(
                    one(tensor_file(NNEF, &[1], 4, &[1.0])),
                    BLOCK + 8,
                    &9u32.to_le_bytes(),
                ),
            ),
            (
                "huge-shape",
                one(tensor_file(NNEF, &[u32::MAX, u32::MAX, 4], 16, &[0.0; 4])),
            ),
            ("too-large", one(tensor_file(NNEF, &[8 << 20], 4, &[0.0]))),
            (
                "length-mismatch",
                one(tensor_file(NNEF, &[3], 12, &[1.0, 2.0])),
            ),
            (
                "not-float",
                one(with(good.clone(), 44, &16u32.to_le_bytes())),
            ),
            ("nan", one(tensor_file(NNEF, &[2], 8, &[1.0, f32::NAN]))),
            ("unexpected", archive(&[("evil.sh", b"#!/bin/sh".to_vec())])),
            (
                "twice",
                archive(&[("a.dat", good.clone()), ("a.dat", good.clone())]),
            ),
            ("no-graph", one(good.clone())),
            ("truncated-data", one(good.clone())[..BLOCK + 140].to_vec()),
            ("not-a-file", with(archive(&[("d", Vec::new())]), 156, b"5")),
            ("bad-size", with(one(good.clone()), 124, b"zzzzzzzzzzz")),
            (
                "well-formed-but-not-pinned",
                archive(&[("graph.nnef", b"version 1.0;".to_vec()), ("a.dat", good)]),
            ),
        ];
        for (tag, bytes) in cases {
            refused_every_way(tag, &bytes);
        }
    }

    /// The real archive cut short anywhere is refused, never a panic.
    #[test]
    fn truncated_real_archive_is_refused() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/dfn3");
        if !dir.is_dir() {
            // Outside Sew's repository (the published crate) the weights are not here.
            eprintln!(
                "SKIPPED: DeepFilterNet3 weights are not in this tree ({} is absent)",
                dir.display()
            );
            return;
        }
        let bytes = std::fs::read(dir.join(super::super::WEIGHTS_FILE)).unwrap();
        for k in 1..12usize {
            let cut = bytes.len() * k / 12 + (k * 37) % BLOCK;
            for (how, r) in [
                ("file", read_file(&format!("cut{k}"), &bytes[..cut])),
                ("bytes", read_bytes(&bytes[..cut])),
                ("4093-byte pieces", read_pieces(&bytes[..cut], 4_093)),
            ] {
                match r {
                    Err(LoadError::Format(_) | LoadError::Mismatch { .. }) => {}
                    other => panic!("cut at {cut} ({how}): {:?}", other.map(|t| t.len())),
                }
            }
        }
    }

    /// The real archive read from the file, from the bytes at once and in odd-sized pieces gives
    /// the same tensors, bit for bit; one byte changed or one byte added is refused.
    #[test]
    fn real_archive_reads_the_same_every_way() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/dfn3");
        if !dir.is_dir() {
            eprintln!(
                "SKIPPED: DeepFilterNet3 weights are not in this tree ({} is absent)",
                dir.display()
            );
            return;
        }
        let path = dir.join(super::super::WEIGHTS_FILE);
        let bytes = std::fs::read(&path).unwrap();
        let a = read_archive(&path).unwrap();
        for (how, b) in [
            ("bytes", read_bytes(&bytes).unwrap()),
            ("4093-byte pieces", read_pieces(&bytes, 4_093).unwrap()),
            ("65536-byte pieces", read_pieces(&bytes, 65_536).unwrap()),
        ] {
            assert_eq!(a.len(), b.len(), "{how}");
            for (k, t) in &a {
                let u = &b[k];
                assert_eq!(t.shape, u.shape, "{how} {k}");
                assert!(
                    t.data
                        .iter()
                        .zip(&u.data)
                        .all(|(x, y)| x.to_bits() == y.to_bits())
                        && t.data.len() == u.data.len(),
                    "{how} {k}"
                );
            }
        }
        let mut flipped = bytes.clone();
        flipped[bytes.len() / 2] ^= 1;
        assert!(matches!(
            read_bytes(&flipped),
            Err(LoadError::Mismatch { .. } | LoadError::Format(_))
        ));
        let mut longer = bytes;
        longer.push(0);
        assert!(read_bytes(&longer).is_err());
    }
}
