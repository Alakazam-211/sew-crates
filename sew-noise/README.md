# sew-noise

sew-noise is microphone noise removal in pure Rust: a real-time capture stage, a frame API and a stream API over `nnnoiseless`, the Rust port of RNNoise.

It takes 48 kHz mono speech and removes background noise before the audio is encoded. The
model is RNNoise's (by Jean-Marc Valin and Mozilla), run by
[`nnnoiseless`](https://crates.io/crates/nnnoiseless) (Joe Neeman's port to Rust), with the
weights compiled in: nothing is loaded at run time, there is no C code and no `unsafe`.

- `Stage`: for a native capture path. One 480-sample (10 ms) block in place, a fixed latency
  of one block, and a mode (`Off`, `Standard`) that any thread can change while audio runs.
- `Denoiser`: one 480-sample frame in, one out.
- `StreamDenoiser`: blocks of any size at any rate from 8 to 192 kHz (it resamples to and
  from 48 kHz), for a Web Audio worklet or any callback whose block size is not 480.

After construction nothing allocates. Non-finite input samples are read as silence and the
output is always finite.

It is part of Sew, Alakazam Labs' self-hosted team chat server, where it cleans the microphone
of voice calls on the sender's device.

- Source: <https://github.com/Alakazam-211/sew-crates> (read-only: it is generated from Sew's
  repository at each release, so pull requests are not merged there)
- Security problems: report them privately, as
  <https://github.com/Alakazam-211/sew-crates/security/policy> says.

## Usage

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
  the input through bit for bit and does not run the model. `Strong` names a stronger model that this version does not contain;
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

- It works on 48 kHz mono. Other rates go through `StreamDenoiser`, whose converter is linear
  interpolation: fine for speech at 44.1 kHz, audibly aliased far below it.
- It does not cancel echo or control gain.
- It does not remove a second voice near the microphone.

## Licence

`MIT OR Apache-2.0`, at your option. `nnnoiseless` is BSD-3-Clause; its notice is in
[NOTICE](NOTICE) and must travel with a binary that includes this crate.
[PROVENANCE.md](PROVENANCE.md) records where the model comes from.

RNNoise is the work of Jean-Marc Valin, Mozilla and the Xiph.Org Foundation. This crate is not
affiliated with or endorsed by them or by the authors of `nnnoiseless`.
