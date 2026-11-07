//! Keyframe requests of one received video track, rate-limited: at most one PLI per 300 ms; if
//! no keyframe arrives within 1 s of the first PLI, one FIR; a track still without a keyframe
//! 5 s after the FIR is stalled (reported once), and PLIs go on at one a second until a keyframe
//! comes.

use std::time::{Duration, Instant};

/// The shortest time between two PLIs.
pub(crate) const PLI_INTERVAL: Duration = Duration::from_millis(300);
/// No keyframe this long after the first PLI: one FIR.
pub(crate) const FIR_AFTER: Duration = Duration::from_secs(1);
/// No keyframe this long after the FIR: the track is stalled.
pub(crate) const STALLED_AFTER: Duration = Duration::from_secs(5);
/// PLIs while stalled.
pub(crate) const STALLED_PLI_INTERVAL: Duration = Duration::from_secs(1);

/// A request to send now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyframeAsk {
    /// Picture Loss Indication.
    Pli,
    /// Full Intra Request.
    Fir,
}

/// What [`KeyframeLimiter::poll`] says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Due {
    /// Send this request now.
    pub(crate) ask: Option<KeyframeAsk>,
    /// The track just became stalled (reported once per episode).
    pub(crate) stalled: bool,
}

/// One track's keyframe-request state.
#[derive(Debug, Default)]
pub(crate) struct KeyframeLimiter {
    /// A request waits for the rate limit.
    wanted: bool,
    last_pli: Option<Instant>,
    /// The first PLI of the current wait for a keyframe.
    first_pli: Option<Instant>,
    fir_at: Option<Instant>,
    stalled: bool,
    /// PLIs and FIRs sent (counters).
    pub(crate) plis: u64,
    pub(crate) firs: u64,
}

impl KeyframeLimiter {
    pub(crate) fn new() -> KeyframeLimiter {
        KeyframeLimiter::default()
    }

    /// Something needs a keyframe (a new sink, a loss, a decrypt failure, a decoder error, a
    /// resume): a PLI goes out at the next [`KeyframeLimiter::poll`] the rate limit allows.
    pub(crate) fn request(&mut self) {
        self.wanted = true;
    }

    /// Whether a keyframe is being waited for.
    pub(crate) fn waiting(&self) -> bool {
        self.wanted || self.first_pli.is_some()
    }

    #[cfg(test)]
    pub(crate) fn stalled(&self) -> bool {
        self.stalled
    }

    /// A keyframe arrived: the wait ends. Answers whether the track was stalled.
    pub(crate) fn on_keyframe(&mut self) -> bool {
        self.wanted = false;
        self.first_pli = None;
        self.fir_at = None;
        std::mem::take(&mut self.stalled)
    }

    /// Forget any wait (the track's frames stopped: no sink).
    pub(crate) fn cancel(&mut self) {
        self.on_keyframe();
    }

    fn pli_interval(&self) -> Duration {
        if self.stalled {
            STALLED_PLI_INTERVAL
        } else {
            PLI_INTERVAL
        }
    }

    /// What is due at `now`.
    pub(crate) fn poll(&mut self, now: Instant) -> Due {
        let mut due = Due::default();
        if let Some(first) = self.first_pli {
            match self.fir_at {
                None if now.saturating_duration_since(first) >= FIR_AFTER => {
                    self.fir_at = Some(now);
                    self.firs += 1;
                    due.ask = Some(KeyframeAsk::Fir);
                    return due;
                }
                Some(f) if !self.stalled && now.saturating_duration_since(f) >= STALLED_AFTER => {
                    self.stalled = true;
                    due.stalled = true;
                    // PLIs continue at one a second from here.
                    self.wanted = true;
                }
                _ => {}
            }
        }
        if self.stalled && self.first_pli.is_some() {
            self.wanted = true;
        }
        let allowed = self
            .last_pli
            .is_none_or(|l| now.saturating_duration_since(l) >= self.pli_interval());
        if self.wanted && allowed {
            self.wanted = false;
            self.last_pli = Some(now);
            self.first_pli.get_or_insert(now);
            self.plis += 1;
            due.ask = Some(KeyframeAsk::Pli);
        }
        due
    }

    /// When [`KeyframeLimiter::poll`] next has something to do.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        let mut d: Option<Instant> = None;
        let mut at = |t: Instant| d = Some(d.map_or(t, |x: Instant| x.min(t)));
        if self.wanted || self.stalled {
            match self.last_pli {
                Some(l) => at(l + self.pli_interval()),
                None => at(Instant::now()),
            }
        }
        if let Some(first) = self.first_pli {
            match self.fir_at {
                None => at(first + FIR_AFTER),
                Some(f) if !self.stalled => at(f + STALLED_AFTER),
                Some(_) => {}
            }
        }
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn one_pli_per_300_ms() {
        let t = Instant::now();
        let mut k = KeyframeLimiter::new();
        k.request();
        assert_eq!(k.poll(t).ask, Some(KeyframeAsk::Pli));
        k.request();
        assert_eq!(k.poll(t + ms(100)).ask, None, "held by the limit");
        assert_eq!(k.deadline(), Some(t + PLI_INTERVAL));
        k.request();
        assert_eq!(k.poll(t + ms(299)).ask, None);
        assert_eq!(k.poll(t + ms(300)).ask, Some(KeyframeAsk::Pli));
        assert_eq!(k.poll(t + ms(700)).ask, None, "no new request");
        assert_eq!(k.plis, 2);
    }

    #[test]
    fn a_keyframe_ends_the_wait() {
        let t = Instant::now();
        let mut k = KeyframeLimiter::new();
        k.request();
        k.poll(t);
        assert!(k.waiting());
        assert!(!k.on_keyframe());
        assert!(!k.waiting());
        assert_eq!(
            k.poll(t + ms(5_000)),
            Due::default(),
            "no FIR after a keyframe"
        );
        assert_eq!(k.deadline(), None);
    }

    #[test]
    fn no_keyframe_within_1_s_sends_one_fir_then_stalls_after_5_s() {
        let t = Instant::now();
        let mut k = KeyframeLimiter::new();
        k.request();
        assert_eq!(k.poll(t).ask, Some(KeyframeAsk::Pli));
        assert_eq!(k.deadline(), Some(t + FIR_AFTER));
        assert_eq!(k.poll(t + ms(999)).ask, None);
        assert_eq!(k.poll(t + ms(1_000)).ask, Some(KeyframeAsk::Fir));
        assert_eq!(k.firs, 1);
        assert_eq!(k.poll(t + ms(2_000)).ask, None, "one FIR only");
        assert_eq!(k.deadline(), Some(t + ms(1_000) + STALLED_AFTER));
        let d = k.poll(t + ms(6_000));
        assert!(d.stalled);
        assert_eq!(d.ask, Some(KeyframeAsk::Pli));
        assert!(k.stalled());
        // Stalled: PLIs at one a second, the stall reported once.
        let d = k.poll(t + ms(6_500));
        assert_eq!(d, Due::default());
        let d = k.poll(t + ms(7_000));
        assert_eq!(d.ask, Some(KeyframeAsk::Pli));
        assert!(!d.stalled);
        assert_eq!(k.poll(t + ms(7_400)).ask, None);
        assert_eq!(k.poll(t + ms(8_000)).ask, Some(KeyframeAsk::Pli));
        assert_eq!(k.firs, 1, "no second FIR while stalled");
        assert!(k.on_keyframe(), "the keyframe ends the stall");
        assert_eq!(k.poll(t + ms(20_000)), Due::default());
    }

    #[test]
    fn a_request_during_the_fir_wait_still_respects_the_limit() {
        let t = Instant::now();
        let mut k = KeyframeLimiter::new();
        k.request();
        k.poll(t);
        k.request();
        assert_eq!(k.poll(t + ms(100)).ask, None);
        assert_eq!(k.poll(t + ms(400)).ask, Some(KeyframeAsk::Pli));
        // The FIR is timed from the first PLI of the wait, not the latest.
        assert_eq!(k.poll(t + ms(1_000)).ask, Some(KeyframeAsk::Fir));
    }

    #[test]
    fn cancel_forgets_the_wait() {
        let t = Instant::now();
        let mut k = KeyframeLimiter::new();
        k.request();
        k.poll(t);
        k.cancel();
        assert_eq!(k.poll(t + ms(3_000)), Due::default());
        assert_eq!(k.deadline(), None);
    }
}
