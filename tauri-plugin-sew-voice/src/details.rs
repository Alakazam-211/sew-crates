//! Call details (0.4.1), beside the frozen events: the call's key state and security code, each
//! media participant's engine identity and label, the connection quality, and an incoming DM
//! call (a ring). Each is an event (`sew-voice://security`, `roster`, `quality`, `incoming`) and
//! part of the `call_details` command's answer; a ring is set and cleared with the `ring` and
//! `ring_ended` commands (or [`crate::VoiceControl::ring`]).
//!
//! These commands are not in `sew-voice:default`: a window gets them with
//! `sew-voice:call-details` (or one `allow-*` each). No type here carries a token, a key or a
//! display name.

use serde::{Deserialize, Serialize};

/// The commands, as the webview invokes them after `plugin:sew-voice|`: `call_details` (the
/// four below for a late window), `ring {call_id, target, room, started_by?, silent?, reason?}`
/// and `ring_ended {call_id}`.
pub const DETAIL_COMMANDS: [&str; 3] = ["call_details", "ring", "ring_ended"];

/// The events, without `sew-voice://`.
pub const DETAIL_EVENTS: [&str; 4] = ["security", "roster", "quality", "incoming"];

/// The `security` event (and `call_details`' `security`): the call's end-to-end key state and
/// the active epoch's security code, or `null` with no call.
///
/// `{"state": "active", "epoch": 4, "since_ms": 1730851200000, "code": "01234 56789 …"}`. `state`
/// is `preparing`, `joining`, `active`, `securing` (show "Securing…" only `show_after_ms` after
/// `since_ms`), `recovering`, `device_not_enrolled`, `server_identity_changed`, `unsupported`,
/// `failed` or `left`; `reason` says why for the last five that have one. `code` is six groups
/// of five digits, the same on every device in the epoch, written only while an epoch is active
/// and Sew's policy shows it. It changes with every epoch: compare it by voice, never store it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecurityInfo {
    /// The key state.
    pub state: String,
    /// The active epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<u64>,
    /// Since when (the key session's clock, ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<u64>,
    /// With `securing`: how long after `since_ms` to show it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_after_ms: Option<u64>,
    /// Why (`recovering`, `unsupported`, `failed`, `left`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The security code (`"01234 56789 01234 56789 01234 56789"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl From<&sew_call_client::CallSecurity> for SecurityInfo {
    fn from(s: &sew_call_client::CallSecurity) -> SecurityInfo {
        SecurityInfo {
            state: s.state.clone(),
            epoch: s.epoch,
            since_ms: s.since_ms,
            show_after_ms: s.show_after_ms,
            reason: s.reason.clone(),
            code: s.code.clone(),
        }
    }
}

/// How a media participant is labelled ([`RosterEntry::label`]).
pub mod label {
    /// A member with a leaf in the call's key group: their media decrypts.
    pub const MEMBER: &str = "member";
    /// The recorder (Sew Notes), which has no member.
    pub const RECORDER: &str = "recorder";
    /// In the media room with no key: "not in the encrypted session". Nothing of theirs plays.
    pub const NOT_IN_SESSION: &str = "not_in_session";
}

/// One media participant in the `roster` event: everyone the media room has, this device and
/// the recorder included (`participants` lists members only).
///
/// `{"identity": "p_…", "member_id": "48831765147549700", "is_self": false, "label": "member"}`.
/// `identity` is the engine identity of one join (a re-join gets a new one); `member_id` is
/// written once the keys name the member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RosterEntry {
    /// The engine identity (`p_…`).
    pub identity: String,
    /// Sew's member id, once the call's keys bind the identity to a member.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_id: Option<String>,
    /// This device.
    pub is_self: bool,
    /// [`label`]: `member`, `recorder` or `not_in_session`.
    pub label: String,
}

/// The engine's participants as the `roster` event: this device first, as the engine lists them.
pub fn roster_of(p: &[sew_call_client::Participant]) -> Vec<RosterEntry> {
    p.iter()
        .map(|p| RosterEntry {
            identity: p.identity.clone(),
            member_id: p.member_id.clone(),
            is_self: p.is_self,
            label: if p.recorder {
                label::RECORDER
            } else if p.member_id.is_some() {
                label::MEMBER
            } else {
                label::NOT_IN_SESSION
            }
            .to_owned(),
        })
        .collect()
}

/// The connection quality levels ([`QualityInfo::quality`]).
pub mod quality {
    /// Little loss, low delay.
    pub const EXCELLENT: &str = "excellent";
    /// Some loss or delay; speech is fine.
    pub const GOOD: &str = "good";
    /// Enough loss, concealment or delay to hear it.
    pub const POOR: &str = "poor";
    /// Nothing arrives: the call is reconnecting, or a window received none of the expected
    /// audio.
    pub const LOST: &str = "lost";
    /// Not measured yet (no call, connecting, or the first window has not ended).
    pub const UNKNOWN: &str = "unknown";
}

/// Packet loss at or above this is `good` at best (2 %).
pub const QUALITY_GOOD_LOSS: f64 = 0.02;
/// Packet loss at or above this is `poor` (8 %).
pub const QUALITY_POOR_LOSS: f64 = 0.08;
/// Concealed audio at or above this share of the played audio is `good` at best (3 %).
pub const QUALITY_GOOD_CONCEALED: f64 = 0.03;
/// Concealed audio at or above this share is `poor` (10 %).
pub const QUALITY_POOR_CONCEALED: f64 = 0.10;
/// A round trip at or above this is `good` at best (ms).
pub const QUALITY_GOOD_RTT_MS: u32 = 200;
/// A round trip at or above this is `poor` (ms).
pub const QUALITY_POOR_RTT_MS: u32 = 400;
/// Jitter at or above this is `good` at best (ms).
pub const QUALITY_GOOD_JITTER_MS: u32 = 30;
/// Jitter at or above this is `poor` (ms).
pub const QUALITY_POOR_JITTER_MS: u32 = 80;

/// The `quality` event (and `call_details`' `quality`): this device's connection quality, from
/// the engine's last stats window (every 15 s while connected), `lost` while reconnecting.
///
/// `{"quality": "good", "packet_loss": 0.031, "concealed": 0.012, "jitter_ms": 14, "rtt_ms":
/// 85}`; the numbers are the window's, written when measured (`packet_loss` and `concealed`
/// are shares, 0 to 1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualityInfo {
    /// [`quality`]: `excellent`, `good`, `poor`, `lost` or `unknown`.
    pub quality: String,
    /// Received audio packets lost, of those expected (0 to 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub packet_loss: Option<f64>,
    /// Played audio that was concealed (0 to 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concealed: Option<f64>,
    /// The worst received stream's jitter (ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jitter_ms: Option<u32>,
    /// The media path's round trip (ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<u32>,
}

impl Default for QualityInfo {
    fn default() -> QualityInfo {
        QualityInfo::level(quality::UNKNOWN)
    }
}

impl QualityInfo {
    /// A level with no numbers.
    pub fn level(q: &str) -> QualityInfo {
        QualityInfo {
            quality: q.to_owned(),
            packet_loss: None,
            concealed: None,
            jitter_ms: None,
            rtt_ms: None,
        }
    }

    /// `unknown`.
    pub fn unknown() -> QualityInfo {
        QualityInfo::level(quality::UNKNOWN)
    }

    /// `lost` (reconnecting).
    pub fn lost() -> QualityInfo {
        QualityInfo::level(quality::LOST)
    }

    /// One window's audio counts (summed over the received streams) and the path's round trip.
    ///
    /// - `lost`: audio was expected and none arrived;
    /// - `poor`: loss ≥ 8 %, concealment ≥ 10 %, jitter ≥ 80 ms or a round trip ≥ 400 ms;
    /// - `good`: loss ≥ 2 %, concealment ≥ 3 %, jitter ≥ 30 ms or a round trip ≥ 200 ms;
    /// - `excellent`: otherwise;
    /// - `unknown`: nothing measured (alone in the call, before the path's round trip is known).
    #[allow(clippy::too_many_arguments)]
    pub fn from_counts(
        packets_expected: u32,
        packets_received: u32,
        packets_lost: u32,
        played_ms: u32,
        concealed_ms: u32,
        jitter_ms: u32,
        rtt_ms: Option<u32>,
    ) -> QualityInfo {
        let heard = packets_expected > 0;
        let packet_loss =
            heard.then(|| (f64::from(packets_lost) / f64::from(packets_expected)).clamp(0.0, 1.0));
        let concealed = (played_ms > 0)
            .then(|| (f64::from(concealed_ms) / f64::from(played_ms)).clamp(0.0, 1.0));
        let jitter = heard.then_some(jitter_ms);
        let q = if heard && packets_received == 0 {
            quality::LOST
        } else if !heard && rtt_ms.is_none() {
            quality::UNKNOWN
        } else {
            let at_least = |loss: f64, conc: f64, jit: u32, rtt: u32| {
                packet_loss.is_some_and(|l| l >= loss)
                    || concealed.is_some_and(|c| c >= conc)
                    || jitter.is_some_and(|j| j >= jit)
                    || rtt_ms.is_some_and(|r| r >= rtt)
            };
            if at_least(
                QUALITY_POOR_LOSS,
                QUALITY_POOR_CONCEALED,
                QUALITY_POOR_JITTER_MS,
                QUALITY_POOR_RTT_MS,
            ) {
                quality::POOR
            } else if at_least(
                QUALITY_GOOD_LOSS,
                QUALITY_GOOD_CONCEALED,
                QUALITY_GOOD_JITTER_MS,
                QUALITY_GOOD_RTT_MS,
            ) {
                quality::GOOD
            } else {
                quality::EXCELLENT
            }
        };
        QualityInfo {
            quality: q.to_owned(),
            packet_loss,
            concealed,
            jitter_ms: jitter,
            rtt_ms,
        }
    }

    /// The engine's stats window ([`QualityInfo::from_counts`] on its numbers).
    pub fn from_window(w: &sew_call_client::StatsWindow) -> QualityInfo {
        QualityInfo::from_counts(
            w.audio_packets_expected,
            w.audio_packets_received,
            w.audio_packets_lost,
            w.audio_played_ms,
            w.audio_concealed_ms,
            w.audio_jitter_ms,
            w.rtt_ms,
        )
    }
}

/// An incoming DM call (a ring): the `ring` command's arguments, the `incoming` event (or
/// `null` once it stops ringing) and `call_details`' `incoming`.
///
/// `{"call_id": "48831765147549710", "target": "dm", "room": "48831765147549702",
/// "started_by": "48831765147549700", "silent": false}`. `target` and `room` are what a `join`
/// that accepts it sends (the host's meaning, as for `join`); a `join` with the same `target`
/// and `room` accepts it and clears it. The page sets it from Sew's `call_ring` (or the host
/// from a push, [`crate::VoiceControl::ring`]) and clears it with `ring_ended` on the ring's end
/// or a decline (the decline itself is the page's or the host's request to Sew).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IncomingCall {
    /// The DM call (Sew's id, a decimal string).
    pub call_id: String,
    /// The `join` target that accepts it.
    pub target: String,
    /// The `join` room that accepts it (the DM channel, usually).
    pub room: String,
    /// Who is calling (Sew's member id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_by: Option<String>,
    /// Ring without a sound (Sew's `silent`: do-not-disturb, a muted channel).
    #[serde(default)]
    pub silent: bool,
    /// Sew's ring reason, when it gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `ring_ended`'s arguments: `{call_id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RingEndedArgs {
    /// The DM call that stopped ringing (another call's id changes nothing).
    pub call_id: String,
}

/// The `call_details` command's answer, for a window opened mid-call: `{security, quality,
/// roster, incoming}` (`security` and `incoming` `null` when there is none).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallDetails {
    /// As the `security` event.
    pub security: Option<SecurityInfo>,
    /// As the `quality` event.
    pub quality: QualityInfo,
    /// As the `roster` event (`[]` with no call).
    pub roster: Vec<RosterEntry>,
    /// As the `incoming` event.
    pub incoming: Option<IncomingCall>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine_participant(
        identity: &str,
        member: Option<&str>,
        is_self: bool,
        recorder: bool,
    ) -> sew_call_client::Participant {
        let mut p = sew_call_client::Participant::default();
        p.identity = identity.into();
        p.member_id = member.map(Into::into);
        p.is_self = is_self;
        p.recorder = recorder;
        p
    }

    #[test]
    fn the_roster_labels_members_the_recorder_and_the_unkeyed() {
        let r = roster_of(&[
            engine_participant("p_me", Some("1"), true, false),
            engine_participant("p_a", Some("2"), false, false),
            engine_participant("p_b", None, false, false),
            engine_participant("p_rec", None, false, true),
        ]);
        let got: Vec<(&str, Option<&str>, bool, &str)> = r
            .iter()
            .map(|e| {
                (
                    e.identity.as_str(),
                    e.member_id.as_deref(),
                    e.is_self,
                    e.label.as_str(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("p_me", Some("1"), true, label::MEMBER),
                ("p_a", Some("2"), false, label::MEMBER),
                ("p_b", None, false, label::NOT_IN_SESSION),
                ("p_rec", None, false, label::RECORDER),
            ]
        );
    }

    #[test]
    fn quality_levels_from_a_windows_counts() {
        let q = |exp, rcv, lost, played, conc, jit, rtt| {
            QualityInfo::from_counts(exp, rcv, lost, played, conc, jit, rtt).quality
        };
        assert_eq!(q(1000, 1000, 0, 10_000, 0, 5, Some(40)), quality::EXCELLENT);
        assert_eq!(q(1000, 980, 20, 10_000, 0, 5, Some(40)), quality::GOOD);
        assert_eq!(q(1000, 1000, 0, 10_000, 300, 5, Some(40)), quality::GOOD);
        assert_eq!(q(1000, 1000, 0, 10_000, 0, 30, Some(40)), quality::GOOD);
        assert_eq!(q(1000, 1000, 0, 10_000, 0, 5, Some(200)), quality::GOOD);
        assert_eq!(q(1000, 920, 80, 10_000, 0, 5, Some(40)), quality::POOR);
        assert_eq!(q(1000, 1000, 0, 10_000, 1_000, 5, Some(40)), quality::POOR);
        assert_eq!(q(1000, 1000, 0, 10_000, 0, 80, Some(40)), quality::POOR);
        assert_eq!(q(1000, 1000, 0, 10_000, 0, 5, Some(400)), quality::POOR);
        // Expected audio and none arrived.
        assert_eq!(q(1000, 0, 1000, 10_000, 10_000, 0, Some(40)), quality::LOST);
        // Alone in the call: the round trip alone, or nothing.
        assert_eq!(q(0, 0, 0, 0, 0, 0, Some(40)), quality::EXCELLENT);
        assert_eq!(q(0, 0, 0, 0, 0, 0, Some(450)), quality::POOR);
        assert_eq!(q(0, 0, 0, 0, 0, 0, None), quality::UNKNOWN);
        // The numbers: shares and what was measured.
        let i = QualityInfo::from_counts(1000, 970, 30, 10_000, 100, 12, Some(90));
        assert_eq!(i.packet_loss, Some(0.03));
        assert_eq!(i.concealed, Some(0.01));
        assert_eq!((i.jitter_ms, i.rtt_ms), (Some(12), Some(90)));
        let alone = QualityInfo::from_counts(0, 0, 0, 0, 0, 0, Some(40));
        assert_eq!(
            (alone.packet_loss, alone.concealed, alone.jitter_ms),
            (None, None, None)
        );
    }

    #[test]
    fn the_engines_security_is_the_events() {
        let s = sew_call_client::CallSecurity::active(4, Some("01234 56789".into()));
        let w = SecurityInfo::from(&s);
        assert_eq!(
            (w.state.as_str(), w.epoch, w.code.as_deref()),
            ("active", Some(4), Some("01234 56789"))
        );
    }
}
