//! [`StreamDenoiser`]: blocks of any size at any context rate, a fixed latency, no allocation
//! after construction.

use std::collections::VecDeque;

use crate::standard::{FLOOR, Standard};
use crate::{Denoiser, Error, FRAME, Linear, MAX_RATE, MIN_RATE, SAMPLE_RATE};

/// The largest block a stream takes (the Web Audio render quantum is 128; 16384 leaves room
/// for a future configurable quantum).
const MAX_BLOCK: usize = 16_384;

/// The 48 kHz half: frames in, denoised frames out.
#[derive(Debug)]
struct Core {
    den: Denoiser,
    frame_in: [f32; FRAME],
    filled: usize,
    frame_out: [f32; FRAME],
    /// The previous input frame: the bypass output, so turning suppression off keeps the
    /// latency (RNNoise's output is one frame late too), and what Standard keeps of the input.
    prev_in: [f32; FRAME],
    /// The floor and the onset guard, as the native stage has them.
    standard: Standard,
    guard_opens: u64,
    enabled: bool,
    vad: f32,
}

impl Core {
    fn push(&mut self, y: f32, sink: &mut impl FnMut(f32)) {
        self.frame_in[self.filled] = y;
        self.filled += 1;
        if self.filled < FRAME {
            return;
        }
        self.filled = 0;
        // RNNoise keeps running while bypassed, so turning it back on has no warm-up.
        let vad = self
            .den
            .process_frame(&self.frame_in, &mut self.frame_out)
            .unwrap_or(0.0);
        self.vad = vad;
        // The guard follows the input while bypassed too, so it is right when turned back on.
        let step = self.standard.step(&self.frame_in);
        self.guard_opens += u64::from(step.opened);
        if self.enabled {
            for (i, (&wet, &dry)) in self.frame_out.iter().zip(&self.prev_in).enumerate() {
                // A non-finite input sample is silence, as the model reads it.
                let dry = if dry.is_finite() { dry } else { 0.0 };
                sink(step.mixed(i, wet, dry));
            }
        } else {
            for &s in &self.prev_in {
                sink(s);
            }
        }
        self.prev_in = self.frame_in;
    }
}

/// Noise removal for a stream of blocks at the context rate (what a Web Audio worklet runs).
///
/// The output is the model's with the same floor and onset guard as [`crate::Stage`] in
/// Standard: the input always stays in the output 30 dB under its own level, and it passes
/// unchanged for 40 ms from a lasting, not spiky, 20 dB rise in level. Neither adds latency:
/// the stream already holds the frame before the newest one.
///
/// At 48 kHz the latency is two frames, 960 samples (20 ms): one frame of buffering (render
/// quanta do not line up with 480-sample frames) and RNNoise's own frame. At other rates the
/// stream converts to 48 kHz and back ([`Linear`]) and the latency is about the same 20 ms
/// ([`Self::latency_samples`]).
#[derive(Debug)]
pub struct StreamDenoiser {
    core: Core,
    rate: u32,
    max_block: usize,
    /// Context rate to 48 kHz, and back; `None` at 48 kHz.
    conv: Option<(Linear, Linear)>,
    out: VecDeque<f32>,
    latency: usize,
    underruns: u64,
}

impl StreamDenoiser {
    /// A stream at `rate` Hz taking blocks of up to `max_block` samples.
    pub fn new(rate: u32, max_block: usize) -> Result<Self, Error> {
        if !(MIN_RATE..=MAX_RATE).contains(&rate) {
            return Err(Error::SampleRate(rate));
        }
        if max_block == 0 || max_block > MAX_BLOCK {
            return Err(Error::MaxBlock(max_block));
        }
        let conv = (rate != SAMPLE_RATE).then(|| {
            (
                Linear::new(rate, SAMPLE_RATE),
                Linear::new(SAMPLE_RATE, rate),
            )
        });
        // One frame at the context rate, plus two samples for the converters' rounding.
        let frame_at_rate = (FRAME as u64 * u64::from(rate)).div_ceil(u64::from(SAMPLE_RATE));
        let prefill = if conv.is_some() {
            frame_at_rate as usize + 2
        } else {
            FRAME
        };
        // The queue never holds more than the prefill, a frame and a block (at the context
        // rate); twice that is room to spare, so pushing never allocates.
        let mut out = VecDeque::with_capacity(2 * (prefill + frame_at_rate as usize + max_block));
        out.extend(std::iter::repeat_n(0.0, prefill));
        let latency = prefill
            + frame_at_rate as usize
            + if conv.is_some() {
                // Each converter lags by up to one of its input samples.
                2
            } else {
                0
            };
        Ok(Self {
            core: Core {
                den: Denoiser::new(),
                frame_in: [0.0; FRAME],
                filled: 0,
                frame_out: [0.0; FRAME],
                prev_in: [0.0; FRAME],
                standard: Standard::new(FLOOR),
                guard_opens: 0,
                enabled: true,
                vad: 0.0,
            },
            rate,
            max_block,
            conv,
            out,
            latency,
            underruns: 0,
        })
    }

    /// Denoises one block: `output` gets as many samples as `input` holds, `latency_samples`
    /// late.
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> Result<(), Error> {
        if input.len() != output.len() || input.len() > self.max_block {
            return Err(Error::BlockLength {
                input: input.len(),
                output: output.len(),
                max: self.max_block,
            });
        }
        let Self {
            core, conv, out, ..
        } = self;
        match conv {
            None => {
                for &x in input {
                    core.push(x, &mut |s| out.push_back(s));
                }
            }
            Some((up, down)) => {
                let mut to_out = |s: f32| down.push(s, &mut |z| out.push_back(z));
                for &x in input {
                    up.push(x, &mut |y| core.push(y, &mut to_out));
                }
            }
        }
        for o in output.iter_mut() {
            *o = match self.out.pop_front() {
                Some(s) => s,
                None => {
                    self.underruns += 1;
                    0.0
                }
            };
        }
        Ok(())
    }

    /// How late the output is, in samples at the context rate (exact at 48 kHz, within two
    /// samples otherwise).
    pub fn latency_samples(&self) -> usize {
        self.latency
    }

    /// Output samples that had to be made up as silence (0 unless the block sizes are wild).
    pub fn underruns(&self) -> u64 {
        self.underruns
    }

    /// The context rate.
    pub fn sample_rate(&self) -> u32 {
        self.rate
    }

    /// The largest block [`Self::process`] takes.
    pub fn max_block(&self) -> usize {
        self.max_block
    }

    /// On (the default) or bypassed. Bypass passes the input through at the same latency.
    pub fn set_enabled(&mut self, on: bool) {
        self.core.enabled = on;
    }

    /// Whether suppression is on.
    pub fn enabled(&self) -> bool {
        self.core.enabled
    }

    /// How many times the onset guard opened (let the input through at a lasting rise in
    /// level).
    pub fn guard_opens(&self) -> u64 {
        self.core.guard_opens
    }

    /// RNNoise's voice probability for the latest frame (0 to 1).
    pub fn voice_probability(&self) -> f32 {
        self.core.vad
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn tone(rate: u32, n: usize, hz: f32, amp: f32) -> Vec<f32> {
        (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * hz * i as f32 / rate as f32).sin() * amp)
            .collect()
    }

    fn run(s: &mut StreamDenoiser, input: &[f32], block: usize) -> Vec<f32> {
        let mut out = vec![0.0; input.len()];
        for (i, o) in input.chunks(block).zip(out.chunks_mut(block)) {
            s.process(i, o).unwrap();
        }
        out
    }

    #[test]
    fn bad_arguments_are_refused() {
        assert_eq!(
            StreamDenoiser::new(7_999, 128).unwrap_err(),
            Error::SampleRate(7_999)
        );
        assert_eq!(
            StreamDenoiser::new(192_001, 128).unwrap_err(),
            Error::SampleRate(192_001)
        );
        assert_eq!(
            StreamDenoiser::new(48_000, 0).unwrap_err(),
            Error::MaxBlock(0)
        );
        assert!(StreamDenoiser::new(48_000, 16_385).is_err());
        let mut s = StreamDenoiser::new(48_000, 128).unwrap();
        let mut out = [0.0; 129];
        assert!(s.process(&[0.0; 129], &mut out).is_err());
        let mut out = [0.0; 64];
        assert!(s.process(&[0.0; 128], &mut out[..]).is_err());
        assert!(s.process(&[0.0; 64], &mut out).is_ok());
    }

    #[test]
    fn latency_at_48k_is_two_frames_exactly() {
        // Bypassed, the stream is a pure delay: an impulse comes out exactly latency later.
        for block in [128, 480, 100, 1] {
            let mut s = StreamDenoiser::new(48_000, block.max(128)).unwrap();
            assert_eq!(s.latency_samples(), 960);
            s.set_enabled(false);
            assert!(!s.enabled());
            let mut x = vec![0.0; 4_800];
            x[1_000] = 0.75;
            let y = run(&mut s, &x, block);
            let at = y.iter().position(|v| *v != 0.0).unwrap();
            assert_eq!(at, 1_000 + 960, "block {block}");
            assert_eq!(y[at], 0.75);
            assert_eq!(s.underruns(), 0);
        }
    }

    #[test]
    fn latency_off_48k_is_as_reported() {
        for rate in [44_100, 32_000, 16_000, 96_000] {
            let mut s = StreamDenoiser::new(rate, 128).unwrap();
            s.set_enabled(false);
            let n = rate as usize / 2;
            // A five-sample triangle, not a one-sample impulse: the linear converter down from
            // 96 kHz takes every other sample and would skip a lone impulse entirely.
            let mut x = vec![0.0; n];
            x[1_998..=2_002].copy_from_slice(&[0.25, 0.5, 0.75, 0.5, 0.25]);
            let y = run(&mut s, &x, 128);
            // The peak of the interpolated impulse.
            let (at, _) = y
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
                .unwrap();
            let lat = s.latency_samples() as i64;
            let got = at as i64 - 2_000;
            assert!(
                (got - lat).abs() <= 2,
                "{rate} Hz: impulse {got} samples late, latency_samples says {lat}"
            );
            assert_eq!(s.underruns(), 0, "{rate} Hz");
        }
    }

    #[test]
    fn no_underrun_over_a_minute_at_any_rate() {
        for rate in [48_000, 44_100, 22_050, 88_200, 192_000, 8_000] {
            let mut s = StreamDenoiser::new(rate, 128).unwrap();
            let block = vec![0.01; 128];
            let mut out = vec![0.0; 128];
            for _ in 0..(rate as usize * 60 / 128) {
                s.process(&block, &mut out).unwrap();
            }
            assert_eq!(s.underruns(), 0, "{rate} Hz");
            assert_eq!(s.sample_rate(), rate);
            assert_eq!(s.max_block(), 128);
        }
    }

    #[test]
    fn queue_never_reallocates() {
        for rate in [48_000, 44_100, 8_000, 192_000] {
            for block in [1, 127, 128, 480, 1_024] {
                let mut s = StreamDenoiser::new(rate, 1_024).unwrap();
                let cap = s.out.capacity();
                let x = vec![0.0; block];
                let mut y = vec![0.0; block];
                for _ in 0..(rate as usize / block).max(400) {
                    s.process(&x, &mut y).unwrap();
                }
                assert_eq!(s.out.capacity(), cap, "{rate} Hz, block {block}");
            }
        }
    }

    #[test]
    fn bypass_is_the_input_exactly_960_samples_late_at_48k() {
        let mut s = StreamDenoiser::new(48_000, 128).unwrap();
        s.set_enabled(false);
        let mut seed = 5u64;
        let x: Vec<f32> = (0..48_000)
            .map(|i| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                ((seed >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0) * 0.1
                    + (i as f32 * 0.01).sin() * 0.2
            })
            .collect();
        let y = run(&mut s, &x, 128);
        assert_eq!(s.latency_samples(), 960);
        assert!(y[..960].iter().all(|v| *v == 0.0));
        for i in 960..x.len() {
            assert_eq!(y[i], x[i - 960], "sample {i}");
        }
    }

    #[test]
    fn a_tone_passes_bypass_unchanged_at_44k1() {
        let mut s = StreamDenoiser::new(44_100, 128).unwrap();
        s.set_enabled(false);
        let x = tone(44_100, 44_100, 300.0, 0.5);
        let y = run(&mut s, &x, 128);
        let lat = s.latency_samples();
        // The converters' fractional delay makes the best integer lag within two samples.
        let best = (lat - 2..=lat + 2)
            .map(|l| {
                (10_000..40_000)
                    .map(|n| f64::from(y[n] - x[n - l]).powi(2))
                    .sum::<f64>()
            })
            .fold(f64::MAX, f64::min);
        let p: f64 = x[10_000..40_000]
            .iter()
            .map(|v| f64::from(*v).powi(2))
            .sum();
        let snr = 10.0 * (p / best).log10();
        assert!(snr > 25.0, "bypass round trip at 44.1 kHz: {snr:.1} dB");
    }

    #[test]
    fn enabled_suppresses_and_reports_voice_probability() {
        // Fan noise sits on the -30 dB floor (a bypass gives 0 dB and fails).
        let mut s = StreamDenoiser::new(48_000, 128).unwrap();
        let x = crate::test_signals::fan(77, 48_000 * 3, 0.05);
        let y = run(&mut s, &x, 128);
        let p = |v: &[f32]| v.iter().map(|a| f64::from(*a).powi(2)).sum::<f64>();
        let db = 10.0 * (p(&y[48_000..]) / p(&x[48_000..])).log10();
        assert!(
            (-30.5..=-29.0).contains(&db),
            "fan noise through the stream: {db:.1} dB"
        );
        assert_eq!(s.guard_opens(), 0, "a steady noise opens no guard");
        assert!((0.0..=1.0).contains(&s.voice_probability()));
        assert!(s.enabled());
    }
}
