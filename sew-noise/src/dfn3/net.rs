//! DeepFilterNet3 (low-latency variant)'s three networks, written out by hand: the encoder,
//! the ERB-gain decoder and the deep-filter decoder, one 10 ms frame at a time. Every buffer is
//! made when a [`NetState`] is built; a frame allocates nothing.
//!
//! Layout: a feature map of `C` channels over `F` frequency bins is `F * C` floats, bin major
//! (`[f * C + c]`): a bin's 64 channels sit side by side, so every convolution works on runs
//! of 64 floats whatever the number of bins, and the GRUs' 512 inputs and outputs (8 bins of
//! 64 channels, as the model reshapes them) need no reordering. The convolutions' weights are
//! reordered once, at load, to match. "Causal" convolutions read the previous frames from the
//! state; the first frames see zeros, as the authors' padded export does.

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
    /// `[kt][kf][c]`.
    dw: Vec<f32>,
    /// `[ci][co]`.
    pw: Vec<f32>,
    b: Vec<f32>,
}

/// A matrix `[rows][cols]` (row major) as `[cols][rows]`.
fn transposed(w: &[f32], rows: usize, cols: usize) -> Vec<f32> {
    let mut out = vec![0.0; w.len()];
    for (r, row) in w.chunks_exact(cols).take(rows).enumerate() {
        for (c, v) in row.iter().enumerate() {
            out[c * rows + r] = *v;
        }
    }
    out
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
    /// `[kt][kf][c]`.
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
    /// `[ci][kt][co]`.
    f_convp_dw: Vec<f32>,
    /// `[ci][co]`.
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
            dw: transposed(&self.get(&format!("{p}/dw"), &[C, 1, kt, kf])?, C, kt * kf),
            pw: transposed(&self.get(&format!("{p}/pw"), &[C, C])?, C, C),
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
            e_conv0_w: transposed(&t.get("enc/erb_conv0/w", &[C, 1, 3, 3])?, C, 9),
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
            f_convp_dw: transposed(&t.get("df/convp/dw", &[10, 32, CONVP_T])?, 10, 32 * CONVP_T),
            f_convp_pw: transposed(&t.get("df/convp/pw", &[10, 10])?, 10, 10),
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

/// `acc[c] += w[c] * x[c]`.
#[inline]
fn mul_add(acc: &mut [f32], w: &[f32], x: &[f32]) {
    for ((a, w), x) in acc.iter_mut().zip(w).zip(x) {
        *a += w * x;
    }
}

/// One output bin of a 3x3 convolution whose output channels each read one input channel:
/// `frames` (the oldest first) hold `x.len() / n` input channels of `n` bins, and the output
/// channels are split evenly among them. Adds bin `f`'s 64 sums to `acc`; `k` is
/// `[kt][kf][c]`.
#[inline]
fn input_conv_at(k: &[f32], frames: &[&[f32]; 3], n: usize, f: usize, acc: &mut [f32; C]) {
    for (t, x) in frames.iter().enumerate() {
        let k = &k[t * 3 * C..(t + 1) * 3 * C];
        let per = C * n / x.len();
        for (part, x) in x.chunks_exact(n).enumerate() {
            for c in part * per..(part + 1) * per {
                let mut a = k[C + c] * x[f];
                if f > 0 {
                    a += k[c] * x[f - 1];
                }
                if f + 1 < n {
                    a += k[2 * C + c] * x[f + 1];
                }
                acc[c] += a;
            }
        }
    }
}

/// One output bin of a causal depthwise convolution over time (`frames[0]` the oldest, the
/// last the current; each `fi` bins of `C` channels) and frequency (3 taps, one bin of zero
/// padding each side): the 64 channels' sums around input bin `centre`. `dw` is `[kt][kf][c]`.
#[inline]
fn depthwise_at(dw: &[f32], frames: &[&[f32]], fi: usize, centre: usize, acc: &mut [f32; C]) {
    acc.fill(0.0);
    for (t, frame) in frames.iter().enumerate() {
        let w = &dw[t * 3 * C..(t + 1) * 3 * C];
        let mut a = [0.0f32; C];
        for ((a, w), x) in a
            .iter_mut()
            .zip(&w[C..2 * C])
            .zip(&frame[centre * C..(centre + 1) * C])
        {
            *a = w * x;
        }
        if centre > 0 {
            mul_add(&mut a, &w[..C], &frame[(centre - 1) * C..centre * C]);
        }
        if centre + 1 < fi {
            mul_add(
                &mut a,
                &w[2 * C..],
                &frame[(centre + 1) * C..(centre + 2) * C],
            );
        }
        for (acc, a) in acc.iter_mut().zip(&a) {
            *acc += a;
        }
    }
}

/// 1x1 convolution `N -> N` at one bin, with bias, then ReLU. `pw` is `[ci][co]`.
#[inline]
fn pointwise_at<const N: usize>(pw: &[f32], b: &[f32], x: &[f32; N], out: &mut [f32]) {
    let mut o = [0.0f32; N];
    o.copy_from_slice(b);
    for (row, &v) in pw.as_chunks::<N>().0.iter().zip(x) {
        axpy(&mut o, v, row);
    }
    relu(&mut o);
    out.copy_from_slice(&o);
}

/// A separable convolution, causal over `frames` (the oldest first, each `fi` bins), stride
/// `stride` over frequency, then ReLU: `out` gets `((fi - 1) / stride + 1) * C` floats.
fn separable(l: &Sep, frames: &[&[f32]], fi: usize, stride: usize, out: &mut [f32]) {
    let fo = (fi - 1) / stride + 1;
    let mut acc = [0.0f32; C];
    for (f, o) in out[..fo * C].as_chunks_mut::<C>().0.iter_mut().enumerate() {
        depthwise_at(&l.dw, frames, fi, f * stride, &mut acc);
        pointwise_at(&l.pw, &l.b, &acc, o);
    }
}

/// A transposed separable convolution over frequency (3 taps, stride 2, padding 1, one bin of
/// output padding), then ReLU: `fi` bins in, `2 * fi` out. Input bin `i`'s tap `k` lands on
/// output bin `2i + k - 1`.
fn separable_up(l: &Sep, x: &[f32], fi: usize, out: &mut [f32]) {
    let mut acc = [0.0f32; C];
    for (p, o) in out[..2 * fi * C]
        .as_chunks_mut::<C>()
        .0
        .iter_mut()
        .enumerate()
    {
        let i = p / 2;
        acc.fill(0.0);
        if p % 2 == 0 {
            mul_add(&mut acc, &x[i * C..(i + 1) * C], &l.dw[C..2 * C]);
        } else {
            mul_add(&mut acc, &x[i * C..(i + 1) * C], &l.dw[2 * C..]);
            if i + 1 < fi {
                mul_add(&mut acc, &x[(i + 1) * C..(i + 2) * C], &l.dw[..C]);
            }
        }
        pointwise_at(&l.pw, &l.b, &acc, o);
    }
}

/// `out[f][c] = relu(w[c] * x[f][c] + b[c]) + add[f][c]`.
fn skip_add(p: &(Vec<f32>, Vec<f32>), x: &[f32], add: &[f32], out: &mut [f32]) {
    for ((o, x), add) in out
        .as_chunks_mut::<C>()
        .0
        .iter_mut()
        .zip(x.as_chunks::<C>().0)
        .zip(add.as_chunks::<C>().0)
    {
        for c in 0..C {
            o[c] = (p.0[c] * x[c] + p.1[c]).max(0.0) + add[c];
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
    let mut acc = [0.0f32; C];
    // erb_conv0: 1 -> 64 channels, 3x3, causal over 3 frames, bias, ReLU.
    {
        let frames: [&[f32]; 3] = [&s.erb_hist[0], &s.erb_hist[1], erb];
        for (f, o) in s.e0.as_chunks_mut::<C>().0.iter_mut().enumerate() {
            acc.copy_from_slice(&w.e_conv0_b);
            input_conv_at(&w.e_conv0_w, &frames, NB_ERB, f, &mut acc);
            relu(&mut acc);
            o.copy_from_slice(&acc);
        }
        s.erb_hist[0] = s.erb_hist[1];
        s.erb_hist[1] = *erb;
    }
    // erb_conv1..3: separable, causal over 2 frames.
    separable(&w.e_conv1, &[&s.e0_prev[..], &s.e0[..]], 32, 2, &mut s.e1);
    s.e0_prev.copy_from_slice(&s.e0);
    separable(&w.e_conv2, &[&s.e1_prev[..], &s.e1[..]], 16, 2, &mut s.e2);
    s.e1_prev.copy_from_slice(&s.e1);
    separable(&w.e_conv3, &[&s.e2_prev[..], &s.e2[..]], 8, 1, &mut s.e3);
    s.e2_prev.copy_from_slice(&s.e2);

    // df_conv0: 2 -> 64 channels in two groups (real -> 0..32, imaginary -> 32..64), 3x3,
    // causal over 3 frames; then 1x1, bias, ReLU.
    {
        let frames: [&[f32]; 3] = [&s.spec_hist[0], &s.spec_hist[1], spec];
        for (f, o) in s.c0.as_chunks_mut::<C>().0.iter_mut().enumerate() {
            acc.fill(0.0);
            input_conv_at(&w.e_df0.dw, &frames, NB_DF, f, &mut acc);
            pointwise_at(&w.e_df0.pw, &w.e_df0.b, &acc, o);
        }
        s.spec_hist[0] = s.spec_hist[1];
        s.spec_hist[1] = *spec;
    }
    // df_conv1: separable, stride 2 -> 48 bins.
    separable(&w.e_df1, &[&s.c0_prev[..], &s.c0[..]], NB_DF, 2, &mut s.s_b);
    s.c0_prev.copy_from_slice(&s.c0);

    // df_fc_emb (grouped 48 x 64 = 3072 -> 512, ReLU), plus e3 (8 x 64 = 512).
    grouped(
        &w.e_df_fc_emb,
        16,
        192,
        32,
        &s.s_b[..48 * C],
        &mut s.s_a[..H],
    );
    relu(&mut s.s_a[..H]);
    for (a, e) in s.s_a[..H].iter_mut().zip(&s.e3) {
        *a += e;
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
    // The embedding as 8 bins of 64 channels.
    grouped(&w.d_gru_out, 16, 32, 32, &s.d_h1, &mut s.s_b[..H]);
    relu(&mut s.s_b[..H]);
    // a3 = conv3p(e3) + embedding, into s_c[..512].
    skip_add(&w.d_p3, &s.e3, &s.s_b[..C * 8], &mut s.s_c[..C * 8]);
    // convt3: separable, causal over 2 frames, 8 bins -> s_b[..512].
    separable(
        &w.d_t3,
        &[&s.t3_prev[..], &s.s_c[..C * 8]],
        8,
        1,
        &mut s.s_b,
    );
    s.t3_prev.copy_from_slice(&s.s_c[..C * 8]);
    // a2 = conv2p(e2) + that.
    skip_add(&w.d_p2, &s.e2, &s.s_b[..C * 8], &mut s.s_c[..C * 8]);
    // convt2: 8 -> 16 bins.
    separable_up(&w.d_t2, &s.s_c, 8, &mut s.s_b);
    skip_add(&w.d_p1, &s.e1, &s.s_b[..C * 16], &mut s.s_c[..C * 16]);
    // convt1: 16 -> 32 bins.
    separable_up(&w.d_t1, &s.s_c, 16, &mut s.s_b);
    skip_add(&w.d_p0, &s.e0, &s.s_b[..C * 32], &mut s.s_c[..C * 32]);
    // conv0_out: 64 -> 1 channel, 2 x 3, causal over 2 frames, bias, sigmoid.
    for (f, g) in gains.iter_mut().enumerate() {
        let mut acc = w.d_out_b;
        for c in 0..C {
            for (t, x) in [&s.out_prev, &s.s_c].iter().enumerate() {
                let k = &w.d_out_w[(c * 2 + t) * 3..(c * 2 + t) * 3 + 3];
                acc += k[1] * x[f * C + c];
                if f > 0 {
                    acc += k[0] * x[(f - 1) * C + c];
                }
                if f + 1 < 32 {
                    acc += k[2] * x[(f + 1) * C + c];
                }
            }
        }
        *g = sigmoid(acc);
    }
    s.out_prev.copy_from_slice(&s.s_c[..C * 32]);
}

/// Bins of the deep-filter pathway worked on side by side.
const CONVP_BINS: usize = 8;

/// The deep-filter decoder: the filter's complex taps, `coefs[f * 10 + 2 * o + {0: re, 1: im}]`
/// for bin `f` and tap `o` (the oldest frame first).
pub(crate) fn decode_df(w: &Weights, s: &mut NetState, coefs: &mut [f32; NB_DF * DF_ORDER * 2]) {
    // The pathway: c0 over the last 5 runs.
    s.c0_hist[s.c0_head].copy_from_slice(&s.c0);
    s.c0_head = (s.c0_head + 1) % CONVP_T;
    // 10 channels from c0's two halves (5 each), (5, 1) kernel over time; then 1x1, bias,
    // ReLU, into s_b as `[f][k]`. Eight bins at a time: each sum runs over the 32 channels
    // and 5 frames in order, and eight of them run side by side.
    for f0 in (0..NB_DF).step_by(CONVP_BINS) {
        let mut acc = [[0.0f32; CONVP_BINS]; 10];
        for ci in 0..32 {
            for t in 0..CONVP_T {
                // t = 0 is the oldest frame.
                let frame = &s.c0_hist[(s.c0_head + t) % CONVP_T];
                let mut re = [0.0f32; CONVP_BINS];
                let mut im = [0.0f32; CONVP_BINS];
                for (b, bin) in frame[f0 * C..(f0 + CONVP_BINS) * C]
                    .as_chunks::<C>()
                    .0
                    .iter()
                    .enumerate()
                {
                    re[b] = bin[ci];
                    im[b] = bin[32 + ci];
                }
                let k = &w.f_convp_dw[(ci * CONVP_T + t) * 10..(ci * CONVP_T + t + 1) * 10];
                for (o, acc) in acc.iter_mut().enumerate() {
                    axpy(acc, k[o], if o < 5 { &re } else { &im });
                }
            }
        }
        for b in 0..CONVP_BINS {
            let mut p = [0.0f32; 10];
            for (p, acc) in p.iter_mut().zip(&acc) {
                *p = acc[b];
            }
            let f = f0 + b;
            pointwise_at(
                &w.f_convp_pw,
                &w.f_convp_b,
                &p,
                &mut s.s_b[f * 10..(f + 1) * 10],
            );
        }
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
    // Out: grouped 512 -> 960, tanh, plus the pathway.
    grouped(&w.f_out, 16, 32, 60, &s.s_a[..H], &mut s.s_c[..NB_DF * 10]);
    for ((c, g), p) in coefs
        .iter_mut()
        .zip(&s.s_c[..NB_DF * 10])
        .zip(&s.s_b[..NB_DF * 10])
    {
        *c = g.tanh() + p;
    }
}
