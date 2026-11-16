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
//! to [`Dfn3Model::load`], which checks the archive's sha256 against [`WEIGHTS_SHA256`] before
//! reading a weight. A [`Stage`](crate::Stage) built with the model ([`crate::Stage::with_dfn3`])
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

    /// Bytes of weights held in memory.
    pub fn weight_bytes(&self) -> usize {
        self.weights.bytes()
    }
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
