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
//!   `latency_samples` (480, 10 ms) older than its stamp;
//! - for a grace window after a busy moment of the device (the stage starting, a capture
//!   stream starting or restarting, the phone's audio session activating, a route change, and
//!   each `TransientEvent` reported through `AudioEngine::transient_event`), slow calls are
//!   timed but do not count toward a strike; a fault is still final at once. The window is as
//!   long as the stage's own (`sew_noise::DEFAULT_GRACE`), so the pipeline and the stage hold
//!   their timing rules back for the same moment;
//! - the calls are timed on the processing thread's CPU time where the platform has it (the
//!   default clock, kept here), so time the device spends on other threads does not count
//!   against the stage; a call long enough on the wall clock (the default starvation cutoff)
//!   still counts as slow whatever its CPU time.

use kessel_audio::{AudioEngine, ProcessingConfig, StageBypassReason, StageClock, TransientEvent};

use super::{CaptureStageClock, CaptureTransient};

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

/// The pipeline's grace window for the stage: the stage's own (`sew_noise::DEFAULT_GRACE`).
pub(crate) const STAGE_GRACE: std::time::Duration = sew_noise::DEFAULT_GRACE;

/// Hands `stage` to the pipeline, with kessel-audio's own noise suppression off (its fallback
/// for a bypassed stage stays at the default) and its grace window for the stage as long as
/// the stage's own ([`STAGE_GRACE`]). `true`: the stage is set.
pub(crate) fn install(processing: &mut ProcessingConfig, stage: Box<dyn CaptureStage>) -> bool {
    processing.noise_suppression = None;
    processing.capture_stage = Some(stage);
    processing.stage_grace = STAGE_GRACE;
    true
}

/// kessel-audio's name for a busy moment.
pub(crate) fn transient_event(t: CaptureTransient) -> TransientEvent {
    match t {
        CaptureTransient::CallStart => TransientEvent::CallStart,
        CaptureTransient::AudioSessionActivated => TransientEvent::AudioSessionActivated,
        CaptureTransient::RouteChange => TransientEvent::RouteChange,
        CaptureTransient::CameraStart => TransientEvent::CameraStart,
        CaptureTransient::AppSceneChange => TransientEvent::AppSceneChange,
        CaptureTransient::Other => TransientEvent::Other,
    }
}

/// Ours for kessel-audio's clock (a clock it adds later is `None`).
fn stage_clock(c: StageClock) -> Option<CaptureStageClock> {
    match c {
        StageClock::ThreadCpu => Some(CaptureStageClock::ThreadCpu),
        StageClock::Wall => Some(CaptureStageClock::Wall),
        _ => None,
    }
}

/// Ours for kessel-audio's (a kind it adds later is `Other`).
fn capture_transient(t: TransientEvent) -> CaptureTransient {
    match t {
        TransientEvent::CallStart => CaptureTransient::CallStart,
        TransientEvent::AudioSessionActivated => CaptureTransient::AudioSessionActivated,
        TransientEvent::RouteChange => CaptureTransient::RouteChange,
        TransientEvent::CameraStart => CaptureTransient::CameraStart,
        TransientEvent::AppSceneChange => CaptureTransient::AppSceneChange,
        _ => CaptureTransient::Other,
    }
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
    /// A grace window is open now.
    pub(crate) grace: bool,
    /// Blocks processed inside grace windows so far.
    pub(crate) grace_blocks: u64,
    /// Of those, the calls over the budget that a window excused.
    pub(crate) grace_over_budget: u64,
    /// The last busy moment reported.
    pub(crate) last_transient: Option<CaptureTransient>,
    /// The clock timing the calls.
    pub(crate) clock: Option<CaptureStageClock>,
    /// The longest call on the wall clock, microseconds.
    pub(crate) max_wall_us: u32,
    /// Calls over the budget on the wall clock but not in CPU time.
    pub(crate) preempted_over_budget: u64,
    /// Calls past the starvation cutoff.
    pub(crate) starved: u64,
}

/// kessel-audio's stage counters, when a stage is set.
pub(crate) fn pipeline_stage(engine: &AudioEngine) -> Option<PipelineStage> {
    let st = engine.stats().capture_stage?;
    Some(PipelineStage {
        strikes: st.strikes,
        bypassed: st.bypassed.map(bypass_of),
        p99_us: st.p99_block_us,
        grace: st.grace,
        grace_blocks: st.grace_blocks,
        grace_over_budget: st.grace_over_budget,
        last_transient: st.last_transient.map(capture_transient),
        clock: stage_clock(st.clock),
        max_wall_us: st.max_wall_us,
        preempted_over_budget: st.preempted_over_budget,
        starved: st.starved,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Each busy moment maps to kessel-audio's own kind and back.
    #[test]
    fn transients_round_trip() {
        for t in [
            CaptureTransient::CallStart,
            CaptureTransient::AudioSessionActivated,
            CaptureTransient::RouteChange,
            CaptureTransient::CameraStart,
            CaptureTransient::AppSceneChange,
            CaptureTransient::Other,
        ] {
            assert_eq!(capture_transient(transient_event(t)), t);
        }
    }

    struct Pass;

    impl CaptureStage for Pass {
        fn process(&mut self, _block: &mut [f32; 480]) {}
        fn latency_samples(&self) -> usize {
            0
        }
        fn reset(&mut self) {}
    }

    /// The pipeline's grace window is the stage's own, and one the pipeline accepts.
    #[test]
    fn install_sets_the_stages_grace() {
        assert_eq!(STAGE_GRACE, sew_noise::DEFAULT_GRACE);
        assert!(STAGE_GRACE <= kessel_audio::MAX_STAGE_GRACE);
        assert!(STAGE_GRACE >= kessel_audio::DEFAULT_STAGE_GRACE);
        let mut p = ProcessingConfig::default();
        assert!(install(&mut p, Box::new(Pass)));
        assert_eq!(p.stage_grace, STAGE_GRACE);
        // The default clock and starvation cutoff are kept.
        let d = ProcessingConfig::default();
        assert_eq!(p.stage_clock, d.stage_clock);
        assert_eq!(p.stage_starvation_cutoff, d.stage_starvation_cutoff);
        assert_eq!(
            stage_clock(StageClock::default()),
            Some(if StageClock::ThreadCpu.is_supported() {
                CaptureStageClock::ThreadCpu
            } else {
                CaptureStageClock::Wall
            })
        );
        assert!(p.capture_stage.is_some());
        assert!(p.noise_suppression.is_none());
    }
}
