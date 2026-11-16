//! `sew-noise`: microphone noise removal, a thin API over [`nnnoiseless`] (Joe Neeman's
//! pure-Rust port of RNNoise, by Jean-Marc Valin and Mozilla; BSD-3-Clause, pinned exactly).
//!
//! - [`Stage`]: the native capture stage. One [`FRAME`] (480 samples, 10 ms) block of 48 kHz
//!   mono `f32` in place, a fixed latency of one block, a [`Mode`] that any thread can change
//!   at any block without a click, and per-block timing counters ([`StageStats`]).
//! - [`Denoiser`]: one frame at a time, [`FRAME`] samples of 48 kHz mono `f32` in the range
//!   `[-1, 1]`, in and out. The output lags the input by one frame ([`Denoiser::LATENCY`],
//!   10 ms), RNNoise's overlap-add.
//! - [`StreamDenoiser`]: any block size at any rate (8–192 kHz). It buffers to frames,
//!   resamples linearly to and from 48 kHz when the rate differs, and keeps a fixed latency
//!   ([`StreamDenoiser::latency_samples`]). This is what a Web Audio worklet calls.
//!
//! No allocation after construction, no C, no unsafe. Non-finite input samples are read as
//! silence (one NaN would otherwise poison the RNN state for good) and the output is always
//! finite.
//!
//! RNNoise's model is compiled in from `nnnoiseless`; without feature `dfn3` nothing is
//! loaded at run time.
//!
//! Cargo features:
//! - `capture-stage`: the `capture_stage` module, the capture-hook trait an audio pipeline
//!   calls a [`Stage`] through.
//! - `dfn3`: [`Mode::Strong`], DeepFilterNet3's low-latency model (by Hendrik Schröter and
//!   the DeepFilterNet authors; MIT OR Apache-2.0), run by inference code in this crate. Its
//!   weights are not in the crate: an app ships them beside itself and loads them with
//!   `dfn3::Dfn3Model::load`, which checks their pinned sha256 first (RustCrypto's `sha2`).
//!   The feature adds `sha2` (and `realfft`, which `nnnoiseless` already brings), and no C, no
//!   unsafe code and no neural-network runtime.

#[cfg(feature = "capture-stage")]
pub mod capture_stage;
#[cfg(feature = "dfn3")]
pub mod dfn3;
mod resample;
mod stage;
mod standard;
mod stream;
mod strong;

pub use resample::Linear;
pub use stage::{Mode, ModeUnavailable, Stage, StageControl, StageStats, warm_up_thread};
pub use stream::StreamDenoiser;
pub use strong::{StepDownReason, StrongState};

use nnnoiseless::DenoiseState;

/// RNNoise's only rate.
pub const SAMPLE_RATE: u32 = 48_000;
/// Samples per frame (10 ms at 48 kHz).
pub const FRAME: usize = DenoiseState::FRAME_SIZE;
/// The lowest context rate [`StreamDenoiser`] takes.
pub const MIN_RATE: u32 = 8_000;
/// The highest context rate [`StreamDenoiser`] takes.
pub const MAX_RATE: u32 = 192_000;

/// RNNoise works on `f32` holding 16-bit PCM values.
const SCALE: f32 = 32_768.0;

/// A refused call. Nothing is processed when one is returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// A frame call's input or output is not [`FRAME`] samples.
    FrameLength {
        /// The input's length.
        input: usize,
        /// The output's length.
        output: usize,
    },
    /// A stream call's input and output differ in length, or exceed the block size given at
    /// construction.
    BlockLength {
        /// The input's length.
        input: usize,
        /// The output's length.
        output: usize,
        /// The largest block the stream takes.
        max: usize,
    },
    /// The context rate is outside [`MIN_RATE`]..=[`MAX_RATE`].
    SampleRate(u32),
    /// The largest block is 0 or above 16384 samples.
    MaxBlock(usize),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FrameLength { input, output } => write!(
                f,
                "a frame is {FRAME} samples in and out (got {input} in, {output} out)"
            ),
            Self::BlockLength { input, output, max } => write!(
                f,
                "a block is the same length in and out, at most {max} samples (got {input} in, {output} out)"
            ),
            Self::SampleRate(r) => write!(
                f,
                "the sample rate must be {MIN_RATE}..={MAX_RATE} Hz (got {r})"
            ),
            Self::MaxBlock(n) => write!(f, "the largest block must be 1..=16384 samples (got {n})"),
        }
    }
}

impl std::error::Error for Error {}

/// README.md's examples, compiled and run as doc tests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeExamples;

/// One RNNoise state: 480-sample frames of 48 kHz mono `f32` in `[-1, 1]`.
pub struct Denoiser {
    state: Box<DenoiseState<'static>>,
    scaled_in: [f32; FRAME],
    scaled_out: [f32; FRAME],
}

impl std::fmt::Debug for Denoiser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Denoiser").finish_non_exhaustive()
    }
}

impl Default for Denoiser {
    fn default() -> Self {
        Self::new()
    }
}

impl Denoiser {
    /// The output lags the input by this many samples (one frame).
    pub const LATENCY: usize = FRAME;

    /// A fresh state with RNNoise's built-in model (compiled in; nothing is loaded).
    ///
    /// It warms the calling thread up first ([`warm_up_thread`]): nnnoiseless makes (and
    /// allocates) its FFT plans on first use, cached per thread, so the first real frame must
    /// not be the one that allocates (on wasm32 an allocation may grow the memory, which
    /// detaches a worklet's views on it). The warm-up runs a frame through a throwaway state,
    /// never through this one: a frame through the live state changes its history.
    pub fn new() -> Self {
        warm_up_thread();
        Self {
            state: DenoiseState::new(),
            scaled_in: [0.0; FRAME],
            scaled_out: [0.0; FRAME],
        }
    }

    /// Denoises one frame and returns RNNoise's voice probability for it (0 to 1).
    pub fn process_frame(&mut self, input: &[f32], output: &mut [f32]) -> Result<f32, Error> {
        if input.len() != FRAME || output.len() != FRAME {
            return Err(Error::FrameLength {
                input: input.len(),
                output: output.len(),
            });
        }
        for (s, &x) in self.scaled_in.iter_mut().zip(input) {
            *s = if x.is_finite() {
                x.clamp(-1.0, 1.0) * SCALE
            } else {
                0.0
            };
        }
        let vad = self
            .state
            .process_frame(&mut self.scaled_out[..], &self.scaled_in[..]);
        for (y, &s) in output.iter_mut().zip(&self.scaled_out) {
            let v = s / SCALE;
            *y = if v.is_finite() {
                v.clamp(-1.0, 1.0)
            } else {
                0.0
            };
        }
        Ok(if vad.is_finite() {
            vad.clamp(0.0, 1.0)
        } else {
            0.0
        })
    }

    /// Forgets all history (as if new).
    pub fn reset(&mut self) {
        *self = Self::new();
    }
}

#[cfg(test)]
pub(crate) mod test_signals {
    //! The unit tests' "is it suppressing" signal is fan noise (low-passed brown noise plus a
    //! 100 Hz hum with harmonics), which the model takes down by tens of dB; white noise it
    //! takes down by only a few dB, so a white-noise floor cannot tell a working filter from
    //! a bypassed one.

    /// `n` samples of fan noise at 48 kHz with RMS `amp`, deterministic in `seed`.
    pub(crate) fn fan(seed: u64, n: usize, amp: f32) -> Vec<f32> {
        let mut s = seed | 1;
        let (mut brown, mut lp) = (0.0f32, 0.0f32);
        let mut v: Vec<f32> = (0..n)
            .map(|i| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                let w = (s >> 40) as f32 / (1u64 << 23) as f32 - 1.0;
                brown = (brown + w * 0.02) * 0.998;
                lp += 0.05 * (brown - lp);
                let t = i as f32 / 48_000.0;
                let hum: f32 = [(100.0, 1.0), (200.0, 0.5), (300.0, 0.3), (1_250.0, 0.15)]
                    .iter()
                    .map(|(f, a)| a * (2.0 * std::f32::consts::PI * f * t).sin())
                    .sum();
                lp * 8.0 + hum * 0.05
            })
            .collect();
        let rms = (v.iter().map(|x| f64::from(*x).powi(2)).sum::<f64>() / n.max(1) as f64).sqrt();
        for x in &mut v {
            *x = (f64::from(*x) / rms) as f32 * amp;
        }
        v
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn noise(seed: &mut u64, n: usize, amp: f32) -> Vec<f32> {
        (0..n)
            .map(|_| {
                *seed ^= *seed << 13;
                *seed ^= *seed >> 7;
                *seed ^= *seed << 17;
                ((*seed >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0) * amp
            })
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
    }

    #[test]
    fn frame_is_480_at_48k() {
        assert_eq!(FRAME, 480);
        assert_eq!(SAMPLE_RATE as usize / 100, FRAME);
    }

    #[test]
    fn wrong_lengths_are_refused() {
        let mut d = Denoiser::new();
        let mut out = [0.0; FRAME];
        assert_eq!(
            d.process_frame(&[0.0; 479], &mut out),
            Err(Error::FrameLength {
                input: 479,
                output: 480
            })
        );
        let mut short = [0.0; 10];
        assert!(d.process_frame(&[0.0; FRAME], &mut short).is_err());
        assert!(
            Error::FrameLength {
                input: 1,
                output: 2
            }
            .to_string()
            .contains("480")
        );
    }

    #[test]
    fn silence_stays_silent() {
        let mut d = Denoiser::new();
        let mut out = [1.0; FRAME];
        for _ in 0..20 {
            let vad = d.process_frame(&[0.0; FRAME], &mut out).unwrap();
            assert!((0.0..=1.0).contains(&vad));
            assert!(
                out.iter().all(|v| v.abs() < 1e-6),
                "silence in, silence out"
            );
        }
    }

    /// Output over input power (dB) of `x` through a fresh denoiser, after 50 frames of warm-up.
    fn reduction_db(x: &[f32]) -> f32 {
        let mut d = Denoiser::new();
        let mut out = [0.0; FRAME];
        let (mut pin, mut pout) = (0.0f32, 0.0f32);
        for (i, f) in x.as_chunks::<FRAME>().0.iter().enumerate() {
            d.process_frame(f, &mut out).unwrap();
            if i >= 50 {
                pin += rms(f).powi(2);
                pout += rms(&out).powi(2);
            }
        }
        10.0 * (pout / pin).log10()
    }

    /// Fan noise down by at least 40 dB (a bypass gives 0 dB and fails), white
    /// noise never made louder.
    #[test]
    fn fan_noise_is_suppressed_and_white_noise_never_louder() {
        let fan = reduction_db(&test_signals::fan(0x5eed, 300 * FRAME, 0.05));
        assert!(
            fan <= -40.0,
            "fan noise down by at least 40 dB (got {fan:.1} dB)"
        );
        let mut seed = 0x5eed_u64;
        let white = reduction_db(&noise(&mut seed, 300 * FRAME, 0.05));
        assert!(
            white <= 0.0,
            "white noise never made louder (got {white:+.1} dB)"
        );
    }

    #[test]
    fn non_finite_input_does_not_poison_the_state() {
        let mut d = Denoiser::new();
        let mut out = [0.0; FRAME];
        let mut bad = [0.1f32; FRAME];
        bad[3] = f32::NAN;
        bad[7] = f32::INFINITY;
        bad[9] = f32::NEG_INFINITY;
        bad[11] = 7.0;
        d.process_frame(&bad, &mut out).unwrap();
        assert!(out.iter().all(|v| v.is_finite() && v.abs() <= 1.0));
        let mut seed = 9;
        for _ in 0..50 {
            let x = noise(&mut seed, FRAME, 0.2);
            d.process_frame(&x, &mut out).unwrap();
            assert!(out.iter().all(|v| v.is_finite() && v.abs() <= 1.0));
        }
        // After a reset the state is as new: silence gives silence at once.
        d.reset();
        d.process_frame(&[0.0; FRAME], &mut out).unwrap();
        assert!(out.iter().all(|v| v.abs() < 1e-6));
    }

    #[test]
    fn output_lags_by_one_frame() {
        // A loud tone burst in frame 10: its energy comes out in frame 11, not frame 10.
        let mut d = Denoiser::new();
        let mut out = [0.0; FRAME];
        let mut energy = Vec::new();
        for i in 0..14 {
            let x: Vec<f32> = (0..FRAME)
                .map(|n| {
                    if i == 10 {
                        (n as f32 * 0.05).sin() * 0.5
                    } else {
                        0.0
                    }
                })
                .collect();
            d.process_frame(&x, &mut out).unwrap();
            energy.push(rms(&out));
        }
        // The spectral gains spread a little of the burst into its own frame: raw nnnoiseless
        // 0.3.2, 0.4.0 and 0.5.2 all put about 0.0016 rms there against 0.35 in the next frame
        // (-47 dB), so the bound is relative: at least 40 dB under the next frame.
        assert!(
            energy[10] < energy[11] / 100.0,
            "nothing much comes out in the burst's own frame: {energy:?}"
        );
        assert!(
            energy[11] > energy[10] * 10.0 && energy[11] > 1e-3,
            "{energy:?}"
        );
    }
}
