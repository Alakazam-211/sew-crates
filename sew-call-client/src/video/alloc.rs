//! The send-side rate split: str0m's bandwidth estimate shared out over the layers of the
//! published screen share and camera.
//!
//! - **Budget:** `0.9 × estimate − 48 kbit/s` (the audio's reserve).
//! - **Order:** screen-share layers first, then the camera's, lowest layer first. A publication
//!   with a backup codec has two lanes (the primary, then the backup) in one budget: every
//!   lane's lowest layer comes first (primary, then backup), then the next layer of each, and
//!   so on. Each layer gets `min(max, what is left)`; it runs only with at least 60% of its
//!   `max` and at least its size's floor ([`min_bitrate_bps`]: a 720p layer at 300 kbit/s
//!   would only drop frames), otherwise it and every layer above it in its lane are paused
//!   here (apart from a dynacast pause, which the caller tracks: a layer nobody wants takes no
//!   budget and holds no layer back).
//! - **Hysteresis:** a cut applies at once. A layer starts again only after the estimate has
//!   covered it with 15% to spare for 3 s, and at most one raise every 2 s. A publication's
//!   first split applies at once (the camera does not wait 3 s to start), and so does an
//!   exempt layer (the backup's lowest layer when the backup starts: a subscriber without the
//!   primary codec waits for nothing else; a primary layer above it is cut first).
//! - **Kept layers:** each lane's lowest wanted layer is its subscribers' only video (the
//!   primary's for those who decode the primary codec, the backup's for a VP8-only subscriber),
//!   so while anyone wants the lane it is never paused here: it gets what is left, and at least
//!   what it needs to run (60% of its cap or its floor, at most its cap), whatever the estimate.
//!   libwebrtc does the same: it never suspends a sender's lowest simulcast layer, and when the
//!   estimate is below the sum of the senders' minimums every sender still gets its minimum.
//!   The estimate can't stand in for the path: str0m's estimate of an application-limited
//!   sender sits near what it sends (with the primary paused by dynacast, a few hundred kbit/s
//!   on an 80 ms TURN-TLS path), right at a layer's start threshold. Paused, the layer would
//!   send nothing to raise it, and its subscribers would wait on probes alone (seconds, or
//!   never). With both lanes wanted and an estimate short of both minimums, neither lane goes
//!   first: the primary's lowest layer takes the budget first as before, and the backup's runs
//!   at what it needs beyond it; both run at their floors, over the estimate, rather than
//!   leave one codec's subscribers with no video. The layers above follow the estimate and the
//!   hysteresis as before, and the caps (heat, low power) still apply: a kept layer runs at
//!   most at its cap, which the publisher never sets below its floor.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

/// The audio's share of the estimate, bit/s.
pub(crate) const AUDIO_RESERVE_BPS: u64 = 48_000;
/// How long the estimate must cover a layer (with [`RAISE_MARGIN_PCT`]) before it starts.
pub(crate) const RAISE_HOLD: Duration = Duration::from_secs(3);
/// The shortest time between two raises.
pub(crate) const RAISE_EVERY: Duration = Duration::from_secs(2);
/// The margin a raise needs, percent of the layer.
pub(crate) const RAISE_MARGIN_PCT: u64 = 115;
/// A layer runs only with at least this share of its cap, percent.
pub(crate) const MIN_SHARE_PCT: u64 = 60;

/// The lowest useful bitrate of a 720p (or larger) layer, bit/s. Below it a software VP8
/// encoder sits at its top quantizer and drops frames to hold the rate, so the layer is paused
/// instead of starved.
pub(crate) const MIN_720P_BPS: u64 = 1_000_000;
/// The lowest useful bitrate of a 360p layer (up to 720p), bit/s.
pub(crate) const MIN_360P_BPS: u64 = 400_000;
/// The lowest useful bitrate of a 180p layer (up to 360p) and of anything smaller, bit/s.
pub(crate) const MIN_180P_BPS: u64 = 150_000;

/// A layer's lowest useful bitrate by its height ([`MIN_720P_BPS`], [`MIN_360P_BPS`],
/// [`MIN_180P_BPS`]).
pub(crate) fn min_bitrate_bps(height: u32) -> u64 {
    match height {
        720.. => MIN_720P_BPS,
        360.. => MIN_360P_BPS,
        _ => MIN_180P_BPS,
    }
}

/// A layer: (publication, lane: 0 the primary codec, 1 the backup; index from the lowest).
pub(crate) type Key = (u64, usize, usize);

/// The split's order: screen first, then by publication, layer index, lane.
fn order_key(l: &AllocLayer) -> (bool, u64, usize, usize) {
    (!l.screen, l.key.0, l.key.2, l.key.1)
}

/// One layer as the split sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AllocLayer {
    pub(crate) key: Key,
    /// A screen share's layer (served first).
    pub(crate) screen: bool,
    /// Its cap, bit/s.
    pub(crate) max_bps: u64,
    /// Someone subscribes to it (dynacast; always with dynacast off).
    pub(crate) wanted: bool,
    /// Its lowest useful rate, bit/s ([`min_bitrate_bps`]).
    pub(crate) min_bps: u64,
    /// Starts on the next split that gives it 60% of its cap, without the raise hysteresis
    /// (the backup's lowest wanted layer as the backup starts).
    pub(crate) exempt: bool,
    /// Runs whatever the estimate, at least at what it [needs](AllocLayer::needs) (at most its
    /// cap): each running lane's lowest wanted layer (primary and backup), its subscribers'
    /// only video.
    pub(crate) kept: bool,
}

impl AllocLayer {
    /// What the layer needs to run: the stricter of 60% of its cap and its floor.
    fn needs(&self) -> u64 {
        (self.max_bps * MIN_SHARE_PCT)
            .div_ceil(100)
            .max(self.min_bps)
    }
}

/// The split of `available` bit/s over `layers`: the running layers and their rates.
pub(crate) fn split(layers: &[AllocLayer], available: u64) -> BTreeMap<Key, u64> {
    let mut order: Vec<&AllocLayer> = layers.iter().collect();
    order.sort_by_key(|l| order_key(l));
    let mut left = available;
    let mut out = BTreeMap::new();
    let mut blocked: BTreeSet<(u64, usize)> = BTreeSet::new();
    for l in order {
        if !l.wanted || blocked.contains(&(l.key.0, l.key.1)) {
            continue;
        }
        let mut give = l.max_bps.min(left);
        if l.kept {
            give = give.max(l.needs().min(l.max_bps));
        } else if give < l.needs() || give == 0 {
            blocked.insert((l.key.0, l.key.1));
            continue;
        }
        left = left.saturating_sub(give);
        out.insert(l.key, give);
    }
    out
}

/// The layers `available` bit/s covers with the raise margin: in the split's order, each at
/// its full cap and with [`RAISE_MARGIN_PCT`] of it left when it is reached.
fn margin_fits(layers: &[AllocLayer], available: u64) -> BTreeSet<Key> {
    let mut order: Vec<&AllocLayer> = layers.iter().collect();
    order.sort_by_key(|l| order_key(l));
    let mut left = available;
    let mut out = BTreeSet::new();
    let mut blocked: BTreeSet<(u64, usize)> = BTreeSet::new();
    for l in order {
        if !l.wanted || blocked.contains(&(l.key.0, l.key.1)) {
            continue;
        }
        if left < l.max_bps * RAISE_MARGIN_PCT / 100 || l.max_bps < l.min_bps {
            blocked.insert((l.key.0, l.key.1));
            continue;
        }
        left -= l.max_bps;
        out.insert(l.key);
    }
    out
}

/// The budget an estimate leaves for video.
pub(crate) fn budget(estimate_bps: u64) -> u64 {
    (estimate_bps * 9 / 10).saturating_sub(AUDIO_RESERVE_BPS)
}

/// The allocator's state between estimates.
#[derive(Debug, Default)]
pub(crate) struct Allocator {
    /// The layers running (not paused here), whether or not anyone wants them now.
    running: BTreeSet<Key>,
    /// Since when each stopped layer has fitted with the margin.
    fits_since: BTreeMap<Key, Instant>,
    last_raise: Option<Instant>,
    /// Publications that had their first split.
    started: BTreeSet<u64>,
}

impl Allocator {
    /// A new split for `estimate_bps`: the rate of each layer that runs and is wanted now
    /// (absent: paused, here or by dynacast).
    pub(crate) fn update(
        &mut self,
        now: Instant,
        estimate_bps: u64,
        layers: &[AllocLayer],
    ) -> BTreeMap<Key, u64> {
        let pubs: BTreeSet<u64> = layers.iter().map(|l| l.key.0).collect();
        self.running.retain(|k| pubs.contains(&k.0));
        self.fits_since.retain(|k, _| pubs.contains(&k.0));
        self.started.retain(|p| pubs.contains(p));
        let target = split(layers, budget(estimate_bps));
        let margin = margin_fits(layers, budget(estimate_bps));
        // Cuts: a wanted running layer the estimate no longer covers (and every layer above
        // it, which the split left out with it).
        let cut: Vec<Key> = layers
            .iter()
            .filter(|l| l.wanted && self.running.contains(&l.key) && !target.contains_key(&l.key))
            .map(|l| l.key)
            .collect();
        for k in cut {
            self.running.remove(&k);
        }
        // A publication's first split: at once.
        for p in &pubs {
            if self.started.insert(*p) {
                for k in target.keys().filter(|k| k.0 == *p) {
                    self.running.insert(*k);
                }
            }
        }
        // An exempt layer the split covers, and a kept one (the split always covers it): at
        // once (no hold, no spacing, not a raise).
        for l in layers.iter().filter(|l| (l.exempt || l.kept) && l.wanted) {
            if target.contains_key(&l.key) && self.running.insert(l.key) {
                self.fits_since.remove(&l.key);
            }
        }
        // Raises: lowest first, one per RAISE_EVERY, each after RAISE_HOLD with the margin.
        let mut sorted: Vec<&AllocLayer> = layers.iter().collect();
        sorted.sort_by_key(|l| order_key(l));
        for l in sorted {
            if !l.wanted || self.running.contains(&l.key) {
                self.fits_since.remove(&l.key);
                continue;
            }
            if !margin.contains(&l.key) {
                self.fits_since.remove(&l.key);
                continue;
            }
            let since = *self.fits_since.entry(l.key).or_insert(now);
            let below_running = layers
                .iter()
                .filter(|o| {
                    o.key.0 == l.key.0 && o.key.1 == l.key.1 && o.key.2 < l.key.2 && o.wanted
                })
                .all(|o| self.running.contains(&o.key));
            let spaced = self
                .last_raise
                .is_none_or(|t| now.saturating_duration_since(t) >= RAISE_EVERY);
            if below_running && spaced && now.saturating_duration_since(since) >= RAISE_HOLD {
                self.running.insert(l.key);
                self.fits_since.remove(&l.key);
                self.last_raise = Some(now);
            }
        }
        layers
            .iter()
            .filter(|l| l.wanted && self.running.contains(&l.key))
            .map(|l| (l.key, target.get(&l.key).copied().unwrap_or(l.max_bps)))
            .collect()
    }

    /// When a raise could next become due (for the engine's timer).
    pub(crate) fn deadline(&self) -> Option<Instant> {
        let hold = self.fits_since.values().map(|t| *t + RAISE_HOLD).min()?;
        Some(match self.last_raise {
            Some(r) => hold.max(r + RAISE_EVERY),
            None => hold,
        })
    }

    /// Forgets a publication (unpublished).
    pub(crate) fn forget(&mut self, publication: u64) {
        self.running.retain(|k| k.0 != publication);
        self.fits_since.retain(|k, _| k.0 != publication);
        self.started.remove(&publication);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cam(p: u64, caps: &[u64]) -> Vec<AllocLayer> {
        caps.iter()
            .enumerate()
            .map(|(i, m)| AllocLayer {
                key: (p, 0, i),
                screen: false,
                max_bps: *m,
                wanted: true,
                min_bps: 0,
                exempt: false,
                kept: false,
            })
            .collect()
    }

    const Q: u64 = 150_000;
    const H: u64 = 500_000;
    const F: u64 = 1_200_000;

    /// The estimate that leaves `b` for video.
    fn est(b: u64) -> u64 {
        (b + AUDIO_RESERVE_BPS) * 10 / 9 + 1
    }

    #[test]
    fn the_split_serves_screen_first_then_lowest_first_and_stops_below_sixty_percent() {
        let mut layers = cam(1, &[Q, H, F]);
        layers.extend(cam(2, &[300_000, 1_000_000]).into_iter().map(|mut l| {
            l.screen = true;
            l
        }));
        // 300k + 1000k for the screen, then q 150k, h 500k; f would get 300k of 1200k (25%).
        let s = split(&layers, 1_950_000);
        assert_eq!(s.get(&(2, 0, 0)), Some(&300_000));
        assert_eq!(s.get(&(2, 0, 1)), Some(&1_000_000));
        assert_eq!(s.get(&(1, 0, 0)), Some(&Q));
        assert_eq!(s.get(&(1, 0, 1)), Some(&H));
        assert_eq!(s.get(&(1, 0, 2)), None);
        // h at 60% runs (with less than its cap); below 60% it and f stop.
        let s = split(&cam(1, &[Q, H, F]), Q + 300_000);
        assert_eq!(s.get(&(1, 0, 1)), Some(&300_000));
        let s = split(&cam(1, &[Q, H, F]), Q + 299_000 + F);
        assert_eq!(s.get(&(1, 0, 1)), Some(&H));
        let s = split(&cam(1, &[Q, H, F]), Q + 299_999);
        assert_eq!(s.keys().copied().collect::<Vec<_>>(), vec![(1, 0, 0)]);
        // An unwanted layer takes nothing and holds nothing back.
        let mut l = cam(1, &[Q, H, F]);
        l[1].wanted = false;
        let s = split(&l, Q + F);
        assert_eq!(
            s.keys().copied().collect::<Vec<_>>(),
            vec![(1, 0, 0), (1, 0, 2)]
        );
        assert_eq!(budget(1_500_000), 1_302_000);
    }

    #[test]
    fn a_layer_below_its_floor_is_paused_not_starved() {
        assert_eq!(min_bitrate_bps(720), MIN_720P_BPS);
        assert_eq!(min_bitrate_bps(1080), MIN_720P_BPS);
        assert_eq!(min_bitrate_bps(360), MIN_360P_BPS);
        assert_eq!(min_bitrate_bps(540), MIN_360P_BPS);
        assert_eq!(min_bitrate_bps(180), MIN_180P_BPS);
        assert_eq!(min_bitrate_bps(90), MIN_180P_BPS);
        // q 180p, h 360p, f 720p, with their floors.
        let mut layers = cam(1, &[150_000, 500_000, 1_700_000]);
        for (l, h) in layers.iter_mut().zip([180, 360, 720]) {
            l.min_bps = min_bitrate_bps(h);
        }
        // 1.6 Mbit/s for video: f would get 950 kbit/s, 56% of its cap and below 720p's
        // floor: paused, with q and h at their caps.
        let s = split(&layers, 1_600_000);
        assert_eq!(s.get(&(1, 0, 0)), Some(&150_000));
        assert_eq!(s.get(&(1, 0, 1)), Some(&500_000));
        assert_eq!(s.get(&(1, 0, 2)), None, "f paused, not starved");
        // A cap below the floor never runs: f capped at 900 kbit/s gets it all, still paused.
        let mut low = layers.clone();
        low[2].max_bps = 900_000;
        assert_eq!(split(&low, 5_000_000).get(&(1, 0, 2)), None);
        // The floor is stricter than 60%: h at 360p needs 400 of its 500 kbit/s.
        let s = split(&layers, 150_000 + 399_000);
        assert_eq!(s.keys().copied().collect::<Vec<_>>(), vec![(1, 0, 0)]);
        let s = split(&layers, 150_000 + 400_000);
        assert_eq!(s.get(&(1, 0, 1)), Some(&400_000));
        // A low estimate through the allocator: f is never sent at a starved rate.
        let mut a = Allocator::default();
        let r = a.update(Instant::now(), est(1_600_000), &layers);
        assert_eq!(
            r.keys().copied().collect::<Vec<_>>(),
            vec![(1, 0, 0), (1, 0, 1)]
        );
        assert!(r.values().all(|bps| *bps >= MIN_180P_BPS));
    }

    #[test]
    fn cuts_apply_at_once_and_raises_wait_three_seconds_with_margin() {
        let t0 = Instant::now();
        let layers = cam(1, &[Q, H, F]);
        let mut a = Allocator::default();
        // The first split at once: everything fits.
        let r = a.update(t0, est(Q + H + F), &layers);
        assert_eq!(r.len(), 3);
        // The estimate drops: f goes at once.
        let r = a.update(t0 + Duration::from_millis(100), est(Q + H), &layers);
        assert_eq!(
            r.keys().copied().collect::<Vec<_>>(),
            vec![(1, 0, 0), (1, 0, 1)]
        );
        // Back up, without the margin: f stays off however long.
        let t1 = t0 + Duration::from_secs(1);
        let r = a.update(t1, est(Q + H + F), &layers);
        assert_eq!(r.len(), 2);
        let r = a.update(t1 + Duration::from_secs(10), est(Q + H + F), &layers);
        assert_eq!(r.len(), 2, "no margin, no raise");
        // With the margin: f after 3 s, not before.
        let t2 = t1 + Duration::from_secs(11);
        let up = est((Q + H + F) * 116 / 100);
        assert_eq!(a.update(t2, up, &layers).len(), 2);
        assert_eq!(a.deadline(), Some(t2 + RAISE_HOLD));
        assert_eq!(
            a.update(t2 + Duration::from_millis(2_900), up, &layers)
                .len(),
            2
        );
        assert_eq!(a.update(t2 + RAISE_HOLD, up, &layers).len(), 3);
        assert_eq!(a.deadline(), None);
    }

    #[test]
    fn raises_are_spaced_two_seconds_apart_and_go_lowest_first() {
        let t0 = Instant::now();
        let layers = cam(1, &[Q, H, F]);
        let mut a = Allocator::default();
        // Starts with q only.
        let r = a.update(t0, est(Q), &layers);
        assert_eq!(r.len(), 1);
        let up = est((Q + H + F) * 2);
        let t1 = t0 + Duration::from_secs(1);
        a.update(t1, up, &layers);
        // After 3 s: h is raised; f fits too but waits 2 s after h's raise.
        let r = a.update(t1 + RAISE_HOLD, up, &layers);
        assert_eq!(r.len(), 2);
        let r = a.update(t1 + RAISE_HOLD + Duration::from_millis(1_500), up, &layers);
        assert_eq!(r.len(), 2);
        let r = a.update(t1 + RAISE_HOLD + RAISE_EVERY, up, &layers);
        assert_eq!(r.len(), 3);
    }

    /// A camera with a backup: the primary lane's caps, then the backup's (lane 1).
    fn with_backup(p: u64, primary: &[u64], backup: &[u64]) -> Vec<AllocLayer> {
        let mut out = cam(p, primary);
        out.extend(backup.iter().enumerate().map(|(i, m)| AllocLayer {
            key: (p, 1, i),
            screen: false,
            max_bps: *m,
            wanted: true,
            min_bps: 0,
            exempt: false,
            kept: false,
        }));
        out
    }

    #[test]
    fn both_lanes_share_one_budget_every_lowest_layer_first() {
        // Primary q h f, backup q h: q, backup q, h, backup h, f.
        let layers = with_backup(1, &[Q, H, F], &[Q, H]);
        let s = split(&layers, Q + Q + H + H + F);
        assert_eq!(s.len(), 5);
        // Short of f: f is the one left out.
        let s = split(&layers, Q + Q + H + H);
        assert_eq!(
            s.keys().copied().collect::<Vec<_>>(),
            vec![(1, 0, 0), (1, 0, 1), (1, 1, 0), (1, 1, 1)]
        );
        // Room for both lowest layers and h: the backup's q comes before the primary's h.
        let s = split(&layers, Q + Q + H);
        assert_eq!(
            s.keys().copied().collect::<Vec<_>>(),
            vec![(1, 0, 0), (1, 0, 1), (1, 1, 0)]
        );
        let s = split(&layers, Q + Q + 100_000);
        assert_eq!(
            s.keys().copied().collect::<Vec<_>>(),
            vec![(1, 0, 0), (1, 1, 0)],
            "the primary's h gets 100k of 500k: paused; the backup's q runs"
        );
        // A lane that stops holds back only its own layers: the backup's q, its cap below its
        // floor, never runs and takes nothing; the primary's h still runs.
        let mut l = with_backup(1, &[Q, H], &[400_000, H]);
        l[2].min_bps = 450_000;
        let s = split(&l, Q + H + 400_000);
        assert_eq!(
            s.keys().copied().collect::<Vec<_>>(),
            vec![(1, 0, 0), (1, 0, 1)],
            "the backup's q stops the backup lane only"
        );
    }

    #[test]
    fn the_backups_lowest_layer_starts_at_once_and_cuts_the_primarys_top() {
        let t0 = Instant::now();
        let mut a = Allocator::default();
        // The primary runs q h f on an estimate that just covers them.
        let primary = cam(1, &[Q, H, F]);
        let e = est(Q + H + F);
        assert_eq!(a.update(t0, e, &primary).len(), 3);
        // The backup starts (q and h wanted, q exempt): the next split runs its q at once and
        // cuts the primary's f; its h waits for the hysteresis.
        let mut layers = with_backup(1, &[Q, H, F], &[Q, H]);
        layers[3].exempt = true;
        let t1 = t0 + Duration::from_secs(10);
        let r = a.update(t1, e, &layers);
        assert_eq!(
            r.keys().copied().collect::<Vec<_>>(),
            vec![(1, 0, 0), (1, 0, 1), (1, 1, 0)]
        );
        // Without the exemption the backup's q would wait 3 s (and the margin).
        let mut b = Allocator::default();
        assert_eq!(b.update(t0, e, &primary).len(), 3);
        let plain = with_backup(1, &[Q, H, F], &[Q, H]);
        let r = b.update(t1, e, &plain);
        assert!(!r.contains_key(&(1, 1, 0)), "{r:?}");
        // The backup's h with room: only after RAISE_HOLD with the margin.
        layers[3].exempt = false;
        let up = est((Q + H + F + Q + H) * 2);
        let t2 = t1 + Duration::from_secs(1);
        assert!(!a.update(t2, up, &layers).contains_key(&(1, 1, 1)));
        let r = a.update(t2 + RAISE_HOLD, up, &layers);
        assert!(
            r.contains_key(&(1, 1, 1)) || r.contains_key(&(1, 0, 2)),
            "{r:?}"
        );
        let r = a.update(t2 + RAISE_HOLD + RAISE_EVERY, up, &layers);
        assert_eq!(r.len(), 5, "{r:?}");
        // Exempt alone (not kept), a layer the budget can't cover at 60% does not run.
        let mut c = Allocator::default();
        let mut tight = with_backup(1, &[Q], &[Q]);
        tight[1].exempt = true;
        let r = c.update(t0, est(Q + Q / 2), &tight);
        assert_eq!(r.keys().copied().collect::<Vec<_>>(), vec![(1, 0, 0)]);
    }

    /// The case that starved a VP8-only subscriber on an 80 ms TURN-TLS path: the primary
    /// paused by dynacast (only VP8 viewers), the estimate near what the backup's q alone sends
    /// (~200–300 kbit/s), below what q needs once the audio's reserve is taken.
    #[test]
    fn the_backups_kept_layer_runs_whatever_the_estimate() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        // Primary q h f nobody wants; backup q (180p, its floor 150k) and h, q kept.
        let mut layers = with_backup(1, &[Q, H, F], &[Q, H]);
        for l in &mut layers[..3] {
            l.wanted = false;
        }
        layers[3].min_bps = MIN_180P_BPS;
        layers[4].min_bps = MIN_360P_BPS;
        let e = 200_000;
        assert!(budget(e) < layers[3].needs(), "the estimate can't cover q");
        // Without the keep, the backup's start runs nothing on that estimate (exempt or not):
        // no frame, so nothing to raise the estimate with.
        let mut old = layers.clone();
        old[3].exempt = true;
        let mut a = Allocator::default();
        assert!(a.update(t0, e, &old).is_empty());
        assert!(a.update(at(10), e, &old).is_empty());
        // Kept: q runs at once at what it needs (its floor here), h stays off.
        layers[3].exempt = true;
        layers[3].kept = true;
        let mut a = Allocator::default();
        let r = a.update(t0, e, &layers);
        assert_eq!(r.into_iter().collect::<Vec<_>>(), vec![((1, 1, 0), Q)]);
        // The estimate falls further (even to nothing): q is not cut.
        layers[3].exempt = false;
        for (s, low_est) in [(1, 120_000), (2, 0), (3, 60_000)] {
            let r = a.update(at(s), low_est, &layers);
            assert_eq!(r.into_iter().collect::<Vec<_>>(), vec![((1, 1, 0), Q)]);
        }
        // q at its cap when the budget has more; h after the hysteresis as before.
        let up = est((Q + H) * 2);
        assert_eq!(a.update(at(4), up, &layers).len(), 1);
        let r = a.update(at(4) + RAISE_HOLD, up, &layers);
        assert_eq!(
            r.into_iter().collect::<Vec<_>>(),
            vec![((1, 1, 0), Q), ((1, 1, 1), H)]
        );
        // A cap below the floor (heat): kept, it runs at its cap.
        let mut low = layers.clone();
        low[3].max_bps = 100_000;
        assert_eq!(split(&low, 0).get(&(1, 1, 0)), Some(&100_000));
        // With the primary wanted too: the primary's q takes the budget first, the kept layer
        // still runs at what it needs, and nothing above either.
        for l in &mut layers[..3] {
            l.wanted = true;
        }
        let s = split(&layers, Q + 50_000);
        assert_eq!(
            s.into_iter().collect::<Vec<_>>(),
            vec![((1, 0, 0), Q), ((1, 1, 0), Q)]
        );
    }

    /// A primary camera (q 180p, h 360p, f 720p, with their floors), its q kept.
    fn primary_kept() -> Vec<AllocLayer> {
        let mut layers = cam(1, &[Q, H, F]);
        for (l, h) in layers.iter_mut().zip([180, 360, 720]) {
            l.min_bps = min_bitrate_bps(h);
        }
        layers[0].kept = true;
        layers
    }

    /// The primary's starvation: a primary-only publication (no backup) on an estimate below
    /// what its q needs, a subscriber wanting it. Unkept, the split ran nothing (no frame to
    /// raise the estimate with); kept, q runs at what it needs.
    #[test]
    fn the_primarys_kept_layer_runs_whatever_the_estimate() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let layers = primary_kept();
        let e = 200_000;
        assert!(budget(e) < layers[0].needs(), "the estimate can't cover q");
        // Unkept: nothing runs, on the first split or any later one.
        let mut old = layers.clone();
        old[0].kept = false;
        let mut a = Allocator::default();
        assert!(a.update(t0, e, &old).is_empty());
        assert!(a.update(at(10_000), e, &old).is_empty());
        // Kept: q at once at what it needs, on the first split and on a later one (a primary
        // running nothing, paused by dynacast, until a subscriber came).
        let mut a = Allocator::default();
        let r = a.update(t0, e, &layers);
        assert_eq!(r.into_iter().collect::<Vec<_>>(), vec![((1, 0, 0), Q)]);
        let mut b = Allocator::default();
        let mut nobody = layers.clone();
        for l in &mut nobody {
            l.wanted = false;
        }
        assert!(b.update(t0, e, &nobody).is_empty());
        let r = b.update(at(500), e, &layers);
        assert_eq!(r.into_iter().collect::<Vec<_>>(), vec![((1, 0, 0), Q)]);
        // Lower still, even no estimate: q is never cut.
        for (ms, low) in [(1_000, 100_000), (2_000, 0), (3_000, 50_000)] {
            let r = a.update(at(ms), low, &layers);
            assert_eq!(r.into_iter().collect::<Vec<_>>(), vec![((1, 0, 0), Q)]);
        }
        // Only h wanted (q not): h is the lowest wanted, kept at its floor.
        let mut h_only = layers.clone();
        h_only[0].wanted = false;
        h_only[0].kept = false;
        h_only[1].kept = true;
        assert_eq!(
            split(&h_only, 0).into_iter().collect::<Vec<_>>(),
            vec![((1, 0, 1), MIN_360P_BPS)]
        );
    }

    #[test]
    fn above_the_primarys_kept_layer_the_estimate_and_the_hysteresis_rule() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let layers = primary_kept();
        let mut a = Allocator::default();
        // Starts low: q only.
        let r = a.update(t0, est(Q / 2), &layers);
        assert_eq!(r.keys().copied().collect::<Vec<_>>(), vec![(1, 0, 0)]);
        // Room for h without the margin: h stays off however long.
        let no_margin = est(Q + H);
        a.update(at(1_000), no_margin, &layers);
        let r = a.update(at(20_000), no_margin, &layers);
        assert_eq!(r.len(), 1, "no margin, no raise: {r:?}");
        // With the margin for h (not f): h after RAISE_HOLD, not before.
        let up = est((Q + H) * 116 / 100);
        assert_eq!(a.update(at(21_000), up, &layers).len(), 1);
        assert_eq!(a.update(at(23_900), up, &layers).len(), 1);
        let r = a.update(at(24_000), up, &layers);
        assert_eq!(
            r.into_iter().collect::<Vec<_>>(),
            vec![((1, 0, 0), Q), ((1, 0, 1), H)]
        );
        // With the margin for f too: f only after its own hold, and RAISE_EVERY after h's.
        let all = est((Q + H + F) * 2);
        assert_eq!(a.update(at(25_000), all, &layers).len(), 2);
        assert_eq!(a.update(at(27_900), all, &layers).len(), 2);
        assert_eq!(a.update(at(28_000), all, &layers).len(), 3);
        // The estimate collapses: h and f are cut at once, q stays at what it needs.
        let r = a.update(at(28_100), est(Q / 3), &layers);
        assert_eq!(r.into_iter().collect::<Vec<_>>(), vec![((1, 0, 0), Q)]);
        // A budget that covers q but not h's floor (400k): h stays cut.
        let r = a.update(at(40_000), est(Q + 399_000), &layers);
        assert_eq!(r.len(), 1, "{r:?}");
    }

    /// Heat and low power cap layers before the split (the publisher never takes a kept layer
    /// below its floor, but a policy cap below the floor stands): a kept layer runs at most at
    /// its cap, and a capped layer above it still pauses below its floor.
    #[test]
    fn a_kept_primary_layer_stays_under_its_caps() {
        let t0 = Instant::now();
        // q's cap below its floor (a policy cap of 100k): kept, it runs at its cap whatever
        // the estimate; unkept, it never ran at all.
        let mut low = primary_kept();
        low[0].max_bps = 100_000;
        assert_eq!(split(&low, 0).get(&(1, 0, 0)), Some(&100_000));
        assert_eq!(split(&low, 5_000_000).get(&(1, 0, 0)), Some(&100_000));
        let mut unkept = low.clone();
        unkept[0].kept = false;
        assert!(!split(&unkept, 5_000_000).contains_key(&(1, 0, 0)));
        // Heat at half the rate on h and f (q kept at its cap, its floor): h's 250k is below
        // its 400k floor and f's 850k below its 1 Mbit/s, so on any estimate only q runs.
        let mut hot = primary_kept();
        hot[1].max_bps = H / 2;
        hot[2].max_bps = F / 2;
        let mut a = Allocator::default();
        let r = a.update(t0, est(10_000_000), &hot);
        assert_eq!(r.into_iter().collect::<Vec<_>>(), vec![((1, 0, 0), Q)]);
        // One layer under heat (max_layers 1: the publisher hands over q alone): q runs on any
        // estimate, at its cap.
        let one = &primary_kept()[..1];
        let mut b = Allocator::default();
        for (s, e) in [(0, 0), (1, est(10_000_000)), (2, 30_000)] {
            let r = b.update(t0 + Duration::from_secs(s), e, one);
            assert_eq!(r.into_iter().collect::<Vec<_>>(), vec![((1, 0, 0), Q)]);
        }
    }

    /// Both lanes wanted (H.264 and VP8-only subscribers), each lowest layer kept, on an
    /// estimate that covers one minimum or none: both run at what they need (libwebrtc gives
    /// every sender its minimum below the sum of minimums); nothing above them runs.
    #[test]
    fn both_kept_layers_run_at_their_floors_when_the_estimate_covers_one() {
        let t0 = Instant::now();
        let mut layers = with_backup(1, &[Q, H, F], &[Q, H]);
        for (l, h) in layers.iter_mut().zip([180, 360, 720, 180, 360]) {
            l.min_bps = min_bitrate_bps(h);
        }
        layers[0].kept = true;
        layers[3].kept = true;
        let both = vec![((1, 0, 0), Q), ((1, 1, 0), Q)];
        // One minimum, a little more, none at all: both q at their needs.
        for b in [Q, Q + 50_000, 0] {
            assert_eq!(
                split(&layers, b).into_iter().collect::<Vec<_>>(),
                both,
                "{b}"
            );
        }
        // Through the allocator: both at once, neither cut as the estimate falls.
        let mut a = Allocator::default();
        let r = a.update(t0, est(Q), &layers);
        assert_eq!(r.into_iter().collect::<Vec<_>>(), both);
        let r = a.update(t0 + Duration::from_secs(1), 0, &layers);
        assert_eq!(r.into_iter().collect::<Vec<_>>(), both);
        // Room for both and the primary's h: h runs after the hysteresis; the backup's h waits
        // for more.
        let up = est((Q + Q + H) * 116 / 100);
        let t1 = t0 + Duration::from_secs(2);
        assert_eq!(a.update(t1, up, &layers).len(), 2);
        let r = a.update(t1 + RAISE_HOLD, up, &layers);
        assert_eq!(
            r.keys().copied().collect::<Vec<_>>(),
            vec![(1, 0, 0), (1, 0, 1), (1, 1, 0)]
        );
    }

    #[test]
    fn a_dynacast_pause_is_not_a_cut_and_resumes_at_once() {
        let t0 = Instant::now();
        let mut layers = cam(1, &[Q, H, F]);
        let mut a = Allocator::default();
        let e = est(Q + H + F);
        assert_eq!(a.update(t0, e, &layers).len(), 3);
        layers[2].wanted = false;
        assert_eq!(a.update(t0 + Duration::from_secs(1), e, &layers).len(), 2);
        layers[2].wanted = true;
        assert_eq!(
            a.update(t0 + Duration::from_secs(2), e, &layers).len(),
            3,
            "wanted again within the estimate: no hysteresis"
        );
        a.forget(1);
        assert_eq!(
            a.update(t0 + Duration::from_secs(3), est(Q), &layers).len(),
            1
        );
    }
}
