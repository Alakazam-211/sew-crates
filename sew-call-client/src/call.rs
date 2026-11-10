//! The public engine API: [`CallClient`] (one per app) and [`Call`] (one live
//! call), their events, and the frame API the audio pipeline (kessel-audio, or a host's own)
//! drives.
//!
//! Recording (Sew 0.7.3): the call's recording as Sew says it is
//! [`Call::recording`] and [`CallEvent::Recording`]; the person answers its notice with
//! [`Call::consent`] and withdraws with [`Call::withdraw_consent`] ("Stop recording me"), both
//! through the host ([`JoinProvider::consent`]). A join into a recorded room that has no answer
//! is held at [`CallState::ConsentRequired`] (no media, no keys, nothing captured) until the
//! host answers or leaves. No answer is never consent: this device's key reaches the recorder
//! only after Sew says `granted`.
//!
//! Record and Stop (Sew 0.9.0): [`Call::start_recording`] and [`Call::stop_recording`], the
//! person's own acts, also through the host ([`JoinProvider::recording`]). The recording says
//! what started it ([`CallRecording::trigger`]) and whether the room requires it
//! ([`CallRecording::required`]).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bytes::Bytes;
use serde::Serialize;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::config::ClientConfig;
use crate::e2ee::E2eeProvider;
use crate::engine;
use crate::provider::{
    ConsentRequired, JoinProvider, JoinRequest, ProviderError, RecordingControl,
};
use crate::stats::{CallStatsReport, PlayoutStats, StatsWindow};
use sew_proto::recording::RecordingTrigger;

/// Frames the send queue holds (20 ms each: one second).
pub(crate) const SEND_QUEUE: usize = 50;
/// Items the receive queue holds before it drops (and counts) new frames.
pub(crate) const RECV_QUEUE: usize = 1024;
/// Items the video receive queue holds before it drops (and counts) new frames.
pub(crate) const VIDEO_RECV_QUEUE: usize = 256;

/// Why a call ended (`ended.reason`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum EndReason {
    /// The host left.
    Left,
    /// The host has no Sew session.
    SigninRequired,
    /// The network or the engine is gone for good (or relay-only had no `turns:`).
    Network,
    /// The keys refused the call.
    E2eeRefused,
    /// Sew no longer accepts the call credential (another device took over: `replaced`, or a
    /// 4011 from a Sew older than 0.7.2 with no engine `Disconnect` before it).
    CredentialLost,
    /// The call credential expired: Sew refused its renewal as `expired`, or no renewal got
    /// through (the network, a `5xx`) before it ran out (`credential_expired`).
    CredentialExpired,
    /// The engine removed this participant and Sew gave no reason within 1.5 s (`kicked`).
    Kicked,
    /// The engine closed or deleted the room and Sew gave no reason within 1.5 s
    /// (`room_closed`).
    RoomClosed,
    /// Sew ended this join and said why (Sew 0.7.2): its voice leave reason as it arrived
    /// (`perms_lost`, `removed_from_space`, `timed_out`, `channel_archived`,
    /// `account_removed`, `removed_by_moderator`, `session_revoked`, `device_revoked`, …;
    /// an open set). From the call socket's self copy of `voice_state_updated` or its `4011`
    /// close; the key group's removal only when the call socket said nothing within 1.5 s of
    /// it (it names `disconnected` for a reason the key protocol has no code for).
    Removed(String),
    /// Sew refused the (re-)join; the code is in the `errors` event.
    Refused(String),
}

/// The call's state (`state` event).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
#[non_exhaustive]
pub enum CallState {
    /// Getting media and keys up.
    Connecting,
    /// Media is up **and** a key epoch is active: you can hear and be heard.
    Connected,
    /// Resuming or reconnecting (attempt counts from 1).
    Reconnecting {
        /// The attempt.
        attempt: u32,
    },
    /// Held (Sew 0.7.3): the room is recorded and Sew refused the join until the person
    /// answers its notice (`409 recording_consent_required`). No media, no keys, nothing is
    /// captured or sent. The notice is [`Call::recording`]'s `consent_prompt`; answer with
    /// [`Call::consent`] (the engine re-joins with the answer) or give up with
    /// [`Call::leave`]. Waits as long as the host does (a lock-screen answer may take a while).
    ConsentRequired {
        /// The recording (Sew's id, a decimal string), or `auto` for an auto-record channel's
        /// first join.
        recording_id: String,
        /// The notice's version: what [`Call::consent`] echoes.
        notice_version: u32,
    },
    /// Over.
    Ended {
        /// Why.
        reason: EndReason,
    },
}

/// One participant (`participants` event). Names are the host's.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct Participant {
    /// The engine identity (`p_…`, one per join).
    pub identity: String,
    /// The Sew member, once the keys' roster or the call socket named it.
    pub member_id: Option<String>,
    /// This device.
    pub is_self: bool,
    /// Muted (self or by a moderator, the track is muted, or SPEAK denied).
    pub muted: bool,
    /// Deafened.
    pub deafened: bool,
    /// Muted by a moderator.
    pub server_muted: bool,
    /// Deafened by a moderator.
    pub server_deafened: bool,
    /// The member lacks SPEAK here (Sew's `VoiceState.speak_denied`; for this device also the
    /// engine's permission): nothing is captured or published until it is granted again. Not a
    /// moderator mute (`server_muted`).
    pub speak_denied: bool,
    /// The recorder (Sew Notes): the engine's recorder participant, which has no member.
    pub recorder: bool,
    /// The participant's camera track, if it publishes one (`video-core` builds).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub camera: Option<crate::video::VideoTrackView>,
    /// The participant's screen-share track, if it publishes one (`video-core` builds).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen: Option<crate::video::VideoTrackView>,
}

/// A person's answer to a recording's notice, as Sew stores it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RecordingConsent {
    /// Not answered (never recorded).
    Pending,
    /// Agreed: recorded while captured.
    Granted,
    /// Declined (unrecorded, or not joined when declining means not joining).
    Declined,
    /// Agreed, then withdrew ("Stop recording me").
    Withdrawn,
}

impl RecordingConsent {
    /// From Sew's consent (`None`: not a participant yet, so `pending`; a value this engine
    /// does not know is not a grant, so `declined`).
    pub fn from_sew(c: Option<sew_proto::recording::Consent>) -> RecordingConsent {
        use sew_proto::recording::Consent as C;
        match c {
            None | Some(C::Pending) => RecordingConsent::Pending,
            Some(C::Granted) => RecordingConsent::Granted,
            Some(C::Withdrawn) => RecordingConsent::Withdrawn,
            Some(_) => RecordingConsent::Declined,
        }
    }
}

/// One participant of the recording, as everyone in the call sees it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct RecordingMember {
    /// The member (Sew's id, a decimal string).
    pub member_id: String,
    /// Their answer.
    pub consent: RecordingConsent,
    /// The recorder captures them now (granted and captured).
    pub recorded: bool,
}

/// The call's recording as Sew says it (`recording` event, [`Call::recording`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct CallRecording {
    /// The recording (Sew's id, a decimal string), or `auto` while a join into an auto-record
    /// channel waits for an answer.
    pub recording_id: String,
    /// Started by an auto-record channel.
    pub auto: bool,
    /// Who started it (Sew's member id), when known.
    pub started_by: Option<String>,
    /// This person's answer.
    pub my_consent: RecordingConsent,
    /// The notice to show while this person has not answered (`pending`), with its
    /// `notice_version`; `None` once answered.
    pub consent_prompt: Option<sew_proto::recording::ConsentPrompt>,
    /// Everyone in it.
    pub participants: Vec<RecordingMember>,
    /// What started it (Sew 0.9.0): a person pressing Record (`manual`), an auto-record
    /// room's first join (`auto`), or the room reaching two people while it requires recording
    /// (`required`). From `auto` against an older Sew.
    pub trigger: RecordingTrigger,
    /// The room requires its recording (Sew 0.9.0): it records every call with two or more
    /// people in it, so Sew refuses a Stop (`recording_required`) while two or more are in the
    /// room. "Stop recording me" ([`Call::withdraw_consent`]) always works. True when Sew
    /// started it under that rule (`trigger: required`) or its notice says so
    /// (`consent_prompt.required`).
    pub required: bool,
}

/// What started `r`: Sew's `trigger`. An older Sew sends none (read as `manual`), so an
/// automatic start there shows only in `auto`: `auto` with `manual` reads as `auto` (Sew 0.9.0
/// never sends that pair: a required start has `auto` and `required`).
fn trigger_of(r: &sew_proto::recording::Recording) -> RecordingTrigger {
    if r.auto && r.trigger == RecordingTrigger::Manual {
        RecordingTrigger::Auto
    } else {
        r.trigger
    }
}

/// The notice says the room requires recording (its `required` slot).
fn prompt_required(p: &sew_proto::recording::ConsentPrompt) -> bool {
    p.required
}

impl CallRecording {
    /// The recording in Sew's shape (the member's own view).
    pub fn from_sew(r: &sew_proto::recording::Recording) -> CallRecording {
        use sew_proto::recording::Capture;
        let trigger = trigger_of(r);
        CallRecording {
            recording_id: r.id.to_string(),
            auto: r.auto,
            started_by: Some(r.started_by.to_string()),
            my_consent: RecordingConsent::from_sew(r.my_consent),
            consent_prompt: r.consent_prompt.clone(),
            participants: r
                .participants
                .iter()
                .map(|p| RecordingMember {
                    member_id: p.member_id.to_string(),
                    consent: RecordingConsent::from_sew(Some(p.consent)),
                    recorded: p.capture == Some(Capture::Recorded),
                })
                .collect(),
            trigger,
            required: trigger == RecordingTrigger::Required
                || r.consent_prompt.as_ref().is_some_and(prompt_required),
        }
    }

    /// The recording a held join shows: Sew's `409`'s recording with its prompt, or (an
    /// auto-record channel's first join) the prompt alone.
    pub fn from_consent_required(c: &ConsentRequired) -> CallRecording {
        match &c.recording {
            Some(r) => {
                let mut out = CallRecording::from_sew(r);
                out.consent_prompt = Some(c.consent_prompt.clone());
                out.my_consent = RecordingConsent::Pending;
                out.required |= prompt_required(&c.consent_prompt);
                out
            }
            // No recording yet: the answer is held for the one the room starts (an
            // auto-record room's first join, or a room that requires recording once a second
            // person joins).
            None => {
                let required = prompt_required(&c.consent_prompt);
                CallRecording {
                    recording_id: c.consent_prompt.recording_id.to_string(),
                    auto: c.auto,
                    started_by: c
                        .consent_prompt
                        .started_by
                        .as_ref()
                        .map(|m| m.member_id.to_string()),
                    my_consent: RecordingConsent::Pending,
                    consent_prompt: Some(c.consent_prompt.clone()),
                    participants: Vec::new(),
                    trigger: if required && !c.auto {
                        RecordingTrigger::Required
                    } else {
                        RecordingTrigger::from_auto(c.auto)
                    },
                    required,
                }
            }
        }
    }
}

/// A speaking level (`speaking` event).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[non_exhaustive]
pub struct Speaking {
    /// The engine identity.
    pub identity: String,
    /// The member, when known.
    pub member_id: Option<String>,
    /// 0..1.
    pub level: f32,
}

/// An error for the host (`errors` event): `{code, detail}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct CallErrorEvent {
    /// `turn_unreachable`, `relay_only_no_turns`, `e2ee_refused`, a provider's code, …
    pub code: String,
    /// Details.
    pub detail: String,
}

impl CallErrorEvent {
    /// An error with its code and details (for a host's own engine or a test).
    pub fn new(code: impl Into<String>, detail: impl Into<String>) -> CallErrorEvent {
        CallErrorEvent {
            code: code.into(),
            detail: detail.into(),
        }
    }
}

/// Something that happened in the call.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum CallEvent {
    /// The state changed.
    State(CallState),
    /// The participants changed.
    Participants(Vec<Participant>),
    /// Speaking levels (at most 10 Hz).
    Speaking(Vec<Speaking>),
    /// An error or warning.
    Error(CallErrorEvent),
    /// A moderator moved this member (Sew's `voice_server_updated`): the call goes on in the
    /// destination. Sent before the engine reconnects there; a re-join after it asks the
    /// provider for [`CallMoved::room`].
    Moved(CallMoved),
    /// A stats window ended: its numbers, the beacon sample sent (if any) and Sew's answer
    /// (also in [`Call::stats`]).
    Stats(Box<StatsWindow>),
    /// The call's recording changed (Sew 0.7.3): started, someone answered, this person's
    /// prompt, or `None` once no recording is active (also [`Call::recording`]).
    Recording(Option<Box<CallRecording>>),
    /// The call's key state or its security code changed (also [`CallSnapshot::security`]):
    /// a new epoch, the keys securing or recovering, or a re-join's new key session.
    Security(CallSecurity),
    /// A remote camera or screen track appeared (`video-core` builds). It is disabled at the
    /// media server until the host opens a sink for it ([`Call::video_sink`]).
    VideoAdded(crate::video::VideoTrack),
    /// A remote video track ended (unpublished, or its publisher left).
    VideoRemoved {
        /// Its id ([`crate::VideoTrack::id`]).
        id: u64,
        /// Its track sid.
        track_sid: String,
    },
    /// A remote video track changed: muted, its size or layers, its codec (known from its
    /// first packet), or paused by the server for congestion.
    VideoChanged(crate::video::VideoTrack),
    /// A local camera or screen share went on (the SFU confirmed it) or off (and why:
    /// `unpublished`, `video_disabled`, `server_unpublished`, `lease_lost`, …).
    LocalVideo(crate::video::LocalVideo),
    /// The media policy changed (`media_policy_updated`, a move): the latest hints, with the
    /// join's `camera_allowed` and `screen_allowed` (also [`Call::media_policy`]).
    MediaPolicy(Box<sew_proto::voice::MediaPolicy>),
}

/// The call's end-to-end key state and its security code, as the keys report them
/// ([`CallEvent::Security`], [`CallSnapshot::security`]). Ids, an epoch and a code only: no
/// key material.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct CallSecurity {
    /// The key state: `preparing`, `joining`, `active`, `securing`, `recovering`,
    /// `device_not_enrolled`, `server_identity_changed`, `unsupported`, `failed` or `left`.
    pub state: String,
    /// The active epoch (with `active`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epoch: Option<u64>,
    /// Since when the keys are in this state (the key session's clock, ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<u64>,
    /// With `securing`: show "Securing…" only this long after `since_ms`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_after_ms: Option<u64>,
    /// Why (`recovering`, `unsupported`, `failed`, `left`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The active epoch's security code, six groups of five digits separated by spaces
    /// (`"01234 56789 …"`), the same on every device in the epoch. `None` before an epoch is
    /// active, or when Sew's policy hides it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl CallSecurity {
    /// A key state with nothing else (for a host's own engine or a test).
    pub fn new(state: impl Into<String>) -> CallSecurity {
        CallSecurity {
            state: state.into(),
            ..CallSecurity::default()
        }
    }

    /// `active` in `epoch` with `code` (for a host's own engine or a test).
    pub fn active(epoch: u64, code: Option<String>) -> CallSecurity {
        CallSecurity {
            state: "active".to_owned(),
            epoch: Some(epoch),
            code,
            ..CallSecurity::default()
        }
    }
}

/// Where a moderator move took the call (Sew's `voice_server_updated`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct CallMoved {
    /// The destination voice channel (Sew's id, a decimal string).
    pub channel_id: String,
    /// The destination DM call, when the destination is one (Sew's id, a decimal string).
    pub call_id: Option<String>,
    /// The destination's media room (`r_...`).
    pub media_room: String,
}

impl CallMoved {
    /// The move in Sew's `voice_server_updated`.
    pub fn from_update(u: &sew_proto::voice::VoiceServerUpdated) -> CallMoved {
        CallMoved {
            channel_id: u.channel_id.to_string(),
            call_id: u.call_id.map(|c| c.to_string()),
            media_room: u.room.as_str().to_owned(),
        }
    }

    /// The `room` a host joins for the destination: the DM call's id when it is a call, else
    /// the voice channel's id. The engine's re-joins after the move ask for this room.
    pub fn room(&self) -> &str {
        self.call_id.as_deref().unwrap_or(&self.channel_id)
    }
}

/// Counters of one call.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub struct CallCounters {
    /// Opus frames encrypted and sent.
    pub frames_sent: u64,
    /// Frames dropped before sending (no media path, no send key, muted).
    pub frames_not_sent: u64,
    /// Frames the send queue was too full for.
    pub send_queue_drops: u64,
    /// Remote frames decrypted and handed over.
    pub frames_received: u64,
    /// Remote frames that did not decrypt (never played): a tag that did not verify, a
    /// malformed frame, or a key that stayed missing past the keys' grace.
    pub decrypt_failures: u64,
    /// Remote frames dropped because the sender's key at the frame's index was not installed
    /// yet (never played): frames that crossed a join or a key switch. Not decrypt failures.
    pub frames_before_key: u64,
    /// Remote frames of tracks the keys do not allow (never played): before the subscription
    /// check passes, or from a sender no longer verified.
    pub refused_frames: u64,
    /// Frames the receive queue was too full for.
    pub recv_queue_drops: u64,
    /// Packets reported lost.
    pub lost_marks: u64,
    /// Resumes completed.
    pub resumes: u64,
    /// Full reconnects completed.
    pub full_reconnects: u64,
    /// Re-joins through the provider.
    pub rejoins: u64,
    /// TURN allocations made.
    pub turn_allocations: u64,
    /// Call-credential renewals.
    pub credential_renewals: u64,
    /// Network changes the host reported ([`Call::network_changed`]).
    pub network_changes: u64,
    /// Remote video frames decrypted and handed over ([`Call::video_frames`]).
    pub video_frames_received: u64,
    /// Remote video frames that did not decrypt (never handed over); each breaks its track
    /// until a keyframe.
    pub video_decrypt_failures: u64,
    /// Remote video frames dropped because the sender's key was not installed yet.
    pub video_frames_before_key: u64,
    /// Remote video frames lost (a gap the retransmissions did not fill) or dropped while
    /// their track waited for a keyframe.
    pub video_frames_dropped: u64,
    /// Video packets of tracks with no open sink (not forwarded) or of no known track.
    pub video_packets_unwanted: u64,
    /// Video packets skipped over by the reorder buffer (never arrived in time).
    pub video_packets_lost: u64,
    /// Video packets that arrived after their gap was skipped (dropped).
    pub video_packets_late: u64,
    /// Video frames the video receive queue was too full for.
    pub video_recv_queue_drops: u64,
    /// Picture Loss Indications sent.
    pub video_plis: u64,
    /// Full Intra Requests sent.
    pub video_firs: u64,
}

/// A point-in-time view of a call (`state` command).
#[derive(Clone, Debug, PartialEq, Serialize)]
#[non_exhaustive]
pub struct CallSnapshot {
    /// The state.
    pub state: CallState,
    /// The participants (this device first).
    pub participants: Vec<Participant>,
    /// The relay's transport in use (`udp`, `tcp`, `tls`), if any.
    pub relay: Option<String>,
    /// Whether this join is relay-only.
    pub relay_only: bool,
    /// Counters.
    pub counters: CallCounters,
    /// The last moderator move, if any ([`CallEvent::Moved`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moved: Option<CallMoved>,
    /// The call's active recording, if any ([`CallEvent::Recording`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recording: Option<CallRecording>,
    /// The call's key state and security code, once the keys reported one
    /// ([`CallEvent::Security`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub security: Option<CallSecurity>,
}

impl Default for CallSnapshot {
    fn default() -> CallSnapshot {
        CallSnapshot {
            state: CallState::Connecting,
            participants: Vec::new(),
            relay: None,
            relay_only: false,
            counters: CallCounters::default(),
            moved: None,
            recording: None,
            security: None,
        }
    }
}

/// One encoded 20 ms Opus frame to send (kessel-audio's `EncodedFrame`).
#[derive(Clone, Debug, PartialEq)]
pub struct OpusFrame {
    /// The plaintext Opus packet (the engine encrypts it).
    pub payload: Bytes,
    /// Capture timestamp in 48 kHz units (the RTP timestamp; wraps).
    pub ts48k: u32,
    /// Voice activity after processing.
    pub vad: bool,
    /// Processed level, dBov (0 full scale … -127).
    pub level_dbov: f32,
}

/// One decrypted, verified remote frame (kessel-audio's `RecvFrame`).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct RecvOpus {
    /// The engine's stable id for the remote track.
    pub participant: u64,
    /// The publisher's engine identity.
    pub identity: String,
    /// The Opus packet (plaintext).
    pub payload: Bytes,
    /// RTP sequence number.
    pub seq: u16,
    /// RTP timestamp, 48 kHz units.
    pub ts48k: u32,
    /// When it arrived.
    pub arrival: Instant,
}

/// What the receive side hands the audio pipeline, in order.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum RecvItem {
    /// A remote track starts (call `add_participant` before its frames).
    Added {
        /// Its id.
        participant: u64,
        /// The publisher's engine identity.
        identity: String,
    },
    /// A remote track ends.
    Removed {
        /// Its id.
        participant: u64,
    },
    /// A frame.
    Frame(RecvOpus),
    /// A packet is known lost (a gap that did not fill, or a frame that failed decryption).
    Lost {
        /// The track.
        participant: u64,
        /// Its RTP sequence number.
        seq: u16,
    },
    /// Capture on (`true`) or off (muted, by self or a moderator, or SPEAK denied).
    Capture(bool),
    /// Playout off (`true`, deafened) or on.
    Deafened(bool),
}

/// The receive side's queue (one consumer).
#[derive(Debug)]
pub struct FrameStream(pub(crate) mpsc::Receiver<RecvItem>);

impl FrameStream {
    /// The next item; `None` once the call ended.
    pub async fn recv(&mut self) -> Option<RecvItem> {
        self.0.recv().await
    }

    /// The next item if one is waiting.
    pub fn try_recv(&mut self) -> Option<RecvItem> {
        self.0.try_recv().ok()
    }
}

/// Why a call could not start or a command failed.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum CallError {
    /// One call per app.
    #[error("already_in_call")]
    AlreadyInCall,
    /// The provider refused.
    #[error(transparent)]
    Provider(#[from] ProviderError),
    /// The call has ended.
    #[error("the call has ended")]
    Ended,
    /// The engine could not set itself up (TLS roots or crypto provider, e.g.
    /// `no_crypto_provider`; HTTP client).
    #[error("setup: {0}")]
    Setup(String),
    /// No recording is active (or held) to answer.
    #[error("no_recording")]
    NoRecording,
}

pub(crate) enum Cmd {
    Mute {
        muted: Option<bool>,
        deafened: Option<bool>,
        reply: oneshot::Sender<()>,
    },
    Leave(oneshot::Sender<()>),
    /// The [`Call`] handle was dropped without a leave: the engine leaves (whoever else still
    /// holds its command queue: a video sink, a camera pipeline, a [`CallLeaver`]).
    Dropped,
    Consent {
        granted: bool,
        notice_version: Option<u32>,
        reply: oneshot::Sender<Result<(), CallError>>,
    },
    /// The person's Record or Stop.
    Recording {
        control: RecordingControl,
        reply: oneshot::Sender<Result<(), CallError>>,
    },
    NetworkChanged,
    /// A video sink opened, changed, asked for a keyframe or closed.
    Video(crate::video::subscribe::SinkCmd),
    Publish(crate::video::publish::VideoCmd),
}

/// The engine of one app: at most one live call.
pub struct CallClient {
    cfg: ClientConfig,
    e2ee: Arc<dyn E2eeProvider>,
    busy: Arc<AtomicBool>,
    /// The video codec preference and the declared decoders, for every call of this engine.
    codecs: Arc<crate::video::codec_pref::CodecSettings>,
}

impl std::fmt::Debug for CallClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallClient")
            .field("cfg", &self.cfg)
            .finish_non_exhaustive()
    }
}

impl CallClient {
    /// An engine with `cfg` whose call keys come from `e2ee` (the `sew-e2ee` key session;
    /// there is no plaintext alternative).
    pub fn new(cfg: ClientConfig, e2ee: Arc<dyn E2eeProvider>) -> CallClient {
        CallClient {
            cfg,
            e2ee,
            busy: Arc::new(AtomicBool::new(false)),
            codecs: Arc::default(),
        }
    }

    /// Sets this device's video codec preference ([`crate::VideoCodecPreference`], a user
    /// setting) for every call of this engine, the live one included: a camera or screen
    /// turned on after it publishes under it ([`crate::effective_publish_codec`]).
    /// [`crate::VideoCodecPreference::PreferVp8`] also stops this device offering to receive
    /// H.264, from its next join. The engine keeps it until the process ends; persisting it
    /// across launches is the host's job (store [`CallClient::video_codec_preference`] with
    /// the app's settings and set it again at start).
    pub fn set_video_codec_preference(&self, pref: crate::VideoCodecPreference) {
        self.codecs.set_preference(pref);
    }

    /// This device's video codec preference ([`CallClient::set_video_codec_preference`];
    /// [`crate::VideoCodecPreference::Automatic`] until set).
    pub fn video_codec_preference(&self) -> crate::VideoCodecPreference {
        self.codecs.preference()
    }

    /// Declares the video codecs this host can decode, from the next join: the OS decoders its
    /// kessel-video backend lists (its capabilities' decode entries, a run-time answer) and the
    /// codecs its webview decodes (a WebCodecs probe). The media connection then offers to
    /// receive VP8 always and H.264 only if `codecs` lists it, never on Linux and never under
    /// [`crate::VideoCodecPreference::PreferVp8`]; the media server then sends this device a
    /// sender's VP8 instead of H.264. Nothing is assumed: list only what was found.
    ///
    /// Until a host declares, [`ClientConfig::video_h264`] alone decides, as before.
    pub fn set_video_decode(&self, codecs: &[crate::VideoCodec]) {
        self.codecs.set_decode(codecs);
    }

    /// The codecs [`CallClient::set_video_decode`] declared (sorted, `None` until then).
    pub fn video_decode(&self) -> Option<Vec<crate::VideoCodec>> {
        self.codecs.decode()
    }

    /// An engine with `cfg` and Sew's MLS call keys ([`crate::e2ee::MlsKeys`]), the device key
    /// where `cfg.device_store` says (by default a 0600 file; the host's own store with
    /// [`ClientConfig::with_device_key_store`], a hardware key with
    /// [`ClientConfig::with_hardware_device_key`]), enrolled as `cfg.device_platform`: the
    /// production constructor.
    pub fn with_mls(cfg: ClientConfig) -> CallClient {
        let keys =
            crate::e2ee::MlsKeys::new(cfg.device_store.clone()).with_platform(cfg.device_platform);
        CallClient::new(cfg, Arc::new(keys))
    }

    /// Every device key this engine's data directory holds a record of
    /// ([`crate::device::known_device_keys`]), sorted: with a hardware key, one Keychain item
    /// or Keystore alias each ([`crate::device::DeviceKeyId::account`]). A host store's
    /// ([`ClientConfig::with_device_key_store`]) devices are the host's and not listed.
    /// Blocking (it reads the directory).
    pub fn known_device_keys(
        &self,
    ) -> Result<Vec<crate::device::DeviceKeyId>, crate::device::DeviceError> {
        crate::device::known_device_keys(&self.cfg.data_dir)
    }

    /// The host's sign-out from one Sew install ([`crate::device::forget_install`] with
    /// `cfg.device_store`): every device of `install_id` is reset (its key deleted, a hardware
    /// key through `HardwareDeviceKey::delete`; the device and the pins cleared) and its files
    /// removed. Answers the ids it removed. Refused with
    /// [`crate::device::DeviceError::InCall`] while a call is live or joining; a join while it
    /// runs is refused with [`CallError::AlreadyInCall`].
    pub async fn forget_server(
        &self,
        install_id: u64,
    ) -> Result<Vec<crate::device::DeviceKeyId>, crate::device::DeviceError> {
        if self.busy.swap(true, Ordering::AcqRel) {
            return Err(crate::device::DeviceError::InCall);
        }
        let _busy = BusyGuard(self.busy.clone());
        let vault = crate::device::VaultConfig {
            data_dir: self.cfg.data_dir.clone(),
            app_id: self.cfg.app_id.clone(),
            store: self.cfg.device_store.clone(),
            label: String::new(),
            platform: self.cfg.device_platform,
        };
        crate::device::forget_install(&vault, install_id).await
    }

    /// Joins through `provider` and starts the call.
    ///
    /// A room that is recorded (Sew's `409 recording_consent_required`, the provider's
    /// [`ProviderError::ConsentRequired`]) is not an error: the call comes back held at
    /// [`CallState::ConsentRequired`], with the notice in [`Call::recording`], until the host
    /// answers ([`Call::consent`]) or leaves.
    pub async fn join(
        &self,
        provider: Arc<dyn JoinProvider>,
        req: JoinRequest,
    ) -> Result<Call, CallError> {
        if self.busy.swap(true, Ordering::AcqRel) {
            return Err(CallError::AlreadyInCall);
        }
        let busy = BusyGuard(self.busy.clone());
        let first = match provider.join(req.clone()).await {
            Ok(answer) => engine::First::Answer(Box::new(answer)),
            Err(ProviderError::ConsentRequired(c)) => engine::First::Held(c),
            Err(e) => return Err(e.into()),
        };
        let (events, _) = broadcast::channel(256);
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (push_tx, push_rx) = mpsc::channel(SEND_QUEUE);
        let (frames_tx, frames_rx) = mpsc::channel(RECV_QUEUE);
        let (video_tx, video_rx) = mpsc::channel(VIDEO_RECV_QUEUE);
        let snapshot = Arc::new(Mutex::new(CallSnapshot::default()));
        let shared = Arc::new(engine::Shared {
            events: events.clone(),
            snapshot: snapshot.clone(),
            send_queue_drops: std::sync::atomic::AtomicU64::new(0),
            stats: Mutex::new(CallStatsReport::default()),
            playout: Mutex::new(None),
            beacon_stopped: AtomicBool::new(false),
            policy: Mutex::new(None),
            video_stats: Arc::new(Mutex::new(crate::video::VideoSendStats::default())),
            video_codecs: self.codecs.negotiated(self.cfg.video_h264),
            codec_settings: Arc::clone(&self.codecs),
        });
        if let engine::First::Held(c) = &first {
            // Held before `join` returns: a host that reads the snapshot at once (to decide
            // whether to open the microphone) sees `consent_required`, never `connecting`.
            engine::preset_held(&snapshot, c);
        }
        engine::spawn(engine::Start {
            cfg: self.cfg.clone(),
            e2ee: self.e2ee.clone(),
            provider,
            req,
            first,
            shared: shared.clone(),
            cmd_rx,
            push_rx,
            frames_tx,
            video_tx,
            busy,
        })
        .map_err(CallError::Setup)?;
        Ok(Call {
            cmd: cmd_tx,
            push: push_tx,
            shared,
            frames: Mutex::new(Some(FrameStream(frames_rx))),
            video: Mutex::new(Some(crate::video::VideoFrameStream(video_rx))),
            next_sink: std::sync::atomic::AtomicU64::new(1),
            id: NEXT_CALL.fetch_add(1, Ordering::Relaxed),
        })
    }
}

/// [`Call::local_id`]'s counter.
static NEXT_CALL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Clears the app's call slot when the engine task ends.
pub(crate) struct BusyGuard(Arc<AtomicBool>);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Leaves a call without its [`Call`] handle ([`Call::leaver`]). Cloneable; a leave of a call
/// that is already over answers [`CallError::Ended`].
#[derive(Clone)]
pub struct CallLeaver {
    cmd: mpsc::UnboundedSender<Cmd>,
    id: u64,
}

impl std::fmt::Debug for CallLeaver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallLeaver").finish_non_exhaustive()
    }
}

impl CallLeaver {
    /// Leaves, as [`Call::leave`]: when it answers, the app's call slot is free.
    pub async fn leave(&self) -> Result<(), CallError> {
        let (reply, rx) = oneshot::channel();
        self.cmd
            .send(Cmd::Leave(reply))
            .map_err(|_| CallError::Ended)?;
        rx.await.map_err(|_| CallError::Ended)
    }

    /// The call's [`Call::local_id`].
    pub fn local_id(&self) -> u64 {
        self.id
    }
}

/// The send queue of a call, cloneable ([`Call::frame_sender`]).
#[derive(Clone)]
pub struct FrameSender {
    push: mpsc::Sender<OpusFrame>,
    shared: Arc<engine::Shared>,
}

impl std::fmt::Debug for FrameSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameSender").finish_non_exhaustive()
    }
}

impl FrameSender {
    /// Queues one encoded frame (see [`Call::push_opus`]).
    pub fn push_opus(&self, f: OpusFrame) {
        if self.push.try_send(f).is_err() {
            self.shared.send_queue_drops.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// One live call.
pub struct Call {
    cmd: mpsc::UnboundedSender<Cmd>,
    push: mpsc::Sender<OpusFrame>,
    shared: Arc<engine::Shared>,
    frames: Mutex<Option<FrameStream>>,
    video: Mutex<Option<crate::video::VideoFrameStream>>,
    next_sink: std::sync::atomic::AtomicU64,
    id: u64,
}

impl Drop for Call {
    /// The handle is gone: the engine leaves now, even while a video sink, a camera pipeline
    /// or a [`CallLeaver`] still holds its command queue (after a leave, nothing happens).
    fn drop(&mut self) {
        let _ = self.cmd.send(Cmd::Dropped);
    }
}

impl std::fmt::Debug for Call {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Call").finish_non_exhaustive()
    }
}

impl Call {
    /// The call's events (state, participants, speaking, errors).
    pub fn events(&self) -> broadcast::Receiver<CallEvent> {
        self.shared.events.subscribe()
    }

    /// The current state and participants.
    pub fn snapshot(&self) -> CallSnapshot {
        let mut s = self
            .shared
            .snapshot
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default();
        s.counters.send_queue_drops = self.shared.send_queue_drops.load(Ordering::Relaxed);
        s
    }

    /// The stats windows so far (the latest 256) and the beacon's terms: the same numbers the
    /// beacon sends, measured whether or not it is on (see the `stats` module docs for how
    /// each is derived).
    pub fn stats(&self) -> CallStatsReport {
        self.shared
            .stats
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default()
    }

    /// Where the beacon's played and concealed audio come from: the host's own audio
    /// pipeline (one that does not use [`crate::AudioBridge`], which reports kessel-audio's).
    /// Without one, the engine reports what it handed the pipeline.
    pub fn set_playout_stats(&self, source: Arc<dyn PlayoutStats>) {
        if let Ok(mut p) = self.shared.playout.lock() {
            *p = Some(source);
        }
    }

    /// Mute and deafen (`None` leaves it). Deafened implies muted; un-deafening returns
    /// `muted` to its value before the deafen.
    pub async fn mute(&self, muted: Option<bool>, deafened: Option<bool>) -> Result<(), CallError> {
        let (reply, rx) = oneshot::channel();
        self.cmd
            .send(Cmd::Mute {
                muted,
                deafened,
                reply,
            })
            .map_err(|_| CallError::Ended)?;
        rx.await.map_err(|_| CallError::Ended)
    }

    /// The call's active recording as Sew says it (or the notice of a held join), if any.
    pub fn recording(&self) -> Option<CallRecording> {
        self.shared
            .snapshot
            .lock()
            .ok()
            .and_then(|s| s.recording.clone())
    }

    /// The person's answer to the recording's notice (`notice_version`: the version they saw,
    /// [`CallState::ConsentRequired`]'s or the prompt's). Held at `consent_required`, the
    /// engine re-joins with the answer (a decline joins unrecorded, or is refused when declining
    /// means not joining: the call ends). In a call, the host sends it
    /// ([`JoinProvider::consent`]). Either way this device's key reaches the recorder only
    /// once Sew says `granted`. A notice that changed meanwhile answers
    /// `Provider(ConsentRequired)` with the current one (the call stays as it was).
    pub async fn consent(&self, granted: bool, notice_version: u32) -> Result<(), CallError> {
        self.answer(granted, Some(notice_version)).await
    }

    /// "Stop recording me": withdraws a granted consent through the host (Sew stops capturing
    /// this person at once and the keys rekey so the recorder gets no later key). Before an
    /// answer it is a decline.
    pub async fn withdraw_consent(&self) -> Result<(), CallError> {
        self.answer(false, None).await
    }

    /// Record (Sew 0.9.0): the person starts recording the call's room through the host
    /// ([`JoinProvider::recording`], with the person's own session). Pressing Record is the
    /// person's own consent, so show the notice as a confirmation first; everyone else stays
    /// unrecorded until they answer. Sew's answer is the call's recording at once
    /// ([`Call::recording`], [`CallEvent::Recording`]). Refusals are Sew's, as
    /// `Provider(Refused { code, .. })` (`recording_active`, `recording_disabled`,
    /// `cap_ceiling`, …); while the join is held for a notice the person is not in the room
    /// yet, and it is refused `not_in_voice` without asking Sew.
    pub async fn start_recording(&self) -> Result<(), CallError> {
        self.control(RecordingControl::Start).await
    }

    /// Stop (Sew 0.9.0): the person stops the room's active recording through the host
    /// ([`JoinProvider::recording`]). [`CallError::NoRecording`] when none is active. Sew
    /// refuses `recording_required` while the room requires its recording and two or more
    /// people are in it ([`CallRecording::required`]); "Stop recording me"
    /// ([`Call::withdraw_consent`]) still works then.
    pub async fn stop_recording(&self) -> Result<(), CallError> {
        self.control(RecordingControl::Stop).await
    }

    async fn control(&self, control: RecordingControl) -> Result<(), CallError> {
        let (reply, rx) = oneshot::channel();
        self.cmd
            .send(Cmd::Recording { control, reply })
            .map_err(|_| CallError::Ended)?;
        rx.await.map_err(|_| CallError::Ended)?
    }

    async fn answer(&self, granted: bool, notice_version: Option<u32>) -> Result<(), CallError> {
        let (reply, rx) = oneshot::channel();
        self.cmd
            .send(Cmd::Consent {
                granted,
                notice_version,
                reply,
            })
            .map_err(|_| CallError::Ended)?;
        rx.await.map_err(|_| CallError::Ended)?
    }

    /// The device's network changed: the host calls this from its path monitor (iOS
    /// `NWPathMonitor`, Android `ConnectivityManager.NetworkCallback`) when the default route
    /// moves (Wi-Fi to cellular and back, a new Wi-Fi network, a VPN coming up). Never blocks;
    /// safe from any thread and to repeat.
    ///
    /// In a call it runs the resume path now instead of waiting for the old connections to time
    /// out: the signalling WebSocket is replaced and the session resumes (`JoinRequest.reconnect`,
    /// the PC kept), a fresh transport is opened on the new network (a new TURN allocation over a
    /// new TURN-TLS / TCP / UDP socket and new host candidates) and swapped into the PC, then an
    /// ICE restart moves the media onto it; the call socket of the keys reconnects and resumes
    /// too. The state shows `reconnecting` until the resume is done. If the new transport can't
    /// be opened, or the restart does not connect, the engine falls back to its full reconnect
    /// (and re-join) inside the usual window. Before the call is connected it retries the
    /// connect at once; while the join is held for a recording notice it does nothing (no
    /// connection is open).
    pub fn network_changed(&self) {
        let _ = self.cmd.send(Cmd::NetworkChanged);
    }

    /// Leaves (Sew's leave with the credential, then the engine's). When it answers, the app's
    /// call slot is free: a [`CallClient::join`] right after it is never
    /// [`CallError::AlreadyInCall`]. Dropping the `Call` without it leaves too.
    pub async fn leave(self) -> Result<(), CallError> {
        self.leaver().leave().await
    }

    /// A handle that leaves this call from anywhere, without the `Call` itself: for a host that
    /// keeps the `Call` behind a lock that its camera or screen commands hold while they wait
    /// on a device, so a leave never waits for them (they get [`CallError::Ended`] or
    /// [`crate::VideoError::Ended`] once the call is over).
    pub fn leaver(&self) -> CallLeaver {
        CallLeaver {
            cmd: self.cmd.clone(),
            id: self.id,
        }
    }

    /// A number this call alone has in the process (never reused), to tell its video
    /// pipelines from a later call's (`VideoBridge::close_call`, with `video-core`).
    pub fn local_id(&self) -> u64 {
        self.id
    }

    /// Queues one encoded frame to send. Never blocks: a full queue drops the frame (and
    /// counts it); the engine drops it too unless media is up and a send key is active.
    pub fn push_opus(&self, f: OpusFrame) {
        self.frame_sender().push_opus(f);
    }

    /// A cloneable handle to the send queue (for an audio thread or task).
    pub fn frame_sender(&self) -> FrameSender {
        FrameSender {
            push: self.push.clone(),
            shared: self.shared.clone(),
        }
    }

    /// The receive side (decrypted Opus per remote track, loss marks, mute and deafen), for the
    /// audio pipeline. The first call takes it; later calls get `None`.
    pub fn frames(&self) -> Option<FrameStream> {
        self.frames.lock().ok().and_then(|mut f| f.take())
    }

    /// The video receive side: decrypted, verified **encoded** frames of the remote camera and
    /// screen tracks that have an open sink ([`Call::video_sink`]), with each track's `Added`,
    /// `Removed`, `Broken` and `KeyframeNeeded`. The first call takes it; later calls get
    /// `None`. Without `video-core` the engine negotiates no video and nothing arrives.
    pub fn video_frames(&self) -> Option<crate::video::VideoFrameStream> {
        self.video.lock().ok().and_then(|mut f| f.take())
    }

    /// The video codecs this call's media connection offers to receive (VP8 first; H.264 only
    /// when the join's [`CallClient::set_video_decode`] and preference allowed it). Fixed at
    /// the join; the publish side can send only these too. Empty without `video-core`.
    pub fn video_codecs(&self) -> Vec<crate::VideoCodec> {
        if crate::pc::VIDEO {
            self.shared.video_codecs.codecs()
        } else {
            Vec::new()
        }
    }

    /// This device's video codec preference now ([`CallClient::set_video_codec_preference`]),
    /// for the publish side when a camera or screen turns on.
    pub fn video_codec_preference(&self) -> crate::VideoCodecPreference {
        self.shared.codec_settings.preference()
    }

    /// Opens a sink for the remote video track `track_sid` ([`CallEvent::VideoAdded`]): while
    /// at least one of a track's sinks is open and visible, the engine asks the media server
    /// for it (the largest visible sink's size, 30 fps for a large tile, 15 for a grid tile)
    /// and its frames flow through [`Call::video_frames`], starting at a keyframe. With none,
    /// the track is disabled at the server and nothing is forwarded. Dropping the sink closes
    /// it.
    pub fn video_sink(
        &self,
        track_sid: &str,
        spec: crate::video::VideoSinkSpec,
    ) -> crate::video::VideoSink {
        let id = self.next_sink.fetch_add(1, Ordering::Relaxed);
        let _ = self
            .cmd
            .send(Cmd::Video(crate::video::subscribe::SinkCmd::Open {
                id,
                track_sid: track_sid.to_owned(),
                spec,
            }));
        crate::video::VideoSink {
            id,
            cmd: self.cmd.clone(),
        }
    }

    /// The client media policy of the join (its `camera_allowed` and `screen_allowed`) with
    /// the latest layer hints (`media_policy_updated`, a move), if the join answer had one.
    pub fn media_policy(&self) -> Option<sew_proto::voice::MediaPolicy> {
        self.shared.policy.lock().ok().and_then(|p| p.clone())
    }

    /// Whether this call's media connection negotiates H.264 ([`crate::ClientConfig::video_h264`]):
    /// a sender publishes H.264 only then.
    #[cfg(feature = "video-core")]
    pub(crate) fn decodes_h264(&self) -> bool {
        self.shared.video_codecs.contains(crate::VideoCodec::H264)
    }

    /// The send-side video counters.
    pub fn video_stats(&self) -> crate::video::VideoSendStats {
        self.shared
            .video_stats
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default()
    }

    async fn video(
        &self,
        cmd: impl FnOnce(
            oneshot::Sender<Result<(), crate::video::VideoError>>,
        ) -> crate::video::publish::VideoCmd,
    ) -> Result<(), crate::video::VideoError> {
        let (reply, rx) = oneshot::channel();
        self.cmd
            .send(Cmd::Publish(cmd(reply)))
            .map_err(|_| crate::video::VideoError::Ended)?;
        rx.await.map_err(|_| crate::video::VideoError::Ended)?
    }

    /// Takes (`on`) or releases a video lease: Sew's `self_video` for the camera, `self_stream`
    /// for a screen share (`POST /api/v1/me/voice/state` with the call credential). Refused
    /// with `video_not_allowed` when the join's policy has video off or this session may not
    /// hold it, and with Sew's own refusals (`video_slots_full`, `media_busy` with its
    /// `Retry-After`, `media_capacity`, `video_disabled`). A publication needs the lease
    /// first; releasing it does not unpublish ([`Call::unpublish_video`] does both).
    pub async fn set_video_lease(
        &self,
        source: crate::video::VideoSource,
        on: bool,
    ) -> Result<(), crate::video::VideoError> {
        self.video(|reply| crate::video::publish::VideoCmd::Lease { source, on, reply })
            .await
    }

    /// Publishes a camera or screen share under its lease: the `AddTrackRequest` with the
    /// layers that opened and the codec, the SFU's `TrackPublished` confirmed GCM by the keys,
    /// then the simulcast send section. The handle takes each layer's encoded frames and says
    /// what the engine needs ([`crate::video::VideoControl`]: keyframes, which layers run at
    /// what rates, a republish, the end). Refused with `no_lease` without the lease,
    /// `e2ee_downgraded` when the SFU drops GCM, `already_published` for a second publication
    /// of the source.
    pub async fn publish_video(
        &self,
        req: crate::video::VideoPublish,
    ) -> Result<crate::video::VideoPublication, crate::video::VideoError> {
        let (reply, rx) = oneshot::channel();
        self.cmd
            .send(Cmd::Publish(crate::video::publish::VideoCmd::Publish {
                req,
                reply,
            }))
            .map_err(|_| crate::video::VideoError::Ended)?;
        rx.await.map_err(|_| crate::video::VideoError::Ended)?
    }

    /// Unpublishes the source's track (its send section goes inactive; the producer gets
    /// `Ended { reason: "unpublished" }`) and, with `release_lease`, releases its lease.
    pub async fn unpublish_video(
        &self,
        source: crate::video::VideoSource,
        release_lease: bool,
    ) -> Result<(), crate::video::VideoError> {
        self.unpublish_video_because(source, release_lease, crate::video::publish::UNPUBLISHED)
            .await
    }

    /// [`Call::unpublish_video`] with `reason` as the producer's `Ended` and `local_video`'s
    /// reason (who turned it off: `camera_off`, `camera_switch`, `system_ui`, …).
    pub async fn unpublish_video_because(
        &self,
        source: crate::video::VideoSource,
        release_lease: bool,
        reason: &str,
    ) -> Result<(), crate::video::VideoError> {
        let reason = reason.to_owned();
        self.video(|reply| crate::video::publish::VideoCmd::Unpublish {
            source,
            release: release_lease,
            reason,
            reply,
        })
        .await
    }

    /// A handle that ends this call's publications from another thread (the bridge's
    /// pipeline, when its source ends).
    #[cfg(feature = "video-core")]
    pub(crate) fn video_ctl(&self) -> crate::video::publish::VideoCtl {
        crate::video::publish::VideoCtl::new(self.cmd.clone())
    }

    /// Mutes or unmutes the source's published track at the SFU (`MuteTrackRequest`) while
    /// keeping the publication and its lease: subscribers see the track muted (a phone app in
    /// the background, whose producer stops sending frames). Sent again after a republish or a
    /// reconnect. Nothing happens without a publication of the source.
    pub fn set_video_muted(&self, source: crate::video::VideoSource, muted: bool) {
        let _ = self
            .cmd
            .send(Cmd::Publish(crate::video::publish::VideoCmd::Mute {
                source,
                muted,
            }));
    }

    /// Caps on top of the policy (thermal pressure, low power): each layer's bitrate, the
    /// number of layers and the frame rate, applied before the bandwidth split.
    pub fn set_video_caps(&self, caps: crate::video::VideoCaps) {
        let _ = self
            .cmd
            .send(Cmd::Publish(crate::video::publish::VideoCmd::Caps(caps)));
    }
}
