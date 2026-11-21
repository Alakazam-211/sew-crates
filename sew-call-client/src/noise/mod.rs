//! Noise removal on the capture path: the setting ([`NoiseRemoval`]) and what runs
//! ([`NoiseRemovalState`]).
//!
//! kessel-audio runs a capture stage between echo cancellation and gain control (its capture
//! hook); with feature `audio-core` this crate builds that stage over `sew-noise` and hands it
//! over ([`NoiseRemovalControl`], [`crate::AudioBridge::start_with_noise`]). With a stage set,
//! kessel-audio's own noise suppression is off.
//!
//! [`NoiseRemoval::Standard`] runs:
//! - with feature `dfn3` and the DeepFilterNet3 weights given to the control
//!   (`NoiseRemovalControl::set_dfn3_weights`), DeepFilterNet3's low-latency model, with
//!   RNNoise kept warm beside it and stepped down to when the device can't keep up
//!   ([`NoiseRemovalState::model`] says which runs);
//! - otherwise RNNoise.
//!
//! On phones the OS's voice processing does its own noise suppression first. Without the
//! DeepFilterNet3 weights no stage runs there (the state says `unavailable`); with them the
//! stage runs after the OS's processing, as on desktop. [`NoiseRemoval::Off`] turns the OS's
//! processing off too: the phone's audio goes raw ([`PhoneAudioMode::Raw`]), at the start of a
//! call and when the setting changes during one.

use serde::{Deserialize, Serialize};

#[cfg(feature = "noise-bench")]
pub mod bench;
#[cfg(feature = "audio-core")]
mod control;
#[cfg(feature = "audio-core")]
mod hook;
#[cfg(any(feature = "dfn3", feature = "noise-bench"))]
mod mem;
#[cfg(feature = "noise-probe")]
pub mod probe;

#[cfg(feature = "dfn3")]
pub use control::{DFN3_IDLE_UNLOAD, Dfn3LoadInfo};
#[cfg(any(feature = "dfn3", feature = "noise-bench"))]
pub(crate) use mem::rss_kib;
#[cfg(feature = "noise-bench")]
pub(crate) use mem::rss_peak_kib;

#[cfg(feature = "audio-core")]
pub(crate) use control::LiveAudio;
#[cfg(feature = "audio-core")]
pub use control::NoiseRemovalControl;

/// Where a call reads its noise removal state (its stats windows, [`crate::Call::noise_removal`]):
/// the [`NoiseRemovalControl`] its audio started with. `None` once that audio ended.
pub(crate) trait NoiseSource: Send + Sync {
    fn noise_state(&self) -> Option<NoiseRemovalState>;
    /// The device is busy with something else for a moment (a camera starting): the call's
    /// stage holds its timing rules back for a grace window.
    fn grace(&self) {}
}

/// How much noise this device's microphone has removed. The default is
/// [`NoiseRemoval::Standard`].
///
/// Its JSON is `"off"` or `"standard"`. More modes may come in a later version.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum NoiseRemoval {
    /// No noise removal: the full audio, with echo cancellation and gain control kept. For
    /// music or a studio microphone.
    ///
    /// On iOS and Android the OS's voice processing is off as well ([`PhoneAudioMode::Raw`]):
    /// iOS opens the plain RemoteIO unit; Android opens the microphone with its unprocessed
    /// source where the device supports it
    /// (`NoiseRemovalControl::set_android_unprocessed_supported`), else its voice recognition
    /// source, with no effects attached. kessel-audio then cancels echo and controls gain
    /// itself, with no noise suppression. A call that starts in Off opens that way; a change
    /// during a call switches the phone's audio at once, with a short pause of the microphone
    /// while it reopens ([`NoiseRemovalState::phone_mode_gap_ms`]).
    Off,
    /// Noise removal between echo cancellation and gain control: DeepFilterNet3 where it is
    /// built in and its weights are loaded, with RNNoise as its automatic step-down; RNNoise
    /// otherwise.
    #[default]
    Standard,
}

impl NoiseRemoval {
    /// Every mode this version has, weakest first.
    pub const ALL: [NoiseRemoval; 2] = [NoiseRemoval::Off, NoiseRemoval::Standard];

    /// The mode's name: `off` or `standard`.
    pub fn as_str(self) -> &'static str {
        match self {
            NoiseRemoval::Off => "off",
            NoiseRemoval::Standard => "standard",
        }
    }

    /// The mode of a name [`NoiseRemoval::as_str`] gives; `None` for any other (`strong`
    /// included: this version has no stronger mode).
    pub fn from_name(name: &str) -> Option<NoiseRemoval> {
        NoiseRemoval::ALL.into_iter().find(|m| m.as_str() == name)
    }
}

impl std::fmt::Display for NoiseRemoval {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why the noise removal that runs ([`NoiseRemovalState::active`]) is less than what was asked
/// ([`NoiseRemovalState::mode`]).
///
/// Its JSON is `"budget"`, `"fault"`, `"unavailable"`, `"switch_failed"` or `"loading"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum NoiseRemovalReason {
    /// The audio pipeline stopped the stage because it took too long (two blocks in a row of
    /// 9 ms or more, or over 6 ms at the 99th percentile): the microphone goes on with
    /// kessel-audio's own noise suppression instead, until a retry 10 s later brings the stage
    /// back (after 3 such stops within 60 s, for the rest of the call).
    Budget,
    /// The audio pipeline stopped the stage after a fault (a panic, or samples that were not
    /// finite), for the rest of the call; kessel-audio's own noise suppression runs instead.
    Fault,
    /// This platform or build can't run the mode (phones; an audio pipeline without the
    /// capture hook).
    Unavailable,
    /// The phone's audio could not switch to the capture the setting asks for (the microphone
    /// would not open that way): it went back to the capture it had, and kessel-audio reported
    /// the error. In [`NoiseRemovalState::phone_mode_reason`] only.
    SwitchFailed,
    /// DeepFilterNet3's model is being loaded (it is loaded when a call first needs it): RNNoise
    /// runs meanwhile (on phones, the OS's processing alone), and the call steps up to
    /// DeepFilterNet3 once it is ready.
    Loading,
}

impl NoiseRemovalReason {
    /// The reason's name: `budget`, `fault`, `unavailable`, `switch_failed` or `loading`.
    pub fn as_str(self) -> &'static str {
        match self {
            NoiseRemovalReason::Budget => "budget",
            NoiseRemovalReason::Fault => "fault",
            NoiseRemovalReason::Unavailable => "unavailable",
            NoiseRemovalReason::SwitchFailed => "switch_failed",
            NoiseRemovalReason::Loading => "loading",
        }
    }
}

/// How a phone's microphone is captured (iOS and Android with feature `mobile`): through the
/// OS's voice processing, or raw, with the OS's processing off ([`NoiseRemoval::Off`]).
///
/// Its JSON is `"os"` or `"raw"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PhoneAudioMode {
    /// The OS's voice processing: its echo cancellation, noise suppression and gain control
    /// ([`NoiseRemoval::Standard`]).
    Os,
    /// No OS processing: the full microphone signal. kessel-audio cancels echo and controls
    /// gain, with no noise suppression ([`NoiseRemoval::Off`]).
    Raw,
}

impl PhoneAudioMode {
    /// The capture a setting asks for: raw for Off, the OS's processing otherwise.
    pub fn for_mode(mode: NoiseRemoval) -> PhoneAudioMode {
        match mode {
            NoiseRemoval::Off => PhoneAudioMode::Raw,
            NoiseRemoval::Standard => PhoneAudioMode::Os,
        }
    }

    /// The mode's name: `os` or `raw`.
    pub fn as_str(self) -> &'static str {
        match self {
            PhoneAudioMode::Os => "os",
            PhoneAudioMode::Raw => "raw",
        }
    }
}

impl std::fmt::Display for PhoneAudioMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The model that removes the noise when [`NoiseRemoval::Standard`] runs.
///
/// Its JSON is `"dfn3"` or `"rnnoise"`. More models may come in a later version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum NoiseModel {
    /// DeepFilterNet3's low-latency model (feature `dfn3`, its weights loaded).
    Dfn3,
    /// RNNoise.
    Rnnoise,
}

impl NoiseModel {
    /// The model's name: `dfn3` or `rnnoise`.
    pub fn as_str(self) -> &'static str {
        match self {
            NoiseModel::Dfn3 => "dfn3",
            NoiseModel::Rnnoise => "rnnoise",
        }
    }
}

impl std::fmt::Display for NoiseModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why the call's stage last stepped down from DeepFilterNet3 to RNNoise
/// ([`NoiseRemovalState::step_down_reason`]): the stage's own rule names.
///
/// Its JSON is `"slow_block"`, `"slow_window"`, `"heavy"`, `"fault"`, `"forced"` or
/// `"starved"`. More may come in a later version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum NoiseStepDownReason {
    /// One block took too long on its own.
    SlowBlock,
    /// Too many recent blocks were slow (the 99th percentile; in a grace window, the second
    /// block over the audio pipeline's own line).
    SlowWindow,
    /// Recent blocks were steadily close to the budget (the median).
    Heavy,
    /// The model gave a sample that was not finite: final for the call.
    Fault,
    /// Asked for by a measurement.
    Forced,
    /// The audio thread fell behind real time for several blocks in a row by the wall clock,
    /// whatever the cause.
    Starved,
}

impl NoiseStepDownReason {
    /// The reason's name: `slow_block`, `slow_window`, `heavy`, `fault`, `forced` or
    /// `starved`.
    pub fn as_str(self) -> &'static str {
        match self {
            NoiseStepDownReason::SlowBlock => "slow_block",
            NoiseStepDownReason::SlowWindow => "slow_window",
            NoiseStepDownReason::Heavy => "heavy",
            NoiseStepDownReason::Fault => "fault",
            NoiseStepDownReason::Forced => "forced",
            NoiseStepDownReason::Starved => "starved",
        }
    }
}

/// Where the call's stage is with DeepFilterNet3 ([`NoiseRemovalState::dfn3_state`]).
///
/// Its JSON is `"idle"`, `"warming"`, `"running"`, `"stepped_down"` or `"retired"`. More may
/// come in a later version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum NoiseDfn3State {
    /// Not asked for (the setting is Off), or not started yet.
    Idle,
    /// The model runs on the microphone while RNNoise's output goes on: the call's start, or
    /// a retry after a step-down.
    Warming,
    /// The model's output goes on.
    Running,
    /// Stepped down to RNNoise; retried later (`retry_in_ms`).
    SteppedDown,
    /// Stepped down for the rest of the call.
    Retired,
}

impl NoiseDfn3State {
    /// The state's name: `idle`, `warming`, `running`, `stepped_down` or `retired`.
    pub fn as_str(self) -> &'static str {
        match self {
            NoiseDfn3State::Idle => "idle",
            NoiseDfn3State::Warming => "warming",
            NoiseDfn3State::Running => "running",
            NoiseDfn3State::SteppedDown => "stepped_down",
            NoiseDfn3State::Retired => "retired",
        }
    }
}

impl std::fmt::Display for NoiseDfn3State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::fmt::Display for NoiseStepDownReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A busy moment of the device reported to the audio pipeline, which opens its grace window
/// for the stage ([`NoiseRemovalState::capture_stage_last_transient`]).
///
/// Its JSON is `"call_start"`, `"audio_session_activated"`, `"route_change"`,
/// `"camera_start"`, `"app_scene_change"` or `"other"`. More may come in a later version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CaptureTransient {
    /// The call's audio started (a join or a rejoin).
    CallStart,
    /// The phone's audio session became active.
    AudioSessionActivated,
    /// The audio route changed.
    RouteChange,
    /// A camera started or restarted.
    CameraStart,
    /// The app changed scene (foreground, background, focus).
    AppSceneChange,
    /// Anything else.
    Other,
}

impl CaptureTransient {
    /// The moment's name: `call_start`, `audio_session_activated`, `route_change`,
    /// `camera_start`, `app_scene_change` or `other`.
    pub fn as_str(self) -> &'static str {
        match self {
            CaptureTransient::CallStart => "call_start",
            CaptureTransient::AudioSessionActivated => "audio_session_activated",
            CaptureTransient::RouteChange => "route_change",
            CaptureTransient::CameraStart => "camera_start",
            CaptureTransient::AppSceneChange => "app_scene_change",
            CaptureTransient::Other => "other",
        }
    }
}

impl std::fmt::Display for CaptureTransient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The clock the audio pipeline times the noise removal stage by
/// ([`NoiseRemovalState::capture_stage_clock`]).
///
/// Its JSON is `"thread_cpu"` or `"wall"`. More may come in a later version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CaptureStageClock {
    /// The processing thread's own CPU time: time the device spends on other threads while the
    /// stage waits does not count.
    ThreadCpu,
    /// The wall clock: everything between a block's start and end counts.
    Wall,
}

impl CaptureStageClock {
    /// The clock's name: `thread_cpu` or `wall`.
    pub fn as_str(self) -> &'static str {
        match self {
            CaptureStageClock::ThreadCpu => "thread_cpu",
            CaptureStageClock::Wall => "wall",
        }
    }
}

impl std::fmt::Display for CaptureStageClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which scheduling the audio pipeline's processing thread asks the OS for
/// ([`CaptureDspSched::mode`], `NoiseRemovalControl::set_dsp_sched`). Only macOS and iOS act
/// on it.
///
/// Its JSON is `"workgroup"`, `"time_constraint_only"`, `"none"`, `"own_workgroup"` or
/// `"unsupported"`. More may come in a later version.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CaptureDspSchedMode {
    /// Asks for real-time scheduling by the time-constraint policy, and on iOS joins the audio
    /// unit's own I/O workgroup while the phone's audio unit runs: what a call does when nothing
    /// is set.
    #[default]
    Workgroup,
    /// Asks for real-time scheduling by the time-constraint policy with the given figures, in
    /// no workgroup.
    TimeConstraintOnly,
    /// No request: the OS's default scheduling.
    None,
    /// Asks for real-time scheduling by the time-constraint policy, and in a work interval of
    /// the thread's own: each block is due [`CaptureDspSched::interval_deadline_us`] after it
    /// starts.
    OwnWorkgroup,
    /// Only reported, as the mode in effect: this platform has none of these calls (Linux,
    /// Android, Windows). Asking for it is refused.
    Unsupported,
}

impl CaptureDspSchedMode {
    /// The mode's name: `workgroup`, `time_constraint_only`, `none`, `own_workgroup` or
    /// `unsupported`.
    pub const fn as_str(self) -> &'static str {
        match self {
            CaptureDspSchedMode::Workgroup => "workgroup",
            CaptureDspSchedMode::TimeConstraintOnly => "time_constraint_only",
            CaptureDspSchedMode::None => "none",
            CaptureDspSchedMode::OwnWorkgroup => "own_workgroup",
            CaptureDspSchedMode::Unsupported => "unsupported",
        }
    }
}

/// Why the processing thread runs in [`CaptureDspSchedMode::Workgroup`] and not in the mode
/// asked for ([`NoiseRemovalState::capture_stage_dsp_sched_fallback`]).
///
/// Its JSON is `"time_constraint_refused"`, `"own_workgroup_unavailable"`,
/// `"own_workgroup_join_refused"`, `"interval_refused"` or `"other"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CaptureDspSchedFallback {
    /// The OS refused the time-constraint policy.
    TimeConstraintRefused,
    /// The system has no work intervals for an app's audio threads, or making one failed.
    OwnWorkgroupUnavailable,
    /// The OS refused the thread joining its own work interval.
    OwnWorkgroupJoinRefused,
    /// The OS refused an interval during the call; the thread left its own work interval.
    IntervalRefused,
    /// A reason this version does not know.
    Other,
}

/// Why [`CaptureDspSched::validate`] (and `NoiseRemovalControl::set_dsp_sched`) refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CaptureDspSchedError {
    /// [`CaptureDspSchedMode::Unsupported`] is only reported, never asked for.
    #[error("the mode `unsupported` is only reported, never asked for")]
    ModeNotAskable,
    /// The figures must satisfy 0 < computation <= constraint <= period <= 1 s.
    #[error("the time constraint needs 0 < computation <= constraint <= period <= 1 s")]
    TimeConstraint,
    /// The interval deadline must be above zero and at most the period.
    #[error("the interval deadline must be above 0 and at most the period")]
    IntervalDeadline,
}

/// What the audio pipeline's processing thread asks the OS for in the calls that start after
/// it is set (`NoiseRemovalControl::set_dsp_sched`): the mode, the time-constraint policy's
/// figures and, for [`CaptureDspSchedMode::OwnWorkgroup`], each block's deadline. The
/// pipeline reads it once, as a call's audio starts; a change reaches the next call.
///
/// Its JSON is `{mode, period_us, computation_us, constraint_us, interval_deadline_us}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CaptureDspSched {
    /// Which set-up to ask for.
    pub mode: CaptureDspSchedMode,
    /// How often the thread has work, microseconds (one 10 ms block by default).
    pub period_us: u32,
    /// The CPU time it needs per period, microseconds (6 ms by default).
    pub computation_us: u32,
    /// The time from the start of the period by which that work must be done, microseconds
    /// (9 ms by default).
    pub constraint_us: u32,
    /// [`CaptureDspSchedMode::OwnWorkgroup`] only: how long after a block starts its work is
    /// due, microseconds (3 ms by default). The other modes ignore it.
    pub interval_deadline_us: u32,
}

impl Default for CaptureDspSched {
    fn default() -> CaptureDspSched {
        CaptureDspSched::new(CaptureDspSchedMode::Workgroup)
    }
}

impl CaptureDspSched {
    /// `mode` with the default figures: a 10 ms period, 6 ms of computation, a 9 ms
    /// constraint and a 3 ms interval deadline.
    pub const fn new(mode: CaptureDspSchedMode) -> CaptureDspSched {
        CaptureDspSched {
            mode,
            period_us: 10_000,
            computation_us: 6_000,
            constraint_us: 9_000,
            interval_deadline_us: 3_000,
        }
    }

    /// With these time-constraint figures, microseconds.
    pub const fn with_time_constraint_us(
        mut self,
        period_us: u32,
        computation_us: u32,
        constraint_us: u32,
    ) -> CaptureDspSched {
        self.period_us = period_us;
        self.computation_us = computation_us;
        self.constraint_us = constraint_us;
        self
    }

    /// With this interval deadline, microseconds.
    pub const fn with_interval_deadline_us(mut self, deadline_us: u32) -> CaptureDspSched {
        self.interval_deadline_us = deadline_us;
        self
    }

    /// Whether the audio pipeline would take these figures: a mode that can be asked for,
    /// 0 < computation <= constraint <= period <= 1 s, and an interval deadline above zero and
    /// at most the period.
    pub const fn validate(&self) -> Result<(), CaptureDspSchedError> {
        if matches!(self.mode, CaptureDspSchedMode::Unsupported) {
            return Err(CaptureDspSchedError::ModeNotAskable);
        }
        if self.computation_us == 0
            || self.computation_us > self.constraint_us
            || self.constraint_us > self.period_us
            || self.period_us > 1_000_000
        {
            return Err(CaptureDspSchedError::TimeConstraint);
        }
        if self.interval_deadline_us == 0 || self.interval_deadline_us > self.period_us {
            return Err(CaptureDspSchedError::IntervalDeadline);
        }
        Ok(())
    }
}

/// How the OS schedules the audio pipeline's processing thread, the thread the noise removal
/// stage runs on ([`NoiseRemovalState::capture_stage_dsp_scheduling`]): the best of the
/// requests the pipeline made that the OS granted. On macOS and iOS the thread asks for
/// real-time scheduling; elsewhere it keeps the OS's default.
///
/// Its JSON is `"workgroup"`, `"time_constraint"`, `"qos"`, `"none"` or `"own_workgroup"`. More
/// may come in a later version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CaptureDspScheduling {
    /// Real-time, and joined to the audio unit's own I/O workgroup, so the OS schedules it with
    /// the audio I/O thread (iOS, while the phone's audio unit runs).
    Workgroup,
    /// The OS's real-time time-constraint policy.
    TimeConstraint,
    /// Only the OS's user-interactive quality of service: the OS refused the real-time policy.
    Qos,
    /// The OS's default scheduling (Linux, Android and Windows, or where the OS refused every
    /// request).
    None,
    /// Real-time, and joined to a work interval of the thread's own
    /// ([`CaptureDspSchedMode::OwnWorkgroup`]): each block is an interval with its own deadline.
    OwnWorkgroup,
}

impl CaptureDspScheduling {
    /// The mode's name: `workgroup`, `time_constraint`, `qos`, `none` or `own_workgroup`.
    pub fn as_str(self) -> &'static str {
        match self {
            CaptureDspScheduling::Workgroup => "workgroup",
            CaptureDspScheduling::TimeConstraint => "time_constraint",
            CaptureDspScheduling::Qos => "qos",
            CaptureDspScheduling::None => "none",
            CaptureDspScheduling::OwnWorkgroup => "own_workgroup",
        }
    }
}

impl std::fmt::Display for CaptureDspScheduling {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The noise removal setting and what runs: `{mode, active, reason?, model?, model_reason?,
/// step_downs?, phone_mode?, phone_mode_reason?, phone_mode_switches?, phone_mode_gap_ms?,
/// step_down_reason?, last_step_down_ms_ago?, slowest_block_us?, blocks_over_budget?,
/// in_grace?, dfn3_state?, retry_in_ms?, grace_step_downs?, slowest_block_cpu_us?,
/// p99_block_us?, p99_block_cpu_us?, capture_stage_strikes?, capture_stage_bypassed?,
/// capture_stage_p99_us?, capture_stage_grace?, capture_stage_grace_blocks?,
/// capture_stage_grace_over_budget?, capture_stage_last_transient?, capture_stage_clock?,
/// capture_stage_max_wall_us?, capture_stage_preempted_over_budget?, capture_stage_starved?,
/// capture_stage_dsp_scheduling?}`.
///
/// `mode` is what was asked, `active` what the microphone's audio goes through now, and
/// `reason` (absent when `active` is `mode`) why they differ. With no call, `active` is what a
/// call would start with.
///
/// On phones (iOS and Android with feature `mobile`) `phone_mode` says how the microphone is
/// captured ([`PhoneAudioMode`]: `raw` for Off, `os` otherwise; with no call, what a call
/// would start with). A change of the setting during a call switches it: `phone_mode` follows
/// once the microphone has reopened, `phone_mode_switches` counts the call's switches and
/// `phone_mode_gap_ms` is how long the microphone paused for the last one. If a switch fails,
/// the phone keeps the capture it had and `phone_mode_reason` is `switch_failed` until the
/// setting and the capture agree again. These fields are absent elsewhere and while unset.
///
/// Where DeepFilterNet3 is in play (feature `dfn3`, once the app has given the control its
/// weights, whether they checked out or not), `model` says which model runs while `active` is
/// Standard, `model_reason` why it is RNNoise rather than DeepFilterNet3 (`budget`: the stage
/// stepped down because the device could not keep up; `fault`; `loading`: the model is being
/// loaded for this call; `unavailable`: no usable weights), and `step_downs` how many times
/// this call's stage stepped down. They are absent otherwise, so the JSON is then
/// `{mode, active, reason?}` as before. A UI shows, for example,
/// "DeepFilterNet3" or "RNNoise (stepped down: budget)".
///
/// For a call's log, where DeepFilterNet3 is in play and a call's stage runs, the state also
/// carries the stage's timing: `step_down_reason` (the stage's rule at the latest step-down),
/// `last_step_down_ms_ago`, `slowest_block_us`, `blocks_over_budget` and `in_grace` (a grace
/// window is open: the timing rules are held back while the device is busy with something
/// else, see `NoiseRemovalControl::app_scene_changed`); where DeepFilterNet3 is
/// (`dfn3_state`), how long until its retry while it is stepped down (`retry_in_ms`), the
/// step-downs tied to a grace window or a reset (`grace_step_downs`: they do not count towards
/// retiring the model), and the stage's blocks by the audio thread's CPU time beside the wall
/// clock (`slowest_block_cpu_us`, `p99_block_us`, `p99_block_cpu_us`: the stage judges CPU
/// time where the platform has a clock for it, so a block that waited for a core does not
/// count against the model); and kessel-audio's view of the stage: `capture_stage_strikes`,
/// `capture_stage_bypassed` (why the pipeline last bypassed the stage, kept after a retry
/// brought it back) and `capture_stage_p99_us`, and the audio pipeline's own grace window for
/// the stage: `capture_stage_grace` (open now: its slow blocks do not count toward a bypass),
/// `capture_stage_grace_blocks`, `capture_stage_grace_over_budget` (slow blocks the window
/// excused) and `capture_stage_last_transient` (the last busy moment reported to it,
/// [`CaptureTransient`]), and how it times the stage: `capture_stage_clock`
/// ([`CaptureStageClock`]), `capture_stage_max_wall_us` (the slowest block on the wall
/// clock), `capture_stage_preempted_over_budget` (blocks over the budget on the wall clock but
/// not in the thread's CPU time) and `capture_stage_starved` (blocks long enough on the wall
/// clock to count as slow whatever their CPU time), and how the OS schedules the thread the
/// stage runs on: `capture_stage_dsp_scheduling` ([`CaptureDspScheduling`]; the thread asks
/// for real-time scheduling on macOS and iOS). Each is absent while unset or zero. The
/// counters (`last_step_down_ms_ago`, `slowest_block_us`, `blocks_over_budget`,
/// `retry_in_ms`, `slowest_block_cpu_us`, `p99_block_us`, `p99_block_cpu_us`,
/// `capture_stage_p99_us`) and the pipeline's grace, clock and scheduling fields are read with
/// the state and ride along with each change; they do not make a change by themselves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub struct NoiseRemovalState {
    /// What was asked.
    pub mode: NoiseRemoval,
    /// What runs.
    pub active: NoiseRemoval,
    /// Why `active` is less than `mode`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<NoiseRemovalReason>,
    /// The model running while `active` is Standard (absent where DeepFilterNet3 is not in
    /// play, and while nothing of ours runs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<NoiseModel>,
    /// Why `model` is RNNoise when DeepFilterNet3 was wanted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_reason: Option<NoiseRemovalReason>,
    /// How many times the call's stage stepped down from DeepFilterNet3 to RNNoise.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub step_downs: u32,
    /// Phones: how the microphone is captured (absent elsewhere).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone_mode: Option<PhoneAudioMode>,
    /// Phones: why `phone_mode` is not what `mode` asks for (`switch_failed`); absent while a
    /// switch is under way and when they agree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone_mode_reason: Option<NoiseRemovalReason>,
    /// Phones: how many times this call's microphone switched capture.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub phone_mode_switches: u32,
    /// Phones: how long the microphone paused for the last switch that reopened it, ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone_mode_gap_ms: Option<u32>,
    /// The stage's rule at its latest step-down from DeepFilterNet3 (absent before any).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_down_reason: Option<NoiseStepDownReason>,
    /// How long ago, in the call's audio, the latest step-down was (ms; absent before any).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_step_down_ms_ago: Option<u64>,
    /// The call's slowest stage block so far, microseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slowest_block_us: Option<u64>,
    /// The call's stage blocks over the stage's budget so far.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub blocks_over_budget: u64,
    /// A grace window is open: the stage's timing rules are held back while the device is busy
    /// with something else (the call's start, the audio session, a route change, a camera, an
    /// app switch).
    #[serde(default, skip_serializing_if = "is_false")]
    pub in_grace: bool,
    /// Where the call's stage is with DeepFilterNet3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dfn3_state: Option<NoiseDfn3State>,
    /// While `dfn3_state` is `stepped_down`: how long until the retry may start, in the call's
    /// audio (ms; 0 when it is due and waits for the device to settle).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_in_ms: Option<u64>,
    /// Step-downs (of `step_downs`) tied to a grace window or to a restart of the capture
    /// stage: they do not count towards retiring DeepFilterNet3 for the call.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub grace_step_downs: u32,
    /// The call's slowest stage block by the audio thread's CPU time, microseconds (absent
    /// where the platform has no clock for it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slowest_block_cpu_us: Option<u64>,
    /// The 99th percentile of the stage's last 100 blocks by the wall clock, microseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p99_block_us: Option<u64>,
    /// The same by the audio thread's CPU time (absent where the platform has no clock for
    /// it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p99_block_cpu_us: Option<u64>,
    /// kessel-audio's budget strikes and faults against the stage so far.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub capture_stage_strikes: u32,
    /// Why kessel-audio last bypassed the stage (`budget` or `fault`), kept after a retry
    /// brought it back (`reason` says whether it is bypassed now).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_stage_bypassed: Option<NoiseRemovalReason>,
    /// kessel-audio's 99th percentile of the stage's last 100 blocks, microseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_stage_p99_us: Option<u32>,
    /// kessel-audio's grace window for the stage is open now: its slow blocks do not count
    /// toward a bypass.
    #[serde(default, skip_serializing_if = "is_false")]
    pub capture_stage_grace: bool,
    /// kessel-audio's stage blocks inside its grace windows so far.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub capture_stage_grace_blocks: u64,
    /// Of those, the blocks over the budget that a grace window excused.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub capture_stage_grace_over_budget: u64,
    /// The last busy moment reported to kessel-audio (absent before any).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_stage_last_transient: Option<CaptureTransient>,
    /// The clock kessel-audio times the stage by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_stage_clock: Option<CaptureStageClock>,
    /// kessel-audio's slowest stage block on the wall clock so far, microseconds, whatever
    /// the clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_stage_max_wall_us: Option<u32>,
    /// With the thread's CPU time: blocks over the budget on the wall clock that were not over
    /// it in CPU time (the thread waited; the stage was not slow).
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub capture_stage_preempted_over_budget: u64,
    /// With the thread's CPU time: blocks long enough on the wall clock to count as slow
    /// whatever their CPU time.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub capture_stage_starved: u64,
    /// How the OS schedules the processing thread the stage runs on (absent before the
    /// pipeline reports it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_stage_dsp_scheduling: Option<CaptureDspScheduling>,
    /// The scheduling mode asked for (`NoiseRemovalControl::set_dsp_sched`, or the default)
    /// and the mode the processing thread runs in. Absent before the pipeline reports them,
    /// and on a platform without these calls unless a mode was asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_stage_dsp_sched_requested: Option<CaptureDspSchedMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_stage_dsp_sched_effective: Option<CaptureDspSchedMode>,
    /// Why the mode in effect is not the mode asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_stage_dsp_sched_fallback: Option<CaptureDspSchedFallback>,
    /// The time-constraint figures asked for, microseconds (absent in the modes that do not
    /// ask for the policy).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_stage_dsp_period_us: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_stage_dsp_computation_us: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_stage_dsp_constraint_us: Option<u32>,
    /// The deadline of each block's work interval, microseconds
    /// ([`CaptureDspSchedMode::OwnWorkgroup`] only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_stage_dsp_interval_deadline_us: Option<u32>,
    /// Work intervals finished on the thread's own workgroup so far.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub capture_stage_dsp_intervals: u64,
}

fn is_zero(v: &u32) -> bool {
    *v == 0
}

fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

fn is_false(v: &bool) -> bool {
    !*v
}

impl NoiseRemovalState {
    /// A state, as an engine reports it.
    pub fn new(
        mode: NoiseRemoval,
        active: NoiseRemoval,
        reason: Option<NoiseRemovalReason>,
    ) -> NoiseRemovalState {
        NoiseRemovalState {
            mode,
            active,
            reason,
            model: None,
            model_reason: None,
            step_downs: 0,
            phone_mode: None,
            phone_mode_reason: None,
            phone_mode_switches: 0,
            phone_mode_gap_ms: None,
            step_down_reason: None,
            last_step_down_ms_ago: None,
            slowest_block_us: None,
            blocks_over_budget: 0,
            in_grace: false,
            dfn3_state: None,
            retry_in_ms: None,
            grace_step_downs: 0,
            slowest_block_cpu_us: None,
            p99_block_us: None,
            p99_block_cpu_us: None,
            capture_stage_strikes: 0,
            capture_stage_bypassed: None,
            capture_stage_p99_us: None,
            capture_stage_grace: false,
            capture_stage_grace_blocks: 0,
            capture_stage_grace_over_budget: 0,
            capture_stage_last_transient: None,
            capture_stage_clock: None,
            capture_stage_max_wall_us: None,
            capture_stage_preempted_over_budget: 0,
            capture_stage_starved: 0,
            capture_stage_dsp_scheduling: None,
            capture_stage_dsp_sched_requested: None,
            capture_stage_dsp_sched_effective: None,
            capture_stage_dsp_sched_fallback: None,
            capture_stage_dsp_period_us: None,
            capture_stage_dsp_computation_us: None,
            capture_stage_dsp_constraint_us: None,
            capture_stage_dsp_interval_deadline_us: None,
            capture_stage_dsp_intervals: 0,
        }
    }

    /// This state without the counters that change with every block
    /// (`last_step_down_ms_ago`, `slowest_block_us`, `blocks_over_budget`, `retry_in_ms`,
    /// `slowest_block_cpu_us`, `p99_block_us`, `p99_block_cpu_us`, `capture_stage_p99_us`) and
    /// the pipeline's grace, clock and scheduling fields: what a change of state is judged on.
    #[cfg_attr(not(feature = "audio-core"), allow(dead_code))]
    pub(crate) fn change_key(mut self) -> NoiseRemovalState {
        self.last_step_down_ms_ago = None;
        self.slowest_block_us = None;
        self.blocks_over_budget = 0;
        self.retry_in_ms = None;
        self.slowest_block_cpu_us = None;
        self.p99_block_us = None;
        self.p99_block_cpu_us = None;
        self.capture_stage_p99_us = None;
        self.capture_stage_grace = false;
        self.capture_stage_grace_blocks = 0;
        self.capture_stage_grace_over_budget = 0;
        self.capture_stage_last_transient = None;
        self.capture_stage_clock = None;
        self.capture_stage_max_wall_us = None;
        self.capture_stage_preempted_over_budget = 0;
        self.capture_stage_starved = 0;
        self.capture_stage_dsp_scheduling = None;
        self.capture_stage_dsp_sched_requested = None;
        self.capture_stage_dsp_sched_effective = None;
        self.capture_stage_dsp_sched_fallback = None;
        self.capture_stage_dsp_period_us = None;
        self.capture_stage_dsp_computation_us = None;
        self.capture_stage_dsp_constraint_us = None;
        self.capture_stage_dsp_interval_deadline_us = None;
        self.capture_stage_dsp_intervals = 0;
        self
    }

    /// This state with the phone's capture, why it is not what the setting asks for
    /// (`switch_failed`), the call's switches and the last switch's pause (ms).
    pub fn with_phone_mode(
        mut self,
        phone_mode: Option<PhoneAudioMode>,
        phone_mode_reason: Option<NoiseRemovalReason>,
        switches: u32,
        gap_ms: Option<u32>,
    ) -> NoiseRemovalState {
        self.phone_mode = phone_mode;
        self.phone_mode_reason = phone_mode_reason;
        self.phone_mode_switches = switches;
        self.phone_mode_gap_ms = gap_ms;
        self
    }

    /// This state with the model that runs, why it is not DeepFilterNet3 when that was wanted,
    /// and the step-downs so far.
    pub fn with_model(
        mut self,
        model: Option<NoiseModel>,
        model_reason: Option<NoiseRemovalReason>,
        step_downs: u32,
    ) -> NoiseRemovalState {
        self.model = model;
        self.model_reason = model_reason;
        self.step_downs = step_downs;
        self
    }

    /// `mode` running as asked.
    pub fn running(mode: NoiseRemoval) -> NoiseRemovalState {
        NoiseRemovalState::new(mode, mode, None)
    }

    /// `mode` asked, nothing of ours running, for `reason` (`Off` asked: nothing is missing,
    /// so no reason).
    pub fn stepped_down(mode: NoiseRemoval, reason: NoiseRemovalReason) -> NoiseRemovalState {
        if mode == NoiseRemoval::Off {
            NoiseRemovalState::running(NoiseRemoval::Off)
        } else {
            NoiseRemovalState::new(mode, NoiseRemoval::Off, Some(reason))
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn names_and_json() {
        assert_eq!(NoiseRemoval::default(), NoiseRemoval::Standard);
        for m in NoiseRemoval::ALL {
            assert_eq!(NoiseRemoval::from_name(m.as_str()), Some(m));
            assert_eq!(
                serde_json::to_value(m).unwrap(),
                serde_json::json!(m.as_str())
            );
        }
        assert_eq!(NoiseRemoval::from_name("strong"), None);
        assert_eq!(NoiseRemoval::from_name("Standard"), None);
        let s = NoiseRemovalState::running(NoiseRemoval::Standard);
        assert_eq!(
            serde_json::to_value(s).unwrap(),
            serde_json::json!({"mode": "standard", "active": "standard"})
        );
        let s = NoiseRemovalState::stepped_down(NoiseRemoval::Standard, NoiseRemovalReason::Budget);
        assert_eq!(
            serde_json::to_value(s).unwrap(),
            serde_json::json!({"mode": "standard", "active": "off", "reason": "budget"})
        );
        for (r, n) in [
            (NoiseRemovalReason::Budget, "budget"),
            (NoiseRemovalReason::Fault, "fault"),
            (NoiseRemovalReason::Unavailable, "unavailable"),
            (NoiseRemovalReason::SwitchFailed, "switch_failed"),
            (NoiseRemovalReason::Loading, "loading"),
        ] {
            assert_eq!(r.as_str(), n);
            assert_eq!(serde_json::to_value(r).unwrap(), serde_json::json!(n));
        }
        assert_eq!(
            NoiseRemovalState::stepped_down(NoiseRemoval::Off, NoiseRemovalReason::Fault),
            NoiseRemovalState::running(NoiseRemoval::Off)
        );
        let back: NoiseRemovalState =
            serde_json::from_value(serde_json::json!({"mode": "off", "active": "off"})).unwrap();
        assert_eq!(back, NoiseRemovalState::running(NoiseRemoval::Off));
    }

    #[test]
    fn model_fields_json() {
        let s = NoiseRemovalState::running(NoiseRemoval::Standard).with_model(
            Some(NoiseModel::Dfn3),
            None,
            0,
        );
        assert_eq!(
            serde_json::to_value(s).unwrap(),
            serde_json::json!({"mode": "standard", "active": "standard", "model": "dfn3"})
        );
        let s = NoiseRemovalState::running(NoiseRemoval::Standard).with_model(
            Some(NoiseModel::Rnnoise),
            Some(NoiseRemovalReason::Budget),
            2,
        );
        let v = serde_json::to_value(s).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"mode": "standard", "active": "standard", "model": "rnnoise",
                "model_reason": "budget", "step_downs": 2})
        );
        let back: NoiseRemovalState = serde_json::from_value(v).unwrap();
        assert_eq!(back, s);
        for (m, n) in [(NoiseModel::Dfn3, "dfn3"), (NoiseModel::Rnnoise, "rnnoise")] {
            assert_eq!(m.as_str(), n);
            assert_eq!(serde_json::to_value(m).unwrap(), serde_json::json!(n));
        }
    }

    /// The timing fields: absent when unset (the JSON is as before), their names when set, read
    /// back; the counters do not make a change by themselves, the reason and the grace do.
    #[test]
    fn timing_fields_json() {
        let base = NoiseRemovalState::running(NoiseRemoval::Standard);
        assert_eq!(
            serde_json::to_value(base).unwrap(),
            serde_json::json!({"mode": "standard", "active": "standard"})
        );
        let mut s = base;
        s.step_down_reason = Some(NoiseStepDownReason::SlowWindow);
        s.last_step_down_ms_ago = Some(1_234);
        s.slowest_block_us = Some(7_100);
        s.blocks_over_budget = 3;
        s.in_grace = true;
        s.dfn3_state = Some(NoiseDfn3State::SteppedDown);
        s.retry_in_ms = Some(4_200);
        s.grace_step_downs = 1;
        s.slowest_block_cpu_us = Some(1_300);
        s.p99_block_us = Some(6_900);
        s.p99_block_cpu_us = Some(900);
        s.capture_stage_strikes = 1;
        s.capture_stage_bypassed = Some(NoiseRemovalReason::Budget);
        s.capture_stage_p99_us = Some(6_200);
        s.capture_stage_grace = true;
        s.capture_stage_grace_blocks = 150;
        s.capture_stage_grace_over_budget = 4;
        s.capture_stage_last_transient = Some(CaptureTransient::CameraStart);
        s.capture_stage_clock = Some(CaptureStageClock::ThreadCpu);
        s.capture_stage_max_wall_us = Some(21_000);
        s.capture_stage_preempted_over_budget = 5;
        s.capture_stage_starved = 1;
        s.capture_stage_dsp_scheduling = Some(CaptureDspScheduling::TimeConstraint);
        let v = serde_json::to_value(s).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"mode": "standard", "active": "standard",
                "step_down_reason": "slow_window", "last_step_down_ms_ago": 1234,
                "slowest_block_us": 7100, "blocks_over_budget": 3, "in_grace": true,
                "dfn3_state": "stepped_down", "retry_in_ms": 4200, "grace_step_downs": 1,
                "slowest_block_cpu_us": 1300, "p99_block_us": 6900, "p99_block_cpu_us": 900,
                "capture_stage_strikes": 1, "capture_stage_bypassed": "budget",
                "capture_stage_p99_us": 6200, "capture_stage_grace": true,
                "capture_stage_grace_blocks": 150, "capture_stage_grace_over_budget": 4,
                "capture_stage_last_transient": "camera_start",
                "capture_stage_clock": "thread_cpu", "capture_stage_max_wall_us": 21000,
                "capture_stage_preempted_over_budget": 5, "capture_stage_starved": 1,
                "capture_stage_dsp_scheduling": "time_constraint"})
        );
        let back: NoiseRemovalState = serde_json::from_value(v).unwrap();
        assert_eq!(back, s);
        for (r, n) in [
            (NoiseStepDownReason::SlowBlock, "slow_block"),
            (NoiseStepDownReason::SlowWindow, "slow_window"),
            (NoiseStepDownReason::Heavy, "heavy"),
            (NoiseStepDownReason::Fault, "fault"),
            (NoiseStepDownReason::Forced, "forced"),
            (NoiseStepDownReason::Starved, "starved"),
        ] {
            assert_eq!(r.as_str(), n);
            assert_eq!(r.to_string(), n);
            assert_eq!(serde_json::to_value(r).unwrap(), serde_json::json!(n));
        }
        for (d, n) in [
            (NoiseDfn3State::Idle, "idle"),
            (NoiseDfn3State::Warming, "warming"),
            (NoiseDfn3State::Running, "running"),
            (NoiseDfn3State::SteppedDown, "stepped_down"),
            (NoiseDfn3State::Retired, "retired"),
        ] {
            assert_eq!(d.as_str(), n);
            assert_eq!(d.to_string(), n);
            assert_eq!(serde_json::to_value(d).unwrap(), serde_json::json!(n));
        }
        // An older reader's JSON (none of the new fields) reads with them unset.
        let old: NoiseRemovalState = serde_json::from_value(serde_json::json!({
            "mode": "standard", "active": "standard", "slowest_block_us": 7100}))
        .unwrap();
        assert_eq!(
            (old.dfn3_state, old.retry_in_ms, old.grace_step_downs),
            (None, None, 0)
        );
        // The counters ride along; the reason, the grace and the strikes make a change.
        let mut counters = base;
        counters.last_step_down_ms_ago = Some(10);
        counters.slowest_block_us = Some(1);
        counters.blocks_over_budget = 9;
        counters.capture_stage_p99_us = Some(5);
        counters.retry_in_ms = Some(3_000);
        counters.slowest_block_cpu_us = Some(2);
        counters.p99_block_us = Some(3);
        counters.p99_block_cpu_us = Some(4);
        counters.capture_stage_grace = true;
        counters.capture_stage_grace_blocks = 7;
        counters.capture_stage_grace_over_budget = 2;
        counters.capture_stage_last_transient = Some(CaptureTransient::AppSceneChange);
        counters.capture_stage_clock = Some(CaptureStageClock::Wall);
        counters.capture_stage_max_wall_us = Some(9);
        counters.capture_stage_preempted_over_budget = 3;
        counters.capture_stage_starved = 1;
        counters.capture_stage_dsp_scheduling = Some(CaptureDspScheduling::Workgroup);
        assert_eq!(counters.change_key(), base.change_key());
        for (d, n) in [
            (CaptureDspScheduling::Workgroup, "workgroup"),
            (CaptureDspScheduling::TimeConstraint, "time_constraint"),
            (CaptureDspScheduling::Qos, "qos"),
            (CaptureDspScheduling::None, "none"),
        ] {
            assert_eq!(d.as_str(), n);
            assert_eq!(d.to_string(), n);
            assert_eq!(serde_json::to_value(d).unwrap(), serde_json::json!(n));
            let back: CaptureDspScheduling = serde_json::from_value(serde_json::json!(n)).unwrap();
            assert_eq!(back, d);
        }
        for (c, n) in [
            (CaptureStageClock::ThreadCpu, "thread_cpu"),
            (CaptureStageClock::Wall, "wall"),
        ] {
            assert_eq!(c.as_str(), n);
            assert_eq!(c.to_string(), n);
            assert_eq!(serde_json::to_value(c).unwrap(), serde_json::json!(n));
        }
        for (t, n) in [
            (CaptureTransient::CallStart, "call_start"),
            (
                CaptureTransient::AudioSessionActivated,
                "audio_session_activated",
            ),
            (CaptureTransient::RouteChange, "route_change"),
            (CaptureTransient::CameraStart, "camera_start"),
            (CaptureTransient::AppSceneChange, "app_scene_change"),
            (CaptureTransient::Other, "other"),
        ] {
            assert_eq!(t.as_str(), n);
            assert_eq!(t.to_string(), n);
            assert_eq!(serde_json::to_value(t).unwrap(), serde_json::json!(n));
        }
        // Where DeepFilterNet3 is, and the step-downs tied to grace, make a change.
        let mut state = base;
        state.dfn3_state = Some(NoiseDfn3State::Warming);
        assert_ne!(state.change_key(), base.change_key());
        let mut tied = base;
        tied.grace_step_downs = 1;
        assert_ne!(tied.change_key(), base.change_key());
        let mut grace = base;
        grace.in_grace = true;
        assert_ne!(grace.change_key(), base.change_key());
        let mut reason = base;
        reason.step_down_reason = Some(NoiseStepDownReason::Heavy);
        assert_ne!(reason.change_key(), base.change_key());
        let mut strikes = base;
        strikes.capture_stage_strikes = 1;
        assert_ne!(strikes.change_key(), base.change_key());
    }

    /// The phone fields: absent when unset (the JSON is as before), present when set, and
    /// read back.
    #[test]
    fn phone_fields_json() {
        let s = NoiseRemovalState::running(NoiseRemoval::Off).with_phone_mode(
            Some(PhoneAudioMode::Raw),
            None,
            0,
            None,
        );
        assert_eq!(
            serde_json::to_value(s).unwrap(),
            serde_json::json!({"mode": "off", "active": "off", "phone_mode": "raw"})
        );
        let s = NoiseRemovalState::running(NoiseRemoval::Off).with_phone_mode(
            Some(PhoneAudioMode::Os),
            Some(NoiseRemovalReason::SwitchFailed),
            3,
            Some(42),
        );
        let v = serde_json::to_value(s).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"mode": "off", "active": "off", "phone_mode": "os",
                "phone_mode_reason": "switch_failed", "phone_mode_switches": 3,
                "phone_mode_gap_ms": 42})
        );
        let back: NoiseRemovalState = serde_json::from_value(v).unwrap();
        assert_eq!(back, s);
        for (m, n) in [(PhoneAudioMode::Os, "os"), (PhoneAudioMode::Raw, "raw")] {
            assert_eq!(m.as_str(), n);
            assert_eq!(m.to_string(), n);
            assert_eq!(serde_json::to_value(m).unwrap(), serde_json::json!(n));
        }
        assert_eq!(
            PhoneAudioMode::for_mode(NoiseRemoval::Off),
            PhoneAudioMode::Raw
        );
        assert_eq!(
            PhoneAudioMode::for_mode(NoiseRemoval::Standard),
            PhoneAudioMode::Os
        );
    }
}
