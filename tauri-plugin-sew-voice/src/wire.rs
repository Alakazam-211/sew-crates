//! The JSON the webview sends and receives (frozen with K2, whose webview speaks it).
//!
//! Every command argument and answer, every event payload, and the command error are here, so
//! the contract tests (`tests/contract.rs`) pin exactly what crosses the IPC bridge. No type here
//! carries a token, a credential, a key or a display name.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Sew's consent modal data (`recording`'s `consent_prompt`), from `sew-proto` unchanged.
pub use sew_call_client::ConsentPrompt;

/// The call's state: the `state` event and the `state` command's snapshot.
///
/// `{"state": "idle" | "connecting" | "connected"}`, `{"state": "reconnecting", "attempt": n}`,
/// `{"state": "consent_required", "recording_id": "...", "notice_version": n}` or
/// `{"state": "ended", "reason": "..."}`. `connected` means media is up and a key epoch is
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
    /// Held (Sew 0.7.3): the room is recorded and Sew lets the join in only with the person's
    /// answer to its notice (the `recording` event's `consent_prompt`). No microphone, no
    /// media, no keys. Answer with `consent {granted, notice_version}` (the call then joins,
    /// recorded only after a grant) or give up with `leave`. It waits as long as the app does.
    ConsentRequired {
        /// The recording (Sew's id, a decimal string), or `auto` for an auto-record channel's
        /// first join.
        recording_id: String,
        /// The notice's version, to echo in `consent`.
        notice_version: u32,
    },
    /// The call is over. `reason` is Sew's voice leave reason as it arrived (Sew 0.7.2:
    /// `perms_lost`, `removed_from_space`, `timed_out`, `channel_archived`, `account_removed`,
    /// `removed_by_moderator`, `session_revoked`, `device_revoked`, `media_path_down`, ...),
    /// the key group's when the call socket gave none within 1.5 s of the key group's
    /// removal (`disconnected` for a reason the key protocol cannot name), the engine's when
    /// Sew gave none within 1.5 s of its `Disconnect` (`kicked`, `room_closed`), or one of the
    /// plugin's: `left`, `signin_required`, `network`,
    /// `e2ee_refused`, `credential_lost` (another device took the call over), `unavailable`,
    /// `provider_error`, `engine_stopped`, or a provider refusal's code. See [`reason`].
    Ended {
        /// Why it ended. An open set: show unknown reasons as "call ended".
        reason: String,
    },
}

impl CallState {
    /// `connecting`, `connected`, `reconnecting` or `consent_required`: a second `join` is
    /// refused.
    pub fn is_live(&self) -> bool {
        matches!(
            self,
            CallState::Connecting
                | CallState::Connected
                | CallState::Reconnecting { .. }
                | CallState::ConsentRequired { .. }
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
    /// Sew's recording leave reasons (Sew 0.7.3; only where declining means not joining,
    /// `recording.require_consent_to_join`), as they arrive.
    pub const RECORDING: [&str; 3] = [
        RECORDING_DECLINED,
        RECORDING_WITHDRAWN,
        RECORDING_CONSENT_TIMEOUT,
    ];
    /// The person declined the recording notice.
    pub const RECORDING_DECLINED: &str = "recording_declined";
    /// The person withdrew their consent ("Stop recording me").
    pub const RECORDING_WITHDRAWN: &str = "recording_withdrawn";
    /// The notice stayed unanswered too long.
    pub const RECORDING_CONSENT_TIMEOUT: &str = "recording_consent_timeout";
    /// A held join's decline was refused because declining means not joining (Sew's refusal
    /// reason, passed through as a refusal code).
    pub const RECORDING_CONSENT_REQUIRED_TO_JOIN: &str = "recording_consent_required_to_join";
    /// The engine removed the participant and Sew gave no reason within 1.5 s (a fallback).
    pub const KICKED: &str = "kicked";
    /// The engine closed the room and Sew gave no reason within 1.5 s (a fallback).
    pub const ROOM_CLOSED: &str = "room_closed";
    /// Another device of the member took the call over (Sew's `replaced`).
    pub const CREDENTIAL_LOST: &str = "credential_lost";
    /// The call credential expired: Sew refused its renewal as `expired`, or no renewal got
    /// through before it ran out.
    pub const CREDENTIAL_EXPIRED: &str = "credential_expired";
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

/// This device's own mute and deafen, for a native listener
/// ([`VoiceListener::on_self_mute`](crate::VoiceListener::on_self_mute)): what CallKit's or
/// Telecom's mute button mirrors, from every source (the app's command, the webview, a
/// moderator, SPEAK lost). The webview has no such event: its own row in `participants` carries
/// the same flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelfMute {
    /// Nothing of yours is heard: your own mute, or forced by a deafen, a moderator mute or
    /// SPEAK lost. What CallKit shows.
    pub muted: bool,
    /// Deafened (by yourself).
    pub deafened: bool,
    /// Your own choice only: `set_muted(false)` undoes this and nothing else.
    pub self_muted: bool,
    /// Muted by a moderator.
    pub server_muted: bool,
    /// You lack SPEAK in this channel (a role or an overwrite).
    pub speak_denied: bool,
}

impl SelfMute {
    /// From this device's row and its own mute choice (`own`), the flags as [`SelfMute`]
    /// names them. While nothing else forces the mute (no deafen, no moderator mute, SPEAK allowed)
    /// the row's `muted` is the choice itself.
    pub fn of(me: &Participant, own: bool) -> SelfMute {
        let forced = me.deafened || me.server_muted || me.speak_denied;
        SelfMute {
            muted: me.muted || forced,
            deafened: me.deafened,
            self_muted: if forced { own } else { me.muted },
            server_muted: me.server_muted,
            speak_denied: me.speak_denied,
        }
    }

    /// Whether `me`'s row says the choice outright (nothing else forces the mute).
    pub fn is_unforced(me: &Participant) -> bool {
        !(me.deafened || me.server_muted || me.speak_denied)
    }
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
    /// `mute` (and [`VoiceControl`](crate::VoiceControl)'s `set_muted` and `set_deafened`)
    /// with no call.
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
    /// `consent` or `withdraw_consent` with no recording to answer, or `stop_recording` with
    /// no recording to stop.
    pub const NO_RECORDING: &str = "no_recording";
    /// The notice changed before the answer reached Sew: `detail` is Sew's refusal
    /// (`{recording, auto, consent_prompt}`); show the new notice and answer again.
    pub const RECORDING_CONSENT_REQUIRED: &str = "recording_consent_required";
    /// The host's provider sends no consent (its `JoinProvider::consent` is the default).
    pub const CONSENT_UNSUPPORTED: &str = "consent_unsupported";
    /// `start_recording` or `stop_recording`: the host's provider sends no Record or Stop (its
    /// `JoinProvider::recording` is the default).
    pub const RECORDING_UNSUPPORTED: &str = "recording_unsupported";
    /// `start_recording` while the join is held for the recording notice: the person is not in
    /// the room yet (Sew's own reason for a start without a voice entry).
    pub const NOT_IN_VOICE: &str = "not_in_voice";
    /// `forget_server` while a call is connecting, held, connected or reconnecting: leave first.
    pub const IN_CALL: &str = "in_call";
    /// `forget_server` or `known_device_keys`: the device store failed (`detail`: what failed).
    /// A hardware key that can't be deleted stops `forget_server` with this, and a retry finds
    /// that device again.
    pub const DEVICE_STORE: &str = "device_store";
}

/// One device key the plugin's device store holds (`known_device_keys`, `forget_server`):
/// `{install_id, member_id, device_scope, account}`. Ids only, never key material. With a
/// hardware key, `account` is the Keychain tag / Keystore alias the host's `HardwareDeviceKey`
/// was given.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceKeyInfo {
    /// Sew's install id (a decimal string).
    pub install_id: String,
    /// The member the device is enrolled for (a decimal string).
    pub member_id: String,
    /// The login lineage the device belongs to.
    pub device_scope: String,
    /// `<install_id>:<member_id>:<device_scope>` (`DeviceKeyId::account`).
    pub account: String,
}

impl From<&sew_call_client::device::DeviceKeyId> for DeviceKeyInfo {
    fn from(id: &sew_call_client::device::DeviceKeyId) -> DeviceKeyInfo {
        DeviceKeyInfo {
            install_id: id.install_id.to_string(),
            member_id: id.member_id.to_string(),
            device_scope: id.device_scope.clone(),
            account: id.account(),
        }
    }
}

/// `forget_server`'s arguments: `{install_id}`, Sew's install id as a decimal string (the
/// `install_id` of a `known_device_keys` entry).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForgetServerArgs {
    /// Sew's install id (a decimal string).
    pub install_id: String,
}

impl ForgetServerArgs {
    /// The install id, or `invalid_args`.
    pub fn install(&self) -> Result<u64, ErrorInfo> {
        self.install_id.parse::<u64>().map_err(|_| {
            ErrorInfo::with_detail(
                code::INVALID_ARGS,
                "forget_server needs install_id, a decimal string",
            )
        })
    }
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

/// A native join's options ([`crate::VoiceControl::join_with`]), for what CallKit's or
/// Telecom's start says (video calls): `JoinOptions::default()` is a plain join.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct JoinOptions {
    /// Turn the camera on as soon as the call is connected and the app is in front (the
    /// engine takes the lease and publishes). Refusals come back as `local_video {camera:
    /// "off", reason: <code>}`; the call still joins.
    pub camera: bool,
    /// The camera (a `video_devices` id or `"default"`; `None`: the one `set_video_device`
    /// chose, else the default).
    pub camera_device: Option<String>,
    /// The video codecs the host's webview decodes (its WebCodecs probe; the `join` command's
    /// `decode`). The engine offers to receive H.264 only when this or its own OS decoders list
    /// it, and never on Linux. `None`: nothing is assumed about the webview.
    pub decode: Option<Vec<VideoCodec>>,
}

impl JoinOptions {
    /// A join with the camera on (the chosen camera).
    pub fn with_camera() -> JoinOptions {
        JoinOptions {
            camera: true,
            camera_device: None,
            decode: None,
        }
    }

    /// With the webview's decoders `codecs` ([`JoinOptions::decode`]).
    pub fn with_decode(mut self, codecs: &[VideoCodec]) -> JoinOptions {
        self.decode = Some(codecs.to_vec());
        self
    }

    /// With camera `device` (a `video_devices` id or `"default"`).
    pub fn camera_device(mut self, device: &str) -> JoinOptions {
        self.camera_device = Some(device.to_owned());
        self
    }
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

/// `consent`'s arguments: `{granted, notice_version}`, the person's answer to the notice they
/// saw (the `recording` event's `consent_prompt.notice_version`, or `consent_required`'s).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsentArgs {
    /// Agree to be recorded (`true`), or decline (`false`).
    pub granted: bool,
    /// The notice's version the person saw.
    pub notice_version: u32,
}

/// A person's answer to the recording notice, as Sew stores it: `pending` (not answered:
/// never recorded), `granted`, `declined` or `withdrawn`.
pub mod consent {
    /// Not answered: never recorded.
    pub const PENDING: &str = "pending";
    /// Agreed.
    pub const GRANTED: &str = "granted";
    /// Declined.
    pub const DECLINED: &str = "declined";
    /// Agreed, then withdrew ("Stop recording me").
    pub const WITHDRAWN: &str = "withdrawn";

    /// The engine's answer as the wire writes it (a value the engine adds later is never a
    /// grant: `declined`).
    pub fn of(c: sew_call_client::RecordingConsent) -> &'static str {
        use sew_call_client::RecordingConsent as C;
        match c {
            C::Pending => PENDING,
            C::Granted => GRANTED,
            C::Withdrawn => WITHDRAWN,
            _ => DECLINED,
        }
    }
}

/// One person in the `recording` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordingParticipant {
    /// Sew's member id.
    pub member_id: String,
    /// Their answer ([`consent`]).
    pub consent: String,
    /// The recorder captures them now.
    pub recorded: bool,
}

/// The `recording` event (and the `state` command's `recording`): the call's active recording
/// as Sew says it, or `null` once none is active (Sew 0.7.3).
///
/// `{"recording_id": "...", "auto": false, "started_by": "...", "my_consent": "pending",
/// "consent_prompt": {...}, "participants": [...], "recorder_present": true}`. Show the
/// recording badge while it is non-null; show the consent modal from `consent_prompt` while
/// `my_consent` is `pending` (it is written only then), and "Stop recording me" while it is
/// `granted`. This device is recorded only after Sew says `granted`: no answer is never
/// consent.
///
/// Sew 0.9.0 adds `trigger` (what started it: [`trigger`]) and `required` (the room requires
/// its recording: Stop is refused `recording_required` while two or more people are in it).
///
/// Plugin 0.6.1 adds `can_record` and `can_stop`: whether Sew would take this person's Record
/// and Stop now, worked out from the host's [`RecordingRights`] (both `false` until the host
/// gives them). Each is written only when `true`.
///
/// Non-exhaustive (Sew 0.9.0): later releases add fields without a breaking change; outside
/// this crate, read it (or build it from JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct RecordingInfo {
    /// The recording (Sew's id), or `auto` while a held join into an auto-record channel waits.
    pub recording_id: String,
    /// Started by an auto-record channel.
    pub auto: bool,
    /// Who started it (Sew's member id), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_by: Option<String>,
    /// This person's answer ([`consent`]).
    pub my_consent: String,
    /// The notice (Sew's `ConsentPrompt`, unchanged) while `my_consent` is `pending`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consent_prompt: Option<ConsentPrompt>,
    /// Everyone in it.
    pub participants: Vec<RecordingParticipant>,
    /// The recorder (Sew Notes) is in the call's media now. It is never in `participants`
    /// (it is not a member).
    pub recorder_present: bool,
    /// What started it (Sew 0.9.0): `manual` (a person pressed Record), `auto` (an
    /// auto-record room's first join) or `required` (the room reached two people while it
    /// requires recording); a value a later Sew adds is passed through. Read as `manual` when
    /// absent.
    #[serde(default = "trigger::manual")]
    pub trigger: String,
    /// The room requires its recording (Sew 0.9.0): hide Stop while two or more people are in
    /// the room (Sew refuses it `recording_required`); "Stop recording me" still works. Read
    /// as `false` when absent.
    #[serde(default)]
    pub required: bool,
    /// Record would not be refused now (plugin 0.6.1; [`RecordingControls::can_record`]).
    /// Always `false` here: Sew refuses a second recording (`recording_active`). Written only
    /// when `true`; read as `false` when absent.
    #[serde(default, skip_serializing_if = "is_false")]
    pub can_record: bool,
    /// Stop would not be refused now (plugin 0.6.1; [`RecordingControls::can_stop`]): show
    /// Stop while it is `true`. "Stop recording me" (`withdraw_consent`) is separate and always
    /// works. Written only when `true`; read as `false` when absent.
    #[serde(default, skip_serializing_if = "is_false")]
    pub can_stop: bool,
}

/// What the person may do about the call's recording now (plugin 0.6.1): the
/// `recording_controls` event and the `state` command's `recording_controls`.
///
/// `{"can_record": true, "can_stop": false}`. The plugin works both out from the host's
/// [`RecordingRights`] (`set_recording_rights`) and what it knows of the call: the state, who
/// is in the room, the recording and who started it. Both are `false` with no call, while the
/// join is held for a notice, and until the host gives the rights. They follow Sew's own
/// checks; Sew still decides, so show its refusal if one comes.
///
/// Non-exhaustive: later releases add fields without a breaking change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct RecordingControls {
    /// Record would not be refused now: in the call, no recording active, recording on for the
    /// org (and for DMs, in a DM), RECORD_CALLS held, and neither an agent nor a session whose
    /// pass may not record.
    pub can_record: bool,
    /// Stop would not be refused now: a recording is active, and the person is its starter,
    /// holds RECORD_CALLS or MANAGE_CHANNELS (in a DM: is in the call), is neither an agent
    /// nor a session whose pass may not record, and the recording is not a required one with
    /// two or more people in the room.
    pub can_stop: bool,
}

impl RecordingControls {
    /// Neither (the `state` command leaves it out then).
    pub fn is_none(&self) -> bool {
        !self.can_record && !self.can_stop
    }
}

/// `set_recording_rights`' arguments (plugin 0.6.1): what the host knows of the person's
/// rights in the call's room, for [`RecordingControls`].
///
/// `{"room": "48831765147549698", "my_perms": "8388608", "dm": false,
/// "recording_enabled": true, "recording_dm_enabled": false, "record_capped": false,
/// "agent": false}`. Only `room` and `my_perms` are required; the others read `false` when
/// absent. Send it on (or before) each join, and again whenever one of them changes (Sew's
/// permission updates for the channel, a policy change, a move: the `state` event's new
/// `room`). It applies while the call is in `room`, and is forgotten when the call ends.
///
/// Non-exhaustive: later releases add fields (each optional) without a breaking change.
/// Outside this crate, build one with [`RecordingRights::new`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct RecordingRights {
    /// The room the rights are for, as `join`'s `room` (or the `state` event's after a move).
    pub room: String,
    /// The person's permissions on the room's voice channel or DM, as Sew reports them (the
    /// channel's `my_perms`, a decimal string). A session whose pass may not record already
    /// has RECORD_CALLS left out there.
    pub my_perms: String,
    /// The room is a DM or group DM call.
    #[serde(default)]
    pub dm: bool,
    /// The org policy `recording.enabled` (the policy digest's `recording_enabled`).
    #[serde(default)]
    pub recording_enabled: bool,
    /// The org policy `recording.dm_enabled` (the policy digest's `recording_dm_enabled`).
    #[serde(default)]
    pub recording_dm_enabled: bool,
    /// The session's pass lacks `voice:record` where its App requires it: Sew refuses both
    /// Record and Stop with `cap_ceiling`.
    #[serde(default)]
    pub record_capped: bool,
    /// The person is an agent (or the session acts as one): Sew refuses agents both.
    #[serde(default)]
    pub agent: bool,
}

impl RecordingRights {
    /// Rights for `room` with `my_perms` (a decimal string); every flag `false`.
    pub fn new(room: impl Into<String>, my_perms: impl Into<String>) -> RecordingRights {
        RecordingRights {
            room: room.into(),
            my_perms: my_perms.into(),
            dm: false,
            recording_enabled: false,
            recording_dm_enabled: false,
            record_capped: false,
            agent: false,
        }
    }

    /// `my_perms` as bits, or `None` when it is not a decimal number.
    pub fn perms(&self) -> Option<u64> {
        self.my_perms.parse().ok()
    }
}

/// The `recording` event's `trigger` values (Sew 0.9.0).
pub mod trigger {
    /// A person pressed Record.
    pub const MANUAL: &str = "manual";
    /// An auto-record room's first join.
    pub const AUTO: &str = "auto";
    /// The room reached two people while it requires recording.
    pub const REQUIRED: &str = "required";

    pub(crate) fn manual() -> String {
        MANUAL.to_owned()
    }

    /// The engine's trigger as the wire writes it (Sew's own name for a value added later).
    pub fn of(t: sew_call_client::RecordingTrigger) -> String {
        serde_json::to_value(t)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_else(manual)
    }
}

impl RecordingInfo {
    /// The engine's recording (`sew-call-client`'s), with `recorder_present`.
    pub fn from_engine(
        r: &sew_call_client::CallRecording,
        recorder_present: bool,
    ) -> RecordingInfo {
        RecordingInfo {
            recording_id: r.recording_id.clone(),
            auto: r.auto,
            started_by: r.started_by.clone(),
            my_consent: consent::of(r.my_consent).to_owned(),
            consent_prompt: r.consent_prompt.clone(),
            participants: r
                .participants
                .iter()
                .map(|p| RecordingParticipant {
                    member_id: p.member_id.clone(),
                    consent: consent::of(p.consent).to_owned(),
                    recorded: p.recorded,
                })
                .collect(),
            recorder_present,
            trigger: trigger::of(r.trigger),
            required: r.required,
            can_record: false,
            can_stop: false,
        }
    }
}

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
///
/// Non-exhaustive (0.5.0): later releases add fields without a breaking change. Outside this
/// crate, build one with [`StateSnapshot::new`] and set the fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
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
    /// The call's id while there is one (not yet while `consent_required`).
    pub call_id: Option<String>,
    /// The call's active recording (Sew 0.7.3), as the `recording` event; absent when none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<RecordingInfo>,
    /// The call's video tracks, as the `video` event; absent when there are none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub video: Vec<VideoTrack>,
    /// This device's camera and screen, as the `local_video` event; absent while both are
    /// off with no reason.
    #[serde(default, skip_serializing_if = "LocalVideo::is_idle")]
    pub local_video: LocalVideo,
    /// The room's media policy (Sew's join answer's `MediaPolicy`, or the last
    /// `media_policy_updated`), as the `policy` event; absent with no call or from an engine
    /// without video.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<Value>,
    /// The call has video, local (this device's camera or screen on or paused) or remote (an
    /// unmuted camera or screen): a phone routes the call's audio to the speaker while it is
    /// true (the listener's `video_changed`). Absent while false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub has_video: bool,
    /// What the person may do about the recording now (plugin 0.6.1), as the
    /// `recording_controls` event; absent while both are `false`.
    #[serde(default, skip_serializing_if = "RecordingControls::is_none")]
    pub recording_controls: RecordingControls,
}

impl StateSnapshot {
    /// A snapshot of `state` and nothing else: no `target` or `room`, no participants, no call
    /// id, recording, video or policy.
    pub fn new(state: CallState) -> StateSnapshot {
        StateSnapshot {
            state,
            target: None,
            room: None,
            participants: Vec::new(),
            call_id: None,
            recording: None,
            video: Vec::new(),
            local_video: LocalVideo::default(),
            policy: None,
            has_video: false,
            recording_controls: RecordingControls::default(),
        }
    }
}

/// The plugin's events. Each window gets every event as `sew-voice://<name>`.
///
/// Non-exhaustive (0.5.0): later releases add events without a breaking change, so a `match`
/// outside this crate needs a `_` arm.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
#[non_exhaustive]
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
    /// `recording`: [`RecordingInfo`], or `null` when no recording is active.
    Recording(Option<RecordingInfo>),
    /// `security`: [`crate::details::SecurityInfo`], or `null` with no call (0.4.1).
    Security(Option<crate::details::SecurityInfo>),
    /// `roster`: `[RosterEntry]`, every media participant with its engine identity and label
    /// (0.4.1).
    Roster(Vec<crate::details::RosterEntry>),
    /// `quality`: [`crate::details::QualityInfo`] (0.4.1).
    Quality(crate::details::QualityInfo),
    /// `incoming`: [`crate::details::IncomingCall`], or `null` once it stops ringing (0.4.1).
    Incoming(Option<crate::details::IncomingCall>),
    /// `video`: `[VideoTrack]`, the whole list on any change.
    Video(Vec<VideoTrack>),
    /// `local_video`: [`LocalVideo`].
    LocalVideo(LocalVideo),
    /// `video_devices`: [`VideoDevices`], on hot-plug.
    VideoDevices(VideoDevices),
    /// `policy`: the room's new `MediaPolicy` (Sew's `media_policy_updated`).
    Policy(Value),
    /// `recording_controls`: [`RecordingControls`], when either changed (plugin 0.6.1).
    RecordingControls(RecordingControls),
}

/// The prefix of every event name the plugin emits: `sew-voice://state` and so on.
pub const EVENT_PREFIX: &str = "sew-voice://";

/// The event names, without [`EVENT_PREFIX`]: the six of voice, then the four of video.
pub const EVENT_NAMES: [&str; 10] = [
    "state",
    "participants",
    "speaking",
    "errors",
    "devices",
    "recording",
    "video",
    "local_video",
    "video_devices",
    "policy",
];

/// The recording controls' event (plugin 0.6.1), beside [`EVENT_NAMES`].
pub const RECORDING_EVENTS: [&str; 1] = ["recording_controls"];

impl PluginEvent {
    /// The event's frozen name: `state`, `participants`, `speaking`, `errors`, `devices`,
    /// `recording`, `video`, `local_video`, `video_devices` or `policy` (and
    /// [`RECORDING_EVENTS`], the details' `crate::DETAIL_EVENTS`).
    pub fn name(&self) -> &'static str {
        match self {
            PluginEvent::State(_) => "state",
            PluginEvent::Participants(_) => "participants",
            PluginEvent::Speaking(_) => "speaking",
            PluginEvent::Errors(_) => "errors",
            PluginEvent::Devices(_) => "devices",
            PluginEvent::Recording(_) => "recording",
            PluginEvent::Security(_) => "security",
            PluginEvent::Roster(_) => "roster",
            PluginEvent::Quality(_) => "quality",
            PluginEvent::Incoming(_) => "incoming",
            PluginEvent::Video(_) => "video",
            PluginEvent::LocalVideo(_) => "local_video",
            PluginEvent::VideoDevices(_) => "video_devices",
            PluginEvent::Policy(_) => "policy",
            PluginEvent::RecordingControls(_) => "recording_controls",
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
            PluginEvent::Recording(r) => serde_json::to_value(r),
            PluginEvent::Security(s) => serde_json::to_value(s),
            PluginEvent::Roster(r) => serde_json::to_value(r),
            PluginEvent::Quality(q) => serde_json::to_value(q),
            PluginEvent::Incoming(i) => serde_json::to_value(i),
            PluginEvent::Video(v) => serde_json::to_value(v),
            PluginEvent::LocalVideo(l) => serde_json::to_value(l),
            PluginEvent::VideoDevices(d) => serde_json::to_value(d),
            PluginEvent::Policy(p) => Ok(p.clone()),
            PluginEvent::RecordingControls(c) => serde_json::to_value(c),
        };
        // These types are plain structs of strings, bools and numbers; serde_json cannot fail
        // on them (a NaN level is written as null).
        v.unwrap_or(Value::Null)
    }
}

// ───────────────────────────── video ─────────────────────────────

pub use crate::video::{Tile, VideoCodec};
/// The per-device video codec setting and why "Prefer H.264" can't apply (sew-call-client's).
pub use sew_call_client::{CodecPreferenceReason, VideoCodecPreference};

/// The `video_caps` answer: what this device's video can do.
///
/// `{camera, screen: {supported, system_picker, list, windows}, publish_codecs, decode_codecs,
/// receive, self_view, live_switch, backend}`. An engine without video (and a plugin built
/// without it) answers everything off: `camera: false`, no screen, no codecs, `receive`,
/// `self_view` and `live_switch` `false`, backend `null`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VideoCaps {
    /// A camera API is present (there may still be no camera).
    pub camera: bool,
    /// Screen capture.
    pub screen: ScreenCaps,
    /// The codecs this device can send.
    pub publish_codecs: Vec<VideoCodec>,
    /// The codecs the engine can decode itself (the decoded fallback; the webview's own
    /// WebCodecs probe is separate).
    pub decode_codecs: Vec<VideoCodec>,
    /// The engine receives remote camera and screen tracks: decrypted, checked encoded frames
    /// through the sinks, for the webview's WebCodecs (the plugin's `video-core`). Missing in
    /// an older payload: `false`.
    #[serde(default)]
    pub receive: bool,
    /// The `local:camera` and `local:screen` sinks are fed this device's own camera and screen
    /// during a call (the published layers' own frames) as well as by the preview. Missing in
    /// an older payload: `false` (an older plugin feeds `local:camera` from the preview only).
    #[serde(default)]
    pub self_view: bool,
    /// `set_video_device` switches a camera that is being published now, not only the one the
    /// next `camera {on}` opens. Missing in an older payload: `false`.
    #[serde(default)]
    pub live_switch: bool,
    /// The video backend (`apple`, `windows`, `linux`, `android`, `fake`, or `null` for an
    /// engine without video).
    pub backend: String,
}

impl Default for VideoCaps {
    fn default() -> VideoCaps {
        VideoCaps {
            camera: false,
            screen: ScreenCaps::default(),
            publish_codecs: Vec::new(),
            decode_codecs: Vec::new(),
            receive: false,
            self_view: false,
            live_switch: false,
            backend: "null".into(),
        }
    }
}

/// Screen capture support (`video_caps.screen`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScreenCaps {
    /// Any screen capture (false on Linux without the portal or libpipewire, and on phones).
    pub supported: bool,
    /// `screen {on: true}` without a `source` shows the OS picker (macOS 14+, Windows, the
    /// Linux portal).
    pub system_picker: bool,
    /// `screen_sources` lists displays and windows (macOS, Windows).
    pub list: bool,
    /// Single windows, not only displays.
    pub windows: bool,
}

/// Which way a camera faces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Facing {
    /// The front (selfie) camera.
    Front,
    /// The back camera.
    Back,
    /// A plugged-in camera.
    External,
    /// The OS doesn't say (most desktop cameras).
    Unknown,
}

/// One camera.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Camera {
    /// The stable id `set_video_device` and `camera` take.
    pub id: String,
    /// The OS's name for it.
    pub name: String,
    /// Which way it faces.
    pub facing: Facing,
    /// The OS default.
    pub default: bool,
}

/// An OS permission's state: `"granted"`, `"denied"`, `"prompt"` or `"unsupported"`.
///
/// An engine maps the OS's states onto these four: restricted (MDM, parental controls) is
/// `denied`; not asked yet is `prompt`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionState {
    /// Allowed.
    Granted,
    /// Refused, by the person or by policy (the OS's restricted state too).
    Denied,
    /// Not asked yet: the OS will ask on the first `camera`, `screen` or `preview`.
    Prompt,
    /// No such capture on this OS or in this build (screen capture on Linux without the portal
    /// or libpipewire, a build or an engine without video).
    #[default]
    Unsupported,
}

/// The OS permissions video needs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VideoPermissions {
    /// The camera.
    pub camera: PermissionState,
    /// Screen recording.
    pub screen: PermissionState,
}

/// The `video_devices` answer and the `video_devices` event (hot-plug):
/// `{cameras: [{id, name, facing, default}], camera, permission: {camera, screen}}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VideoDevices {
    /// The cameras.
    pub cameras: Vec<Camera>,
    /// The camera chosen with `set_video_device` (`null`: the OS default).
    pub camera: Option<String>,
    /// The OS permissions.
    pub permission: VideoPermissions,
}

/// `set_video_device`'s arguments: `{camera}`, a camera id or `"default"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetVideoDeviceArgs {
    /// The camera's id, or `"default"`.
    pub camera: String,
}

/// `set_video_codec_preference`'s arguments: `{preference}`, `"automatic"`, `"prefer_h264"`
/// or `"prefer_vp8"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct SetVideoCodecPreferenceArgs {
    /// The setting.
    pub preference: VideoCodecPreference,
}

impl SetVideoCodecPreferenceArgs {
    /// The arguments for `preference`.
    pub fn new(preference: VideoCodecPreference) -> SetVideoCodecPreferenceArgs {
        SetVideoCodecPreferenceArgs { preference }
    }
}

/// `video_caps.codec_preference`: the per-device video codec setting and what it gives on this
/// device now.
///
/// `{setting, effective, reason?}`: `setting` is `"automatic"`, `"prefer_h264"` or
/// `"prefer_vp8"`; `effective` the codec a camera or screen turned on now publishes in
/// (`"h264"`, with a VP8 backup, or `"vp8"`); `reason`, only when "Prefer H.264" can't apply,
/// says why: `"not_detected"` (the run-time check found no usable hardware H.264 encoder),
/// `"platform"` (no run-time check on Linux, Android or Windows yet, or an iOS before 17.4,
/// which can't confirm hardware) or `"policy"` (the live call's media policy doesn't allow
/// H.264). An engine without video publish has no readback (`video_caps` leaves
/// `codec_preference` out). The setting lasts until the process ends; persisting it is the
/// host's job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct CodecPreferenceInfo {
    /// The setting.
    pub setting: VideoCodecPreference,
    /// The codec a camera or screen publishes in now.
    pub effective: VideoCodec,
    /// Why "Prefer H.264" can't apply here (written only then).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<CodecPreferenceReason>,
}

impl CodecPreferenceInfo {
    /// The readback of `setting`, publishing in `effective`, for `reason`.
    pub fn new(
        setting: VideoCodecPreference,
        effective: VideoCodec,
        reason: Option<CodecPreferenceReason>,
    ) -> CodecPreferenceInfo {
        CodecPreferenceInfo {
            setting,
            effective,
            reason,
        }
    }
}

/// The `video_caps` command's answer: [`VideoCaps`]' keys and `codec_preference` (left out by
/// an engine without video publish).
#[cfg(feature = "tauri")]
#[derive(Debug, Clone, Serialize)]
pub(crate) struct VideoCapsAnswer {
    #[serde(flatten)]
    pub(crate) caps: VideoCaps,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) codec_preference: Option<CodecPreferenceInfo>,
}

#[cfg(all(test, feature = "tauri"))]
mod answer_tests {
    use super::*;

    /// `video_caps`' answer: the caps' keys, then `codec_preference` (without `reason` when the
    /// preference applies); none without video publish.
    #[test]
    fn the_video_caps_answer_carries_the_codec_preference() {
        let answer = |p| VideoCapsAnswer {
            caps: VideoCaps::default(),
            codec_preference: p,
        };
        let v = serde_json::to_value(answer(Some(CodecPreferenceInfo::new(
            VideoCodecPreference::PreferH264,
            VideoCodec::Vp8,
            Some(CodecPreferenceReason::Platform),
        ))))
        .unwrap_or_default();
        assert_eq!(v["backend"], "null", "{v}");
        assert_eq!(
            v["codec_preference"],
            serde_json::json!({"setting": "prefer_h264", "effective": "vp8", "reason": "platform"})
        );
        let v = serde_json::to_value(answer(Some(CodecPreferenceInfo::new(
            VideoCodecPreference::Automatic,
            VideoCodec::H264,
            None,
        ))))
        .unwrap_or_default();
        assert_eq!(
            v["codec_preference"],
            serde_json::json!({"setting": "automatic", "effective": "h264"})
        );
        let v = serde_json::to_value(answer(None)).unwrap_or_default();
        assert!(v.get("codec_preference").is_none(), "{v}");
    }
}

/// `camera`'s arguments: `{on, device?}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CameraArgs {
    /// Publish the camera, or stop it.
    pub on: bool,
    /// A camera id or `"default"` (absent: the chosen one).
    #[serde(default)]
    pub device: Option<String>,
}

/// `screen`'s arguments: `{on, source?}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScreenArgs {
    /// Share, or stop sharing.
    pub on: bool,
    /// A `screen_sources` id (absent: the OS picker).
    #[serde(default)]
    pub source: Option<String>,
}

/// `preview`'s arguments: `{on, device?}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewArgs {
    /// Open the preview, or close it.
    pub on: bool,
    /// A camera id or `"default"` (absent: the chosen one).
    #[serde(default)]
    pub device: Option<String>,
}

/// `preview`'s answer: `{sink_track: "local:camera"}`, the track to open a sink on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewAnswer {
    /// The track the preview's frames come on.
    pub sink_track: String,
}

/// Whether a display or a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenKind {
    /// A whole display.
    Display,
    /// One window.
    Window,
}

/// One entry of `screen_sources`: `{id, kind, title, app?, width, height, thumbnail_png?}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScreenSource {
    /// The id `screen {on: true, source}` takes.
    pub id: String,
    /// Display or window.
    pub kind: ScreenKind,
    /// The display's name or the window's title.
    pub title: String,
    /// The window's app, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// A small preview: a PNG at most 320 px wide, base64 (standard alphabet, padded, no
    /// `data:` prefix; prefix `data:image/png;base64,` for an `<img>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail_png: Option<String>,
}

/// Which source a video track carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoSource {
    /// A camera.
    Camera,
    /// A screen share.
    Screen,
}

/// Why a track's video is paused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoPaused {
    /// The media server paused it: this device's downlink is congested.
    Congestion,
    /// The sender is in the background (a phone).
    Background,
    /// The room's policy paused it.
    Policy,
}

/// One entry of the `video` event: a video track in the call.
///
/// `{member_id, is_self, source, track, codec, width, height, muted, paused, unsupported?}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VideoTrack {
    /// The sender (Sew's member id).
    pub member_id: String,
    /// This device's own track (self-view uses `local:camera` / `local:screen`, not `track`).
    pub is_self: bool,
    /// Camera or screen.
    pub source: VideoSource,
    /// The `track_sid` to open a sink on.
    pub track: String,
    /// The track's codec.
    pub codec: VideoCodec,
    /// The sender's largest layer, in pixels.
    pub width: u32,
    /// The sender's largest layer, in pixels.
    pub height: u32,
    /// The sender turned the video off for now (the track stays).
    pub muted: bool,
    /// Why no frames come, or `null`.
    pub paused: Option<VideoPaused>,
    /// This device can't show the track (written only when `true`).
    #[serde(default, skip_serializing_if = "is_false")]
    pub unsupported: bool,
}

/// One of this device's own video sources (`local_video.camera`, `.screen`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalVideoState {
    /// Not sending.
    #[default]
    Off,
    /// Asking the OS permission, the lease or the device.
    Asking,
    /// Sending.
    On,
    /// Published but paused (background, thermal).
    Paused,
}

/// The `local_video` event: `{camera, screen, reason?}`.
///
/// `reason` says why the last change happened: a `camera`/`screen` reject code, `background`,
/// `thermal`, `user_stopped`, `source_closed` or `video_disabled`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalVideo {
    /// The camera.
    pub camera: LocalVideoState,
    /// The screen share.
    pub screen: LocalVideoState,
    /// Why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl LocalVideo {
    /// Both off, no reason: what a voice-only call has (the `state` command leaves it out).
    pub fn is_idle(&self) -> bool {
        *self == LocalVideo::default()
    }
}

/// The webview decoder's counters of one sink (`video_sink_update.stats`), **cumulative since
/// the sink opened** (each update replaces the last; the plugin sums a track's sinks).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SinkStats {
    /// Frames decoded.
    pub decoded: u64,
    /// Frames dropped (decoder behind, decode errors).
    pub dropped: u64,
    /// Freezes seen.
    pub freezes: u64,
    /// Time frozen, in ms.
    pub freeze_ms: u64,
}

/// `video_sink`'s arguments (without the `channel`, which is Tauri's):
/// `{track, width, height, visible, tile, decode}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VideoSinkArgs {
    /// A remote `track_sid`, `"local:camera"` or `"local:screen"`.
    pub track: String,
    /// The element's width in device pixels.
    pub width: u32,
    /// The element's height in device pixels.
    pub height: u32,
    /// The element is on screen.
    pub visible: bool,
    /// `large` (30 fps) or `grid` (15 fps).
    pub tile: Tile,
    /// The webview's WebCodecs probe: `["vp8", "h264"]`, or fewer (unknown names are ignored).
    pub decode: Vec<String>,
}

/// How a sink's frames come.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SinkMode {
    /// Encoded frames (kind 1), decoded by the webview's WebCodecs.
    Encoded,
    /// Decoded frames (kinds 2 and 3): the fallback for a webview that can't decode the
    /// track's codec. The engine decodes; frames are I420 (kind 2), or RGBA (kind 3) for a
    /// webview without WebCodecs (an empty `decode`), scaled to fit the sink, at most 640x360
    /// and 15 fps.
    Decoded,
}

/// `video_sink`'s answer: `{sink_id, codec, mode}`. `codec` is the track's (`null` for a
/// self-view sink before the camera or preview runs; every frame's header names it too).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VideoSinkAnswer {
    /// The sink, in every frame header and the later sink commands.
    pub sink_id: u32,
    /// The track's codec (`null` in `decoded` mode, and for a self-view sink before its first
    /// frame).
    pub codec: Option<VideoCodec>,
    /// `encoded`, or `decoded` (the engine decodes; see [`SinkMode::Decoded`]).
    pub mode: SinkMode,
}

/// `video_sink_update`'s arguments: `{sink_id, width?, height?, visible?, tile?, stats?}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VideoSinkUpdateArgs {
    /// The sink.
    pub sink_id: u32,
    /// The new width in device pixels.
    #[serde(default)]
    pub width: Option<u32>,
    /// The new height in device pixels.
    #[serde(default)]
    pub height: Option<u32>,
    /// On screen or not.
    #[serde(default)]
    pub visible: Option<bool>,
    /// The tile class.
    #[serde(default)]
    pub tile: Option<Tile>,
    /// The decoder's counters.
    #[serde(default)]
    pub stats: Option<SinkStats>,
}

/// `video_sink_close` and `request_keyframe`'s arguments: `{sink_id}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SinkIdArgs {
    /// The sink.
    pub sink_id: u32,
}

/// The video command and event codes (each command rejects `{code, detail}`).
pub mod video_code {
    /// The engine has no video (built without it, or an engine that doesn't do video).
    pub const VIDEO_UNSUPPORTED: &str = "video_unsupported";
    /// `video_sink` on a track the call doesn't have.
    pub const UNKNOWN_TRACK: &str = "unknown_track";
    /// A sink command for a sink that isn't open.
    pub const UNKNOWN_SINK: &str = "unknown_sink";
    /// `video_sink`: the webview can't decode the track's codec and the engine can't either
    /// (`detail.codec`): no decoded fallback for it.
    pub const DECODED_UNSUPPORTED: &str = "decoded_unsupported";
    /// `video_sink`: the sink would need the decoded fallback and this device already decodes
    /// its most tiles (`detail.max`, 4). The tile shows its participant's avatar and "video
    /// paused (this device)"; opening it again once another decoded sink closes may succeed.
    pub const DECODED_TILES_FULL: &str = "decoded_tiles_full";
    /// The room's policy allows no video, or no camera for this member.
    pub const VIDEO_NOT_ALLOWED: &str = "video_not_allowed";
    /// Sew turned video off for the room (`policy_denied video_disabled`).
    pub const VIDEO_DISABLED: &str = "video_disabled";
    /// The OS camera permission is denied or restricted.
    pub const CAMERA_DENIED: &str = "camera_denied";
    /// `camera {on}`: the OS camera permission is denied or restricted. The plugin never
    /// prompts on its own: the person changes it in the OS settings.
    pub const CAMERA_PERMISSION_DENIED: &str = "camera_permission_denied";
    /// `camera {on}`: the person was never asked. The plugin never prompts on its own: the
    /// app's camera button calls `request_camera_permission` first (iOS shows the OS prompt;
    /// on Android the app asks for CAMERA itself), then `camera {on}`.
    pub const CAMERA_PERMISSION_NOT_DETERMINED: &str = "camera_permission_not_determined";
    /// Another app holds the camera.
    pub const CAMERA_BUSY: &str = "camera_busy";
    /// No such camera.
    pub const CAMERA_NOT_FOUND: &str = "camera_not_found";
    /// `camera {on}`: the camera opened but gave no frame within 2.5 s (a MacBook's built-in
    /// camera with its lid closed, a camera another app froze): nothing was published, the
    /// camera closed.
    pub const NO_FRAMES: &str = "no_frames";
    /// Every camera slot of the room is taken.
    pub const VIDEO_SLOTS_FULL: &str = "video_slots_full";
    /// The media server is busy (`detail.retry_after`).
    pub const MEDIA_BUSY: &str = "media_busy";
    /// The media server is at capacity.
    pub const MEDIA_CAPACITY: &str = "media_capacity";
    /// No encoder for a codec the room allows.
    pub const CODEC_UNAVAILABLE: &str = "codec_unavailable";
    /// The OS screen-recording permission is denied (macOS: System Settings, Privacy).
    pub const SCREEN_DENIED: &str = "screen_denied";
    /// Every screen-share slot of the room is taken.
    pub const SCREEN_SLOTS_FULL: &str = "screen_slots_full";
    /// The person cancelled the OS picker.
    pub const PICKER_CANCELLED: &str = "picker_cancelled";
    /// No screen capture here (Linux without the portal or libpipewire, phones).
    pub const SCREEN_UNSUPPORTED: &str = "screen_unsupported";
    /// `errors`: a track stayed broken 5 s after a full keyframe request.
    pub const VIDEO_STALLED: &str = "video_stalled";
    /// `errors`: a video encoder failed and couldn't be reopened.
    pub const ENCODER_LOST: &str = "encoder_lost";
    /// `errors`: the camera went away while sending.
    pub const CAMERA_LOST: &str = "camera_lost";
    /// `screen {on: true, source}`: the listed display or window is gone (or was never listed).
    pub const SCREEN_NOT_FOUND: &str = "screen_not_found";
    /// The camera or screen failed in a way none of the other codes says (`detail.error`: the
    /// video backend's own code).
    pub const CAPTURE_FAILED: &str = "capture_failed";
}

/// The `local_video.reason` words that are not reject codes (with them: `camera_denied` /
/// `screen_denied` when the OS permission was revoked while sending, `camera_lost`,
/// `camera_busy` when another app took the camera).
pub mod video_reason {
    /// A shared window is minimised or hidden: frames stop, the share stays (`paused`).
    pub const HIDDEN: &str = "hidden";
    /// The app is in the background (phones; `paused`).
    pub const BACKGROUND: &str = "background";
    /// Thermal pressure stopped the camera (`paused`).
    pub const THERMAL: &str = "thermal";
    /// The camera says it is suspended (`paused`): a MacBook's built-in camera with its lid
    /// closed, a shut lens cover; `on` again when it comes back.
    pub const SUSPENDED: &str = "suspended";
    /// The OS paused the source for a reason it added later (`paused`).
    pub const PAUSED: &str = "paused";
    /// The person stopped the share from the OS's own UI (`off`).
    pub const USER_STOPPED: &str = "user_stopped";
    /// The shared window closed, or the shared display went away (`off`).
    pub const SOURCE_CLOSED: &str = "source_closed";
    /// The room's policy turned video off (`off`).
    pub const VIDEO_DISABLED: &str = "video_disabled";
}
