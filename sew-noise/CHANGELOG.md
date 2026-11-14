# Changelog

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
  - `Stage::process` does not allocate, lock or panic; non-finite input is read as silence.
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
  Fan-type noise, which the model alone takes down by about 70 dB, now sits 30 dB down.
- `Denoiser` is unchanged: it gives the model's output as it is.
- `warm_up_thread`: makes the calling thread ready to denoise without allocating. The FFT plans
  are kept per thread, so a stage or denoiser built on one thread and run on another needs it
  (or `Stage::reset`) called once on the running thread.
- Cargo feature `capture-stage`: the `capture_stage::CaptureStage` trait, implemented for
  `Stage`.
- `Denoiser::new` warms a thread up once, not at every call.
- First packaged for the sew-crates mirror, with README, NOTICE (the BSD-3-Clause text of
  nnnoiseless) and PROVENANCE.md (the model's source).
