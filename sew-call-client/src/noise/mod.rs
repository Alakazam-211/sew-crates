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
//! stage runs after the OS's processing, as on desktop.

use serde::{Deserialize, Serialize};

#[cfg(feature = "noise-bench")]
pub mod bench;
#[cfg(feature = "audio-core")]
mod control;
#[cfg(feature = "audio-core")]
mod hook;
#[cfg(any(feature = "dfn3", feature = "noise-bench"))]
mod mem;

#[cfg(feature = "dfn3")]
pub use control::Dfn3LoadInfo;
#[cfg(any(feature = "dfn3", feature = "noise-bench"))]
pub(crate) use mem::rss_kib;
#[cfg(feature = "noise-bench")]
pub(crate) use mem::rss_peak_kib;

#[cfg(feature = "audio-core")]
pub(crate) use control::BypassSource;
#[cfg(feature = "audio-core")]
pub use control::NoiseRemovalControl;

/// Where a call reads its noise removal state (its stats windows, [`crate::Call::noise_removal`]):
/// the [`NoiseRemovalControl`] its audio started with. `None` once that audio ended.
pub(crate) trait NoiseSource: Send + Sync {
    fn noise_state(&self) -> Option<NoiseRemovalState>;
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
    /// music or a studio microphone. On Android, Off turns off the OS's noise suppression for a
    /// call that starts in Off (a change during a call takes effect from the next call). On iOS
    /// the OS's voice processing keeps its own noise suppression for now (a later kessel-audio
    /// release adds an unprocessed path).
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
/// Its JSON is `"budget"`, `"fault"` or `"unavailable"`.
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
}

impl NoiseRemovalReason {
    /// The reason's name: `budget`, `fault` or `unavailable`.
    pub fn as_str(self) -> &'static str {
        match self {
            NoiseRemovalReason::Budget => "budget",
            NoiseRemovalReason::Fault => "fault",
            NoiseRemovalReason::Unavailable => "unavailable",
        }
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

/// The noise removal setting and what runs: `{mode, active, reason?, model?, model_reason?,
/// step_downs?}`.
///
/// `mode` is what was asked, `active` what the microphone's audio goes through now, and
/// `reason` (absent when `active` is `mode`) why they differ. With no call, `active` is what a
/// call would start with. On phones the OS's own noise suppression is set when a call's audio
/// starts (off for Off on Android; see [`NoiseRemoval::Off`]): a change during a call takes
/// effect there from the next call.
///
/// Where DeepFilterNet3 is in play (feature `dfn3`, once the app has given the control its
/// weights, whether they loaded or not), `model` says which model runs while `active` is
/// Standard, `model_reason` why it is RNNoise rather than DeepFilterNet3 (`budget`: the stage
/// stepped down because the device could not keep up; `fault`; `unavailable`: no usable
/// weights), and `step_downs` how many times this call's stage stepped down. They are absent
/// otherwise, so the JSON is then `{mode, active, reason?}` as before. A UI shows, for example,
/// "DeepFilterNet3" or "RNNoise (stepped down: budget)".
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
}

fn is_zero(v: &u32) -> bool {
    *v == 0
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
        }
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
}
