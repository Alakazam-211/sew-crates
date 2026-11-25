# sew-noise

sew-noise is microphone noise removal in pure Rust: a real-time capture stage, a frame API and a stream API over `nnnoiseless`, the Rust port of RNNoise.

It takes 48 kHz mono speech and removes background noise before the audio is encoded. The
model is RNNoise's (by Jean-Marc Valin and Mozilla), run by
[`nnnoiseless`](https://crates.io/crates/nnnoiseless) (Joe Neeman's port to Rust), with the
weights compiled in: nothing is loaded at run time, there is no C code and no `unsafe`. The
stage's thread clock reads the OS clock through rustix, which on Apple platforms and Android
calls the system C library.

- `Stage`: for a native capture path. One 480-sample (10 ms) block in place, a fixed latency
  of one block, and a mode (`Off`, `Standard`, and with the `dfn3` feature `Strong`) that any
  thread can change while audio runs.
- `Denoiser`: one 480-sample frame in, one out.
- `StreamDenoiser`: blocks of any size at any rate from 8 to 192 kHz (it resamples to and
  from 48 kHz), for a Web Audio worklet or any callback whose block size is not 480.

Once a thread is ready (see "Threads" below), processing allocates nothing. Non-finite input
samples are read as silence and the output is always finite.

It is part of Sew, Alakazam Labs' self-hosted team chat server, where it cleans the microphone
of voice calls on the sender's device.

- Source: <https://github.com/Alakazam-211/sew-crates> (read-only: it is generated from Sew's
  repository at each release, so pull requests are not merged there)
- API documentation: <https://docs.rs/sew-noise>
- Issues: <https://github.com/Alakazam-211/sew-crates/issues/new/choose>,
  with the `sew-noise` form, which labels the issue `crate:sew-noise`
- Security problems: report them privately, as
  [SECURITY.md](https://github.com/Alakazam-211/sew-crates/blob/main/SECURITY.md) says.
- MSRV: Rust 1.95.

## Usage

```toml
[dependencies]
sew-noise = "0.11.6"
# or, with DeepFilterNet3 and the capture-hook trait:
# sew-noise = { version = "0.11.6", features = ["dfn3", "capture-stage"] }
```

No feature is on by default: `dfn3`, `capture-stage` and `probe` (measurement builds only) are
opt-in.

```rust
use sew_noise::{FRAME, Mode, Stage};

// Build the stage on the thread that will run it (see "Threads" below).
let mut stage = Stage::new();
let control = stage.control(); // for the UI thread

// On the audio thread, once per 10 ms block of 48 kHz mono in [-1, 1]:
let mut block = [0.0f32; FRAME];
stage.process(&mut block); // now holds the previous block, with the noise removed

// From any thread, at any time:
control.set_mode(Mode::Off).unwrap(); // pass-through, same latency, faded over one block
assert_eq!(stage.latency_samples(), 480);
```

### The stage

- **Where it goes.** After echo cancellation and before automatic gain control. An echo
  canceller needs the raw microphone, and a gain control after the stage measures the voice
  and not the noise.
- **Latency.** 480 samples (10 ms), fixed, in every mode. `Off` delays the input by the same
  block, so a mode change never shifts the signal in time.
- **Modes.** `Standard` runs the model, with a floor and an onset guard (below). `Off` passes
  the input through bit for bit and does not run the model. `Strong` runs DeepFilterNet3 (the
  `dfn3` feature, below). Without the `dfn3` feature, or on a stage built without the model,
  `set_mode(Mode::Strong)` returns `ModeUnavailable` and changes nothing.
- **Standard's floor.** The input always stays in the output 30 dB under its own level, so
  nothing is taken down by more than 30 dB.
- **Standard's onset guard.** The start of a word can come before the model has opened up
  for speech. So when a 10 ms block's level rises 20 dB above the running noise floor, after
  at least 30 ms that did not, and the block is not spiky (its peak is at most 3 times its RMS
  level; a click is spikier), and the next block is still that far above the floor, the input
  passes unchanged for 40 ms, starting with the block that rose. The stage's own 10 ms delay
  is the look-ahead, so the guard adds no latency. It cannot tell a word from another sound
  that rises and lasts (a dish, a door): such a sound passes for 40 ms too.
  `StageStats::guard_opens` counts the openings.
- **Mode changes.** Taken at a block boundary and cross-faded over one block. Turning removal
  on takes one block more: the model is fed one block before its output is used.
  `StageControl::active` says what the latest block's output was.
- **Timing.** `StageControl::stats` counts the blocks, those that took longer than a budget
  (6 ms unless `Stage::set_block_budget` changes it), and the slowest block, in total and for
  the latest 5 s window. `StageStats::window_p99_over_budget` is the signal for a caller to
  step down to `Off`. The stage never changes its own mode.
- **Reset.** `Stage::reset` drops the block held back, for a device change or a long pause.

### Threads

`nnnoiseless` makes its FFT plans on a thread's first frame and keeps them for that thread;
making them allocates. `Stage::new`, `Denoiser::new` and `Stage::reset` make them on the
thread that calls. So either build the stage on the audio thread, or call `Stage::reset` (or
`sew_noise::warm_up_thread()`) once on the audio thread before the first block. If neither
happens the first block allocates once, and `StageStats::cold_thread_blocks` counts it.

### The `capture-stage` feature

`sew_noise::capture_stage::CaptureStage` is the hook an audio pipeline calls the stage
through: `process(&mut [f32; 480])`, `latency_samples()`, `reset()`. `Stage` implements it.

### The `probe` feature

For measurement builds only, never a release. `probe::ProbeStage` wraps a `Stage` and records
what each block cost (wall time and the thread's CPU time), which core the thread was on and
which parts of the model ran, beside two fixed calibration loops on the same thread. It is
for telling how fast the OS runs the audio thread apart from how much work the stage is. The
output is the wrapped stage's; its step-down rules are off, and the loops add time to some
blocks, so a probed stream can glitch. `probe::reference` runs the same loops on the calling
thread.

### The `dfn3` feature

With the `dfn3` feature the stage gets a second model for `Mode::Strong`: the low-latency
variant of DeepFilterNet3, by Hendrik Schröter and the DeepFilterNet authors
(<https://github.com/Rikorose/DeepFilterNet>, MIT OR Apache-2.0). This crate runs it with its
own inference code, written in Rust: the feature adds RustCrypto's `sha2` crate, and no C
code, no `unsafe` and no neural-network runtime.

The weights are not in this crate. An app ships them beside itself as a resource
and loads them by path; the loader checks the file's sha256 against the pinned one before it
reads a weight, and refuses any other file:

```rust,ignore
use sew_noise::Stage;
use sew_noise::dfn3::Dfn3Model;

// At start, off the audio thread.
let stage = match Dfn3Model::load(resources.join("dfn3/dfn3_ll.nnef.tar")) {
    Ok(model) => Stage::with_dfn3(model), // starts in Mode::Strong
    Err(_) => Stage::new(),               // RNNoise, as without the feature
};
```

In `Strong` the stage runs RNNoise beside the model and times its own blocks, by the audio
thread's CPU time where the platform has a clock for it (Linux, Android, macOS, iOS), so a
block that only waited for a core while another part of the app ran does not count against the
model. When its blocks get too slow for the audio pipeline's budget, or the thread keeps
falling behind real time, it switches to `Standard` at the next block, without a click and
without running the model again, and tries the model again later; after repeated switches it
stays on `Standard`. `StageStats` reports where `Strong` is (`strong_state`), the switches
(`step_downs`, `last_step_down`, `grace_step_downs`), a retry held back (`retry_held`), the
blocks the model ran, and the block times by the wall clock and by CPU time side by side.
Both models delay the audio by the same amount. A block allocates nothing and takes no lock.

A device is briefly busy at a call's start and when the app or the OS does something else
(the audio route changes, a camera starts, the app moves to the background). The stage opens
a grace window by itself when it starts, and an app reports such moments with
`StageControl::grace`: in the window the stage switches only where the audio pipeline's own
budget rule would stop it anyway, a switch made then is retried sooner, and it does not count
towards the switches after which the stage stays on `Standard` (a separate bound applies).

An app that holds the weights in memory (a web page that fetched them, for example) loads them
with `Dfn3Model::load_bytes`, which makes the same check and the same refusals as a load by
path. An app with no thread to spare for the load, such as a Web Audio worklet, can spread it
over time: a `dfn3::Dfn3Loader` takes the bytes in pieces of any size, first to check their
sha256 (nothing is parsed until it matches) and then to read them; `Dfn3Layer::cold` and
`Dfn3Layer::warm_up_step` build the model's layer one block of work at a time; and
`StageControl::attach_dfn3` hands it to a running stage, which fades `Strong` in without a
click. Where the standard library has no clock (`wasm32-unknown-unknown`), `Stage::set_clock`
gives the stage one, so that it can time its blocks and step down.

The same caller can spread each block's model work too: `Stage::begin_block` does the cheap
part, `Stage::step_block` runs one part of the model at a time (`Stage::block_steps_left`
says how many are left), and `Stage::end_block` finishes the block, with the time the caller
counts for it. The output is `Stage::process`'s, bit for bit, however the steps are spread; a
caller that spreads a block over the time until the next one is in sends it out one block
later.

The weights are not in this package. They are the asset `sew-dfn3-0.11.6.tar.gz` of the Sew
0.11.6 release on <https://github.com/Alakazam-211/sew-releases> (tag `v0.11.6`; sha256
`153721fd5f9f33663da7d831b3b611adcb9ca9e65233a6128d94941ad48d6697`). It holds a `dfn3/` folder
with the weights (`dfn3_ll.nnef.tar`), their NOTICE and the MIT and Apache-2.0 texts. The
loader checks the weights file against `dfn3::WEIGHTS_SHA256`
(`b6e52c0bc66b82e42e3403d22cdd3b847143026d6a41cc4380a97b044f7766af`) before it reads a weight.
The file is an NNEF archive (NNEF: the Khronos Group's Neural Network Exchange Format): the
authors' weights, values unchanged, as tensor files. [PROVENANCE.md](PROVENANCE.md) records
the authors' file, its hash and the conversion.

### Frames and streams

```rust
use sew_noise::{Denoiser, FRAME, StreamDenoiser};

let mut denoiser = Denoiser::new();
let (input, mut output) = ([0.0f32; FRAME], [0.0f32; FRAME]);
let voice_probability = denoiser.process_frame(&input, &mut output).unwrap();
assert!((0.0..=1.0).contains(&voice_probability));

// 128-sample blocks at 44.1 kHz.
let mut stream = StreamDenoiser::new(44_100, 128).unwrap();
let (input, mut output) = ([0.0f32; 128], [0.0f32; 128]);
stream.process(&input, &mut output).unwrap();
```

`StreamDenoiser` is two frames (20 ms) late at 48 kHz: one frame of buffering and the model's
own frame. `StreamDenoiser::latency_samples` gives the figure at its rate.

`StreamDenoiser` gives the same output as the stage in `Standard` (the model with the floor
and the onset guard), one frame later. `Denoiser` gives the model's output as it is, with
neither.

## What it does not do

- It is built for 48 kHz mono. `StreamDenoiser` resamples input from 8 to 192 kHz (linear
  interpolation), so feed it 48 kHz where you can.
- It does not cancel echo or control gain.
- It does not remove a second voice near the microphone.

## Licence

`MIT OR Apache-2.0`, at your option. `nnnoiseless` is BSD-3-Clause, and so are RNNoise's
weights, which are built into it; its notice is in [NOTICE](NOTICE) and must travel with a
binary that includes this crate.
[PROVENANCE.md](PROVENANCE.md) records where the model comes from.

RNNoise is the work of Jean-Marc Valin, Mozilla and the Xiph.Org Foundation. DeepFilterNet is
the work of Hendrik Schröter and the DeepFilterNet authors. This crate is not affiliated with
or endorsed by them or by the authors of `nnnoiseless`.

With the `dfn3` feature, a binary that ships the DeepFilterNet3 weights also ships their NOTICE
and the MIT and Apache-2.0 texts that come with them.
