# Changelog

## 0.10.3 (2026-11-16)

### Added

- `dfn3::Dfn3Model::verify(path)`: checks a weights file against the pinned sha256 without
  loading it (read in chunks, nothing kept).
- `Dfn3Layer` and `StageControl::attach_dfn3(layer)` (feature `dfn3`): a running stage built
  without the model is given it later. The layer is built and warmed up off the audio thread;
  the stage takes it at its next block with a try-lock that never waits and no allocation,
  and Strong then warms up and fades in as in a stage built with the model.

### Changed

- The DeepFilterNet3 model's feature maps are laid out bin-major (an internal change to how
  the model's data is laid out). The output is identical, sample for sample.

## 0.10.2 (2026-11-16)

- The first release on crates.io (0.9.3 and 0.10.0 were in the sew-crates mirror only). The
  version follows Sew 0.10.2 (there is no 0.10.1).

### Breaking

- `Mode` and `StageStats` are `#[non_exhaustive]`: a later mode or counter is not a breaking
  change.

### Added

- Cargo feature `dfn3`: `Mode::Strong` is the low-latency variant of DeepFilterNet3 (by Hendrik
  Schröter and the DeepFilterNet authors, MIT OR Apache-2.0), run by this crate's own
  inference code. The feature adds RustCrypto's `sha2` (for the weights' pin) and no C, no
  `unsafe` and no neural-network runtime.
  - `dfn3::Dfn3Model::load(path)`: the weights, from a file an app ships beside itself (they
    are not in this package). The file's sha256 must be `dfn3::WEIGHTS_SHA256`; any other file,
    or a missing one, is refused (`dfn3::LoadError`) before a weight is read.
  - `Stage::with_dfn3(model)`: a stage that starts in `Strong`, the model warmed up when the
    stage is built. `StageControl::strong_available` says whether a stage has the model;
    `set_mode(Mode::Strong)` on a stage without it is refused as before.
  - In `Strong` the stage runs Standard beside the model and steps down to it by itself when
    its blocks get too slow, with a one-block fade that does not run the model again; it
    retries later, and after repeated step-downs it stays on Standard. A model output that is
    not finite is a fault and final at once.
  - `StageStats` gains `strong_state` (`StrongState`), `model_blocks`, `step_downs`,
    `last_step_down` (`StepDownReason`), `last_step_down_block` and `retry_at_block`.
    `StageControl::active` is `Mode::Strong` while the model's output goes out.
  - The latency is the same in every mode, as before; a block allocates nothing and takes
    no lock.
- `Mode::is_available` is true for `Strong` in a build with `dfn3`.

## 0.10.0 (2026-11-15)

- The version follows Sew 0.10.0. No API or behaviour change.

## 0.9.3 (2026-11-12)

- `Stage`: a capture stage for native audio paths. It processes one 480-sample block of 48 kHz
  mono `f32` in place, with a fixed latency of one block (10 ms) in every mode.
  - `Mode`: `Off`, `Standard` (the RNNoise model) and `Strong` (not built into this version:
    asking for it is refused with `ModeUnavailable`).
  - `StageControl`: a handle any thread can use to change the mode and to read what runs, the
    voice probability and the timing counters. The stage and its handles share atomics only.
  - A mode change is cross-faded over one block. `Off` passes the input through unchanged and
    does not run the model.
  - `StageStats`: blocks processed, blocks over a time budget (6 ms by default), the slowest
    block, and the same per 5 s window, for a caller that steps down when a stage overruns.
  - `Stage::process` does not allocate or lock; non-finite input is read as silence.
- `Stage` in `Standard` keeps a floor and has an onset guard, both fixed:
  - the input stays in the output 30 dB under its own level, so nothing is taken down by more
    than 30 dB;
  - when a block's level rises 20 dB above the running noise floor after at least 30 ms that
    did not, the block is not spiky (peak at most 3 times its RMS level) and the next block
    is still that far above the floor, the input passes unchanged for 40 ms from the block
    that rose. No added latency. `StageStats::guard_opens` counts the openings.
- `StreamDenoiser` has the same floor and onset guard, so a Web Audio worklet and a native
  capture path give the same output for the same input. Its latency is unchanged (it already
  held the frame before the newest one). `StreamDenoiser::guard_opens` counts the openings.
  Fan-type noise now sits on the 30 dB floor.
- `Denoiser` is unchanged: it gives the model's output as it is.
- `warm_up_thread`: makes the calling thread ready to denoise without allocating. The FFT plans
  are kept per thread, so a stage or denoiser built on one thread and run on another needs it
  (or `Stage::reset`) called once on the running thread.
- Cargo feature `capture-stage`: the `capture_stage::CaptureStage` trait, implemented for
  `Stage`.
- `Denoiser::new` warms a thread up once, not at every call.
- First packaged for the sew-crates mirror, with README, NOTICE (the BSD-3-Clause text of
  nnnoiseless) and PROVENANCE.md (the model's source).
