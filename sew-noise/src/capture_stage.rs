//! The capture-hook shape an audio pipeline calls a [`Stage`] through (cargo feature
//! `capture-stage`).
//!
//! [`CaptureStage`] is the hook a capture pipeline offers between echo cancellation and gain
//! control: one 10 ms block of 48 kHz mono in place, a fixed latency, a reset. It is declared
//! here, with [`Stage`] implementing it, so the fit is checked by the compiler. When a pipeline
//! crate publishes the trait itself, this module implements that crate's trait for [`Stage`]
//! and re-exports it under the same name.

use crate::{FRAME, Stage};

/// A stage in a capture pipeline. It runs on the processing thread once per 10 ms capture
/// block, after the high-pass filter and echo cancellation and before gain control. No
/// allocation, lock or panic.
pub trait CaptureStage: Send + 'static {
    /// One block of 48 kHz mono in `[-1, 1]`, in place.
    fn process(&mut self, block: &mut [f32; FRAME]);
    /// How far the output lags the input, in samples: fixed for the stage's life.
    fn latency_samples(&self) -> usize;
    /// Forget held-back audio (a device change, a long pause).
    fn reset(&mut self);
}

impl CaptureStage for Stage {
    fn process(&mut self, block: &mut [f32; FRAME]) {
        Stage::process(self, block);
    }

    fn latency_samples(&self) -> usize {
        Stage::LATENCY
    }

    fn reset(&mut self) {
        Stage::reset(self);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::Mode;

    #[test]
    fn a_boxed_stage_runs_through_the_hook() {
        let stage = Stage::new();
        let control = stage.control();
        control.set_mode(Mode::Off).unwrap();
        let mut hook: Box<dyn CaptureStage> = Box::new(stage);
        assert_eq!(hook.latency_samples(), 480);
        let mut first = [0.25f32; FRAME];
        hook.process(&mut first);
        assert!(first.iter().all(|v| *v == 0.0), "one block of latency");
        let mut second = [0.0f32; FRAME];
        hook.process(&mut second);
        assert!(second.iter().all(|v| *v == 0.25));
        hook.reset();
        let mut third = [0.0f32; FRAME];
        hook.process(&mut third);
        assert!(third.iter().all(|v| *v == 0.0), "reset drops held audio");
        // The stage moves to the processing thread.
        std::thread::spawn(move || {
            hook.reset();
            hook.process(&mut [0.0; FRAME]);
        })
        .join()
        .unwrap();
        assert_eq!(control.stats().cold_thread_blocks, 0);
    }
}
