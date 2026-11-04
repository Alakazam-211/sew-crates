//! Roster, anomaly reports and the `report` route's client limits.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use openmls::prelude::Proposal;
use sew_proto::e2ee::{B64, KeyReportRequest, ReportDetails, ReportKind};

use super::{CallSession, Output, Purpose};
use crate::identity::check_leaf;
use crate::ids::{Epoch, LkIdentity, MemberId, Millis};
use crate::roster::{RosterEntry, RosterState};
use crate::wire::KeyRequest;

const HOUR_MS: u64 = 3_600_000;

/// What the roster watch remembers between inputs.
#[derive(Debug, Default)]
pub(super) struct RosterWatch {
    media_first_seen: BTreeMap<LkIdentity, Millis>,
    media_reported: BTreeSet<LkIdentity>,
    voice_known: bool,
    voice_members: BTreeSet<MemberId>,
    voice_missing: BTreeMap<u32, Millis>,
    voice_reported: BTreeSet<u32>,
    buckets: BTreeMap<String, VecDeque<Millis>>,
    slot_bucket: VecDeque<Millis>,
    once: BTreeSet<(String, Option<u32>, Epoch)>,
}

impl RosterWatch {
    /// The next time a roster anomaly comes due.
    pub(super) fn next_due(&self, grace: u64) -> Option<Millis> {
        let media = self
            .media_first_seen
            .iter()
            .filter(|(id, _)| !self.media_reported.contains(*id))
            .map(|(_, t)| t + grace);
        let voice = self
            .voice_missing
            .iter()
            .filter(|(l, _)| !self.voice_reported.contains(*l))
            .map(|(_, t)| t + grace);
        media.chain(voice).min()
    }
}

fn allow(bucket: &mut VecDeque<Millis>, now: Millis, max: u32) -> bool {
    while bucket
        .front()
        .is_some_and(|t| now.saturating_sub(*t) >= HOUR_MS)
    {
        bucket.pop_front();
    }
    if bucket.len() >= max as usize {
        return false;
    }
    bucket.push_back(now);
    true
}

impl CallSession {
    /// Rebuilds the roster from the verified tree, the bindings and the pending proposals;
    /// emits `Output::Roster` when it changed.
    pub(super) fn rebuild_roster(&mut self) {
        let mut out = Vec::new();
        if let Some(group) = self.group.as_ref() {
            let leaving: BTreeSet<u32> = group
                .pending_proposals()
                .filter_map(|q| match q.proposal() {
                    Proposal::Remove(r) => Some(r.removed().u32()),
                    _ => None,
                })
                .collect();
            for m in group.members() {
                let leaf = m.index.u32();
                let (Some(id), Some(b)) = (self.leaf_ids.get(&leaf), self.bindings.get(&leaf))
                else {
                    continue;
                };
                out.push(RosterEntry {
                    leaf_index: Some(leaf),
                    lk_identity: b.lk_identity.clone(),
                    member_id: id.member_id,
                    device_id: id.device_id,
                    platform: id.platform,
                    label: id.label.clone(),
                    state: if leaving.contains(&leaf) {
                        RosterState::Leaving
                    } else {
                        RosterState::Active
                    },
                });
            }
            let lctx = self.leaf_ctx(self.active.as_ref().map_or(0, |a| a.since));
            for q in group.pending_proposals() {
                let Proposal::Add(a) = q.proposal() else {
                    continue;
                };
                let kp = a.key_package();
                let Ok(r) = kp.hash_ref(openmls_traits::OpenMlsProvider::crypto(&self.provider))
                else {
                    continue;
                };
                let Some(b) = self.add_bindings.get(r.as_slice()) else {
                    continue;
                };
                let leaf = kp.leaf_node();
                let mut ctx = lctx;
                // Lifetimes and the clock were checked at Add time; only the identity is read.
                ctx.now = u64::MAX / 2;
                ctx.skew_s = u64::MAX / 4;
                let Ok(id) = check_leaf(leaf.credential(), leaf.signature_key().as_slice(), &ctx)
                else {
                    continue;
                };
                out.push(RosterEntry {
                    leaf_index: None,
                    lk_identity: b.lk_identity.clone(),
                    member_id: id.member_id,
                    device_id: id.device_id,
                    platform: id.platform,
                    label: id.label,
                    state: RosterState::Joining,
                });
            }
        }
        if out != self.roster {
            self.roster = out;
            self.outputs.push_back(Output::Roster);
        }
    }

    /// Whether `id` is an `Active` roster entry (not joining, not leaving).
    pub(super) fn roster_active(&self, id: &LkIdentity) -> bool {
        self.roster
            .iter()
            .any(|e| &e.lk_identity == id && e.state == RosterState::Active)
    }

    pub(super) fn on_media_participants(
        &mut self,
        now: Millis,
        ids: Vec<LkIdentity>,
    ) -> Result<(), crate::error::SessionError> {
        let present: BTreeSet<LkIdentity> = ids.into_iter().collect();
        self.watch
            .media_first_seen
            .retain(|id, _| present.contains(id));
        for id in present {
            if id == self.me || self.is_keyed(&id) || self.rec.identity() == Some(&id) {
                continue;
            }
            self.watch.media_first_seen.entry(id).or_insert(now);
        }
        Ok(())
    }

    pub(super) fn on_voice_members(
        &mut self,
        now: Millis,
        members: Vec<MemberId>,
    ) -> Result<(), crate::error::SessionError> {
        self.watch.voice_known = true;
        self.watch.voice_members = members.into_iter().collect();
        self.refresh_voice_missing(now);
        Ok(())
    }

    fn refresh_voice_missing(&mut self, now: Millis) {
        if !self.watch.voice_known {
            return;
        }
        let own = self.own_leaf();
        let mut missing = BTreeMap::new();
        for (leaf, id) in &self.leaf_ids {
            if Some(*leaf) == own || self.watch.voice_members.contains(&id.member_id) {
                continue;
            }
            let since = self.watch.voice_missing.get(leaf).copied().unwrap_or(now);
            missing.insert(*leaf, since);
        }
        self.watch.voice_missing = missing;
    }

    /// Sends the roster anomalies that came due (each after the 5 s joining window).
    pub(super) fn watch_roster(&mut self, now: Millis) {
        let grace = self.cfg.roster_grace_ms;
        self.refresh_voice_missing(now);
        let due_media: Vec<LkIdentity> = self
            .watch
            .media_first_seen
            .iter()
            .filter(|(id, t)| now >= **t + grace && !self.watch.media_reported.contains(*id))
            .map(|(id, _)| id.clone())
            .collect();
        for id in due_media {
            self.watch.media_reported.insert(id.clone());
            if self.is_keyed(&id) || self.rec.identity() == Some(&id) {
                continue;
            }
            self.report(now, ReportKind::EngineParticipantWithoutLeaf, None, None);
        }
        let due_voice: Vec<u32> = self
            .watch
            .voice_missing
            .iter()
            .filter(|(l, t)| now >= **t + grace && !self.watch.voice_reported.contains(*l))
            .map(|(l, _)| *l)
            .collect();
        for leaf in due_voice {
            self.watch.voice_reported.insert(leaf);
            self.report(now, ReportKind::LeafWithoutVoiceState, Some(leaf), None);
        }
    }

    /// `report`, within the client limits: roster anomalies ≤ 10 per hour per leaf; slot signals
    /// one per (subject leaf, epoch) and ≤ 10 per hour in their own bucket; engine reports once
    /// per kind per epoch and ≤ 10 per hour.
    pub(super) fn report(
        &mut self,
        now: Millis,
        kind: ReportKind,
        leaf: Option<u32>,
        details: Option<ReportDetails>,
    ) {
        let (gid, epoch) = match (self.group_id(), self.mls_epoch()) {
            (Some(g), Some(e)) => (g, e),
            _ => match &self.known_group {
                Some((g, e)) => (g.clone(), *e),
                None => return,
            },
        };
        let max = self.cfg.report_max_per_hour;
        let allowed = if kind.is_slot_signal() {
            let once = (kind.as_str().to_owned(), leaf, epoch);
            if self.watch.once.contains(&once) {
                false
            } else if allow(&mut self.watch.slot_bucket, now, max) {
                self.watch.once.insert(once);
                true
            } else {
                false
            }
        } else if kind.is_engine() {
            let once = (kind.as_str().to_owned(), leaf, epoch);
            if self.watch.once.contains(&once) {
                false
            } else {
                let b = self
                    .watch
                    .buckets
                    .entry(format!("engine:{}", kind.as_str()))
                    .or_default();
                if allow(b, now, max) {
                    self.watch.once.insert(once);
                    true
                } else {
                    false
                }
            }
        } else {
            let key = match leaf {
                Some(l) => format!("leaf:{l}"),
                None => format!("kind:{}", kind.as_str()),
            };
            let b = self.watch.buckets.entry(key).or_default();
            allow(b, now, max)
        };
        if !allowed {
            self.counters.reports_limited += 1;
            return;
        }
        self.counters.reports_sent += 1;
        self.send(
            Purpose::Report,
            KeyRequest::Report(
                self.call_id.clone(),
                KeyReportRequest {
                    join_id: self.join_id.clone(),
                    group_id: B64::new(gid),
                    epoch,
                    kind,
                    leaf,
                    details,
                },
            ),
        );
    }

    /// After `state` answered `in_sync` for an undecryptable peer: `report` `peer_undecryptable`.
    pub(super) fn report_peer(&mut self, now: Millis, id: &LkIdentity) {
        let Some((since, failures, reason, missing)) = self.undecryptable.get(id).copied() else {
            return;
        };
        let Some(a) = &self.active else {
            return;
        };
        let expected = a.slot.get();
        let start = since.max(a.since + self.params.retire_after_ms);
        let leaf = self
            .bindings
            .iter()
            .find(|(_, e)| &e.lk_identity == id)
            .map(|(l, _)| *l);
        let details = ReportDetails {
            expected_slot: Some(expected),
            since_ms: Some(now.saturating_sub(start)),
            failures: Some(failures),
            reason: Some(reason.as_str().to_owned()),
            missing_index: missing.map(|s| s.get()),
            ..Default::default()
        };
        self.report(now, ReportKind::PeerUndecryptable, leaf, Some(details));
    }
}
