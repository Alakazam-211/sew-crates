//! Reading the weights archive: a POSIX tar of NNEF tensor files (one `<label>.dat` per weight,
//! float32, little-endian) beside an NNEF graph that declares them and a README. Pure Rust,
//! streamed from the file: the bytes read are hashed as they go, and each tensor is decoded
//! straight into its own buffer, so loading holds the weights and a 64 KiB chunk, never the
//! whole file twice.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use sha2::{Digest, Sha256};

use super::LoadError;

/// The archive's sha256, hex: the only weights this version loads.
pub(crate) const PINNED_SHA256: &str =
    "b6e52c0bc66b82e42e3403d22cdd3b847143026d6a41cc4380a97b044f7766af";
/// The archive's size in bytes.
pub(crate) const PINNED_BYTES: u64 = 39_034_880;

const BLOCK: usize = 512;

fn hex(d: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(d.len() * 2);
    for b in d {
        let _ = write!(s, "{b:02x}");
    }
    s
}
const CHUNK: usize = 64 * 1024;

/// The sha256 of the file at `path`, read in chunks.
pub(crate) fn sha256_file(path: &Path) -> Result<String, LoadError> {
    let mut f = File::open(path).map_err(|e| LoadError::Io(e.to_string()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = f.read(&mut buf).map_err(|e| LoadError::Io(e.to_string()))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finalize()))
}

/// A reader that hashes what passes through it.
struct Hashing<R> {
    inner: R,
    hash: Sha256,
    read: u64,
}

impl<R: Read> Hashing<R> {
    fn exact(&mut self, buf: &mut [u8]) -> Result<(), LoadError> {
        self.inner
            .read_exact(buf)
            .map_err(|e| LoadError::Format(format!("truncated archive: {e}")))?;
        self.hash.update(&*buf);
        self.read += buf.len() as u64;
        Ok(())
    }
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

/// Reads every tensor of the archive at `path`, checking the bytes against the pin as they are
/// read (the caller has checked the whole file once already; this catches a file that changes
/// in between).
pub(crate) fn read_archive(path: &Path) -> Result<HashMap<String, Tensor>, LoadError> {
    let f = File::open(path).map_err(|e| LoadError::Io(e.to_string()))?;
    let mut r = Hashing {
        inner: BufReader::with_capacity(CHUNK, f),
        hash: Sha256::new(),
        read: 0,
    };
    let mut out = HashMap::new();
    let mut header = [0u8; BLOCK];
    let mut chunk = vec![0u8; CHUNK];
    let mut seen_graph = false;
    loop {
        r.exact(&mut header)?;
        if header.iter().all(|b| *b == 0) {
            // The end: a second zero block, then the record padding (the pin covers it all).
            break;
        }
        let name = {
            let prefix = cstr(&header[345..500]);
            let base = cstr(&header[0..100]);
            if prefix.is_empty() {
                base
            } else {
                format!("{prefix}/{base}")
            }
        };
        let size = octal(&header[124..136])?;
        let kind = header[156];
        if kind != b'0' && kind != 0 {
            return Err(LoadError::Format(format!("{name}: not a plain file")));
        }
        let padded = size.div_ceil(BLOCK as u64) * BLOCK as u64;
        if let Some(label) = name.strip_suffix(".dat") {
            let t = read_tensor(&mut r, &name, size, &mut chunk)?;
            if out.insert(label.to_owned(), t).is_some() {
                return Err(LoadError::Format(format!("{name}: twice in the archive")));
            }
            skip(&mut r, padded - size, &mut chunk)?;
        } else if name == "README.txt" || name == "graph.nnef" {
            seen_graph |= name == "graph.nnef";
            skip(&mut r, padded, &mut chunk)?;
        } else {
            return Err(LoadError::Format(format!(
                "{name}: not expected in the archive"
            )));
        }
    }
    // The rest of the file (the end-of-archive padding) is part of the pinned bytes.
    loop {
        let n = r
            .inner
            .read(&mut chunk)
            .map_err(|e| LoadError::Io(e.to_string()))?;
        if n == 0 {
            break;
        }
        r.hash.update(&chunk[..n]);
        r.read += n as u64;
    }
    let got = hex(&r.hash.finalize());
    if got != PINNED_SHA256 || r.read != PINNED_BYTES {
        return Err(LoadError::Mismatch { sha256: got });
    }
    if !seen_graph {
        return Err(LoadError::Format("no graph.nnef in the archive".into()));
    }
    Ok(out)
}

fn skip<R: Read>(r: &mut Hashing<R>, mut n: u64, chunk: &mut [u8]) -> Result<(), LoadError> {
    while n > 0 {
        let take = n.min(chunk.len() as u64) as usize;
        r.exact(&mut chunk[..take])?;
        n -= take as u64;
    }
    Ok(())
}

fn read_tensor<R: Read>(
    r: &mut Hashing<R>,
    name: &str,
    size: u64,
    chunk: &mut [u8],
) -> Result<Tensor, LoadError> {
    let bad = |what: &str| LoadError::Format(format!("{name}: {what}"));
    if size < 128 {
        return Err(bad("shorter than an NNEF tensor header"));
    }
    let mut h = [0u8; 128];
    r.exact(&mut h)?;
    if h[0] != 0x4E || h[1] != 0xEF || h[2] != 1 {
        return Err(bad("not an NNEF 1.x tensor file"));
    }
    let data_len = u32le(&h, 4) as u64;
    let rank = u32le(&h, 8) as usize;
    if rank == 0 || rank > 8 {
        return Err(bad("rank out of range"));
    }
    let shape: Vec<usize> = (0..rank).map(|i| u32le(&h, 12 + 4 * i) as usize).collect();
    let bits = u32le(&h, 44);
    let item = u32le(&h, 48);
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
    if data_len != 4 * count as u64 || size != 128 + data_len {
        return Err(bad("data length does not match the shape"));
    }
    let mut data = Vec::with_capacity(count);
    let mut left = data_len as usize;
    while left > 0 {
        let take = left.min(chunk.len());
        r.exact(&mut chunk[..take])?;
        data.extend(
            chunk[..take]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| f32::from_le_bytes(*b)),
        );
        left -= take;
    }
    if data.iter().any(|v| !v.is_finite()) {
        return Err(bad("a value is not finite"));
    }
    Ok(Tensor { shape, data })
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

    fn read_bytes(tag: &str, bytes: &[u8]) -> Result<HashMap<String, Tensor>, LoadError> {
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
            match read_bytes(tag, &bytes) {
                Err(LoadError::Format(_) | LoadError::Mismatch { .. }) => {}
                other => panic!("{tag}: {:?}", other.map(|t| t.len())),
            }
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
            match read_bytes(&format!("cut{k}"), &bytes[..cut]) {
                Err(LoadError::Format(_) | LoadError::Mismatch { .. }) => {}
                other => panic!("cut at {cut}: {:?}", other.map(|t| t.len())),
            }
        }
    }
}
