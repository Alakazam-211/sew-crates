//! `open_recording_chunk`: for the admin download of an encrypted recording.
//!
//! Opens one chunk of a recorder-sealed segment or mix part: Sew's blob stream format (Sew's
//! server-side stream cipher, not published), reimplemented on the **open** side only, in pure
//! Rust (aes-gcm), so it builds for wasm32 and never depends on aws-lc-rs.
//!
//! ```text
//! header (16): "SEWB" | version 0x01 | chunk_size_log2 (16..=24) | reserved 0x0000 | nonce_prefix (8)
//! nonce(i)  = nonce_prefix ‖ u32be(i)
//! aad(i)    = "sew-blob-v1" ‖ 0x00 ‖ header ‖ u32be(i) ‖ last (0x01 | 0x00)
//! ```

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use zeroize::Zeroizing;

use crate::error::ChunkError;

const MAGIC: [u8; 4] = *b"SEWB";
const VERSION: u8 = 0x01;
const HEADER_LEN: u64 = 16;
const TAG_LEN: u64 = 16;
const MIN_CHUNK_LOG2: u8 = 16;
const MAX_CHUNK_LOG2: u8 = 24;
const AAD_LABEL: &[u8] = b"sew-blob-v1";

fn parse_header(header: &[u8; 16]) -> Result<u8, ChunkError> {
    if header[0..4] != MAGIC {
        return Err(ChunkError::Header("magic"));
    }
    if header[4] != VERSION {
        return Err(ChunkError::Header("version"));
    }
    if header[6] != 0 || header[7] != 0 {
        return Err(ChunkError::Header("reserved"));
    }
    let log2 = header[5];
    if !(MIN_CHUNK_LOG2..=MAX_CHUNK_LOG2).contains(&log2) {
        return Err(ChunkError::Header("chunk_size_log2"));
    }
    Ok(log2)
}

/// Opens chunk `index` (`ct` = ciphertext ‖ tag). A failed tag returns an error and no
/// plaintext.
pub fn open_recording_chunk(
    key: &[u8; 32],
    header: &[u8; 16],
    index: u32,
    last: bool,
    ct: &[u8],
) -> Result<Zeroizing<Vec<u8>>, ChunkError> {
    let log2 = parse_header(header)?;
    let frame = (1u64 << log2) + TAG_LEN;
    let len = ct.len() as u64;
    if len < TAG_LEN || len > frame {
        return Err(ChunkError::Length);
    }
    if !last && len != frame {
        return Err(ChunkError::Length);
    }
    let mut nonce = [0u8; 12];
    nonce[..8].copy_from_slice(&header[8..16]);
    nonce[8..].copy_from_slice(&index.to_be_bytes());
    let mut aad = Vec::with_capacity(AAD_LABEL.len() + 1 + 16 + 4 + 1);
    aad.extend_from_slice(AAD_LABEL);
    aad.push(0x00);
    aad.extend_from_slice(header);
    aad.extend_from_slice(&index.to_be_bytes());
    aad.push(u8::from(last));
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let pt = cipher
        .decrypt(Nonce::from_slice(&nonce), Payload { msg: ct, aad: &aad })
        .map_err(|_| ChunkError::Aead)?;
    let pt = Zeroizing::new(pt);
    if last && index > 0 && pt.is_empty() {
        return Err(ChunkError::Length);
    }
    Ok(pt)
}

/// The chunk layout of a sealed file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkLayout {
    /// Chunks (`N`).
    pub chunks: u32,
    /// Plaintext bytes per full chunk (`C`).
    pub chunk_size: u64,
    /// Plaintext bytes in all.
    pub plaintext_len: u64,
}

impl ChunkLayout {
    /// Byte offset and length in the file of chunk `index` (the last may be shorter).
    pub fn chunk_range(&self, index: u32, file_len: u64) -> Option<(u64, u64)> {
        if index >= self.chunks {
            return None;
        }
        let frame = self.chunk_size + TAG_LEN;
        let start = HEADER_LEN + u64::from(index) * frame;
        let end = (start + frame).min(file_len);
        Some((start, end.saturating_sub(start)))
    }
}

/// The layout of a file of `file_len` bytes with `header` (rejects lengths no writer makes).
pub fn recording_chunk_layout(file_len: u64, header: &[u8; 16]) -> Result<ChunkLayout, ChunkError> {
    let log2 = parse_header(header)?;
    let chunk_size = 1u64 << log2;
    let frame = chunk_size + TAG_LEN;
    let body = file_len
        .checked_sub(HEADER_LEN)
        .ok_or(ChunkError::FileLength)?;
    if body < TAG_LEN {
        return Err(ChunkError::FileLength);
    }
    let n = body.div_ceil(frame).max(1);
    let last_frame = body - (n - 1) * frame;
    if last_frame < TAG_LEN || (n > 1 && last_frame == TAG_LEN) {
        return Err(ChunkError::FileLength);
    }
    let chunks = u32::try_from(n).map_err(|_| ChunkError::FileLength)?;
    Ok(ChunkLayout {
        chunks,
        chunk_size,
        plaintext_len: body - n * TAG_LEN,
    })
}
