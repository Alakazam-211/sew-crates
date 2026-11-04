//! The mandatory checks of the native engine, on str0m: the same checks Sew's browser client
//! (`@sew/e2ee`) makes:
//!
//! 1. **No plaintext mode.** Nothing here has an "unencrypted" switch. An outgoing frame leaves
//!    only through [`SendGate::seal`], which calls [`FrameCrypto::seal`]; with no active key or
//!    no verified publication the frame is dropped and counted, never sent.
//! 2. **Subscribe only verified encrypted tracks** ([`SubscribeGuard`]): an audio track whose
//!    `TrackInfo.encryption` is `GCM` **and** whose publisher identity is bound to a leaf of the
//!    verified MLS tree (the DS-signed leaf binding table, `lk_identity` = `p_` + base32 of the
//!    leaf's `join_id`). Anything else is left unsubscribed; an unencrypted track is reported
//!    (`report` route, `engine_unencrypted_track`). [`RecvGate::open`] refuses frames of any sender
//!    that did not pass, and a frame that fails to decrypt is counted and never played.
//! 3. **Publish only through the checked path** ([`PublishGuard`]): the `AddTrackRequest` asks
//!    for `GCM`, and the publication counts as encrypted only when the server's
//!    `TrackPublishedResponse` echoes `GCM`. A downgrade refuses the publication for the rest of
//!    the call and is reported (`report` route, `engine_encryption_downgrade`).
//! 4. **No keys in logs**: nothing here logs a frame, a key or an IV; `Debug` prints counts.

use std::fmt;

use sew_e2ee::{EngineAttack, LkIdentity, Slot, UndecryptReason};
use sew_lkproto::livekit::{AddTrackRequest, TrackInfo, TrackType, encryption};

use super::frame::{FrameCrypto, IvParts, OpenError};
use super::keys::{MediaKeys, MediaReport, PublishState, lock};

/// Why a publication was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PublishRefused {
    /// Voice only: the engine never publishes video, screen share or data.
    #[error("only an audio track may be published")]
    NotAudio,
    /// The server answered the publication without `GCM`: the engine downgraded it.
    #[error("the engine downgraded the publication's encryption")]
    Downgraded,
    /// The publication was refused earlier in this call.
    #[error("publication refused earlier in this call")]
    Refused,
}

/// The publish half of the checks (3 above).
pub struct PublishGuard<C: FrameCrypto> {
    keys: MediaKeys<C>,
}

impl<C: FrameCrypto> fmt::Debug for PublishGuard<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PublishGuard")
            .field("state", &self.keys.publish_state())
            .finish()
    }
}

impl<C: FrameCrypto> PublishGuard<C> {
    /// The guard over a call's keys.
    pub fn new(keys: &MediaKeys<C>) -> PublishGuard<C> {
        PublishGuard { keys: keys.clone() }
    }

    /// Checks and completes an `AddTrackRequest` before it is sent: audio only, and
    /// `encryption = GCM` is set here whatever the caller put in. The publication stays
    /// unverified (frames dropped) until [`PublishGuard::on_published`].
    pub fn prepare(&self, req: &mut AddTrackRequest) -> Result<(), PublishRefused> {
        if self.keys.publish_state() == PublishState::Refused {
            return Err(PublishRefused::Refused);
        }
        if req.r#type != TrackType::Audio as i32 {
            return Err(PublishRefused::NotAudio);
        }
        req.encryption = encryption::Type::Gcm as i32;
        lock(&self.keys.state).publish = PublishState::Unverified;
        Ok(())
    }

    /// The server's `TrackPublishedResponse.track`: verified only with `GCM`. Anything else is
    /// a downgrade: the publication is refused for the rest of the call and reported.
    pub fn on_published(&self, track: &TrackInfo) -> Result<(), PublishRefused> {
        let mut s = lock(&self.keys.state);
        if s.publish == PublishState::Refused {
            return Err(PublishRefused::Refused);
        }
        if track.r#type != TrackType::Audio as i32 {
            s.publish = PublishState::Refused;
            return Err(PublishRefused::NotAudio);
        }
        if track.encryption != encryption::Type::Gcm as i32 {
            s.publish = PublishState::Refused;
            drop(s);
            self.keys.report(MediaReport::Attack {
                kind: EngineAttack::EncryptionDowngrade,
                identity: self.keys.local_identity(),
            });
            return Err(PublishRefused::Downgraded);
        }
        s.publish = PublishState::Verified;
        Ok(())
    }

    /// A full reconnect: the new publication must be confirmed again before frames flow (the
    /// sender slot is re-asserted by the key session).
    pub fn on_republish(&self) {
        let mut s = lock(&self.keys.state);
        if s.publish != PublishState::Refused {
            s.publish = PublishState::Unverified;
        }
    }
}

/// What to do with a remote track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SubscribeDecision {
    /// Subscribe: an encrypted audio track of a verified publisher.
    Subscribe,
    /// Not yet: the publisher is not (yet) bound to a leaf of the verified tree. Ask again when
    /// the roster changes (`KeySessionEvent::Roster`).
    Wait,
    /// Never: leave it unsubscribed.
    Refuse(SubscribeRefused),
}

/// Why a remote track is never subscribed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SubscribeRefused {
    /// Not an audio track (voice only).
    NotAudio,
    /// `encryption` is not `GCM` (reported as `engine_unencrypted_track`).
    Unencrypted,
    /// The participant identity is not an engine identity (`p_…`).
    BadIdentity,
    /// The track claims to be the local identity's.
    LocalIdentity,
}

/// The subscribe half of the checks (2 above).
pub struct SubscribeGuard<C: FrameCrypto> {
    keys: MediaKeys<C>,
}

impl<C: FrameCrypto> fmt::Debug for SubscribeGuard<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SubscribeGuard")
    }
}

impl<C: FrameCrypto> SubscribeGuard<C> {
    /// The guard over a call's keys.
    pub fn new(keys: &MediaKeys<C>) -> SubscribeGuard<C> {
        SubscribeGuard { keys: keys.clone() }
    }

    /// Decides on `track`, published by the participant `identity` (the engine's string). A
    /// `Subscribe` also admits the publisher's frames into [`RecvGate::open`].
    pub fn check(&self, identity: &str, track: &TrackInfo) -> SubscribeDecision {
        let Ok(id) = LkIdentity::parse(identity) else {
            return SubscribeDecision::Refuse(SubscribeRefused::BadIdentity);
        };
        if track.r#type != TrackType::Audio as i32 {
            return SubscribeDecision::Refuse(SubscribeRefused::NotAudio);
        }
        if track.encryption != encryption::Type::Gcm as i32 {
            self.keys.report(MediaReport::Attack {
                kind: EngineAttack::UnencryptedTrack,
                identity: Some(id),
            });
            return SubscribeDecision::Refuse(SubscribeRefused::Unencrypted);
        }
        let mut s = lock(&self.keys.state);
        if s.local.as_ref().is_some_and(|(lid, _, _)| *lid == id) {
            return SubscribeDecision::Refuse(SubscribeRefused::LocalIdentity);
        }
        if !s.verified.contains(&id) {
            return SubscribeDecision::Wait;
        }
        s.subscribed.insert(id);
        SubscribeDecision::Subscribe
    }

    /// The publisher unpublished or left: its frames are refused from now on.
    pub fn unsubscribed(&self, identity: &str) {
        if let Ok(id) = LkIdentity::parse(identity) {
            lock(&self.keys.state).subscribed.remove(&id);
        }
    }
}

/// The send gate (check 1): the only way an outgoing frame reaches the transport.
pub struct SendGate<C: FrameCrypto> {
    keys: MediaKeys<C>,
}

impl<C: FrameCrypto> fmt::Debug for SendGate<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SendGate")
    }
}

impl<C: FrameCrypto> SendGate<C> {
    /// The gate over a call's keys.
    pub fn new(keys: &MediaKeys<C>) -> SendGate<C> {
        SendGate { keys: keys.clone() }
    }

    /// Seals one encoded Opus frame with the active local key, or drops it (`None`): no active
    /// slot, a publication not confirmed encrypted, an empty frame, or a cipher failure. A
    /// dropped frame is never sent in any form.
    pub fn seal(&self, ssrc: u32, rtp_timestamp: u32, frame: &[u8]) -> Option<Vec<u8>> {
        let mut guard = lock(&self.keys.state);
        let s = &mut *guard;
        if s.publish != PublishState::Verified {
            s.counters.dropped_unverified_publication += 1;
            return None;
        }
        if frame.is_empty() {
            s.counters.dropped_seal_failed += 1;
            return None;
        }
        let Some((_, slot, key)) = s.local.as_ref() else {
            s.counters.dropped_no_key += 1;
            return None;
        };
        let count = match s.send_count.get(&ssrc) {
            Some(c) => *c,
            None => u32::from(rand::random::<u16>()),
        };
        let iv = IvParts {
            ssrc,
            rtp_timestamp,
            send_count: count,
        };
        let mut out = Vec::with_capacity(frame.len() + 32);
        let r = self.keys.crypto.seal(key, slot.get(), iv, frame, &mut out);
        s.send_count.insert(ssrc, count.wrapping_add(1));
        match r {
            Ok(()) if !out.is_empty() => {
                s.counters.sealed += 1;
                Some(out)
            }
            _ => {
                s.counters.dropped_seal_failed += 1;
                None
            }
        }
    }

    /// The slot frames are sealed under now (the sender self-check).
    pub fn slot(&self) -> Option<Slot> {
        self.keys.local_slot()
    }
}

/// How long frames of one sender must keep failing before the key session reports it (the
/// receiver check debounces short gaps around a key switch).
pub const UNDECRYPTABLE_REPORT_AFTER_MS: u64 = 2_000;

/// Why a received frame was not passed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RecvRefused {
    /// The sender did not pass the subscribe check (or is no longer verified).
    #[error("sender is not a verified encrypted publisher")]
    Unverified,
    /// The frame did not decrypt.
    #[error("frame did not decrypt: {0}")]
    Undecryptable(OpenError),
}

/// The receive gate (check 2): frames of verified senders, decrypted, or nothing.
pub struct RecvGate<C: FrameCrypto> {
    keys: MediaKeys<C>,
    clock: crate::Clock,
}

impl<C: FrameCrypto> fmt::Debug for RecvGate<C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RecvGate")
    }
}

impl<C: FrameCrypto> RecvGate<C> {
    /// The gate over a call's keys.
    pub fn new(keys: &MediaKeys<C>, clock: crate::Clock) -> RecvGate<C> {
        RecvGate {
            keys: keys.clone(),
            clock,
        }
    }

    /// Opens one frame of `sender` (the participant identity of the track's publisher). Only a
    /// decrypted frame comes back; the caller plays nothing else (a loss to the jitter buffer).
    pub fn open(&self, sender: &LkIdentity, frame: &[u8]) -> Result<Vec<u8>, RecvRefused> {
        let now = (self.clock)();
        let mut guard = lock(&self.keys.state);
        let s = &mut *guard;
        if !s.subscribed.contains(sender) || !s.verified.contains(sender) {
            s.counters.dropped_unverified_sender += 1;
            return Err(RecvRefused::Unverified);
        }
        let crypto = &self.keys.crypto;
        let result = match crypto.key_index(frame) {
            Err(e) => Err(e),
            Ok(slot) => match s.remotes.get(sender).and_then(|r| r.get(slot)) {
                None => Err(OpenError::NoKey(slot)),
                Some(key) => {
                    let mut out = Vec::with_capacity(frame.len());
                    crypto.open(key, frame, &mut out).map(|()| out)
                }
            },
        };
        match result {
            Ok(out) => {
                s.counters.opened += 1;
                if let Some(f) = s.failing.remove(sender)
                    && f.reported
                {
                    drop(guard);
                    self.keys.report(MediaReport::DecryptOk {
                        identity: sender.clone(),
                    });
                }
                Ok(out)
            }
            Err(e) => {
                s.counters.decrypt_failures += 1;
                let (reason, slot) = match e {
                    OpenError::NoKey(i) => (UndecryptReason::MissingKey, Slot::new(i)),
                    OpenError::BadTag(i) => (UndecryptReason::InvalidKey, Slot::new(i)),
                    OpenError::BadTrailer => (UndecryptReason::InvalidKey, None),
                };
                let f = s
                    .failing
                    .entry(sender.clone())
                    .or_insert(super::keys::Failing {
                        since: now,
                        failures: 0,
                        reason,
                        slot,
                        reported: false,
                    });
                f.failures = f.failures.saturating_add(1);
                f.reason = reason;
                f.slot = slot;
                let due =
                    !f.reported && now.saturating_sub(f.since) >= UNDECRYPTABLE_REPORT_AFTER_MS;
                let report = due.then(|| {
                    f.reported = true;
                    MediaReport::Undecryptable {
                        identity: sender.clone(),
                        since: f.since,
                        failures: f.failures,
                        reason,
                        slot,
                    }
                });
                drop(guard);
                if let Some(r) = report {
                    self.keys.report(r);
                }
                Err(RecvRefused::Undecryptable(e))
            }
        }
    }
}
