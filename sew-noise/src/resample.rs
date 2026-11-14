//! A linear-interpolation rate converter for the off-48 kHz path of [`crate::StreamDenoiser`].
//!
//! Linear interpolation is enough for voice going into and out of RNNoise at 44.1 kHz (the other
//! common context rate): its images above the input's Nyquist are what RNNoise treats as noise,
//! and 48 → 44.1 kHz folds only 22.05–24 kHz back. Below 44.1 kHz the way down aliases audibly;
//! a web page should run its AudioContext at 48 kHz (no conversion at all).

/// A streaming converter from `from` Hz to `to` Hz. Each input sample emits zero or more output
/// samples; the output lags by at most one input sample.
#[derive(Debug, Clone)]
pub struct Linear {
    /// Input samples per output sample.
    step: f64,
    /// Where the next output falls, in input samples after `prev` (always in `[0, 1)` between
    /// calls).
    pos: f64,
    prev: f32,
}

impl Linear {
    /// A converter from `from` Hz to `to` Hz (both non-zero).
    pub fn new(from: u32, to: u32) -> Self {
        Self {
            step: f64::from(from.max(1)) / f64::from(to.max(1)),
            pos: 0.0,
            prev: 0.0,
        }
    }

    /// Output samples per input sample, rounded up (the most one [`Self::push`] emits).
    pub fn max_per_input(&self) -> usize {
        (1.0 / self.step).ceil() as usize + 1
    }

    /// Feeds one input sample; `emit` gets the output samples it completes, in order.
    pub fn push(&mut self, x: f32, emit: &mut impl FnMut(f32)) {
        while self.pos < 1.0 {
            let t = self.pos as f32;
            emit(self.prev + (x - self.prev) * t);
            self.pos += self.step;
        }
        self.pos -= 1.0;
        self.prev = x;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(from: u32, to: u32, input: &[f32]) -> Vec<f32> {
        let mut r = Linear::new(from, to);
        let mut out = Vec::new();
        for &x in input {
            r.push(x, &mut |y| out.push(y));
        }
        out
    }

    #[test]
    fn same_rate_is_a_one_sample_delay() {
        let input: Vec<f32> = (0..100).map(|i| i as f32).collect();
        let out = run(48_000, 48_000, &input);
        assert_eq!(out.len(), 100);
        assert_eq!(out[0], 0.0);
        assert_eq!(out[1..], input[..99]);
    }

    #[test]
    fn rates_hold_over_a_second() {
        for (from, to) in [
            (44_100, 48_000),
            (48_000, 44_100),
            (16_000, 48_000),
            (48_000, 96_000),
            (96_000, 48_000),
            (8_000, 48_000),
            (48_000, 192_000),
        ] {
            let out = run(from, to, &vec![0.0; from as usize]);
            let diff = out.len() as i64 - i64::from(to);
            assert!(diff.abs() <= 1, "{from} -> {to}: {} samples", out.len());
        }
    }

    #[test]
    fn a_ramp_stays_a_ramp() {
        // Linear interpolation is exact on a straight line (after the first sample's delay).
        let input: Vec<f32> = (0..441).map(|i| i as f32 / 441.0).collect();
        let out = run(44_100, 48_000, &input);
        let step = 44_100.0 / 48_000.0;
        for (m, y) in out.iter().enumerate().skip(2) {
            let want = (m as f64 * step - 1.0) / 441.0;
            assert!((f64::from(*y) - want).abs() < 1e-5, "{m}: {y} vs {want}");
        }
    }

    #[test]
    fn a_tone_survives_the_round_trip() {
        let tone: Vec<f32> = (0..44_100)
            .map(|n| (2.0 * std::f32::consts::PI * 440.0 * n as f32 / 44_100.0).sin() * 0.5)
            .collect();
        let up = run(44_100, 48_000, &tone);
        let back = run(48_000, 44_100, &up);
        // Compare away from the edges, with the two-sample delay of the two converters.
        let (err, sig) = back[1_000..40_000]
            .iter()
            .zip(&tone[998..39_998])
            .fold((0.0f64, 0.0f64), |(e, s), (b, t)| {
                (e + f64::from(b - t).powi(2), s + f64::from(*t).powi(2))
            });
        let snr = 10.0 * (sig / err).log10();
        assert!(snr > 30.0, "440 Hz round trip SNR {snr:.1} dB");
    }

    #[test]
    fn max_per_input_bounds_push() {
        for (from, to) in [
            (8_000, 48_000),
            (44_100, 48_000),
            (48_000, 44_100),
            (48_000, 192_000),
        ] {
            let mut r = Linear::new(from, to);
            let max = r.max_per_input();
            for i in 0..10_000 {
                let mut n = 0;
                r.push(i as f32, &mut |_| n += 1);
                assert!(n <= max, "{from} -> {to}: {n} > {max}");
            }
        }
    }
}
