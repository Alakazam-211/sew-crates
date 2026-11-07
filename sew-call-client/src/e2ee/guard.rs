//! The mandatory checks of the native engine, on str0m: the same checks Sew's browser client
//! (`@sew/e2ee`) makes:
//!
//! 1. **No plaintext mode.** Nothing here has an "unencrypted" switch. An outgoing frame leaves
//!    only through [`SendGate::seal`], which calls [`FrameCrypto::seal`]; with no active key or
//!    no verified publication the frame is dropped and counted, never sent.
//! 2. **Subscribe only verified encrypted tracks** ([`SubscribeGuard`]): an audio track (and, with `video-core`, a camera or screen track) whose
//!    `TrackInfo.encryption` is `GCM` **and** whose publisher identity is bound to a leaf of the
//!    verified MLS tree (the DS-signed leaf binding table, `lk_identity` = `p_` + base32 of the
//!    leaf's `join_id`). Anything else is left unsubscribed; an unencrypted track is reported
//!    (`report` route, `engine_unencrypted_track`). [`RecvGate::open`] refuses frames of any sender
//!    that did not pass, and a frame that fails to decrypt is counted and never played (a frame
//!    whose key is not installed yet, around a join or a key switch, is counted apart:
//!    [`KEY_PENDING_GRACE_MS`]).
//! 3. **Publish only through the checked path** ([`PublishGuard`], [`check_publish`]): the
//!    `AddTrackRequest` asks for `GCM`, and the publication counts as encrypted only when the
//!    server's `TrackPublishedResponse` echoes `GCM`. A downgrade refuses the publication for the
//!    rest of the call and is reported (`report` route, `engine_encryption_downgrade`). Video is
//!    published only under a lease Sew granted ([`Leases`]: `self_video` for a camera,
//!    `self_stream` for a screen share); anything else that is not audio is refused.
//! 4. **No keys in logs**: nothing here logs a frame, a key or an IV; `Debug` prints counts.

use std::fmt;

use sew_e2ee::{EngineAttack, LkIdentity, Slot, UndecryptReason};
use sew_lkproto::livekit::{AddTrackRequest, TrackInfo, TrackSource, TrackType, encryption};

use super::frame::{FrameCrypto, IvParts, OpenError, VideoFrameCodec};
use super::keys::{MediaKeys, MediaReport, PublishState, lock};

/// Why a publication was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PublishRefused {
    /// Neither audio nor video (data tracks, an unknown type or video source): never published.
    #[error("only an audio track or a leased video track may be published")]
    NotAudio,
    /// Video without the lease Sew granted for it (`self_video` for a camera, `self_stream`
    /// for a screen share).
    #[error("no video lease for this source")]
    NoLease,
    /// The server answered the publication without `GCM`: the engine downgraded it.
    #[error("the engine downgraded the publication's encryption")]
    Downgraded,
    /// The publication was refused earlier in this call.
    #[error("publication refused earlier in this call")]
    Refused,
}

/// The video leases this device holds in the call (Sew's `self_video` and `self_stream`,
/// granted through `POST /api/v1/me/voice/state`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Leases {
    /// A camera lease (`self_video`).
    pub camera: bool,
    /// A screen-share lease (`self_stream`).
    pub screen: bool,
}

impl Leases {
    /// No lease.
    pub fn none() -> Leases {
        Leases::default()
    }

    /// The given leases.
    pub fn new(camera: bool, screen: bool) -> Leases {
        Leases { camera, screen }
    }
}

/// The static half of the publish check, for every `AddTrackRequest` the engine sends: audio,
/// or video under its lease (a camera with `self_video`, a screen share with `self_stream`;
/// [`PublishRefused::NoLease`] otherwise); anything else is [`PublishRefused::NotAudio`]. Sets
/// `encryption = GCM` whatever the caller put in.
pub fn check_publish(req: &mut AddTrackRequest, leases: Leases) -> Result<(), PublishRefused> {
    if req.r#type == TrackType::Video as i32 {
        let leased = if req.source == TrackSource::Camera as i32 {
            leases.camera
        } else if req.source == TrackSource::ScreenShare as i32 {
            leases.screen
        } else {
            false
        };
        if !leased {
            return Err(PublishRefused::NoLease);
        }
    } else if req.r#type != TrackType::Audio as i32 {
        return Err(PublishRefused::NotAudio);
    }
    req.encryption = encryption::Type::Gcm as i32;
    Ok(())
}

/// Whether a server-confirmed track is one the engine may publish at all (audio or video).
fn publishable(track: &TrackInfo) -> bool {
    track.r#type == TrackType::Audio as i32 || track.r#type == TrackType::Video as i32
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

    /// Checks and completes an `AddTrackRequest` before it is sent, holding no video lease:
    /// [`PublishGuard::prepare_with_leases`] with [`Leases::none`].
    pub fn prepare(&self, req: &mut AddTrackRequest) -> Result<(), PublishRefused> {
        self.prepare_with_leases(req, Leases::none())
    }

    /// Checks and completes an `AddTrackRequest` before it is sent ([`check_publish`]: audio,
    /// or video under its lease; `encryption = GCM` is set here whatever the caller put in).
    /// The publication stays unverified (frames dropped) until [`PublishGuard::on_published`].
    pub fn prepare_with_leases(
        &self,
        req: &mut AddTrackRequest,
        leases: Leases,
    ) -> Result<(), PublishRefused> {
        if self.keys.publish_state() == PublishState::Refused {
            return Err(PublishRefused::Refused);
        }
        check_publish(req, leases)?;
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
        if !publishable(track) {
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
    /// Neither an audio track nor (with `video-core`) a video track.
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
        // Video (camera and screen) only in a build that receives it (`video-core`).
        let video = crate::pc::VIDEO && track.r#type == TrackType::Video as i32;
        if track.r#type != TrackType::Audio as i32 && !video {
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

    /// Seals one encoded video frame (`codec`: VP8 or an H.264 access unit with 4-byte start
    /// codes) of the local layer `ssrc`, or drops it (`None`), exactly as [`SendGate::seal`]
    /// does for audio: no active slot, a publication not confirmed encrypted, an empty frame,
    /// or a cipher failure. The IV's send count is per SSRC, so each simulcast layer counts
    /// its own.
    pub fn seal_video(
        &self,
        codec: VideoFrameCodec,
        ssrc: u32,
        rtp_timestamp: u32,
        frame: &[u8],
    ) -> Option<Vec<u8>> {
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
        let mut out = Vec::with_capacity(frame.len() + 64);
        let r = self
            .keys
            .crypto
            .seal_video(key, slot.get(), codec, iv, frame, &mut out);
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

/// How long a sender's frames may carry a key index this device holds no key at before they
/// count as decrypt failures. Until then such a frame is one that crossed a join or a key
/// switch (`frames_before_key`): a member that just joined holds only the new epoch's keys
/// while the others still send under the old one until `activate`, and a new epoch's frame can
/// beat its install by a moment. The same span as the receiver check's report, so a key still
/// missing when the key session is told about it is a decrypt failure from then on.
pub const KEY_PENDING_GRACE_MS: u64 = UNDECRYPTABLE_REPORT_AFTER_MS;

/// Why a received frame was not passed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RecvRefused {
    /// The sender did not pass the subscribe check (or is no longer verified).
    #[error("sender is not a verified encrypted publisher")]
    Unverified,
    /// No key at the frame's key index yet, within [`KEY_PENDING_GRACE_MS`] of the sender's
    /// first undecryptable frame (counted in `frames_before_key`, not `decrypt_failures`).
    #[error("no key at index {0} yet")]
    KeyPending(u8),
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
        self.open_frame(sender, None, frame)
    }

    /// Opens one video frame of `codec` from `sender`, with the same checks, counters and
    /// reports as [`RecvGate::open`] (a failing sender is reported once, through
    /// `MediaReport::Undecryptable`). Only a decrypted frame comes back.
    pub fn open_video(
        &self,
        sender: &LkIdentity,
        codec: crate::video::VideoCodec,
        frame: &[u8],
    ) -> Result<Vec<u8>, RecvRefused> {
        self.open_frame(sender, Some(codec), frame)
    }

    fn open_frame(
        &self,
        sender: &LkIdentity,
        video: Option<crate::video::VideoCodec>,
        frame: &[u8],
    ) -> Result<Vec<u8>, RecvRefused> {
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
                    match video {
                        None => crypto.open(key, frame, &mut out),
                        Some(codec) => crypto.open_video(key, codec, frame, &mut out),
                    }
                    .map(|()| out)
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
                // A key index with no key, early in the sender's failing streak: the key has not
                // landed (or, for an old epoch's frame at a member that just joined, never will),
                // which is not a decrypt failure. A tag that does not verify always is.
                let pending = matches!(e, OpenError::NoKey(_))
                    && now.saturating_sub(f.since) < KEY_PENDING_GRACE_MS;
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
                if pending {
                    s.counters.frames_before_key += 1;
                } else {
                    s.counters.decrypt_failures += 1;
                }
                drop(guard);
                if let Some(r) = report {
                    self.keys.report(r);
                }
                match e {
                    OpenError::NoKey(i) if pending => Err(RecvRefused::KeyPending(i)),
                    _ => Err(RecvRefused::Undecryptable(e)),
                }
            }
        }
    }
}
