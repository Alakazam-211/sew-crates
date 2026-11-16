//! DeepFilterNet3 (low-latency variant)'s three networks, written out by hand: the encoder,
//! the ERB-gain decoder and the deep-filter decoder, one 10 ms frame at a time. Every buffer is
//! made when a [`NetState`] is built; a frame allocates nothing.
//!
//! Layout: a feature map of `C` channels over `F` frequency bins is `C * F` floats, channel
//! major (`[c * F + f]`). "Causal" convolutions read the previous frames from the state; the
//! first frames see zeros, as the authors' padded export does.

// The kernels index several arrays with one bin or channel number.
#![allow(clippy::needless_range_loop)]

use std::collections::HashMap;

use super::LoadError;
use super::weights::Tensor;

/// The GRUs' hidden size.
pub(crate) const H: usize = 512;
/// Channels of every convolution.
const C: usize = 64;
/// ERB bands.
pub(crate) const NB_ERB: usize = 32;
/// Bins the deep filter covers.
pub(crate) const NB_DF: usize = 96;
/// The deep filter's taps.
pub(crate) const DF_ORDER: usize = 5;
/// The deep-filter pathway's frames.
const CONVP_T: usize = 5;

/// A separable convolution: depthwise (no bias), then 1x1 with bias.
#[derive(Debug)]
struct Sep {
    dw: Vec<f32>,
    pw: Vec<f32>,
    b: Vec<f32>,
}

/// A GRU layer (gates z, r, n; the reset gate applied after the recurrent product).
#[derive(Debug)]
struct Gru {
    w: Vec<f32>,
    r: Vec<f32>,
    b: Vec<f32>,
}

/// Every weight, as the archive holds it.
#[derive(Debug)]
pub(crate) struct Weights {
    e_conv0_w: Vec<f32>,
    e_conv0_b: Vec<f32>,
    e_conv1: Sep,
    e_conv2: Sep,
    e_conv3: Sep,
    e_df0: Sep,
    e_df1: Sep,
    e_df_fc_emb: Vec<f32>,
    e_gru_in: Vec<f32>,
    e_gru: Gru,
    e_gru_out: Vec<f32>,
    e_lsnr_w: Vec<f32>,
    e_lsnr_b: f32,
    d_gru_in: Vec<f32>,
    d_gru0: Gru,
    d_gru1: Gru,
    d_gru_out: Vec<f32>,
    d_p3: (Vec<f32>, Vec<f32>),
    d_t3: Sep,
    d_p2: (Vec<f32>, Vec<f32>),
    d_t2: Sep,
    d_p1: (Vec<f32>, Vec<f32>),
    d_t1: Sep,
    d_p0: (Vec<f32>, Vec<f32>),
    d_out_w: Vec<f32>,
    d_out_b: f32,
    f_gru_in: Vec<f32>,
    f_gru: [Gru; 3],
    f_skip: Vec<f32>,
    f_convp_dw: Vec<f32>,
    f_convp_pw: Vec<f32>,
    f_convp_b: Vec<f32>,
    f_out: Vec<f32>,
}

struct Take(HashMap<String, Tensor>);

impl Take {
    fn get(&mut self, label: &str, shape: &[usize]) -> Result<Vec<f32>, LoadError> {
        let t = self
            .0
            .remove(label)
            .ok_or_else(|| LoadError::Format(format!("{label}: missing")))?;
        if t.shape != shape {
            return Err(LoadError::Format(format!(
                "{label}: shape {:?}, expected {shape:?}",
                t.shape
            )));
        }
        Ok(t.data)
    }
    fn sep(&mut self, p: &str, kt: usize, kf: usize) -> Result<Sep, LoadError> {
        Ok(Sep {
            dw: self.get(&format!("{p}/dw"), &[C, 1, kt, kf])?,
            pw: self.get(&format!("{p}/pw"), &[C, C])?,
            b: self.get(&format!("{p}/b"), &[C])?,
        })
    }
    fn gru(&mut self, p: &str) -> Result<Gru, LoadError> {
        Ok(Gru {
            w: self.get(&format!("{p}/w"), &[3 * H, H])?,
            r: self.get(&format!("{p}/r"), &[3 * H, H])?,
            b: self.get(&format!("{p}/b"), &[6 * H])?,
        })
    }
    fn affine(&mut self, p: &str) -> Result<(Vec<f32>, Vec<f32>), LoadError> {
        Ok((
            self.get(&format!("{p}/w"), &[C])?,
            self.get(&format!("{p}/b"), &[C])?,
        ))
    }
}

impl Weights {
    pub(crate) fn from_tensors(t: HashMap<String, Tensor>) -> Result<Self, LoadError> {
        let mut t = Take(t);
        let w = Self {
            e_conv0_w: t.get("enc/erb_conv0/w", &[C, 1, 3, 3])?,
            e_conv0_b: t.get("enc/erb_conv0/b", &[C])?,
            e_conv1: t.sep("enc/erb_conv1", 2, 3)?,
            e_conv2: t.sep("enc/erb_conv2", 2, 3)?,
            e_conv3: t.sep("enc/erb_conv3", 2, 3)?,
            e_df0: t.sep("enc/df_conv0", 3, 3)?,
            e_df1: t.sep("enc/df_conv1", 2, 3)?,
            e_df_fc_emb: t.get("enc/df_fc_emb/w", &[16, 192, 32])?,
            e_gru_in: t.get("enc/gru_in/w", &[16, 32, 32])?,
            e_gru: t.gru("enc/gru0")?,
            e_gru_out: t.get("enc/gru_out/w", &[16, 32, 32])?,
            e_lsnr_w: t.get("enc/lsnr/w", &[H])?,
            e_lsnr_b: t.get("enc/lsnr/b", &[1])?[0],
            d_gru_in: t.get("erb/gru_in/w", &[16, 32, 32])?,
            d_gru0: t.gru("erb/gru0")?,
            d_gru1: t.gru("erb/gru1")?,
            d_gru_out: t.get("erb/gru_out/w", &[16, 32, 32])?,
            d_p3: t.affine("erb/conv3p")?,
            d_t3: t.sep("erb/convt3", 2, 3)?,
            d_p2: t.affine("erb/conv2p")?,
            d_t2: t.sep("erb/convt2", 1, 3)?,
            d_p1: t.affine("erb/conv1p")?,
            d_t1: t.sep("erb/convt1", 1, 3)?,
            d_p0: t.affine("erb/conv0p")?,
            d_out_w: t.get("erb/conv0_out/w", &[1, C, 2, 3])?,
            d_out_b: t.get("erb/conv0_out/b", &[1])?[0],
            f_gru_in: t.get("df/gru_in/w", &[8, 64, 64])?,
            f_gru: [t.gru("df/gru0")?, t.gru("df/gru1")?, t.gru("df/gru2")?],
            f_skip: t.get("df/skip/w", &[16, 32, 32])?,
            f_convp_dw: t.get("df/convp/dw", &[10, 32, CONVP_T])?,
            f_convp_pw: t.get("df/convp/pw", &[10, 10])?,
            f_convp_b: t.get("df/convp/b", &[10])?,
            f_out: t.get("df/out/w", &[16, 32, 60])?,
        };
        if let Some(extra) = t.0.keys().next() {
            return Err(LoadError::Format(format!(
                "{extra}: not a weight this version uses"
            )));
        }
        Ok(w)
    }

    /// Bytes of weights held.
    pub(crate) fn bytes(&self) -> usize {
        fn s(sep: &Sep) -> usize {
            sep.dw.len() + sep.pw.len() + sep.b.len()
        }
        fn g(gru: &Gru) -> usize {
            gru.w.len() + gru.r.len() + gru.b.len()
        }
        let n = self.e_conv0_w.len()
            + self.e_conv0_b.len()
            + s(&self.e_conv1)
            + s(&self.e_conv2)
            + s(&self.e_conv3)
            + s(&self.e_df0)
            + s(&self.e_df1)
            + self.e_df_fc_emb.len()
            + self.e_gru_in.len()
            + g(&self.e_gru)
            + self.e_gru_out.len()
            + self.e_lsnr_w.len()
            + 1
            + self.d_gru_in.len()
            + g(&self.d_gru0)
            + g(&self.d_gru1)
            + self.d_gru_out.len()
            + 2 * 4 * C
            + s(&self.d_t3)
            + s(&self.d_t2)
            + s(&self.d_t1)
            + self.d_out_w.len()
            + 1
            + self.f_gru_in.len()
            + self.f_gru.iter().map(g).sum::<usize>()
            + self.f_skip.len()
            + self.f_convp_dw.len()
            + self.f_convp_pw.len()
            + self.f_convp_b.len()
            + self.f_out.len();
        n * 4
    }
}

/// One stream's recurrent state, history and scratch.
#[derive(Debug, Clone)]
pub(crate) struct NetState {
    // Encoder.
    erb_hist: [[f32; NB_ERB]; 2],
    spec_hist: [[f32; 2 * NB_DF]; 2],
    e0_prev: Vec<f32>,
    e1_prev: Vec<f32>,
    e2_prev: Vec<f32>,
    c0_prev: Vec<f32>,
    e_h: Vec<f32>,
    // ERB decoder.
    d_h0: Vec<f32>,
    d_h1: Vec<f32>,
    t3_prev: Vec<f32>,
    out_prev: Vec<f32>,
    // Deep-filter decoder: c0 over its last frames, oldest first by `c0_head`.
    f_h: [Vec<f32>; 3],
    c0_hist: [Vec<f32>; CONVP_T],
    c0_head: usize,
    // The encoder's outputs for the decoders.
    pub(crate) e0: Vec<f32>,
    pub(crate) e1: Vec<f32>,
    pub(crate) e2: Vec<f32>,
    pub(crate) e3: Vec<f32>,
    pub(crate) c0: Vec<f32>,
    pub(crate) emb: Vec<f32>,
    // Scratch.
    s_a: Vec<f32>,
    s_b: Vec<f32>,
    s_c: Vec<f32>,
    s_gates: Vec<f32>,
    s_rec: Vec<f32>,
}

impl NetState {
    pub(crate) fn new() -> Self {
        let z = |n: usize| vec![0.0f32; n];
        Self {
            erb_hist: [[0.0; NB_ERB]; 2],
            spec_hist: [[0.0; 2 * NB_DF]; 2],
            e0_prev: z(C * 32),
            e1_prev: z(C * 16),
            e2_prev: z(C * 8),
            c0_prev: z(C * NB_DF),
            e_h: z(H),
            d_h0: z(H),
            d_h1: z(H),
            t3_prev: z(C * 8),
            out_prev: z(C * 32),
            f_h: [z(H), z(H), z(H)],
            c0_hist: [
                z(C * NB_DF),
                z(C * NB_DF),
                z(C * NB_DF),
                z(C * NB_DF),
                z(C * NB_DF),
            ],
            c0_head: 0,
            e0: z(C * 32),
            e1: z(C * 16),
            e2: z(C * 8),
            e3: z(C * 8),
            c0: z(C * NB_DF),
            emb: z(H),
            s_a: z(C * NB_DF),
            s_b: z(C * NB_DF),
            s_c: z(C * NB_DF),
            s_gates: z(3 * H),
            s_rec: z(3 * H),
        }
    }

    /// Back to the state of a new stream, without allocating.
    pub(crate) fn clear(&mut self) {
        self.erb_hist = [[0.0; NB_ERB]; 2];
        self.spec_hist = [[0.0; 2 * NB_DF]; 2];
        for v in [
            &mut self.e0_prev,
            &mut self.e1_prev,
            &mut self.e2_prev,
            &mut self.c0_prev,
            &mut self.e_h,
            &mut self.d_h0,
            &mut self.d_h1,
            &mut self.t3_prev,
            &mut self.out_prev,
        ] {
            v.fill(0.0);
        }
        for v in self.f_h.iter_mut().chain(self.c0_hist.iter_mut()) {
            v.fill(0.0);
        }
        self.c0_head = 0;
    }
}

// ---- Kernels -------------------------------------------------------------------------------

/// Dot product with 16 running sums (the compiler keeps them in vector registers).
#[inline]
fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0.0f32; 16];
    let (ca, ra) = a.as_chunks::<16>();
    let (cb, rb) = b.as_chunks::<16>();
    for (x, y) in ca.iter().zip(cb) {
        for ((a, x), y) in acc.iter_mut().zip(x).zip(y) {
            *a += x * y;
        }
    }
    let mut tail = 0.0;
    for (x, y) in ra.iter().zip(rb) {
        tail += x * y;
    }
    let s = |k: usize| (acc[k] + acc[k + 4]) + (acc[k + 8] + acc[k + 12]);
    (s(0) + s(1)) + (s(2) + s(3)) + tail
}

/// `out[j] = W[j] . x` for each row `j` of `w` (row-major, `x.len()` columns). One row at a
/// time with 16 running sums: on the machines measured this beat four rows at a time, the work
/// being bound by memory bandwidth, not arithmetic.
fn matvec(w: &[f32], x: &[f32], out: &mut [f32]) {
    for (o, row) in out.iter_mut().zip(w.chunks_exact(x.len())) {
        *o = dot(row, x);
    }
}

/// `out += a * x`.
#[inline]
fn axpy(out: &mut [f32], a: f32, x: &[f32]) {
    for (o, v) in out.iter_mut().zip(x) {
        *o += a * v;
    }
}

fn relu(v: &mut [f32]) {
    for x in v {
        *x = x.max(0.0);
    }
}

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

/// A grouped linear layer: `w` is `[groups, i, h]`; `out[g*h + k] = sum_n x[g*i + n] *
/// w[g][n][k]`.
fn grouped(w: &[f32], groups: usize, i: usize, h: usize, x: &[f32], out: &mut [f32]) {
    for g in 0..groups {
        let o = &mut out[g * h..(g + 1) * h];
        o.fill(0.0);
        for n in 0..i {
            let row = &w[(g * i + n) * h..(g * i + n + 1) * h];
            axpy(o, x[g * i + n], row);
        }
    }
}

/// One GRU step (ONNX's GRU with `linear_before_reset`), `h` updated in place.
fn gru(l: &Gru, x: &[f32], h: &mut [f32], gates: &mut [f32], rec: &mut [f32]) {
    matvec(&l.w, x, gates);
    matvec(&l.r, h, rec);
    let (wb, rb) = l.b.split_at(3 * H);
    for k in 0..H {
        let z = sigmoid(gates[k] + wb[k] + rec[k] + rb[k]);
        let r = sigmoid(gates[H + k] + wb[H + k] + rec[H + k] + rb[H + k]);
        let n = (gates[2 * H + k] + wb[2 * H + k] + r * (rec[2 * H + k] + rb[2 * H + k])).tanh();
        h[k] = (1.0 - z) * n + z * h[k];
    }
}

/// A causal depthwise convolution over time (`kt` frames: `frames[0]` the oldest, the last the
/// current) and frequency (3 taps, one bin of zero padding each side, stride `stride`).
/// `frames` each hold `C * fi` floats; `out` gets `C * fo`.
fn depthwise(dw: &[f32], kt: usize, frames: &[&[f32]], fi: usize, stride: usize, out: &mut [f32]) {
    let fo = (fi - 1) / stride + 1;
    for c in 0..C {
        let o = &mut out[c * fo..(c + 1) * fo];
        o.fill(0.0);
        for (t, frame) in frames.iter().enumerate().take(kt) {
            let x = &frame[c * fi..(c + 1) * fi];
            let w = &dw[(c * kt + t) * 3..(c * kt + t) * 3 + 3];
            for (f, of) in o.iter_mut().enumerate() {
                let centre = f * stride;
                let mut acc = w[1] * x[centre];
                if centre > 0 {
                    acc += w[0] * x[centre - 1];
                }
                if centre + 1 < fi {
                    acc += w[2] * x[centre + 1];
                }
                *of += acc;
            }
        }
    }
}

/// 1x1 convolution `C -> C` over `f` bins, with bias, then ReLU.
fn pointwise(pw: &[f32], b: &[f32], x: &[f32], f: usize, out: &mut [f32]) {
    for co in 0..C {
        let o = &mut out[co * f..(co + 1) * f];
        o.fill(b[co]);
        for ci in 0..C {
            axpy(o, pw[co * C + ci], &x[ci * f..(ci + 1) * f]);
        }
    }
    relu(&mut out[..C * f]);
}

/// A transposed depthwise convolution over frequency (3 taps, stride 2, padding 1, one bin of
/// output padding): `fi` bins in, `2 * fi` out.
fn depthwise_up(dw: &[f32], x: &[f32], fi: usize, out: &mut [f32]) {
    let fo = 2 * fi;
    for c in 0..C {
        let o = &mut out[c * fo..(c + 1) * fo];
        o.fill(0.0);
        let w = &dw[c * 3..c * 3 + 3];
        for (i, &v) in x[c * fi..(c + 1) * fi].iter().enumerate() {
            // Output bin 2i + k - 1 for tap k.
            for (k, wk) in w.iter().enumerate() {
                let ob = 2 * i + k;
                if ob >= 1 && ob - 1 < fo {
                    o[ob - 1] += v * wk;
                }
            }
        }
    }
}

/// `out[c][f] = relu(w[c] * x[c][f] + b[c]) + add[c][f]`.
fn skip_add(p: &(Vec<f32>, Vec<f32>), x: &[f32], add: &[f32], f: usize, out: &mut [f32]) {
    for c in 0..C {
        for k in 0..f {
            let i = c * f + k;
            out[i] = (p.0[c] * x[i] + p.1[c]).max(0.0) + add[i];
        }
    }
}

// ---- The three networks --------------------------------------------------------------------

/// The encoder on one frame: `erb` (32 normalised ERB features) and `spec` (96 bins of the
/// normalised spectrum, real parts then imaginary parts). Fills `s.e0..s.e3`, `s.c0` and
/// `s.emb`; answers the local SNR estimate in dB.
pub(crate) fn encode(
    w: &Weights,
    s: &mut NetState,
    erb: &[f32; NB_ERB],
    spec: &[f32; 2 * NB_DF],
) -> f32 {
    // erb_conv0: 1 -> 64 channels, 3x3, causal over 3 frames, bias, ReLU.
    {
        let frames: [&[f32]; 3] = [&s.erb_hist[0], &s.erb_hist[1], erb];
        for c in 0..C {
            let o = &mut s.e0[c * 32..(c + 1) * 32];
            o.fill(w.e_conv0_b[c]);
            for (t, x) in frames.iter().enumerate() {
                let k = &w.e_conv0_w[(c * 3 + t) * 3..(c * 3 + t) * 3 + 3];
                for (f, of) in o.iter_mut().enumerate() {
                    let mut acc = k[1] * x[f];
                    if f > 0 {
                        acc += k[0] * x[f - 1];
                    }
                    if f + 1 < 32 {
                        acc += k[2] * x[f + 1];
                    }
                    *of += acc;
                }
            }
        }
        relu(&mut s.e0);
        s.erb_hist[0] = s.erb_hist[1];
        s.erb_hist[1] = *erb;
    }
    // erb_conv1..3: separable, causal over 2 frames.
    depthwise(
        &w.e_conv1.dw,
        2,
        &[&s.e0_prev[..], &s.e0[..]],
        32,
        2,
        &mut s.s_a,
    );
    pointwise(&w.e_conv1.pw, &w.e_conv1.b, &s.s_a, 16, &mut s.e1);
    s.e0_prev.copy_from_slice(&s.e0);
    depthwise(
        &w.e_conv2.dw,
        2,
        &[&s.e1_prev[..], &s.e1[..]],
        16,
        2,
        &mut s.s_a,
    );
    pointwise(&w.e_conv2.pw, &w.e_conv2.b, &s.s_a, 8, &mut s.e2);
    s.e1_prev.copy_from_slice(&s.e1);
    depthwise(
        &w.e_conv3.dw,
        2,
        &[&s.e2_prev[..], &s.e2[..]],
        8,
        1,
        &mut s.s_a,
    );
    pointwise(&w.e_conv3.pw, &w.e_conv3.b, &s.s_a, 8, &mut s.e3);
    s.e2_prev.copy_from_slice(&s.e2);

    // df_conv0: 2 -> 64 channels in two groups (real -> 0..32, imaginary -> 32..64), 3x3,
    // causal over 3 frames; then 1x1, bias, ReLU.
    {
        let frames: [&[f32]; 3] = [&s.spec_hist[0], &s.spec_hist[1], spec];
        for c in 0..C {
            let part = c / 32;
            let o = &mut s.s_a[c * NB_DF..(c + 1) * NB_DF];
            o.fill(0.0);
            for (t, fr) in frames.iter().enumerate() {
                let x = &fr[part * NB_DF..(part + 1) * NB_DF];
                let k = &w.e_df0.dw[(c * 3 + t) * 3..(c * 3 + t) * 3 + 3];
                for (f, of) in o.iter_mut().enumerate() {
                    let mut acc = k[1] * x[f];
                    if f > 0 {
                        acc += k[0] * x[f - 1];
                    }
                    if f + 1 < NB_DF {
                        acc += k[2] * x[f + 1];
                    }
                    *of += acc;
                }
            }
        }
        pointwise(&w.e_df0.pw, &w.e_df0.b, &s.s_a, NB_DF, &mut s.c0);
        s.spec_hist[0] = s.spec_hist[1];
        s.spec_hist[1] = *spec;
    }
    // df_conv1: separable, stride 2 -> 48 bins.
    depthwise(
        &w.e_df1.dw,
        2,
        &[&s.c0_prev[..], &s.c0[..]],
        NB_DF,
        2,
        &mut s.s_a,
    );
    pointwise(&w.e_df1.pw, &w.e_df1.b, &s.s_a, 48, &mut s.s_b);
    s.c0_prev.copy_from_slice(&s.c0);

    // Flatten bin-major (f * 64 + c): df_conv1's 48 x 64 = 3072, e3's 8 x 64 = 512.
    for f in 0..48 {
        for c in 0..C {
            s.s_c[f * C + c] = s.s_b[c * 48 + f];
        }
    }
    // df_fc_emb (grouped 3072 -> 512, ReLU), plus e3 flattened.
    grouped(
        &w.e_df_fc_emb,
        16,
        192,
        32,
        &s.s_c[..48 * C],
        &mut s.s_a[..H],
    );
    relu(&mut s.s_a[..H]);
    for f in 0..8 {
        for c in 0..C {
            s.s_a[f * C + c] += s.e3[c * 8 + f];
        }
    }
    // The embedding GRU: grouped in (ReLU), GRU, grouped out (ReLU).
    grouped(&w.e_gru_in, 16, 32, 32, &s.s_a[..H], &mut s.s_b[..H]);
    relu(&mut s.s_b[..H]);
    gru(
        &w.e_gru,
        &s.s_b[..H],
        &mut s.e_h,
        &mut s.s_gates,
        &mut s.s_rec,
    );
    grouped(&w.e_gru_out, 16, 32, 32, &s.e_h, &mut s.emb);
    relu(&mut s.emb);
    sigmoid(dot(&s.emb, &w.e_lsnr_w) + w.e_lsnr_b) * 50.0 - 15.0
}

/// The ERB decoder: the 32 band gains (0 to 1) from the encoder's outputs in `s`.
pub(crate) fn decode_erb(w: &Weights, s: &mut NetState, gains: &mut [f32; NB_ERB]) {
    grouped(&w.d_gru_in, 16, 32, 32, &s.emb, &mut s.s_a[..H]);
    relu(&mut s.s_a[..H]);
    gru(
        &w.d_gru0,
        &s.s_a[..H],
        &mut s.d_h0,
        &mut s.s_gates,
        &mut s.s_rec,
    );
    gru(
        &w.d_gru1,
        &s.d_h0,
        &mut s.d_h1,
        &mut s.s_gates,
        &mut s.s_rec,
    );
    grouped(&w.d_gru_out, 16, 32, 32, &s.d_h1, &mut s.s_b[..H]);
    relu(&mut s.s_b[..H]);
    // To channel-major 64 x 8.
    for f in 0..8 {
        for c in 0..C {
            s.s_a[c * 8 + f] = s.s_b[f * C + c];
        }
    }
    // a3 = conv3p(e3) + embedding, into s_c[..512].
    skip_add(&w.d_p3, &s.e3, &s.s_a[..C * 8], 8, &mut s.s_c[..C * 8]);
    // convt3: separable, causal over 2 frames, 8 bins -> s_b[..512].
    depthwise(
        &w.d_t3.dw,
        2,
        &[&s.t3_prev[..], &s.s_c[..C * 8]],
        8,
        1,
        &mut s.s_a,
    );
    pointwise(&w.d_t3.pw, &w.d_t3.b, &s.s_a, 8, &mut s.s_b);
    s.t3_prev.copy_from_slice(&s.s_c[..C * 8]);
    // a2 = conv2p(e2) + that.
    skip_add(&w.d_p2, &s.e2, &s.s_b[..C * 8], 8, &mut s.s_c[..C * 8]);
    // convt2: 8 -> 16 bins.
    depthwise_up(&w.d_t2.dw, &s.s_c, 8, &mut s.s_a);
    pointwise(&w.d_t2.pw, &w.d_t2.b, &s.s_a, 16, &mut s.s_b);
    skip_add(&w.d_p1, &s.e1, &s.s_b[..C * 16], 16, &mut s.s_c[..C * 16]);
    // convt1: 16 -> 32 bins.
    depthwise_up(&w.d_t1.dw, &s.s_c, 16, &mut s.s_a);
    pointwise(&w.d_t1.pw, &w.d_t1.b, &s.s_a, 32, &mut s.s_b);
    skip_add(&w.d_p0, &s.e0, &s.s_b[..C * 32], 32, &mut s.s_c[..C * 32]);
    // conv0_out: 64 -> 1 channel, 2 x 3, causal over 2 frames, bias, sigmoid.
    for (f, g) in gains.iter_mut().enumerate() {
        let mut acc = w.d_out_b;
        for c in 0..C {
            for (t, x) in [
                &s.out_prev[c * 32..(c + 1) * 32],
                &s.s_c[c * 32..(c + 1) * 32],
            ]
            .iter()
            .enumerate()
            {
                let k = &w.d_out_w[(c * 2 + t) * 3..(c * 2 + t) * 3 + 3];
                acc += k[1] * x[f];
                if f > 0 {
                    acc += k[0] * x[f - 1];
                }
                if f + 1 < 32 {
                    acc += k[2] * x[f + 1];
                }
            }
        }
        *g = sigmoid(acc);
    }
    s.out_prev.copy_from_slice(&s.s_c[..C * 32]);
}

/// The deep-filter decoder: the filter's complex taps, `coefs[f * 10 + 2 * o + {0: re, 1: im}]`
/// for bin `f` and tap `o` (the oldest frame first).
pub(crate) fn decode_df(w: &Weights, s: &mut NetState, coefs: &mut [f32; NB_DF * DF_ORDER * 2]) {
    // The pathway: c0 over the last 5 runs.
    s.c0_hist[s.c0_head].copy_from_slice(&s.c0);
    s.c0_head = (s.c0_head + 1) % CONVP_T;
    // 10 channels from c0's two halves (5 each), (5, 1) kernel over time.
    let p = &mut s.s_a[..10 * NB_DF];
    p.fill(0.0);
    for k in 0..10 {
        let part = k / 5;
        let o = &mut p[k * NB_DF..(k + 1) * NB_DF];
        for ci in 0..32 {
            for t in 0..CONVP_T {
                // t = 0 is the oldest frame.
                let frame = &s.c0_hist[(s.c0_head + t) % CONVP_T];
                let x = &frame[(part * 32 + ci) * NB_DF..(part * 32 + ci + 1) * NB_DF];
                axpy(o, w.f_convp_dw[(k * 32 + ci) * CONVP_T + t], x);
            }
        }
    }
    let q = &mut s.s_b[..10 * NB_DF];
    for k in 0..10 {
        let o = &mut q[k * NB_DF..(k + 1) * NB_DF];
        o.fill(w.f_convp_b[k]);
        for ci in 0..10 {
            axpy(
                o,
                w.f_convp_pw[k * 10 + ci],
                &s.s_a[ci * NB_DF..(ci + 1) * NB_DF],
            );
        }
        relu(o);
    }
    // The GRU path.
    grouped(&w.f_gru_in, 8, 64, 64, &s.emb, &mut s.s_c[..H]);
    relu(&mut s.s_c[..H]);
    let [h0, h1, h2] = &mut s.f_h;
    gru(&w.f_gru[0], &s.s_c[..H], h0, &mut s.s_gates, &mut s.s_rec);
    gru(&w.f_gru[1], &h0[..], h1, &mut s.s_gates, &mut s.s_rec);
    gru(&w.f_gru[2], &h1[..], h2, &mut s.s_gates, &mut s.s_rec);
    // Skip from the embedding, added.
    grouped(&w.f_skip, 16, 32, 32, &s.emb, &mut s.s_a[..H]);
    for (a, b) in s.s_a[..H].iter_mut().zip(h2.iter()) {
        *a += b;
    }
    // Out: grouped 512 -> 960, tanh, plus the pathway (transposed to bin-major).
    grouped(&w.f_out, 16, 32, 60, &s.s_a[..H], &mut s.s_c[..NB_DF * 10]);
    for f in 0..NB_DF {
        for k in 0..10 {
            coefs[f * 10 + k] = s.s_c[f * 10 + k].tanh() + s.s_b[k * NB_DF + f];
        }
    }
}
