//! The seam to kessel-audio's capture hook: the trait the stage is handed over as, putting it
//! into the processing config, and reading the pipeline's bypass state.
//!
//! What kessel-audio does with the stage:
//! - it runs between echo cancellation and gain control, once per 10 ms block on the processing
//!   thread; `reset` runs there once before the first `process` (so the stage's first block
//!   allocates nothing), and again on a stream restart, after a capture gap of more than a
//!   second and before a retry;
//! - two calls in a row of 9 ms or more, or a 99th percentile over 6 ms across the last 100
//!   calls, is a strike (one late call alone never is): the stage is bypassed (the echo
//!   canceller's output goes on at the stage's latency) and retried after 10 s of capture; it is
//!   bypassed for good only after 3 strikes within 60 s of capture (older strikes decay); a panic
//!   or a non-finite sample is a fault, bypassed for good;
//! - while it is bypassed, kessel-audio's own noise suppression (Moderate, the default fallback
//!   kept here) stands in, cross-faded over one block;
//! - `EncodedFrame::ts48k` does not include the stage's latency: the audio in a frame is
//!   `latency_samples` (480, 10 ms) older than its stamp.

use kessel_audio::{AudioEngine, ProcessingConfig, StageBypassReason};

pub(crate) use kessel_audio::CaptureStage;

/// Whether this build's kessel-audio has the capture hook.
pub(crate) const HOOKED: bool = true;

/// Why the pipeline bypasses the stage now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Bypass {
    /// Over its time budget.
    Budget,
    /// A panic or non-finite output.
    Fault,
}

/// Hands `stage` to the pipeline, with kessel-audio's own noise suppression off (its fallback
/// for a bypassed stage stays at the default). `true`: the stage is set.
pub(crate) fn install(processing: &mut ProcessingConfig, stage: Box<dyn CaptureStage>) -> bool {
    processing.noise_suppression = None;
    processing.capture_stage = Some(stage);
    true
}

/// kessel-audio's view of the stage, for the call's log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PipelineStage {
    /// Budget strikes and faults so far.
    pub(crate) strikes: u32,
    /// Why it was last bypassed (sticky: kept after a retry).
    pub(crate) bypassed: Option<Bypass>,
    /// The 99th percentile of the stage's last 100 calls, microseconds.
    pub(crate) p99_us: u32,
}

/// kessel-audio's stage counters, when a stage is set.
pub(crate) fn pipeline_stage(engine: &AudioEngine) -> Option<PipelineStage> {
    let st = engine.stats().capture_stage?;
    Some(PipelineStage {
        strikes: st.strikes,
        bypassed: st.bypassed.map(bypass_of),
        p99_us: st.p99_block_us,
    })
}

fn bypass_of(r: StageBypassReason) -> Bypass {
    match r {
        StageBypassReason::Budget => Bypass::Budget,
        _ => Bypass::Fault,
    }
}

/// Why the stage is bypassed now, if it is: kessel-audio's sticky reason while the stage's
/// output is not what goes on. A stage brought back by a retry is not bypassed (its reason
/// stays in kessel-audio's stats); one warming up after a retry still is.
pub(crate) fn bypass(engine: &AudioEngine) -> Option<Bypass> {
    let st = engine.stats().capture_stage?;
    if st.active {
        return None;
    }
    st.bypassed.map(bypass_of)
}
