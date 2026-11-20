//! Strong: DeepFilterNet3's low-latency model in the stage (cargo feature `dfn3`), with its
//! own step-down to Standard.
//!
//! The stage keeps running Standard (RNNoise) beside the model whenever removal
//! is on, so Standard is always warm and its output is always there to fall back to. This
//! layer runs the model on the same input and puts its output in place of Standard's.
//!
//! Step-down. The stage times every block (the whole `process` call) by the calling thread's
//! CPU time where the target has a clock for it (Linux, Android, macOS, iOS), so time the
//! thread spent waiting for a core, while another thread of the app ran, does not count as
//! the model's; elsewhere, and with a clock given by [`crate::Stage::set_clock`], by that
//! clock. It steps down to Standard at:
//! - one block of [`SLOW_BLOCK`] (6 ms) or more;
//! - [`WINDOW_SLOW_COUNT`] (2) blocks over [`WINDOW_SLOW`] (4 ms) among the last
//!   [`WINDOW`] (100) blocks: the 99th percentile over 4 ms;
//! - [`WINDOW_HEAVY_COUNT`] (50) blocks over [`WINDOW_HEAVY`] (3 ms) among the last 100: the
//!   median over 3 ms, a device that keeps up but with little left (a hot phone);
//! - [`STARVED_BLOCKS`] (3) model blocks in a row over [`STARVED_WALL`] (10 ms) by the wall
//!   clock, in a grace window too: the thread falls behind real time whatever the cause
//!   ([`StepDownReason::Starved`]);
//! - a non-finite sample from the model (a fault: no retry).
//!
//! The window is the stage's last 100 blocks, the model's or not, as the audio pipeline's own
//! window over the stage's calls; it starts afresh at a reset, as the pipeline's does, and is
//! kept across a step-down and a retry.
//!
//! Grace. While a grace window is open ([`crate::StageControl::grace`]; the stage opens one
//! of [`crate::DEFAULT_GRACE`] at its first block, at a reset and when it takes a model handed
//! over) the first three rules are held back: the model steps down only at the second block
//! over 6 ms among the last 100 blocks (reported as [`StepDownReason::SlowWindow`]), the
//! count at which the audio pipeline's own percentile rule would bypass the whole stage. A
//! fault is final as ever, and the starvation cutoff still applies. The window's judged times are
//! forgotten when it closes. A step-down in a
//! window, or within 5 s after it closed, is retried after 5 s; the schedule below is
//! otherwise unchanged. A timing step-down in a window, or within a second after a reset of
//! the stage, does not count towards the three that retire the model: the device was busy,
//! which says little about the model. A separate bound, [`MAX_GRACE_STEP_DOWNS`], retires it
//! after that many such step-downs, so a device busy all the time does not retry for ever.
//!
//! The switch is glitch-free and costs no model run: the block after the decision is the
//! model's held overlap-add tail (what it had already computed for that block) plus
//! Standard's output weighted by the window's complement, which is a one-block cross-fade
//! between two aligned versions of the same audio. After a step-down the model is retried
//! after 30 s, then 60 s; the third counted step-down is final for the stage's life. A retry
//! does not start while a block of the last 100 went over one of the audio pipeline's own
//! lines ([`PIPELINE_LINE`] on the clock the rules judge, or [`PIPELINE_STARVED_WALL`] by the
//! wall clock): one more in the warm-up would make the pipeline bypass the stage. It warms the model up for
//! [`PROBE_BLOCKS`] (0.5 s) on the live input while Standard's output goes on, and fades the
//! model in only if the warm-up's 99th percentile is under the percentile rule's line (the
//! grace rule's line in a grace window) and, outside a grace window, its median under the
//! median rule's; only the starvation cutoff and a fault act during the warm-up.

/// Why the stage stepped down from Strong to Standard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StepDownReason {
    /// One block was too slow on its own.
    SlowBlock,
    /// Too many recent blocks were slow.
    SlowWindow,
    /// Recent blocks were steadily close to the budget.
    Heavy,
    /// The model gave a sample that was not finite. Final.
    Fault,
    /// Asked for ([`crate::StageControl::force_step_down`]).
    Forced,
    /// The thread fell behind real time for several model blocks in a row by the wall
    /// clock, whatever the cause.
    Starved,
}

impl StepDownReason {
    /// The reason's name: `slow_block`, `slow_window`, `heavy`, `fault`, `forced` or
    /// `starved`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SlowBlock => "slow_block",
            Self::SlowWindow => "slow_window",
            Self::Heavy => "heavy",
            Self::Fault => "fault",
            Self::Forced => "forced",
            Self::Starved => "starved",
        }
    }

    #[cfg_attr(not(feature = "dfn3"), allow(dead_code))]
    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::SlowBlock => 1,
            Self::SlowWindow => 2,
            Self::Heavy => 3,
            Self::Fault => 4,
            Self::Forced => 5,
            Self::Starved => 6,
        }
    }

    pub(crate) const fn from_code(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::SlowBlock),
            2 => Some(Self::SlowWindow),
            3 => Some(Self::Heavy),
            4 => Some(Self::Fault),
            5 => Some(Self::Forced),
            6 => Some(Self::Starved),
            _ => None,
        }
    }
}

impl std::fmt::Display for StepDownReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where Strong is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum StrongState {
    /// The stage has no model, or Strong is not asked for.
    #[default]
    Idle,
    /// The model runs on the input while Standard's output goes on (start-up, a retry).
    Warming,
    /// The model's output goes on.
    Running,
    /// Stepped down to Standard; the model is retried later.
    SteppedDown,
    /// Stepped down for the stage's life (three counted step-downs, the bound on those tied to
    /// a grace window, or a fault).
    Retired,
}

impl StrongState {
    /// The state's name: `idle`, `warming`, `running`, `stepped_down` or `retired`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Warming => "warming",
            Self::Running => "running",
            Self::SteppedDown => "stepped_down",
            Self::Retired => "retired",
        }
    }

    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::Idle => 0,
            Self::Warming => 1,
            Self::Running => 2,
            Self::SteppedDown => 3,
            Self::Retired => 4,
        }
    }

    pub(crate) const fn from_code(v: u8) -> Self {
        match v {
            1 => Self::Warming,
            2 => Self::Running,
            3 => Self::SteppedDown,
            4 => Self::Retired,
            _ => Self::Idle,
        }
    }
}

#[cfg(feature = "dfn3")]
mod layer;
#[cfg(feature = "dfn3")]
pub(crate) use layer::Strong;
#[cfg(all(test, feature = "dfn3"))]
pub(crate) use layer::tests::model as test_model;
