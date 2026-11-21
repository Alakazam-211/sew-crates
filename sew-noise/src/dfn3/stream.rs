//! One stream through DeepFilterNet3's low-latency model: 480 samples of 48 kHz in, 480 out,
//! 480 samples (10 ms) behind. The signal path of the authors' real-time runtime (libDF) for
//! one channel, without its post-filter or attenuation limit: a 960-point STFT with a Vorbis
//! window and a 480 hop, ERB and complex features with running normalisation, the encoder, ERB
//! gains on the current frame and a 5-tap deep filter over the lowest 96 bins.
//!
//! Everything is allocated in [`Dfn3Stream::new`]; [`Dfn3Stream::process`] allocates nothing,
//! takes no lock and does not panic. A hop can also be spread over time: [`Dfn3Stream::begin`],
//! [`Dfn3Stream::step`] (half a GRU's worth of the networks at a time) and [`Dfn3Stream::end`],
//! with the same output bit for bit.
//!
//! Ported from libDF, DeepFilterNet's Rust runtime: `libDF/src/lib.rs` (the STFT, the ERB
//! bands, the feature normalisation) and `libDF/src/tract.rs` (the per-hop path: the SNR
//! thresholds, the gains, the deep filter), github.com/Rikorose/DeepFilterNet at commit
//! 978576aa8400552a4ce9730838c635aa30db5e61, MIT OR Apache-2.0. Copyright (c) 2021 Hendrik
//! Schröter. DeepFilterNet is the work of Hendrik Schröter and the DeepFilterNet authors.

// The signal path indexes several arrays with one bin number.
#![allow(clippy::needless_range_loop)]

use std::sync::Arc;

use realfft::num_complex::Complex32;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

use super::Dfn3Model;
use super::net::{self, DF_ORDER, NB_DF, NB_ERB, NetState};

const SR: usize = 48_000;
const FFT: usize = 960;
const HOP: usize = 480;
const BINS: usize = FFT / 2 + 1;
const MIN_ERB_FREQS: usize = 2;
/// libDF's thresholds on the local SNR estimate (dB).
const MIN_DB_THRESH: f32 = -10.0;
const MAX_DB_ERB_THRESH: f32 = 30.0;
const MAX_DB_DF_THRESH: f32 = 20.0;
/// The normalisation's smoothing: libDF's `calc_norm_alpha` for a 1 s time constant at a
/// 10 ms hop (exp(-0.01), rounded to the fewest decimals under 1).
const ALPHA: f32 = 0.99;

/// libDF's `erb_fb`: how many bins each ERB band covers.
fn erb_widths() -> [usize; NB_ERB] {
    let freq2erb = |f: f32| 9.265 * (f / (24.7 * 9.265)).ln_1p();
    let erb2freq = |n: f32| 24.7 * 9.265 * ((n / 9.265).exp() - 1.);
    let nyq = SR / 2;
    let freq_width = SR as f32 / FFT as f32;
    let erb_low = freq2erb(0.);
    let erb_high = freq2erb(nyq as f32);
    let mut erb = [0usize; NB_ERB];
    let step = (erb_high - erb_low) / NB_ERB as f32;
    let min = MIN_ERB_FREQS as i32;
    let mut prev = 0usize;
    let mut over = 0i32;
    for (i, e) in erb.iter_mut().enumerate() {
        let f = erb2freq(erb_low + (i + 1) as f32 * step);
        let fb = (f / freq_width).round() as usize;
        let mut nb = fb as i32 - prev as i32 - over;
        if nb < min {
            over = min - nb;
            nb = min;
        } else {
            over = 0;
        }
        *e = nb as usize;
        prev = fb;
    }
    erb[NB_ERB - 1] += 1;
    let total: usize = erb.iter().sum();
    if total > BINS {
        erb[NB_ERB - 1] -= total - BINS;
    }
    erb
}

/// libDF's Vorbis window.
fn vorbis_window() -> Vec<f32> {
    let pi = std::f64::consts::PI;
    let h = FFT / 2;
    (0..FFT)
        .map(|i| {
            let s = (0.5 * pi * (i as f64 + 0.5) / h as f64).sin();
            (0.5 * pi * s * s).sin() as f32
        })
        .collect()
}

/// Where a hop is ([`Dfn3Stream::begin`], [`Dfn3Stream::step`], [`Dfn3Stream::end`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// No hop begun.
    Idle,
    /// The encoder's first part runs next (then its second).
    Encoder,
    EncoderB,
    /// The ERB decoder's parts run next (four).
    ErbA,
    ErbA2,
    ErbB,
    ErbB2,
    /// The deep-filter decoder's parts run next (six).
    DfA,
    DfA2,
    DfB,
    DfB2,
    DfC,
    DfC2,
    /// The networks are done (or the hop is muted as silence); [`Dfn3Stream::end`] finishes it.
    Done,
}

/// One stream's state.
pub(crate) struct Dfn3Stream {
    model: Arc<Dfn3Model>,
    net: NetState,
    erb: [usize; NB_ERB],
    window: Vec<f32>,
    fwd: Arc<dyn RealToComplex<f32>>,
    inv: Arc<dyn ComplexToReal<f32>>,
    analysis_mem: [f32; HOP],
    synthesis_mem: [f32; HOP],
    fft_in: Vec<f32>,
    fft_out: Vec<f32>,
    spec: Vec<Complex32>,
    out_spec: Vec<Complex32>,
    scratch_f: Vec<Complex32>,
    scratch_i: Vec<Complex32>,
    mean_norm: [f32; NB_ERB],
    unit_norm: [f32; NB_DF],
    /// The last `DF_ORDER` spectra after the gains (`rolling_y`) and as they came in
    /// (`rolling_x`), the oldest at `head`.
    rolling_y: Vec<Vec<Complex32>>,
    rolling_x: Vec<Vec<Complex32>>,
    head: usize,
    skip_counter: usize,
    /// Measurement only: nanoseconds spent in the encoder, the ERB decoder and the deep-filter
    /// decoder, summed, when `profile` is set.
    pub(crate) profile: bool,
    pub(crate) prof_ns: [u64; 3],
    erb_feat: [f32; NB_ERB],
    cplx_feat: [f32; 2 * NB_DF],
    gains: [f32; NB_ERB],
    coefs: [f32; NB_DF * DF_ORDER * 2],
    /// The hop in progress: where it is, its newest spectrum slot, the SNR estimate and what it
    /// runs, and whether it is muted as silence.
    phase: Phase,
    newest: usize,
    lsnr: f32,
    apply_gains: bool,
    gain_zeros: bool,
    apply_df: bool,
    skipped: bool,
}

impl std::fmt::Debug for Dfn3Stream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dfn3Stream").finish_non_exhaustive()
    }
}

/// The window's half sums: the synthesis tail's weight at each sample of the next hop.
pub(crate) fn tail_weight(window: &[f32], i: usize) -> f32 {
    let w = window[HOP + i];
    w * w
}

impl Dfn3Stream {
    pub(crate) fn new(model: Arc<Dfn3Model>) -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        let fwd = planner.plan_fft_forward(FFT);
        let inv = planner.plan_fft_inverse(FFT);
        let zeros = vec![Complex32::default(); BINS];
        let mut s = Self {
            net: NetState::new(),
            erb: erb_widths(),
            window: vorbis_window(),
            scratch_f: fwd.make_scratch_vec(),
            scratch_i: inv.make_scratch_vec(),
            fwd,
            inv,
            analysis_mem: [0.0; HOP],
            synthesis_mem: [0.0; HOP],
            fft_in: vec![0.0; FFT],
            fft_out: vec![0.0; FFT],
            spec: zeros.clone(),
            out_spec: zeros.clone(),
            mean_norm: [0.0; NB_ERB],
            unit_norm: [0.0; NB_DF],
            rolling_y: (0..DF_ORDER).map(|_| zeros.clone()).collect(),
            rolling_x: (0..DF_ORDER).map(|_| zeros.clone()).collect(),
            head: 0,
            skip_counter: 0,
            profile: false,
            prof_ns: [0; 3],
            erb_feat: [0.0; NB_ERB],
            cplx_feat: [0.0; 2 * NB_DF],
            gains: [0.0; NB_ERB],
            coefs: [0.0; NB_DF * DF_ORDER * 2],
            model,
            phase: Phase::Idle,
            newest: 0,
            lsnr: 0.0,
            apply_gains: false,
            gain_zeros: false,
            apply_df: false,
            skipped: false,
        };
        s.clear();
        s
    }

    /// Back to a new stream's state, without allocating.
    pub(crate) fn clear(&mut self) {
        self.net.clear();
        self.analysis_mem = [0.0; HOP];
        self.synthesis_mem = [0.0; HOP];
        for (b, v) in self.mean_norm.iter_mut().enumerate() {
            *v = -60.0 + b as f32 * (-90.0 - -60.0) / (NB_ERB - 1) as f32;
        }
        for (f, v) in self.unit_norm.iter_mut().enumerate() {
            *v = 0.001 + f as f32 * (0.0001 - 0.001) / (NB_DF - 1) as f32;
        }
        for v in self.rolling_y.iter_mut().chain(self.rolling_x.iter_mut()) {
            v.fill(Complex32::default());
        }
        self.head = 0;
        self.skip_counter = 0;
        self.phase = Phase::Idle;
    }

    /// The synthesis memory: what this stream would add to the start of its next output hop.
    /// With [`tail_weight`] it fades the model's output out without running the model again.
    pub(crate) fn tail(&self) -> &[f32; HOP] {
        &self.synthesis_mem
    }

    /// What the last hop ran: the ERB decoder, and the deep-filter decoder (the encoder runs in
    /// every hop that is not muted as silence).
    #[cfg(feature = "probe")]
    pub(crate) fn decoders_ran(&self) -> (bool, bool) {
        (
            !self.skipped && self.apply_gains,
            !self.skipped && self.apply_df,
        )
    }

    /// The last block was muted as silence (the model did not run).
    pub(crate) fn skipping(&self) -> bool {
        self.skip_counter > 5
    }

    pub(crate) fn window(&self) -> &[f32] {
        &self.window
    }

    fn analysis(&mut self, input: &[f32; HOP]) {
        for i in 0..HOP {
            self.fft_in[i] = self.analysis_mem[i] * self.window[i];
            self.fft_in[HOP + i] = input[i] * self.window[HOP + i];
        }
        self.analysis_mem = *input;
        // The lengths are the plan's, so this cannot fail; on an error the spectrum is
        // whatever the buffer held, never a panic.
        let _ =
            self.fwd
                .process_with_scratch(&mut self.fft_in, &mut self.spec, &mut self.scratch_f);
        let wnorm = 1.0 / ((FFT * FFT) as f32 / (2 * HOP) as f32);
        for x in self.spec.iter_mut() {
            *x *= wnorm;
        }
    }

    /// Inverse FFT of `self.out_spec` and overlap-add into `output`.
    fn synthesis(&mut self, output: &mut [f32; HOP]) {
        self.out_spec[0].im = 0.0;
        self.out_spec[BINS - 1].im = 0.0;
        let _ = self.inv.process_with_scratch(
            &mut self.out_spec,
            &mut self.fft_out,
            &mut self.scratch_i,
        );
        for (x, w) in self.fft_out.iter_mut().zip(&self.window) {
            *x *= w;
        }
        for i in 0..HOP {
            output[i] = self.fft_out[i] + self.synthesis_mem[i];
            self.synthesis_mem[i] = self.fft_out[HOP + i];
        }
    }

    /// One hop in, one hop out (libDF's `DfTract::process` for one channel). Answers the local
    /// SNR estimate (dB). The same as [`Self::begin`], [`Self::step`] until it answers true,
    /// and [`Self::end`].
    pub(crate) fn process(&mut self, noisy: &[f32; HOP], enh: &mut [f32; HOP]) -> f32 {
        self.begin(noisy);
        self.end(enh)
    }

    /// Starts a hop: the silence check, the analysis and the features (a small part of the
    /// work). The networks run in [`Self::step`]s; [`Self::end`] finishes the hop.
    pub(crate) fn begin(&mut self, noisy: &[f32; HOP]) {
        let e: f32 = noisy.iter().map(|x| x * x).sum();
        if e / HOP as f32 > 1e-7 {
            self.skip_counter = 0;
        } else {
            self.skip_counter += 1;
        }
        if self.skip_counter > 5 {
            self.skipped = true;
            self.phase = Phase::Done;
            return;
        }
        self.skipped = false;
        self.analysis(noisy);
        // The oldest slot becomes the newest.
        self.newest = self.head;
        self.head = (self.head + 1) % DF_ORDER;
        self.rolling_y[self.newest].copy_from_slice(&self.spec);
        self.rolling_x[self.newest].copy_from_slice(&self.spec);

        // Features of the newest frame.
        let mut k0 = 0;
        for (b, &n) in self.erb.iter().enumerate() {
            let mut acc = 0.0;
            for x in &self.spec[k0..k0 + n] {
                acc += x.norm_sqr();
            }
            k0 += n;
            let v = (acc / n as f32 + 1e-10).log10() * 10.;
            let s = &mut self.mean_norm[b];
            *s = v * (1. - ALPHA) + *s * ALPHA;
            self.erb_feat[b] = (v - *s) / 40.;
        }
        for f in 0..NB_DF {
            let x = self.spec[f];
            let s = &mut self.unit_norm[f];
            *s = x.norm() * (1. - ALPHA) + *s * ALPHA;
            let d = s.sqrt();
            self.cplx_feat[f] = x.re / d;
            self.cplx_feat[NB_DF + f] = x.im / d;
        }
        self.phase = Phase::Encoder;
    }

    /// Parts of the networks' work still to run in this hop (an upper bound until the encoder
    /// has run: which decoders run depends on its SNR estimate).
    pub(crate) fn steps_left(&self) -> u32 {
        let df = if self.apply_df { 6 } else { 0 };
        match self.phase {
            Phase::Encoder => 12,
            Phase::EncoderB => 11,
            Phase::ErbA => 4 + df,
            Phase::ErbA2 => 3 + df,
            Phase::ErbB => 2 + df,
            Phase::ErbB2 => 1 + df,
            Phase::DfA => 6,
            Phase::DfA2 => 5,
            Phase::DfB => 4,
            Phase::DfB2 => 3,
            Phase::DfC => 2,
            Phase::DfC2 => 1,
            Phase::Idle | Phase::Done => 0,
        }
    }

    /// Runs one part of the networks' work (half a GRU and what goes with it); answers true
    /// once none is left. The parts run the operations of a one-shot hop, in the same order, so
    /// the output is the same bit for bit however the parts are spread.
    pub(crate) fn step(&mut self) -> bool {
        let t = self.profile.then(std::time::Instant::now);
        let w = &self.model.weights;
        let after_erb = if self.apply_df {
            Phase::DfA
        } else {
            Phase::Done
        };
        let k = match self.phase {
            Phase::Idle | Phase::Done => return true,
            Phase::Encoder => {
                net::encode_a(w, &mut self.net, &self.erb_feat, &self.cplx_feat);
                self.phase = Phase::EncoderB;
                0
            }
            Phase::EncoderB => {
                self.lsnr = net::encode_b(w, &mut self.net);
                let lsnr = self.lsnr;
                (self.apply_gains, self.gain_zeros, self.apply_df) = if lsnr < MIN_DB_THRESH {
                    (false, true, false)
                } else if lsnr > MAX_DB_ERB_THRESH {
                    (false, false, false)
                } else if lsnr > MAX_DB_DF_THRESH {
                    (true, false, false)
                } else {
                    (true, false, true)
                };
                if self.gain_zeros {
                    self.gains = [0.0; NB_ERB];
                }
                self.phase = if self.apply_gains {
                    Phase::ErbA
                } else if self.apply_df {
                    Phase::DfA
                } else {
                    Phase::Done
                };
                0
            }
            Phase::ErbA => {
                net::decode_erb_a(w, &mut self.net);
                self.phase = Phase::ErbA2;
                1
            }
            Phase::ErbA2 => {
                net::decode_erb_a2(w, &mut self.net);
                self.phase = Phase::ErbB;
                1
            }
            Phase::ErbB => {
                net::decode_erb_b(w, &mut self.net);
                self.phase = Phase::ErbB2;
                1
            }
            Phase::ErbB2 => {
                net::decode_erb_b2(w, &mut self.net, &mut self.gains);
                self.phase = after_erb;
                1
            }
            Phase::DfA => {
                net::decode_df_a(w, &mut self.net);
                self.phase = Phase::DfA2;
                2
            }
            Phase::DfA2 => {
                net::decode_df_a2(w, &mut self.net);
                self.phase = Phase::DfB;
                2
            }
            Phase::DfB => {
                net::decode_df_b(w, &mut self.net);
                self.phase = Phase::DfB2;
                2
            }
            Phase::DfB2 => {
                net::decode_df_b2(w, &mut self.net);
                self.phase = Phase::DfC;
                2
            }
            Phase::DfC => {
                net::decode_df_c(w, &mut self.net);
                self.phase = Phase::DfC2;
                2
            }
            Phase::DfC2 => {
                net::decode_df_c2(w, &mut self.net, &mut self.coefs);
                self.phase = Phase::Done;
                2
            }
        };
        if let Some(t) = t {
            self.prof_ns[k] += u64::try_from(t.elapsed().as_nanos()).unwrap_or(u64::MAX);
        }
        self.phase == Phase::Done
    }

    /// Finishes the hop begun by [`Self::begin`]: any work not yet stepped, then the gains, the
    /// deep filter and the synthesis. Answers the local SNR estimate (dB).
    pub(crate) fn end(&mut self, enh: &mut [f32; HOP]) -> f32 {
        while !self.step() {}
        self.phase = Phase::Idle;
        if self.skipped {
            enh.fill(0.0);
            return -15.0;
        }
        let newest = self.newest;
        if self.apply_gains || self.gain_zeros {
            let spec = &mut self.rolling_y[newest];
            let mut k = 0;
            for (&n, &g) in self.erb.iter().zip(&self.gains) {
                for x in &mut spec[k..k + n] {
                    *x *= g;
                }
                k += n;
            }
            self.skip_counter = 0;
        } else {
            self.skip_counter += 1;
        }
        self.out_spec.copy_from_slice(&self.rolling_y[newest]);
        if self.apply_df {
            // out[f] = sum over taps o (oldest frame first) of x_o[f] * c[f][o].
            for v in &mut self.out_spec[..NB_DF] {
                *v = Complex32::default();
            }
            for o in 0..DF_ORDER {
                let x = &self.rolling_x[(self.head + o) % DF_ORDER];
                for f in 0..NB_DF {
                    let i = (f * DF_ORDER + o) * 2;
                    self.out_spec[f] += x[f] * Complex32::new(self.coefs[i], self.coefs[i + 1]);
                }
            }
        }
        self.synthesis(enh);
        self.lsnr
    }
}
