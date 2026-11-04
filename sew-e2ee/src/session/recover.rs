//! Recovery without user action: the re-add (`invalid`), the group
//! reset (`key_package`), the watchdog, and the device signatures they need.
//!
//! The re-add keeps the old keys in the SDK, makes a fresh key package (new leaf key, same scope;
//! no `join_id` inside, the server binds it to the request's `join_id`), and POSTs `invalid`. The
//! server queues one Replace commit (Remove old leaf + Add new package); the client joins its new
//! leaf through the Welcome. The affected client loses audio only from the first epoch it missed
//! until its re-add activates.

use sew_proto::e2ee::{B64, InvalidReason, KeyInvalidRequest, KeyResetPackageRequest};

use super::{CallSession, E2eeStatus, LeftReason, Purpose, Recovery, ResetState, SessionState};
use crate::error::SessionError;
use crate::ids::{Millis, SignId};
use crate::keypackage::LeafDraft;
use crate::wire::KeyRequest;

/// What a device signature is for.
#[derive(Debug, Clone)]
pub(super) enum SignPurpose {
    /// An `invalid` re-add for `(group, epoch)`.
    ReAdd {
        reason: InvalidReason,
        group_id: Vec<u8>,
        epoch: u64,
    },
    /// A `key_package` package for a resetting call's new group.
    Reset { new_group_id: Vec<u8> },
}

impl CallSession {
    /// Starts a re-add (`invalid`) unless one is already in flight.
    pub(super) fn start_readd(&mut self, now: Millis, reason: InvalidReason) {
        if matches!(self.state, SessionState::Left { .. }) {
            return;
        }
        let in_flight = self
            .sign_waits
            .values()
            .any(|(_, p)| matches!(p, SignPurpose::ReAdd { .. }));
        if matches!(self.state, SessionState::Resyncing { .. }) && in_flight {
            return;
        }
        if matches!(self.state, SessionState::Resyncing { .. }) && self.recovery.is_some() {
            // One `invalid` per (join_id, group_id, epoch); the watchdog repeats it.
            return;
        }
        let (group_id, epoch) = match (self.group_id(), self.mls_epoch()) {
            (Some(g), Some(e)) => (g, e),
            _ => match &self.known_group {
                Some((g, e)) => (g.clone(), *e),
                None => return,
            },
        };
        *self.counters.recoveries.entry(reason.as_str()).or_insert(0) += 1;
        self.clear_transition();
        self.pending = None;
        let tries = self.recovery.as_ref().map_or(1, |r| r.tries);
        self.recovery = Some(Recovery {
            reason: reason.as_str(),
            since: now,
            tries,
        });
        self.state = SessionState::Resyncing {
            reason: reason.as_str(),
            since: now,
        };
        self.set_status(E2eeStatus::Recovering {
            reason: reason.as_str(),
            since: now,
        });
        self.request_readd_package(reason, group_id, epoch);
    }

    fn request_readd_package(&mut self, reason: InvalidReason, group_id: Vec<u8>, epoch: u64) {
        match LeafDraft::new(&self.device, self.scope) {
            Ok(draft) => self.sign(
                draft,
                SignPurpose::ReAdd {
                    reason,
                    group_id,
                    epoch,
                },
            ),
            Err(_) => self.fail_now("key_package"),
        }
    }

    /// A device signature arrived: build the package and send `invalid` or `key_package`.
    pub(super) fn on_signature(
        &mut self,
        now: Millis,
        request: SignId,
        sig: [u8; 64],
    ) -> Result<(), SessionError> {
        let Some((draft, purpose)) = self.sign_waits.remove(&request) else {
            return Ok(());
        };
        let (leaf, bytes) = draft.finish(&self.device, &self.provider, &sig, now, &self.cfg)?;
        self.latest_leaf = Some(leaf.kp_ref);
        self.leaves.insert(leaf.kp_ref, leaf);
        match purpose {
            SignPurpose::ReAdd {
                reason,
                group_id,
                epoch,
            } => {
                let gateway_session = if self.rebind_on_next_invalid {
                    self.rebind_on_next_invalid = false;
                    Some(self.gateway_session.clone())
                } else {
                    None
                };
                self.send(
                    Purpose::Invalid,
                    KeyRequest::Invalid(
                        self.call_id.clone(),
                        KeyInvalidRequest {
                            join_id: self.join_id.clone(),
                            group_id: B64::new(group_id),
                            epoch,
                            reason,
                            key_package: B64::new(bytes),
                            gateway_session,
                        },
                    ),
                );
            }
            SignPurpose::Reset { new_group_id } => {
                self.send(
                    Purpose::ResetPackage,
                    KeyRequest::ResetPackage(
                        self.call_id.clone(),
                        KeyResetPackageRequest {
                            join_id: self.join_id.clone(),
                            new_group_id: B64::new(new_group_id),
                            key_package: B64::new(bytes),
                        },
                    ),
                );
            }
        }
        Ok(())
    }

    /// `reset{new_group_id, slot_base}`: a fresh package for the new group (`key_package`). The old
    /// group's keys stay installed and active until the new group activates.
    pub(super) fn on_reset(
        &mut self,
        now: Millis,
        new_group_id: &[u8],
        slot_base: u8,
    ) -> Result<(), SessionError> {
        if self
            .reset
            .as_ref()
            .is_some_and(|r| r.new_group_id == new_group_id)
            || self.group_id().as_deref() == Some(new_group_id)
        {
            self.counters.ignored_stale += 1;
            return Ok(());
        }
        self.counters.resets += 1;
        self.clear_transition();
        self.pending = None;
        self.reset = Some(ResetState {
            new_group_id: new_group_id.to_vec(),
            slot_base,
        });
        self.state = SessionState::Resetting {
            new_group_id: new_group_id.to_vec(),
            since: now,
        };
        self.recovery = Some(Recovery {
            reason: "reset",
            since: now,
            tries: 1,
        });
        self.request_reset_package(new_group_id.to_vec());
        Ok(())
    }

    fn request_reset_package(&mut self, new_group_id: Vec<u8>) {
        match LeafDraft::new(&self.device, self.scope) {
            Ok(draft) => self.sign(draft, SignPurpose::Reset { new_group_id }),
            Err(_) => self.fail_now("key_package"),
        }
    }

    /// The `Resyncing` / `Resetting` watchdog: every 5 s a fresh package and `invalid`
    /// `recovery_timeout` (or `key_package` again); after 3 tries, `Left{Failed}`.
    pub(super) fn watchdog(&mut self, now: Millis) {
        let Some(r) = self.recovery.clone() else {
            return;
        };
        if now < r.since + self.cfg.recovery_watchdog_ms {
            return;
        }
        tracing::debug!(
            target: "sew_e2ee::session",
            reason = r.reason,
            tries = r.tries,
            "recovery watchdog"
        );
        match &self.state {
            SessionState::Resyncing { .. } => {
                if r.tries >= self.cfg.recovery_watchdog_tries {
                    self.fail(now, "recovery_timeout");
                    return;
                }
                self.recovery = Some(Recovery {
                    reason: "recovery_timeout",
                    since: now,
                    tries: r.tries + 1,
                });
                let (group_id, epoch) = match (self.group_id(), self.mls_epoch()) {
                    (Some(g), Some(e)) => (g, e),
                    _ => match &self.known_group {
                        Some((g, e)) => (g.clone(), *e),
                        None => {
                            self.fail(now, "recovery_timeout");
                            return;
                        }
                    },
                };
                self.sign_waits
                    .retain(|_, (_, p)| !matches!(p, SignPurpose::ReAdd { .. }));
                *self
                    .counters
                    .recoveries
                    .entry(InvalidReason::RecoveryTimeout.as_str())
                    .or_insert(0) += 1;
                self.request_readd_package(InvalidReason::RecoveryTimeout, group_id, epoch);
            }
            SessionState::Resetting { new_group_id, .. } => {
                if r.tries >= self.cfg.recovery_watchdog_tries {
                    self.fail(now, "recovery_timeout");
                    return;
                }
                let gid = new_group_id.clone();
                self.recovery = Some(Recovery {
                    reason: "reset",
                    since: now,
                    tries: r.tries + 1,
                });
                self.sign_waits
                    .retain(|_, (_, p)| !matches!(p, SignPurpose::Reset { .. }));
                self.request_reset_package(gid);
            }
            SessionState::Joining => {
                self.recovery = None;
                self.rebind_on_next_invalid = true;
                self.start_readd(now, InvalidReason::StateLost);
            }
            _ => self.recovery = None,
        }
    }

    /// Fails hard: `Left{Failed(reason)}`, status `Failed{reason}`.
    pub(super) fn fail(&mut self, now: Millis, reason: &'static str) {
        self.teardown(now, LeftReason::Failed(reason));
        self.set_status(E2eeStatus::Failed { reason });
    }

    fn fail_now(&mut self, reason: &'static str) {
        self.fail(0, reason);
    }
}

#[cfg(feature = "testing")]
impl CallSession {
    /// Test hook: the app restarted mid-call and lost every MLS secret of the call (the SDK
    /// keeps its keys); the session re-adds itself (`invalid`, `state_lost`).
    pub fn lose_state_for_test(&mut self, now: Millis) {
        let known = match (self.group_id(), self.mls_epoch()) {
            (Some(g), Some(e)) => Some((g, e)),
            _ => self.known_group.clone(),
        };
        self.known_group = known;
        self.group = None;
        self.signer = None;
        self.pending = None;
        self.own_commit = None;
        self.recovery = None;
        if matches!(self.state, SessionState::Resyncing { .. }) {
            self.state = SessionState::Joining;
        }
        self.start_readd(now, InvalidReason::StateLost);
    }
}
