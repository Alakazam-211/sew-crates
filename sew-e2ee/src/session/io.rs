//! The session's inputs and outputs.

use std::fmt;

use sew_proto::e2ee::{CallKeysMessage, ReadyCallKeys};

use crate::bridge::KeyEvent;
use crate::ids::{
    Consent, Epoch, FenceId, LkIdentity, MemberId, Millis, RecordingId, RequestId, SignId, Slot,
};
use crate::wire::{ApiFailure, KeyReply, KeyRequest};

/// Everything a `CallSession` reacts to. The driver or the TS wrapper feeds them.
pub enum Input {
    /// A `call_keys_delivered` body.
    Server(CallKeysMessage),
    /// The answer to an `Output::Send` (the server's responses, or the failure).
    Reply(RequestId, Result<KeyReply, ApiFailure>),
    /// The bridge applied everything up to `fence`.
    Installed {
        /// The fence.
        fence: FenceId,
    },
    /// The platform signed a `NeedDeviceSignature` message (raw r‖s, 64 bytes).
    DeviceSignature {
        /// The request.
        request: SignId,
        /// The signature.
        sig: [u8; 64],
    },
    /// The gateway resumed (a replay follows; nothing to do).
    GatewayResumed,
    /// A new gateway session (not a resume): the `state` route.
    GatewayReidentified {
        /// READY's `session_id`.
        gateway_session: String,
    },
    /// A fresh READY: pins, CRL, params.
    Ready(ReadyCallKeys),
    /// The app's recording state: the live recording and this member's consent.
    Recording {
        /// The live recording, if any.
        recording_id: Option<RecordingId>,
        /// This member's consent for it.
        my_consent: Consent,
    },
    /// The SDK reports a missing key for `identity` since `since` (bridge telemetry).
    SdkMissingKey {
        /// The remote identity.
        identity: LkIdentity,
        /// When the SDK first reported it.
        since: Millis,
    },
    /// The SDK reports undecryptable frames from `identity` (the receiver check).
    SdkUndecryptable {
        /// The remote identity.
        identity: LkIdentity,
        /// Continuously failing since.
        since: Millis,
        /// Failures counted.
        failures: u32,
        /// `invalid_key` or `missing_key`.
        reason: UndecryptReason,
        /// The key index the frames carried, when known.
        missing_index: Option<Slot>,
    },
    /// The SDK decrypts `identity` again (clears an undecryptable report).
    SdkDecryptOk {
        /// The remote identity.
        identity: LkIdentity,
    },
    /// The bridge's sender self-check: the slot the local sender is on.
    SenderSlotObserved {
        /// What the SDK reports.
        observed: Slot,
    },
    /// The bridge saw the engine misbehave (the `report` engine kinds).
    EngineAttack {
        /// What.
        kind: EngineAttack,
        /// The identity involved (the publisher), if any.
        identity: Option<LkIdentity>,
    },
    /// The SDK's participant identities (for `report` roster reports).
    MediaParticipants(Vec<LkIdentity>),
    /// The members Sew shows in this call (voice states), for `report`.
    VoiceMembers(Vec<MemberId>),
}

impl fmt::Debug for Input {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Input::Server(m) => write!(f, "Server({})", m.kind()),
            Input::Reply(id, r) => write!(
                f,
                "Reply({id}, {})",
                match r {
                    Ok(k) => k.kind(),
                    Err(_) => "failure",
                }
            ),
            Input::Installed { fence } => write!(f, "Installed({fence})"),
            Input::DeviceSignature { request, .. } => write!(f, "DeviceSignature({request})"),
            Input::GatewayResumed => f.write_str("GatewayResumed"),
            Input::GatewayReidentified { .. } => f.write_str("GatewayReidentified"),
            Input::Ready(_) => f.write_str("Ready"),
            Input::Recording {
                recording_id,
                my_consent,
            } => write!(f, "Recording({recording_id:?}, {my_consent:?})"),
            Input::SdkMissingKey { identity, since } => {
                write!(f, "SdkMissingKey({identity}, {since})")
            }
            Input::SdkUndecryptable {
                identity, reason, ..
            } => write!(f, "SdkUndecryptable({identity}, {reason:?})"),
            Input::SdkDecryptOk { identity } => write!(f, "SdkDecryptOk({identity})"),
            Input::SenderSlotObserved { observed } => write!(f, "SenderSlotObserved({observed})"),
            Input::EngineAttack { kind, .. } => write!(f, "EngineAttack({kind:?})"),
            Input::MediaParticipants(v) => write!(f, "MediaParticipants({})", v.len()),
            Input::VoiceMembers(v) => write!(f, "VoiceMembers({})", v.len()),
        }
    }
}

/// Why a receiver cannot decrypt a sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UndecryptReason {
    /// The SDK has a key at the frame's index but the tag fails.
    InvalidKey,
    /// No key at the frame's index.
    MissingKey,
}

impl UndecryptReason {
    /// The `report` route's `details.reason`.
    pub fn as_str(self) -> &'static str {
        match self {
            UndecryptReason::InvalidKey => "invalid_key",
            UndecryptReason::MissingKey => "missing_key",
        }
    }
}

/// Engine misbehaviour a bridge detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EngineAttack {
    /// A publication announced unencrypted (`enable{false}` filtered, or a NONE track refused).
    UnencryptedTrack,
    /// A non-empty SIF trailer (dropped).
    SifTrailer,
    /// A published track's encryption was downgraded (publish refused).
    EncryptionDowngrade,
}

/// What a `CallSession` asks its driver to do.
pub enum Output {
    /// POST a key route; feed the answer back as `Input::Reply(id, …)`.
    Send(RequestId, KeyRequest),
    /// Apply a key event to the bridge, in order.
    Key(KeyEvent),
    /// Have the device key sign `message` (a fresh KeyPackage for a re-add or a reset).
    NeedDeviceSignature {
        /// The request.
        request: SignId,
        /// The bytes to sign (raw r‖s answer).
        message: Vec<u8>,
    },
    /// Ask the bridge for the local sender's slot (the sender self-check); answer with
    /// `Input::SenderSlotObserved`.
    CheckSenderSlot {
        /// The slot it should be on.
        expected: Slot,
    },
    /// The status changed.
    Status(E2eeStatus),
    /// `roster()` changed.
    Roster,
}

impl fmt::Debug for Output {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Output::Send(id, r) => write!(f, "Send({id}, {})", r.kind()),
            Output::Key(ev) => write!(f, "Key({} {:?} {:?})", ev.kind(), ev.identity(), ev.slot()),
            Output::NeedDeviceSignature { request, .. } => {
                write!(f, "NeedDeviceSignature({request})")
            }
            Output::CheckSenderSlot { expected } => write!(f, "CheckSenderSlot({expected})"),
            Output::Status(s) => write!(f, "Status({s:?})"),
            Output::Roster => f.write_str("Roster"),
        }
    }
}

/// The UI status. Every change is an `Output::Status`, carrying its timestamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum E2eeStatus {
    /// Preparing the key package.
    Preparing,
    /// Join sent; waiting for `init` / `welcome`.
    Joining {
        /// Since.
        since: Millis,
    },
    /// Keys active.
    Active {
        /// The active epoch.
        epoch: Epoch,
        /// Since.
        since: Millis,
    },
    /// Media may be connected but keys are not active yet; the UI shows "Securing…" only
    /// `show_after_ms` after `since` (from `params.securing_indicator_after_ms`).
    Securing {
        /// Since.
        since: Millis,
        /// The UI threshold.
        show_after_ms: u64,
    },
    /// Re-adding or resetting; old keys still in use.
    Recovering {
        /// Why.
        reason: &'static str,
        /// Since.
        since: Millis,
    },
    /// No usable device certificate.
    DeviceNotEnrolled,
    /// A pinned server key changed: no joins until the store is cleared.
    ServerIdentityChanged,
    /// This client cannot join encrypted calls.
    Unsupported {
        /// Why.
        reason: String,
    },
    /// Failed for good (`recovery_timeout`, `sender_index_stuck`, `create_failed`, …).
    Failed {
        /// Why.
        reason: &'static str,
    },
    /// Left.
    Left {
        /// Why.
        reason: LeftReason,
    },
}

/// Why a session ended (`SessionState::Left`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeftReason {
    /// `leave()`.
    Left,
    /// A `removed` dispatch (or a commit that removed this leaf).
    Removed(sew_proto::e2ee::RemoveReason),
    /// Failed hard (`create_failed`, `recovery_timeout`, …).
    Failed(&'static str),
}

/// The per-call state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    /// Join sent; waiting for `init` / `welcome`.
    Joining,
    /// A member.
    Active {
        /// The MLS epoch.
        epoch: Epoch,
        /// The transition in progress.
        transition: Transition,
    },
    /// Sent `invalid`; waiting for the re-add Welcome. Old SDK keys stay in place.
    Resyncing {
        /// Why.
        reason: &'static str,
        /// Since.
        since: Millis,
    },
    /// A `reset` is collecting; the old group's keys stay in use until the new group activates.
    Resetting {
        /// The new group.
        new_group_id: Vec<u8>,
        /// Since.
        since: Millis,
    },
    /// Left, removed or failed hard. Terminal; all secrets are zeroized.
    Left {
        /// Why.
        reason: LeftReason,
    },
}

/// A transition in progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transition {
    /// Nothing in flight.
    Idle,
    /// This leaf posted a commit.
    Committing {
        /// The epoch it leaves.
        epoch_from: Epoch,
    },
    /// The new epoch's remote keys went to the bridge; waiting for the fence.
    Installed {
        /// The pending epoch.
        epoch: Epoch,
        /// The fence.
        fence: FenceId,
    },
    /// `ready` sent; waiting for `activate`.
    Ready {
        /// The pending epoch.
        epoch: Epoch,
        /// When `ready` was sent.
        since: Millis,
    },
}
