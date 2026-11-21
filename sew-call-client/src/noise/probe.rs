//! The noise removal stage's probe (feature `noise-probe`; for measurement builds only, never
//! a release): a call whose stage records what each 10 ms block cost on the audio pipeline's
//! processing thread, which core the thread was on and what DeepFilterNet3 ran, beside two
//! fixed loops on the same thread (`sew_noise::probe`). It answers how fast the OS runs that
//! thread, apart from how much work the model is.
//!
//! - `NoiseRemovalControl::set_dsp_sched` before a call, to pick how the processing thread is
//!   scheduled in it ([`NoiseProbePhase::sched`] gives each phase's setting). The pipeline
//!   reads it once as the call's audio starts, so a call is one phase.
//! - [`NoiseRemovalControl::arm_probe`] before a call: that call's stage is probed.
//! - [`NoiseRemovalControl::probe_report`] during or after it: the figures
//!   ([`NoiseProbeReport`], whose JSON an app saves or sends).
//! - [`reference()`] on a thread of the app's, before and after: the same two loops on a thread
//!   that asks the OS for its highest ordinary priority.
//!
//! On macOS and iOS, a probe build of tauri-plugin-sew-voice reads the thread's core with
//! `pthread_cpu_number_np`, linked strongly: such a build needs iOS 14.2 or macOS 11 or later.
//!
//! A probed call keeps DeepFilterNet3 running however slow a block is (the stage's step-down
//! rules are off), and the loops add time to some blocks, so its audio can glitch and the
//! audio pipeline may bypass the stage.
//!
//! What the report does not hold, and an app adds beside it: [`reference()`]'s runs, the
//! device's core counts by type, the audio session's buffer and rate, the thermal state, the
//! power mode, and what else ran (a camera).

use serde::Serialize;
use sew_noise::probe::{self as raw, ProbeHandle, Quantiles};

use super::{
    CaptureDspSched, CaptureDspSchedFallback, CaptureDspSchedMode, NoiseRemovalControl,
    NoiseRemovalState,
};

/// The core the calling thread is on, where a system has a call for it. It runs on the audio
/// thread: no allocation, no lock.
pub type CoreNumber = fn() -> Option<u32>;

/// The report's `schema`.
pub const SCHEMA: &str = "sew-noise-probe/2";

/// How the audio pipeline's processing thread is scheduled in a probed call. A call is one
/// phase: the pipeline reads the scheduling once, as the call's audio starts, and the report
/// names the phase from the mode the pipeline says is in effect.
///
/// Its JSON is `"default"`, `"time_constraint"`, `"unscheduled"` or `"short_deadline"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum NoiseProbePhase {
    /// Phase a: as a call runs without the probe (on macOS and iOS the thread asks for
    /// real-time scheduling; on iOS it also joins the audio unit's own workgroup).
    Default,
    /// Phase b: asks for real-time scheduling by the time-constraint policy alone, in no
    /// workgroup.
    TimeConstraint,
    /// Phase c: no real-time scheduling.
    Unscheduled,
    /// Phase d: asks for real-time scheduling in a workgroup of the pipeline's own, with a
    /// short deadline (the [`CaptureDspSched`] set before the call: 3 ms in
    /// [`NoiseProbePhase::sched`]).
    ShortDeadline,
}

impl NoiseProbePhase {
    /// Every phase, in order.
    pub const ALL: [Self; 4] = [
        Self::Default,
        Self::TimeConstraint,
        Self::Unscheduled,
        Self::ShortDeadline,
    ];

    /// The phase's letter: `a`, `b`, `c` or `d`.
    pub const fn letter(self) -> &'static str {
        match self {
            Self::Default => "a",
            Self::TimeConstraint => "b",
            Self::Unscheduled => "c",
            Self::ShortDeadline => "d",
        }
    }

    /// Whether this build can put the processing thread into the phase:
    /// [`Self::Default`] always, the others where the audio pipeline takes a scheduling
    /// request (`NoiseRemovalControl::dsp_sched_supported`).
    pub const fn available(self) -> bool {
        matches!(self, Self::Default) || super::hook::DSP_SCHED
    }

    /// The scheduling to set before a call (`NoiseRemovalControl::set_dsp_sched`) for the
    /// call to run in this phase, with the pipeline's default figures (a 10 ms period, 6 ms of
    /// computation, a 9 ms constraint; a 3 ms deadline in [`Self::ShortDeadline`]), so the
    /// phases differ in the mode alone. Change the figures on the result for further calls.
    pub const fn sched(self) -> CaptureDspSched {
        CaptureDspSched::new(match self {
            Self::Default => CaptureDspSchedMode::Workgroup,
            Self::TimeConstraint => CaptureDspSchedMode::TimeConstraintOnly,
            Self::Unscheduled => CaptureDspSchedMode::None,
            Self::ShortDeadline => CaptureDspSchedMode::OwnWorkgroup,
        })
    }

    /// The phase of a call whose processing thread runs in `mode` (a platform without the
    /// scheduling calls, and a mode this version does not know, are [`Self::Default`]).
    pub const fn of_mode(mode: CaptureDspSchedMode) -> NoiseProbePhase {
        match mode {
            CaptureDspSchedMode::TimeConstraintOnly => Self::TimeConstraint,
            CaptureDspSchedMode::None => Self::Unscheduled,
            CaptureDspSchedMode::OwnWorkgroup => Self::ShortDeadline,
            _ => Self::Default,
        }
    }

    const fn id(self) -> u8 {
        match self {
            Self::Default => 0,
            Self::TimeConstraint => 1,
            Self::Unscheduled => 2,
            Self::ShortDeadline => 3,
        }
    }

    fn from_id(id: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.id() == id)
    }
}

/// Why [`NoiseRemovalControl::set_probe_phase`] refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum NoiseProbeError {
    /// No probed call is running.
    #[error("no probed call is running")]
    NotProbing,
    /// This build cannot put the processing thread into the phase.
    #[error("probe phase {} is not available in this build", .0.letter())]
    PhaseUnavailable(NoiseProbePhase),
}

/// A probed call: its records, and the phases it went through.
pub(super) struct Run {
    call: u64,
    handle: ProbeHandle,
    current: NoiseProbePhase,
    /// The state as each phase ended, the current one's once the call's audio ended.
    ended: Vec<(NoiseProbePhase, NoiseRemovalState)>,
    /// The deadline each phase was last entered with, microseconds.
    deadlines: Vec<(NoiseProbePhase, u64)>,
    call_ended: bool,
}

/// The control's probe: armed for the next call, and the last probed call.
#[derive(Default)]
pub(super) struct Slot {
    armed: Option<CoreNumber>,
    run: Option<Run>,
}

impl Slot {
    /// The core function, if the next call is to be probed (and no longer after this).
    pub(super) fn take_armed(&mut self) -> Option<CoreNumber> {
        self.armed.take()
    }

    /// Call `call`'s stage is probed, with `handle` on its records.
    pub(super) fn started(&mut self, call: u64, handle: ProbeHandle) {
        self.run = Some(Run {
            call,
            handle,
            current: NoiseProbePhase::Default,
            ended: Vec::new(),
            deadlines: Vec::new(),
            call_ended: false,
        });
    }

    /// Whether call `call` is the probed one, still running.
    pub(super) fn runs(&self, call: u64) -> bool {
        self.run
            .as_ref()
            .is_some_and(|r| r.call == call && !r.call_ended)
    }

    /// Call `call`'s audio ended, in `state`.
    pub(super) fn ended(&mut self, call: u64, state: NoiseRemovalState) {
        if let Some(run) = self
            .run
            .as_mut()
            .filter(|r| r.call == call && !r.call_ended)
        {
            run.ended.push((run.current, state));
            run.call_ended = true;
        }
    }
}

/// What a probed call's processing thread asked the OS for and what it got, as the audio
/// pipeline reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct NoiseProbeSched {
    /// The mode asked for, and the mode in effect.
    pub requested: CaptureDspSchedMode,
    pub effective: CaptureDspSchedMode,
    /// Why they differ.
    pub fallback: Option<CaptureDspSchedFallback>,
    /// The time-constraint figures asked for, microseconds (`null` in a mode that does not
    /// ask for the policy).
    pub period_us: Option<u32>,
    pub computation_us: Option<u32>,
    pub constraint_us: Option<u32>,
    /// Each block's deadline in its work interval, microseconds (the short-deadline phase).
    pub interval_deadline_us: Option<u32>,
    /// Work intervals finished on the thread's own workgroup.
    pub intervals: u64,
}

impl NoiseProbeSched {
    /// The scheduling fields of `state` (`None`: the pipeline did not report them).
    fn of(state: &NoiseRemovalState) -> Option<NoiseProbeSched> {
        Some(NoiseProbeSched {
            requested: state.capture_stage_dsp_sched_requested?,
            effective: state.capture_stage_dsp_sched_effective?,
            fallback: state.capture_stage_dsp_sched_fallback,
            period_us: state.capture_stage_dsp_period_us,
            computation_us: state.capture_stage_dsp_computation_us,
            constraint_us: state.capture_stage_dsp_constraint_us,
            interval_deadline_us: state.capture_stage_dsp_interval_deadline_us,
            intervals: state.capture_stage_dsp_intervals,
        })
    }
}

/// One figure's times: how many, the median, the 99th percentile and the largest, in
/// nanoseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct NoiseProbeTimes {
    pub n: u64,
    pub p50_ns: u64,
    pub p99_ns: u64,
    pub max_ns: u64,
}

impl From<Quantiles> for NoiseProbeTimes {
    fn from(q: Quantiles) -> Self {
        Self {
            n: q.n,
            p50_ns: q.p50_ns,
            p99_ns: q.p99_ns,
            max_ns: q.max_ns,
        }
    }
}

/// Blocks the processing thread ended on one core.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct NoiseProbeCore {
    /// The core's number, as the system counts them.
    pub core: u32,
    pub blocks: u64,
}

/// One phase of a probed call.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct NoiseProbePhaseReport {
    /// The phase and its letter.
    pub phase: NoiseProbePhase,
    pub letter: &'static str,
    /// The workgroup deadline the phase was entered with, microseconds (phase d only).
    pub deadline_us: Option<u64>,
    /// Blocks in the phase, and how many of them the figures leave out (the memory loop ran
    /// in them or in the block before).
    pub blocks: u64,
    pub left_out: u64,
    /// Of the blocks kept: those DeepFilterNet3 ran in, those its ERB decoder ran in, those
    /// its deep-filter decoder ran in, and those whose output was DeepFilterNet3's.
    pub model_blocks: u64,
    pub erb_blocks: u64,
    pub df_blocks: u64,
    pub strong_out_blocks: u64,
    /// The stage's call over the blocks kept: wall time, and the thread's CPU time (`null`
    /// where the system has no thread CPU clock).
    pub stage_wall: NoiseProbeTimes,
    pub stage_cpu: Option<NoiseProbeTimes>,
    /// The same over the blocks in which both decoders ran (the model's full work).
    pub full_wall: NoiseProbeTimes,
    pub full_cpu: Option<NoiseProbeTimes>,
    /// The cache loop (`k1_*`) and the memory loop (`k2_*`) on the processing thread.
    pub k1_wall: NoiseProbeTimes,
    pub k1_cpu: Option<NoiseProbeTimes>,
    pub k2_wall: NoiseProbeTimes,
    pub k2_cpu: Option<NoiseProbeTimes>,
    /// Blocks per core, by core number, and the blocks whose core is not known.
    pub cores: Vec<NoiseProbeCore>,
    pub core_unknown: u64,
    /// The noise removal state as the phase ended (its `capture_stage_dsp_scheduling` is how
    /// the thread was scheduled); for the phase still running, as the report was made.
    pub state: Option<NoiseRemovalState>,
}

/// One block as recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct NoiseProbeBlock {
    /// The phase's letter.
    pub phase: Option<&'static str>,
    pub wall_ns: u32,
    pub cpu_ns: Option<u32>,
    pub core: Option<u32>,
    pub model: bool,
    pub erb: bool,
    pub df: bool,
    pub strong_out: bool,
    pub left_out: bool,
}

/// The size of the two loops and how often they run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct NoiseProbeLoops {
    /// The cache loop (`k1_*`): this many passes over two arrays of this many floats, after every
    /// this many blocks.
    pub k1_passes: usize,
    pub k1_floats: usize,
    pub k1_every_blocks: u64,
    /// The memory loop (`k2_*`): one pass over this many floats, after every this many blocks.
    pub k2_floats: usize,
    pub k2_every_blocks: u64,
}

impl NoiseProbeLoops {
    const NOW: Self = Self {
        k1_passes: raw::CACHE_LOOP_PASSES,
        k1_floats: raw::CACHE_LOOP_FLOATS,
        k1_every_blocks: raw::CACHE_LOOP_EVERY,
        k2_floats: raw::MEMORY_LOOP_FLOATS,
        k2_every_blocks: raw::MEMORY_LOOP_EVERY,
    };
}

/// What a probed call measured. Its JSON is the report an app saves or sends.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct NoiseProbeReport {
    /// [`SCHEMA`].
    pub schema: &'static str,
    /// `target_os` and `target_arch`.
    pub os: &'static str,
    pub arch: &'static str,
    /// The call's audio has ended (the report is final).
    pub call_ended: bool,
    /// The call's phase and its letter: from the scheduling mode in effect where the pipeline
    /// reports it, else [`NoiseProbePhase::Default`].
    pub phase: NoiseProbePhase,
    pub letter: &'static str,
    /// What the processing thread asked the OS for and what it got (`null`: the pipeline did
    /// not report it).
    pub dsp_sched: Option<NoiseProbeSched>,
    pub loops: NoiseProbeLoops,
    /// Blocks recorded, and blocks after the records were full (5 minutes).
    pub blocks: u64,
    pub dropped: u64,
    /// The system has a thread CPU clock.
    pub cpu_clock: bool,
    /// The phases, in the order they began.
    pub phases: Vec<NoiseProbePhaseReport>,
    /// The first 200 blocks.
    pub first_blocks: Vec<NoiseProbeBlock>,
}

/// The two loops on a thread of the app's ([`reference()`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct NoiseProbeReference {
    /// [`SCHEMA`].
    pub schema: &'static str,
    pub loops: NoiseProbeLoops,
    /// How long the thread spun before the loops, milliseconds.
    pub spin_ms: u64,
    /// The core the thread was on after the spin, and at the end.
    pub core_start: Option<u32>,
    pub core_end: Option<u32>,
    pub k1_wall: NoiseProbeTimes,
    pub k1_cpu: Option<NoiseProbeTimes>,
    pub k2_wall: NoiseProbeTimes,
    pub k2_cpu: Option<NoiseProbeTimes>,
}

/// The probe's two loops on the calling thread, after a spin of 300 ms: what the device does
/// on a thread that asks the OS for its highest ordinary priority. Call it on such a thread,
/// never the audio thread, before the call and after it. Blocks for the spin
/// (`REFERENCE_SPIN`) and the loops' runs, and holds 32 MiB. `core`: as for
/// [`NoiseRemovalControl::arm_probe`].
pub fn reference(core: Option<CoreNumber>) -> NoiseProbeReference {
    let r = raw::reference(core.unwrap_or(default_core_number));
    NoiseProbeReference {
        schema: SCHEMA,
        loops: NoiseProbeLoops::NOW,
        spin_ms: u64::try_from(raw::REFERENCE_SPIN.as_millis()).unwrap_or(u64::MAX),
        core_start: r.core_start,
        core_end: r.core_end,
        k1_wall: r.k1_wall.into(),
        k1_cpu: r.k1_cpu.map(Into::into),
        k2_wall: r.k2_wall.into(),
        k2_cpu: r.k2_cpu.map(Into::into),
    }
}

/// The core function a probe uses when the app gives none: `sched_getcpu` on Linux and
/// Android, nothing elsewhere (macOS and iOS have a call for it, which needs `unsafe`: the app
/// gives it).
pub fn default_core_number() -> Option<u32> {
    raw::core_number()
}

impl NoiseRemovalControl {
    /// The next call's stage is probed (once: the call after it is not). `core` answers the
    /// core the calling thread is on; `None` uses `sched_getcpu` on Linux and Android and
    /// records no core elsewhere. Arming again before a call replaces `core`. The last probed
    /// call's report stays until the next probed call starts.
    pub fn arm_probe(&self, core: Option<CoreNumber>) {
        self.probe_slot().armed = Some(core.unwrap_or(default_core_number));
    }

    /// Takes back [`Self::arm_probe`] before a call started.
    pub fn disarm_probe(&self) {
        self.probe_slot().armed = None;
    }

    /// Whether the next call's stage is to be probed.
    pub fn probe_armed(&self) -> bool {
        self.probe_slot().armed.is_some()
    }

    /// The phase the probed call is in (`None`: no probed call is running).
    pub fn probe_phase(&self) -> Option<NoiseProbePhase> {
        let slot = self.probe_slot();
        slot.run
            .as_ref()
            .filter(|r| !r.call_ended)
            .map(|r| r.current)
    }

    /// Labels the blocks from now on as `phase`. It does not change the scheduling: the
    /// pipeline reads that once per call (`Self::set_dsp_sched` before the call), and the
    /// report names the call's phase from the mode in effect, whatever was labelled here.
    /// `deadline` is the workgroup's deadline for [`NoiseProbePhase::ShortDeadline`] (the
    /// [`CaptureDspSched`] default is 3 ms) and is not used by the other phases; the report
    /// carries it.
    /// Refused for a phase this build cannot run ([`NoiseProbePhase::available`]) and without
    /// a probed call running.
    #[deprecated(
        note = "a call is one phase: set the scheduling before the call with set_dsp_sched; the report names the phase from the mode in effect"
    )]
    pub fn set_probe_phase(
        &self,
        phase: NoiseProbePhase,
        deadline: Option<std::time::Duration>,
    ) -> Result<(), NoiseProbeError> {
        if !phase.available() {
            return Err(NoiseProbeError::PhaseUnavailable(phase));
        }
        // Read before the probe's lock: the state takes the live call's.
        let state = self.state();
        let mut slot = self.probe_slot();
        let run = slot
            .run
            .as_mut()
            .filter(|r| !r.call_ended)
            .ok_or(NoiseProbeError::NotProbing)?;
        if run.current != phase {
            run.ended.push((run.current, state));
            run.current = phase;
            run.handle.set_phase(phase.id());
        }
        let deadline = deadline
            .filter(|_| phase == NoiseProbePhase::ShortDeadline)
            .map(|d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX));
        run.deadlines.retain(|(p, _)| *p != phase);
        if let Some(us) = deadline {
            run.deadlines.push((phase, us));
        }
        Ok(())
    }

    /// The last probed call's figures, during the call or after it (`None`: no call was
    /// probed). Allocates and sorts: not for a real-time thread.
    pub fn probe_report(&self) -> Option<NoiseProbeReport> {
        let state = self.state();
        let slot = self.probe_slot();
        let run = slot.run.as_ref()?;
        let r = run.handle.report();
        // The scheduling as the call ended (or now), which names the call's phase.
        let last = run.ended.last().map(|(_, s)| *s);
        let sched = NoiseProbeSched::of(&if run.call_ended {
            last.unwrap_or(state)
        } else {
            state
        });
        let call_phase = sched.map(|s| NoiseProbePhase::of_mode(s.effective));
        let phases = r
            .phases
            .iter()
            .map(|p| {
                let marked = NoiseProbePhase::from_id(p.phase).unwrap_or(NoiseProbePhase::Default);
                // The phase still running reads the state now; one that ended, the state as
                // it last ended.
                let running = !run.call_ended && run.current == marked;
                let at_end = run
                    .ended
                    .iter()
                    .rev()
                    .find(|(ended, _)| *ended == marked)
                    .map(|(_, s)| *s);
                // The mode in effect names the phase where the pipeline reports it.
                let phase = call_phase.unwrap_or(marked);
                NoiseProbePhaseReport {
                    phase,
                    letter: phase.letter(),
                    deadline_us: run
                        .deadlines
                        .iter()
                        .find(|(d, _)| *d == marked)
                        .map(|(_, us)| *us)
                        .or(sched.and_then(|s| s.interval_deadline_us.map(u64::from))),
                    blocks: p.blocks,
                    left_out: p.left_out,
                    model_blocks: p.model_blocks,
                    erb_blocks: p.erb_blocks,
                    df_blocks: p.df_blocks,
                    strong_out_blocks: p.strong_out_blocks,
                    stage_wall: p.stage_wall.into(),
                    stage_cpu: p.stage_cpu.map(Into::into),
                    full_wall: p.full_wall.into(),
                    full_cpu: p.full_cpu.map(Into::into),
                    k1_wall: p.k1_wall.into(),
                    k1_cpu: p.k1_cpu.map(Into::into),
                    k2_wall: p.k2_wall.into(),
                    k2_cpu: p.k2_cpu.map(Into::into),
                    cores: p
                        .cores
                        .iter()
                        .map(|c| NoiseProbeCore {
                            core: c.core,
                            blocks: c.blocks,
                        })
                        .collect(),
                    core_unknown: p.core_unknown,
                    state: if running { Some(state) } else { at_end },
                }
            })
            .collect();
        Some(NoiseProbeReport {
            schema: SCHEMA,
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            call_ended: run.call_ended,
            phase: call_phase.unwrap_or(run.current),
            letter: call_phase.unwrap_or(run.current).letter(),
            dsp_sched: sched,
            loops: NoiseProbeLoops::NOW,
            blocks: r.blocks,
            dropped: r.dropped,
            cpu_clock: r.cpu_clock,
            phases,
            first_blocks: r
                .first_blocks
                .iter()
                .map(|b| NoiseProbeBlock {
                    phase: call_phase
                        .or(NoiseProbePhase::from_id(b.phase))
                        .map(NoiseProbePhase::letter),
                    wall_ns: b.wall_ns,
                    cpu_ns: b.cpu_ns,
                    core: b.core,
                    model: b.model,
                    erb: b.erb,
                    df: b.df,
                    strong_out: b.strong_out,
                    left_out: b.left_out,
                })
                .collect(),
        })
    }
}
