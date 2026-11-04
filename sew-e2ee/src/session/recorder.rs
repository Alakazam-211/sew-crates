//! The device side of the recorder key handoff.
//!
//! A device releases its **own** key for an epoch only when all hold: the recorder certificate
//! verifies against the announcement; the announced recording is the app's live recording (the call
//! is being recorded); and this member's consent for it is `Granted`. Triggers: a valid
//! announcement, consent becoming `Granted` (also between a commit and its `activate`), any Welcome
//! (every held epoch), and every processed commit (the new pending epoch, before `ready`).
//! Deduplicated per `(recording_id, recorder key, group_id, epoch)`.

use std::collections::BTreeSet;

use sew_proto::e2ee::tls::ReleaseInfo;
use sew_proto::e2ee::{B64, KeyRekeyRequest, KeyReleaseRequest, RekeyReason, reasons};

use super::{CallSession, Purpose};
use crate::ids::{Consent, Epoch, Id, LkIdentity, Millis, RecordingId, Slot};
use crate::keys::FrameIkm;
use crate::recorder::{seal_release, verify_recorder_cert};
use crate::wire::{ApiFailure, KeyReply, KeyRequest};

#[derive(Debug, Clone)]
struct Announced {
    recording_id: u64,
    hpke_pub: [u8; 32],
}

/// The recorder state of one session.
#[derive(Debug, Clone)]
pub(super) struct RecorderState {
    announced: Option<Announced>,
    identity: Option<LkIdentity>,
    app_recording: Option<u64>,
    consent: Consent,
    stopped: bool,
    released: BTreeSet<(u64, [u8; 32], Vec<u8>, Epoch)>,
}

impl Default for RecorderState {
    fn default() -> RecorderState {
        RecorderState {
            announced: None,
            identity: None,
            app_recording: None,
            consent: Consent::Pending,
            stopped: false,
            released: BTreeSet::new(),
        }
    }
}

impl RecorderState {
    /// The identity named in a valid `recorder` dispatch (exempt from roster reports).
    pub(super) fn identity(&self) -> Option<&LkIdentity> {
        self.identity.as_ref()
    }
}

impl CallSession {
    pub(super) fn on_recorder(
        &mut self,
        now: Millis,
        recording_id: Id,
        cert: Option<&[u8]>,
        hpke_pub: Option<&[u8]>,
        lk_identity: Option<LkIdentity>,
        removed: bool,
    ) {
        if removed {
            self.rec.announced = None;
            self.rec.identity = None;
            return;
        }
        let (Some(cert), Some(hpke_pub)) = (cert, hpke_pub) else {
            return;
        };
        let Ok(hpke_pub) = <[u8; 32]>::try_from(hpke_pub) else {
            return;
        };
        let ok = verify_recorder_cert(
            cert,
            self.trust.ca_public(),
            self.trust.install_id(),
            &self.call_id,
            recording_id.get(),
            &hpke_pub,
            now,
        );
        if ok.is_err() {
            // No release in any case; status unchanged (a recorder test).
            return;
        }
        self.rec.announced = Some(Announced {
            recording_id: recording_id.get(),
            hpke_pub,
        });
        self.rec.identity = lk_identity;
        self.release_held(now);
    }

    pub(super) fn on_recording(
        &mut self,
        now: Millis,
        recording_id: Option<RecordingId>,
        consent: Consent,
    ) -> Result<(), crate::error::SessionError> {
        let was_granted = self.rec.consent == Consent::Granted && self.rec.app_recording.is_some();
        let rid = recording_id.map(|r| r.get());
        if rid != self.rec.app_recording || consent != self.rec.consent {
            self.rec.stopped = false;
        }
        self.rec.app_recording = rid;
        self.rec.consent = consent;
        if was_granted && consent != Consent::Granted {
            // Withdrawal: stop at once and ask for an immediate rekey (`rekey`).
            self.send(
                Purpose::Rekey,
                KeyRequest::Rekey(
                    self.call_id.clone(),
                    KeyRekeyRequest {
                        join_id: self.join_id.clone(),
                        reason: RekeyReason::ConsentWithdrawn,
                    },
                ),
            );
            return Ok(());
        }
        if consent == Consent::Granted {
            self.release_held(now);
        }
        Ok(())
    }

    /// Every epoch this device holds its own key for that the server's release rule accepts: the
    /// active epoch (of the held group) and the pending one.
    pub(super) fn release_held(&mut self, now: Millis) {
        let current = self.group_id();
        let mut held: Vec<(Vec<u8>, Epoch, Slot, u32, FrameIkm)> = Vec::new();
        if let Some(a) = &self.active
            && Some(&a.group_id) == current.as_ref()
        {
            held.push((
                a.group_id.clone(),
                a.epoch,
                a.slot,
                a.leaf,
                a.ikm.duplicate(),
            ));
        }
        if let Some(p) = &self.pending {
            held.push((
                p.group_id.clone(),
                p.epoch,
                p.slot,
                p.leaf,
                p.ikm.duplicate(),
            ));
        }
        for (gid, epoch, slot, leaf, ikm) in held {
            self.release_one(now, gid, epoch, slot, leaf, &ikm);
        }
    }

    /// The pending epoch only (after a commit).
    pub(super) fn release_pending(&mut self, now: Millis) {
        let Some(p) = &self.pending else {
            return;
        };
        let (gid, epoch, slot, leaf, ikm) = (
            p.group_id.clone(),
            p.epoch,
            p.slot,
            p.leaf,
            p.ikm.duplicate(),
        );
        self.release_one(now, gid, epoch, slot, leaf, &ikm);
    }

    fn release_one(
        &mut self,
        _now: Millis,
        group_id: Vec<u8>,
        epoch: Epoch,
        slot: Slot,
        leaf: u32,
        ikm: &FrameIkm,
    ) {
        let Some(a) = &self.rec.announced else {
            return;
        };
        if self.rec.stopped
            || self.rec.consent != Consent::Granted
            || self.rec.app_recording != Some(a.recording_id)
        {
            return;
        }
        let key = (a.recording_id, a.hpke_pub, group_id.clone(), epoch);
        if self.rec.released.contains(&key) {
            return;
        }
        let Some(recording_id) = Id::new(a.recording_id) else {
            return;
        };
        let info = ReleaseInfo {
            call_id: self.call_id.as_str().as_bytes().to_vec(),
            recording_id: a.recording_id,
            group_id: group_id.clone(),
            epoch,
            leaf_index: leaf,
            slot: slot.get(),
            sender_identity: self.me.as_str().as_bytes().to_vec(),
        };
        let Some((enc, ct)) = seal_release(&a.hpke_pub, &info, ikm) else {
            return;
        };
        let hpke_pub = a.hpke_pub;
        self.rec.released.insert(key);
        self.counters.releases_sent += 1;
        self.send(
            Purpose::Release(recording_id.get(), group_id.clone(), epoch),
            KeyRequest::Release(
                self.call_id.clone(),
                KeyReleaseRequest {
                    join_id: self.join_id.clone(),
                    group_id: B64::new(group_id),
                    epoch,
                    slot: slot.get(),
                    recording_id,
                    leaf,
                    sender_identity: self.me.as_str().to_owned(),
                    enc: B64::new(enc),
                    ct: B64::new(ct),
                },
            ),
        );
        let _ = hpke_pub;
    }

    pub(super) fn on_release_reply(
        &mut self,
        recording: u64,
        group_id: Vec<u8>,
        epoch: Epoch,
        r: Result<KeyReply, ApiFailure>,
    ) {
        let Err(f) = r else {
            return;
        };
        if (f.status == 403 && f.is(reasons::E2EE_CONSENT_NOT_GRANTED))
            || (f.status == 409 && f.is(reasons::E2EE_RECORDING_MISMATCH))
        {
            // Stop releasing until the app's recording input changes. The refused epoch was
            // never released: a later grant must be able to release it (a withdrawal's rekey
            // commit can reach the device before its own `Denied` input, so the pending
            // epoch's release races the withdrawal and is refused).
            self.rec.stopped = true;
            self.rec
                .released
                .retain(|(r, _, g, e)| !(*r == recording && *g == group_id && *e == epoch));
        }
        // `409 e2ee_stale_epoch`: dropped. `422`: a bug (logged by the driver).
    }
}
