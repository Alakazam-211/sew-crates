// Copyright 2023 LiveKit, Inc.
// Copyright 2026 Alakazam Labs
// SPDX-License-Identifier: Apache-2.0
// Rewritten from a written behaviour specification of the earlier port of livekit/livekit's test client (Apache-2.0).

//! The keep-alive timer and the round-trip measurement, on the session's own millisecond clock
//! (milliseconds since the instant the session was created or reset).

use std::time::{Duration, Instant};

/// Keep-alive period until the server names one in its join answer.
const INITIAL_PERIOD: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub(super) struct Heartbeat {
    epoch: Instant,
    period: Duration,
    next_due: Option<Instant>,
    measured_rtt: Option<u64>,
}

impl Heartbeat {
    pub(super) fn new(epoch: Instant) -> Self {
        Self {
            epoch,
            period: INITIAL_PERIOD,
            next_due: None,
            measured_rtt: None,
        }
    }

    /// Session clock reading at `now`: 0 before the epoch, saturating at `u64::MAX`.
    fn clock_ms(&self, now: Instant) -> u64 {
        let ms = now.saturating_duration_since(self.epoch).as_millis();
        u64::try_from(ms).unwrap_or(u64::MAX)
    }

    /// Take the server's period (whole seconds). Zero or negative leaves the current one.
    pub(super) fn adopt_period_secs(&mut self, secs: i32) {
        if let Ok(s) = u64::try_from(secs)
            && s > 0
        {
            self.period = Duration::from_secs(s);
        }
    }

    /// Schedule the next keep-alive one period after `now`.
    pub(super) fn start(&mut self, now: Instant) {
        self.next_due = now.checked_add(self.period);
    }

    pub(super) fn stop(&mut self) {
        self.next_due = None;
    }

    pub(super) fn next_due(&self) -> Option<Instant> {
        self.next_due
    }

    /// If a keep-alive is due at `now`, reschedule from `now` and return `(timestamp, rtt)` for
    /// the ping, both in milliseconds (rtt 0 before the first measurement).
    pub(super) fn tick(&mut self, now: Instant) -> Option<(i64, i64)> {
        let due = self.next_due?;
        if due > now {
            return None;
        }
        self.start(now);
        let stamp = i64::try_from(self.clock_ms(now)).unwrap_or(i64::MAX);
        let rtt = i64::try_from(self.measured_rtt.unwrap_or(0)).unwrap_or(i64::MAX);
        Some((stamp, rtt))
    }

    /// A pong echoing `stamp` arrived at `now`. A stamp from the future is ignored; a negative
    /// one counts as 0.
    pub(super) fn pong(&mut self, now: Instant, stamp: i64) {
        let sent = u64::try_from(stamp).unwrap_or(0);
        if let Some(rtt) = self.clock_ms(now).checked_sub(sent) {
            self.measured_rtt = Some(rtt);
        }
    }

    pub(super) fn rtt_ms(&self) -> Option<u64> {
        self.measured_rtt
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn idle_until_started() {
        let t0 = Instant::now();
        let mut h = Heartbeat::new(t0);
        assert_eq!(h.next_due(), None);
        assert_eq!(h.tick(t0 + Duration::from_secs(60)), None);
        h.start(t0);
        assert_eq!(h.next_due(), Some(t0 + INITIAL_PERIOD));
    }

    #[test]
    fn late_tick_reschedules_from_the_call() {
        let t0 = Instant::now();
        let mut h = Heartbeat::new(t0);
        h.start(t0);
        let late = t0 + Duration::from_secs(17);
        assert_eq!(h.tick(late), Some((17_000, 0)));
        assert_eq!(h.next_due(), Some(late + INITIAL_PERIOD));
        // Not due again until a full period after the late call.
        assert_eq!(h.tick(late + Duration::from_secs(1)), None);
    }

    #[test]
    fn non_positive_period_is_ignored() {
        let t0 = Instant::now();
        let mut h = Heartbeat::new(t0);
        h.adopt_period_secs(0);
        h.adopt_period_secs(-4);
        h.start(t0);
        assert_eq!(h.next_due(), Some(t0 + INITIAL_PERIOD));
        h.adopt_period_secs(9);
        h.start(t0);
        assert_eq!(h.next_due(), Some(t0 + Duration::from_secs(9)));
    }

    #[test]
    fn pong_rules() {
        let t0 = Instant::now();
        let mut h = Heartbeat::new(t0);
        let at = t0 + Duration::from_millis(500);
        h.pong(at, 900);
        assert_eq!(h.rtt_ms(), None, "a stamp from the future is ignored");
        h.pong(at, -20);
        assert_eq!(h.rtt_ms(), Some(500), "a negative stamp counts as 0");
        h.pong(at, 470);
        assert_eq!(h.rtt_ms(), Some(30));
        h.pong(at, 501);
        assert_eq!(h.rtt_ms(), Some(30), "future stamp leaves the last value");
    }
}
