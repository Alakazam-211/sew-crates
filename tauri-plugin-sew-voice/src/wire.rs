//! The JSON the webview sends and receives (frozen with K2, whose webview speaks it).
//!
//! Every command argument and answer, every event payload, and the command error are here, so
//! the contract tests (`tests/contract.rs`) pin exactly what crosses the IPC bridge. No type here
//! carries a token, a credential, a key or a display name.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The call's state: the `state` event and the `state` command's snapshot.
///
/// `{"state": "idle" | "connecting" | "connected"}`, `{"state": "reconnecting", "attempt": n}`
/// or `{"state": "ended", "reason": "..."}`. `connected` means media is up and a key epoch is
/// active (you can hear and be heard).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
// No deny_unknown_fields: it is flattened into StateSnapshot, which carries more fields.
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CallState {
    /// No call yet (the plugin's start).
    Idle,
    /// A `join` is in flight: the provider, then the engine's connect.
    Connecting,
    /// Media up and a key epoch active.
    Connected,
    /// The engine is resuming or reconnecting (1-based attempt).
    Reconnecting {
        /// 1 for the first attempt.
        attempt: u32,
    },
    /// The call is over. `reason` is Sew's voice leave reason as it arrived (Sew 0.7.2:
    /// `perms_lost`, `removed_from_space`, `timed_out`, `channel_archived`, `account_removed`,
    /// `removed_by_moderator`, `session_revoked`, `device_revoked`, `media_path_down`, ...),
    /// the engine's when Sew gave none within 1.5 s of its `Disconnect` (`kicked`,
    /// `room_closed`), or one of the plugin's: `left`, `signin_required`, `network`,
    /// `e2ee_refused`, `credential_lost` (another device took the call over), `unavailable`,
    /// `provider_error`, `engine_stopped`, or a provider refusal's code. See [`reason`].
    Ended {
        /// Why it ended. An open set: show unknown reasons as "call ended".
        reason: String,
    },
}

impl CallState {
    /// `connecting`, `connected` or `reconnecting`: a second `join` is refused.
    pub fn is_live(&self) -> bool {
        matches!(
            self,
            CallState::Connecting | CallState::Connected | CallState::Reconnecting { .. }
        )
    }
}

/// The plugin's end reasons (Sew's own leave reasons pass through unchanged).
pub mod reason {
    /// Sew's voice leave reasons a native call can end with (Sew 0.7.2), as they arrive. The
    /// set is open: a later Sew may send others (show them as "call ended").
    pub const SEW: [&str; 8] = [
        PERMS_LOST,
        REMOVED_FROM_SPACE,
        TIMED_OUT,
        CHANNEL_ARCHIVED,
        ACCOUNT_REMOVED,
        REMOVED_BY_MODERATOR,
        SESSION_REVOKED,
        DEVICE_REVOKED,
    ];
    /// CONNECT or VIEW lost (a role, an overwrite, a member sync that took the person out).
    pub const PERMS_LOST: &str = "perms_lost";
    /// Left or removed from the Space.
    pub const REMOVED_FROM_SPACE: &str = "removed_from_space";
    /// Timed out in the Space.
    pub const TIMED_OUT: &str = "timed_out";
    /// The channel was archived.
    pub const CHANNEL_ARCHIVED: &str = "channel_archived";
    /// The account was deprovisioned.
    pub const ACCOUNT_REMOVED: &str = "account_removed";
    /// A moderator disconnected the member.
    pub const REMOVED_BY_MODERATOR: &str = "removed_by_moderator";
    /// The login was revoked (a sign-out, K2's `sessions/revoke`).
    pub const SESSION_REVOKED: &str = "session_revoked";
    /// The device was revoked.
    pub const DEVICE_REVOKED: &str = "device_revoked";
    /// The engine removed the participant and Sew gave no reason within 1.5 s (a fallback).
    pub const KICKED: &str = "kicked";
    /// The engine closed the room and Sew gave no reason within 1.5 s (a fallback).
    pub const ROOM_CLOSED: &str = "room_closed";
    /// Another device of the member took the call over (Sew's `replaced`).
    pub const CREDENTIAL_LOST: &str = "credential_lost";
    /// The app called `leave`.
    pub const LEFT: &str = "left";
    /// The provider answered [`ProviderError::SigninRequired`](crate::ProviderError).
    pub const SIGNIN_REQUIRED: &str = "signin_required";
    /// The provider answered `Unavailable` (Sew or the App's server is down or busy).
    pub const UNAVAILABLE: &str = "unavailable";
    /// The provider failed in another way (`Other`).
    pub const PROVIDER_ERROR: &str = "provider_error";
    /// The engine's event stream closed without an `ended` state.
    pub const ENGINE_STOPPED: &str = "engine_stopped";
}

/// One participant in the `participants` event. Names are the host's: the plugin has none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Participant {
    /// Sew's member id (a decimal string, as Sew's API writes ids).
    pub member_id: String,
    /// This device's own member.
    pub is_self: bool,
    /// Self-muted (a deafened member is always muted).
    pub muted: bool,
    /// Self-deafened.
    pub deafened: bool,
    /// Muted by a moderator.
    pub server_muted: bool,
    /// Deafened by a moderator.
    pub server_deafened: bool,
    /// The member lacks SPEAK in this channel (Sew 0.7.2): a role or overwrite, not a moderator
    /// (`server_muted`). Such a member is shown `muted`; this device captures and sends
    /// nothing until SPEAK comes back, then its own mute is as it was. Written only when
    /// `true` (an absent field is `false`), so a reader that refuses unknown fields still
    /// reads every participant who may speak.
    #[serde(default, skip_serializing_if = "is_false")]
    pub speak_denied: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// One entry of the `speaking` event: `[{member_id, level}]`, at most 10 a second.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Speaking {
    /// Sew's member id; this device's own member is included.
    pub member_id: String,
    /// 0 (silent) to 1 (full scale).
    pub level: f64,
}

/// One audio device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Device {
    /// The stable device id `set_device` takes.
    pub id: String,
    /// The OS's name for it (for a picker; not a person's name).
    pub name: String,
    /// The OS default.
    pub default: bool,
}

/// The `devices` command's answer and the `devices` event (hot-plug or a default change).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceList {
    /// Microphones.
    pub inputs: Vec<Device>,
    /// Speakers and headsets.
    pub outputs: Vec<Device>,
    /// The input in use (`null`: the OS default).
    pub input: Option<String>,
    /// The output in use (`null`: the OS default).
    pub output: Option<String>,
}

/// The `errors` event, and every command's error: `{code, detail}`.
///
/// Codes: `mic_denied`, `mic_unavailable`, `device_lost`, `turn_unreachable`,
/// `relay_only_no_turns`, `e2ee_refused`, `ca_pin_mismatch`, `key_storage_file` (a warning),
/// the command errors `already_in_call`, `not_in_call`, `invalid_args`, `left`, and the
/// provider's refusals passed through as given.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[serde(deny_unknown_fields)]
#[error("{code}")]
pub struct ErrorInfo {
    /// A stable snake_case code.
    pub code: String,
    /// More about it (`null`, a string or an object); never a token or a key.
    pub detail: Value,
}

impl ErrorInfo {
    /// An error with `detail: null`.
    pub fn new(code: impl Into<String>) -> ErrorInfo {
        ErrorInfo {
            code: code.into(),
            detail: Value::Null,
        }
    }

    /// An error with a detail.
    pub fn with_detail(code: impl Into<String>, detail: impl Into<Value>) -> ErrorInfo {
        ErrorInfo {
            code: code.into(),
            detail: detail.into(),
        }
    }
}

/// The plugin's command error codes.
pub mod code {
    /// `join` while a call is connecting, connected or reconnecting.
    pub const ALREADY_IN_CALL: &str = "already_in_call";
    /// `mute` with no call.
    pub const NOT_IN_CALL: &str = "not_in_call";
    /// A command's arguments make no sense (e.g. `mute {}`).
    pub const INVALID_ARGS: &str = "invalid_args";
    /// `join` answered after a `leave` cancelled it.
    pub const LEFT: &str = "left";
    /// The provider's `Unavailable`.
    pub const UNAVAILABLE: &str = "unavailable";
    /// The provider's `Other`.
    pub const PROVIDER_ERROR: &str = "provider_error";
    /// The provider's `SigninRequired`.
    pub const SIGNIN_REQUIRED: &str = "signin_required";
}

/// `join`'s arguments: `{target, room, muted?, deafened?}`. `target` and `room` are opaque to
/// the plugin and go to the host's `JoinProvider` as given.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JoinArgs {
    /// Which Sew (or which App) to join through; the host's meaning.
    pub target: String,
    /// The voice channel or DM call; the host's meaning.
    pub room: String,
    /// Join muted (default false).
    #[serde(default)]
    pub muted: Option<bool>,
    /// Join deafened (default false; implies muted).
    #[serde(default)]
    pub deafened: Option<bool>,
}

/// `join`'s answer: `{call_id}`. No token in or out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JoinResult {
    /// The call's id (Sew's call room).
    pub call_id: String,
}

/// `mute`'s arguments: `{muted?, deafened?}`; at least one. Deafened implies muted;
/// un-deafening returns `muted` to its value before the deafen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MuteArgs {
    /// Self-mute on or off.
    #[serde(default)]
    pub muted: Option<bool>,
    /// Self-deafen on or off.
    #[serde(default)]
    pub deafened: Option<bool>,
}

/// `set_device`'s arguments: `{input?, output?}` (ids from `devices`, or `"default"`); hot-swaps
/// the stream without leaving the call. An absent (or `null`) direction stays as it is.
///
/// `"default"` ([`DEFAULT_DEVICE`]) goes back to the OS default and follows it when it changes
/// later (kessel-audio's `DeviceChoice::Default`); the `devices` list then says `null` for that
/// direction. No device id is `"default"` (kessel-audio's ids are `<host>:<id>`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetDeviceArgs {
    /// The new input's id, or `"default"`.
    #[serde(default)]
    pub input: Option<String>,
    /// The new output's id, or `"default"`.
    #[serde(default)]
    pub output: Option<String>,
}

/// `set_device`'s `"default"`: the OS default, followed when it changes.
pub const DEFAULT_DEVICE: &str = "default";

impl SetDeviceArgs {
    /// The input's choice: `None` (unchanged), the OS default or a device id.
    pub fn input_choice(&self) -> Option<DeviceChoice> {
        self.input.as_deref().map(DeviceChoice::from_wire)
    }

    /// The output's choice: `None` (unchanged), the OS default or a device id.
    pub fn output_choice(&self) -> Option<DeviceChoice> {
        self.output.as_deref().map(DeviceChoice::from_wire)
    }
}

/// Which device one direction uses (`set_device`; kessel-audio's `DeviceChoice`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DeviceChoice {
    /// The OS default, followed when it changes.
    Default,
    /// A device by its id from `devices`.
    Id(String),
}

impl DeviceChoice {
    /// `"default"` is [`DeviceChoice::Default`]; anything else is an id.
    pub fn from_wire(s: &str) -> DeviceChoice {
        if s == DEFAULT_DEVICE {
            DeviceChoice::Default
        } else {
            DeviceChoice::Id(s.to_owned())
        }
    }
}

/// The `state` event: the call's state (flattened) and which `target` and `room` the call is on.
///
/// `{"state": "connected", "target": "acme", "room": "48831765147549698"}`. `target` and `room`
/// are the `join` arguments as given, from `connecting` until the call's `ended` (both absent
/// while `idle`). After a moderator move (Sew's `voice_server_updated`) `room` is the
/// destination as Sew names it: the DM call's id when it is a call, else the voice channel's
/// id (decimal strings); the plugin sends a `state` event with the new `room` (the state may
/// be unchanged). Both are written only when present, so a reader that refuses unknown fields
/// and predates them still reads an `idle` state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateEvent {
    /// The state.
    #[serde(flatten)]
    pub state: CallState,
    /// The call's `target` (the `join` argument).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// The call's `room` (the `join` argument, or a move's destination).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room: Option<String>,
}

impl StateEvent {
    /// A state with no call (no `target` or `room`).
    pub fn bare(state: CallState) -> StateEvent {
        StateEvent {
            state,
            target: None,
            room: None,
        }
    }
}

/// The `state` command's answer, for a window opened mid-call: the `state` event's fields
/// (flattened: the state, `target` and `room`) plus the participants and the call id.
///
/// `{"state": "connected", "target": "acme", "room": "48831765147549698", "participants":
/// [...], "call_id": "r_..."}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateSnapshot {
    /// The current state.
    #[serde(flatten)]
    pub state: CallState,
    /// The call's `target`, as in [`StateEvent`] (absent while `idle`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// The call's `room`, as in [`StateEvent`] (absent while `idle`; a move's destination
    /// after a move).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room: Option<String>,
    /// The current participants (`[]` with no call).
    pub participants: Vec<Participant>,
    /// The call's id while there is one.
    pub call_id: Option<String>,
}

/// The plugin's events. Each window gets every event as `sew-voice://<name>`.
#[derive(Debug, Clone, PartialEq)]
pub enum PluginEvent {
    /// `state`: [`StateEvent`] (the [`CallState`] with the call's `target` and `room`).
    State(StateEvent),
    /// `participants`: `[Participant]`.
    Participants(Vec<Participant>),
    /// `speaking`: `[Speaking]`, at most 10 a second.
    Speaking(Vec<Speaking>),
    /// `errors`: [`ErrorInfo`].
    Errors(ErrorInfo),
    /// `devices`: [`DeviceList`].
    Devices(DeviceList),
}

/// The prefix of every event name the plugin emits: `sew-voice://state` and so on.
pub const EVENT_PREFIX: &str = "sew-voice://";

/// The five event names, without [`EVENT_PREFIX`].
pub const EVENT_NAMES: [&str; 5] = ["state", "participants", "speaking", "errors", "devices"];

impl PluginEvent {
    /// The event's frozen name: `state`, `participants`, `speaking`, `errors` or `devices`.
    pub fn name(&self) -> &'static str {
        match self {
            PluginEvent::State(_) => "state",
            PluginEvent::Participants(_) => "participants",
            PluginEvent::Speaking(_) => "speaking",
            PluginEvent::Errors(_) => "errors",
            PluginEvent::Devices(_) => "devices",
        }
    }

    /// The name the webview listens to: `sew-voice://<name>`.
    pub fn event_name(&self) -> String {
        format!("{EVENT_PREFIX}{}", self.name())
    }

    /// The event's JSON payload.
    pub fn payload(&self) -> Value {
        let v = match self {
            PluginEvent::State(s) => serde_json::to_value(s),
            PluginEvent::Participants(p) => serde_json::to_value(p),
            PluginEvent::Speaking(s) => serde_json::to_value(s),
            PluginEvent::Errors(e) => serde_json::to_value(e),
            PluginEvent::Devices(d) => serde_json::to_value(d),
        };
        // These types are plain structs of strings, bools and numbers; serde_json cannot fail
        // on them (a NaN level is written as null).
        v.unwrap_or(Value::Null)
    }
}
