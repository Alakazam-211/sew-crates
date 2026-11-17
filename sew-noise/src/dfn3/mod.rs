//! DeepFilterNet3's low-latency model (cargo feature `dfn3`): the weights, loaded from a file
//! beside the app, and the model run by hand-written inference in this crate.
//!
//! Where the code comes from: `net` is written here from the model's published graph (its
//! layers, shapes and order, read from the authors' ONNX files); `stream` is ported from libDF
//! (its header names the files and the commit); `weights` reads POSIX tar and the NNEF tensor
//! file format as their specifications describe.
//!
//! The model is the work of Hendrik Schröter and the DeepFilterNet authors
//! (<https://github.com/Rikorose/DeepFilterNet>, MIT OR Apache-2.0). Its weights are not in this
//! crate: an app ships the weights archive as a resource ([`WEIGHTS_FILE`]) and passes its path
//! to [`Dfn3Model::load`] (or its bytes to [`Dfn3Model::load_bytes`], or to a [`Dfn3Loader`] in
//! pieces), which checks the archive's sha256 against [`WEIGHTS_SHA256`] before reading a
//! weight. A [`Stage`](crate::Stage) built with the model ([`crate::Stage::with_dfn3`])
//! runs it as [`Mode::Strong`](crate::Mode::Strong), with RNNoise kept warm beside it as the
//! step-down; without a model (the file missing, or its hash not the pinned one) an app builds
//! [`crate::Stage::new`] and runs RNNoise.
//!
//! Inference is plain Rust: no ONNX or NNEF runtime, no C, no unsafe. The archive is a POSIX
//! tar holding an NNEF graph that declares each weight and one NNEF tensor file per weight;
//! this module reads the tensor files itself.

mod net;
mod stream;
mod weights;

use std::path::Path;
use std::sync::Arc;

use sha2::{Digest, Sha256};

pub(crate) use stream::{Dfn3Stream, tail_weight};

/// The weights archive's file name, as apps ship it.
pub const WEIGHTS_FILE: &str = "dfn3_ll.nnef.tar";
/// The sha256 (hex) of the only weights archive this version loads.
pub const WEIGHTS_SHA256: &str = weights::PINNED_SHA256;
/// The archive's size in bytes.
pub const WEIGHTS_BYTES: u64 = weights::PINNED_BYTES;

/// Why [`Dfn3Model::load`] refused. Nothing is loaded when one is returned.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum LoadError {
    /// The file could not be read (missing, unreadable).
    Io(String),
    /// The file is not the pinned weights archive: its sha256 differs.
    Mismatch {
        /// The file's sha256, hex.
        sha256: String,
    },
    /// The archive matched the pin but its contents could not be read as this version's
    /// weights (a build whose pin and reader disagree).
    Format(String),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "the DeepFilterNet3 weights could not be read: {e}"),
            Self::Mismatch { sha256 } => write!(
                f,
                "the DeepFilterNet3 weights file's sha256 is {sha256}, not the pinned {WEIGHTS_SHA256}"
            ),
            Self::Format(e) => write!(f, "the DeepFilterNet3 weights archive is malformed: {e}"),
        }
    }
}

impl std::error::Error for LoadError {}

/// DeepFilterNet3's low-latency model, loaded: about 39 MB of weights, shared (read-only) by
/// every stage built with it.
pub struct Dfn3Model {
    weights: net::Weights,
}

impl std::fmt::Debug for Dfn3Model {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dfn3Model")
            .field("weight_bytes", &self.weights.bytes())
            .finish()
    }
}

impl Dfn3Model {
    /// Loads the weights archive at `path`.
    ///
    /// First the whole file's sha256 is checked against [`WEIGHTS_SHA256`]; only a match is
    /// read, and it is hashed again as it is read (a file that changes in between is refused).
    /// Reading streams each weight into its own buffer: memory during the load is the weights
    /// plus a 64 KiB chunk. Slow (tens of milliseconds): call it off the audio thread, once per
    /// app, and share the result.
    pub fn load(path: impl AsRef<Path>) -> Result<Arc<Self>, LoadError> {
        let path = path.as_ref();
        let sha = weights::sha256_file(path)?;
        if sha != WEIGHTS_SHA256 {
            return Err(LoadError::Mismatch { sha256: sha });
        }
        let tensors = weights::read_archive(path)?;
        Ok(Arc::new(Self {
            weights: net::Weights::from_tensors(tensors)?,
        }))
    }

    /// Loads the weights archive from `bytes` (an app that holds the archive in memory, such as
    /// a web page that fetched it).
    ///
    /// The same checks and refusals as [`Self::load`]: first the sha256 of all of `bytes` is
    /// checked against [`WEIGHTS_SHA256`] ([`LoadError::Mismatch`] otherwise), and only then is a
    /// weight read; the bytes are hashed again as they are read. Memory during the load is the
    /// weights beside `bytes`. Slow: call it off the audio thread, or use a [`Dfn3Loader`] to
    /// spread the work.
    pub fn load_bytes(bytes: &[u8]) -> Result<Arc<Self>, LoadError> {
        let mut loader = Dfn3Loader::new();
        loader.hash(bytes)?;
        loader.verify()?;
        loader.read(bytes)?;
        loader.finish()
    }

    /// Checks that the file at `path` is the pinned weights archive (its sha256 is
    /// [`WEIGHTS_SHA256`]) without loading it: the file is hashed in 64 KiB chunks and nothing
    /// of it is kept. For an app that records where the weights are and loads them later.
    pub fn verify(path: impl AsRef<Path>) -> Result<(), LoadError> {
        let sha = weights::sha256_file(path.as_ref())?;
        if sha != WEIGHTS_SHA256 {
            return Err(LoadError::Mismatch { sha256: sha });
        }
        Ok(())
    }

    /// Bytes of weights held in memory.
    pub fn weight_bytes(&self) -> usize {
        self.weights.bytes()
    }
}

/// Loads the weights archive from bytes handed over in pieces, for a caller that has to spread
/// the work over time (an audio thread with no other thread to load on, such as a Web Audio
/// worklet). The bytes go through twice, in order, in pieces of any size:
///
/// 1. [`Self::hash`] every piece, then [`Self::verify`]: refuses unless the bytes are the pinned
///    archive ([`WEIGHTS_SHA256`]). Nothing has been parsed yet.
/// 2. [`Self::read`] every piece again, then [`Self::finish`]: the model. The pieces are hashed
///    again as they are read, so bytes that changed between the passes are refused.
///
/// The checks and refusals are [`Dfn3Model::load`]'s. A call out of this order, or any call
/// after a refusal, is refused. Memory is the weights and nothing else: each piece is read as
/// it comes.
///
/// ```
/// use sew_noise::dfn3::{Dfn3Loader, LoadError};
///
/// let mut loader = Dfn3Loader::new();
/// loader.hash(b"not the weights").unwrap();
/// assert!(matches!(loader.verify(), Err(LoadError::Mismatch { .. })));
/// assert!(loader.read(b"not the weights").is_err(), "refused for good");
/// ```
pub struct Dfn3Loader {
    pass: Pass,
}

enum Pass {
    Hashing { hash: Sha256, bytes: u64 },
    Verified,
    Reading(weights::Reader),
    Refused,
}

impl std::fmt::Debug for Dfn3Loader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pass = match self.pass {
            Pass::Hashing { .. } => "hashing",
            Pass::Verified => "verified",
            Pass::Reading(_) => "reading",
            Pass::Refused => "refused",
        };
        f.debug_struct("Dfn3Loader").field("pass", &pass).finish()
    }
}

impl Default for Dfn3Loader {
    fn default() -> Self {
        Self::new()
    }
}

impl Dfn3Loader {
    /// A loader at the start of the first pass.
    pub fn new() -> Self {
        Self {
            pass: Pass::Hashing {
                hash: Sha256::new(),
                bytes: 0,
            },
        }
    }

    fn out_of_order(&mut self, call: &str) -> LoadError {
        self.pass = Pass::Refused;
        LoadError::Format(format!("the loader's {call} was called out of order"))
    }

    /// The first pass: the next piece of the archive.
    pub fn hash(&mut self, piece: &[u8]) -> Result<(), LoadError> {
        match &mut self.pass {
            Pass::Hashing { hash, bytes } => {
                hash.update(piece);
                *bytes += piece.len() as u64;
                Ok(())
            }
            _ => Err(self.out_of_order("hash")),
        }
    }

    /// The end of the first pass: refuses unless the bytes hashed are the pinned archive.
    pub fn verify(&mut self) -> Result<(), LoadError> {
        match std::mem::replace(&mut self.pass, Pass::Refused) {
            Pass::Hashing { hash, bytes } => {
                let sha = weights::hex(&hash.finalize());
                if sha != WEIGHTS_SHA256 || bytes != WEIGHTS_BYTES {
                    return Err(LoadError::Mismatch { sha256: sha });
                }
                self.pass = Pass::Verified;
                Ok(())
            }
            _ => Err(self.out_of_order("verify")),
        }
    }

    /// The second pass: the next piece of the same bytes, read into weights.
    pub fn read(&mut self, piece: &[u8]) -> Result<(), LoadError> {
        if matches!(self.pass, Pass::Verified) {
            self.pass = Pass::Reading(weights::Reader::new());
        }
        let Pass::Reading(reader) = &mut self.pass else {
            return Err(self.out_of_order("read"));
        };
        if let Err(e) = reader.feed(piece) {
            self.pass = Pass::Refused;
            return Err(e);
        }
        Ok(())
    }

    /// The end of the second pass: the model, shared by every stage built with it.
    pub fn finish(mut self) -> Result<Arc<Dfn3Model>, LoadError> {
        match std::mem::replace(&mut self.pass, Pass::Refused) {
            Pass::Reading(reader) => Ok(Arc::new(Dfn3Model {
                weights: net::Weights::from_tensors(reader.finish()?)?,
            })),
            _ => Err(self.out_of_order("finish")),
        }
    }
}

/// Runs the loader's code once, on bytes that are not the weights: 64 KiB hashed in 4 KiB
/// pieces, then a tiny archive in the weights' format read in 512-byte pieces (and refused, as
/// it is not the pinned archive). For an app that loads the weights on an audio thread later:
/// run at a time when a slow first run does not matter (before audio starts), so that a
/// WebAssembly engine has compiled this code by the time the real load runs. It shares no
/// state with any loader or model, and allocates a few small buffers, freed before it returns.
#[doc(hidden)]
pub fn warm_up_loader() {
    let mut h = Sha256::new();
    let piece = [0u8; 4096];
    for _ in 0..16 {
        h.update(piece);
    }
    let _ = weights::hex(&h.finalize());
    let archive = weights::synthetic_archive();
    let mut reader = weights::Reader::new();
    for p in archive.chunks(512) {
        if reader.feed(p).is_err() {
            return;
        }
    }
    let _ = reader.finish();
}

/// The model alone, one 480-sample hop at a time, with no stage around it: for comparing this
/// crate's inference with a reference runtime and timing its parts. Not part of the API.
#[doc(hidden)]
#[derive(Debug)]
pub struct RawStream(Dfn3Stream);

#[doc(hidden)]
impl RawStream {
    /// A new stream over `model`; `profile`: time the three networks.
    pub fn new(model: Arc<Dfn3Model>, profile: bool) -> Self {
        let mut s = Dfn3Stream::new(model);
        s.profile = profile;
        Self(s)
    }

    /// One hop in, one out (480 samples behind); answers the model's local SNR estimate.
    pub fn process(&mut self, input: &[f32; 480], output: &mut [f32; 480]) -> f32 {
        self.0.process(input, output)
    }

    /// Nanoseconds spent so far in the encoder, the ERB decoder and the deep-filter decoder.
    pub fn profile_ns(&self) -> [u64; 3] {
        self.0.prof_ns
    }
}
