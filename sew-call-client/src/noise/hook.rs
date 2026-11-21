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

use kessel_audio::{
    AudioEngine, DspSchedConfig, DspSchedFallback, DspSchedMode, DspScheduling, DspTimeConstraint,
    ProcessingConfig, StageBypassReason, StageClock, TransientEvent,
};

use super::{
    CaptureDspSched, CaptureDspSchedFallback, CaptureDspSchedMode, CaptureDspScheduling,
    CaptureStageClock, CaptureTransient,
};

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

/// Whether this build's kessel-audio takes a scheduling request for its processing thread.
pub(crate) const DSP_SCHED: bool = true;

/// kessel-audio's mode for ours (`None`: `unsupported`, which is only ever reported).
fn sched_mode(m: CaptureDspSchedMode) -> Option<DspSchedMode> {
    match m {
        CaptureDspSchedMode::Workgroup => Some(DspSchedMode::Workgroup),
        CaptureDspSchedMode::TimeConstraintOnly => Some(DspSchedMode::TimeConstraintOnly),
        CaptureDspSchedMode::None => Some(DspSchedMode::None),
        CaptureDspSchedMode::OwnWorkgroup => Some(DspSchedMode::OwnWorkgroup),
        CaptureDspSchedMode::Unsupported => None,
    }
}

/// Ours for kessel-audio's mode (a mode it adds later is `None`).
fn capture_sched_mode(m: DspSchedMode) -> Option<CaptureDspSchedMode> {
    match m {
        DspSchedMode::Workgroup => Some(CaptureDspSchedMode::Workgroup),
        DspSchedMode::TimeConstraintOnly => Some(CaptureDspSchedMode::TimeConstraintOnly),
        DspSchedMode::None => Some(CaptureDspSchedMode::None),
        DspSchedMode::OwnWorkgroup => Some(CaptureDspSchedMode::OwnWorkgroup),
        DspSchedMode::Unsupported => Some(CaptureDspSchedMode::Unsupported),
        _ => None,
    }
}

/// Ours for kessel-audio's fallback reason (one it adds later is `Other`).
fn sched_fallback(f: DspSchedFallback) -> CaptureDspSchedFallback {
    match f {
        DspSchedFallback::TimeConstraintRefused => CaptureDspSchedFallback::TimeConstraintRefused,
        DspSchedFallback::OwnWorkgroupUnavailable => {
            CaptureDspSchedFallback::OwnWorkgroupUnavailable
        }
        DspSchedFallback::OwnWorkgroupJoinRefused => {
            CaptureDspSchedFallback::OwnWorkgroupJoinRefused
        }
        DspSchedFallback::IntervalRefused => CaptureDspSchedFallback::IntervalRefused,
        _ => CaptureDspSchedFallback::Other,
    }
}

fn micros(us: u32) -> std::time::Duration {
    std::time::Duration::from_micros(u64::from(us))
}

fn as_micros(d: std::time::Duration) -> u32 {
    u32::try_from(d.as_micros()).unwrap_or(u32::MAX)
}

/// kessel-audio's scheduling config for `sched` (`None`: its mode cannot be asked for).
fn sched_config(sched: CaptureDspSched) -> Option<DspSchedConfig> {
    Some(
        DspSchedConfig::new(sched_mode(sched.mode)?)
            .with_time_constraint(DspTimeConstraint::new(
                micros(sched.period_us),
                micros(sched.computation_us),
                micros(sched.constraint_us),
            ))
            .with_interval_deadline(micros(sched.interval_deadline_us)),
    )
}

/// Puts `sched` into the engine's config, for the processing thread to read as it starts.
/// `false`: its mode cannot be asked for, and `cfg` is unchanged.
pub(crate) fn apply_dsp_sched(
    cfg: &mut kessel_audio::EngineConfig,
    sched: CaptureDspSched,
) -> bool {
    let Some(config) = sched_config(sched) else {
        return false;
    };
    cfg.dsp_sched = config;
    true
}

/// kessel-audio's view of its processing thread's scheduling request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DspSchedState {
    /// The mode asked for, and the mode in effect.
    pub(crate) requested: CaptureDspSchedMode,
    pub(crate) effective: CaptureDspSchedMode,
    /// Why they differ.
    pub(crate) fallback: Option<CaptureDspSchedFallback>,
    /// The time-constraint figures asked for: period, computation, constraint, microseconds.
    pub(crate) time_constraint_us: Option<(u32, u32, u32)>,
    /// The work interval's deadline, microseconds.
    pub(crate) interval_deadline_us: Option<u32>,
    /// Work intervals finished.
    pub(crate) intervals: u64,
}

/// The engine's scheduling request and what came of it (`None`: a mode this version does not
/// know).
pub(crate) fn dsp_sched(engine: &AudioEngine) -> Option<DspSchedState> {
    let s = engine.stats().dsp_sched;
    Some(DspSchedState {
        requested: capture_sched_mode(s.mode_requested)?,
        effective: capture_sched_mode(s.mode_effective)?,
        fallback: s.fallback_reason.map(sched_fallback),
        time_constraint_us: s.time_constraint.map(|t| {
            (
                as_micros(t.period),
                as_micros(t.computation),
                as_micros(t.constraint),
            )
        }),
        interval_deadline_us: s.interval_deadline.map(as_micros),
        intervals: s.intervals,
    })
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

/// Ours for kessel-audio's processing thread scheduling (a mode it adds later is `None`).
fn dsp_scheduling(d: DspScheduling) -> Option<CaptureDspScheduling> {
    match d {
        DspScheduling::Workgroup => Some(CaptureDspScheduling::Workgroup),
        DspScheduling::TimeConstraint => Some(CaptureDspScheduling::TimeConstraint),
        DspScheduling::Qos => Some(CaptureDspScheduling::Qos),
        DspScheduling::None => Some(CaptureDspScheduling::None),
        DspScheduling::OwnWorkgroup => Some(CaptureDspScheduling::OwnWorkgroup),
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
    /// How the OS schedules the processing thread the stage runs on.
    pub(crate) dsp_scheduling: Option<CaptureDspScheduling>,
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
        dsp_scheduling: dsp_scheduling(st.dsp_scheduling),
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

    /// Each scheduling mode maps to kessel-audio's of the same name and back; `unsupported` is
    /// only reported; each fallback reason keeps its name; the default setting is
    /// kessel-audio's default, figure for figure.
    #[test]
    fn dsp_sched_maps_each_mode_and_the_defaults() {
        for (ours, k) in [
            (CaptureDspSchedMode::Workgroup, DspSchedMode::Workgroup),
            (
                CaptureDspSchedMode::TimeConstraintOnly,
                DspSchedMode::TimeConstraintOnly,
            ),
            (CaptureDspSchedMode::None, DspSchedMode::None),
            (
                CaptureDspSchedMode::OwnWorkgroup,
                DspSchedMode::OwnWorkgroup,
            ),
        ] {
            assert_eq!(sched_mode(ours), Some(k));
            assert_eq!(capture_sched_mode(k), Some(ours));
            assert_eq!(ours.as_str(), k.as_str());
        }
        assert_eq!(sched_mode(CaptureDspSchedMode::Unsupported), None);
        assert_eq!(
            capture_sched_mode(DspSchedMode::Unsupported),
            Some(CaptureDspSchedMode::Unsupported)
        );
        assert_eq!(
            CaptureDspSchedMode::Unsupported.as_str(),
            DspSchedMode::Unsupported.as_str()
        );
        for (k, ours, name) in [
            (
                DspSchedFallback::TimeConstraintRefused,
                CaptureDspSchedFallback::TimeConstraintRefused,
                "time_constraint_refused",
            ),
            (
                DspSchedFallback::OwnWorkgroupUnavailable,
                CaptureDspSchedFallback::OwnWorkgroupUnavailable,
                "own_workgroup_unavailable",
            ),
            (
                DspSchedFallback::OwnWorkgroupJoinRefused,
                CaptureDspSchedFallback::OwnWorkgroupJoinRefused,
                "own_workgroup_join_refused",
            ),
            (
                DspSchedFallback::IntervalRefused,
                CaptureDspSchedFallback::IntervalRefused,
                "interval_refused",
            ),
        ] {
            assert_eq!(sched_fallback(k), ours);
            assert_eq!(k.as_str(), name);
            assert_eq!(serde_json::to_value(ours).ok(), Some(name.into()));
        }
        assert_eq!(
            sched_config(CaptureDspSched::default()),
            Some(DspSchedConfig::default())
        );
        let own = CaptureDspSched::new(CaptureDspSchedMode::OwnWorkgroup)
            .with_time_constraint_us(10_000, 3_000, 5_000)
            .with_interval_deadline_us(4_000);
        let mut cfg = kessel_audio::EngineConfig::default();
        assert!(apply_dsp_sched(&mut cfg, own));
        assert_eq!(cfg.dsp_sched.mode, DspSchedMode::OwnWorkgroup);
        assert_eq!(cfg.dsp_sched.time_constraint.period, micros(10_000));
        assert_eq!(cfg.dsp_sched.time_constraint.computation, micros(3_000));
        assert_eq!(cfg.dsp_sched.time_constraint.constraint, micros(5_000));
        assert_eq!(cfg.dsp_sched.interval_deadline, micros(4_000));
        let before = cfg.dsp_sched;
        assert!(!apply_dsp_sched(
            &mut cfg,
            CaptureDspSched::new(CaptureDspSchedMode::Unsupported)
        ));
        assert_eq!(cfg.dsp_sched, before);
    }

    /// kessel-audio's processing thread scheduling maps one to one, its default to `none`.
    #[test]
    fn dsp_scheduling_maps_each_mode() {
        for (k, ours) in [
            (DspScheduling::Workgroup, CaptureDspScheduling::Workgroup),
            (
                DspScheduling::TimeConstraint,
                CaptureDspScheduling::TimeConstraint,
            ),
            (DspScheduling::Qos, CaptureDspScheduling::Qos),
            (DspScheduling::None, CaptureDspScheduling::None),
            (
                DspScheduling::OwnWorkgroup,
                CaptureDspScheduling::OwnWorkgroup,
            ),
        ] {
            assert_eq!(dsp_scheduling(k), Some(ours));
            assert_eq!(k.as_str(), ours.as_str());
            assert_eq!(
                serde_json::to_value(ours).ok(),
                Some(k.as_str().into()),
                "the JSON name is kessel-audio's name"
            );
        }
        assert_eq!(
            dsp_scheduling(DspScheduling::default()),
            Some(CaptureDspScheduling::None)
        );
    }
}
