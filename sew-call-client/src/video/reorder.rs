//! The reorder buffer of one received video stream: packets in, keyed on the extended RTP
//! sequence number, out in sequence order. A missing packet is waited for up to the window
//! (the larger of 150 ms and twice the RTT: long enough for a NACK and its retransmission), then
//! skipped; the depacketiser sees the jump and drops the frame it broke.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// The shortest wait for a missing packet.
pub(crate) const MIN_WINDOW: Duration = Duration::from_millis(150);
/// Packets held at most; past it the oldest gap is skipped at once.
pub(crate) const MAX_HELD: usize = 2048;

/// One buffered packet.
#[derive(Debug)]
struct Held<P> {
    at: Instant,
    pkt: P,
}

/// What [`Reorder::push`] did with a packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pushed {
    /// Taken (released now or held for an earlier gap).
    Taken,
    /// A copy of one already taken.
    Duplicate,
    /// Older than what was already released (its gap was skipped): dropped.
    Late,
}

/// The reorder buffer.
#[derive(Debug)]
pub(crate) struct Reorder<P> {
    window: Duration,
    /// The next sequence number to release.
    next: Option<u64>,
    held: BTreeMap<u64, Held<P>>,
    /// Packets skipped over (never arrived within the window).
    pub(crate) skipped: u64,
    /// Late packets dropped.
    pub(crate) late: u64,
}

impl<P> Reorder<P> {
    pub(crate) fn new() -> Reorder<P> {
        Reorder {
            window: MIN_WINDOW,
            next: None,
            held: BTreeMap::new(),
            skipped: 0,
            late: 0,
        }
    }

    /// The window from the current round-trip time: `max(150 ms, 2 × rtt)`.
    pub(crate) fn set_rtt(&mut self, rtt: Duration) {
        self.window = MIN_WINDOW.max(rtt.saturating_mul(2));
    }

    #[cfg(test)]
    pub(crate) fn window(&self) -> Duration {
        self.window
    }

    /// Forgets everything (a new stream: the publisher's SSRC changed, a new PC).
    pub(crate) fn reset(&mut self) {
        self.next = None;
        self.held.clear();
    }

    /// Takes one packet; the packets now in order are appended to `out`.
    pub(crate) fn push(&mut self, seq: u64, now: Instant, pkt: P, out: &mut Vec<P>) -> Pushed {
        match self.next {
            None => self.next = Some(seq),
            Some(n) if seq < n => {
                self.late += 1;
                return Pushed::Late;
            }
            Some(_) => {}
        }
        if self.held.contains_key(&seq) {
            return Pushed::Duplicate;
        }
        self.held.insert(seq, Held { at: now, pkt });
        self.release(out);
        if self.held.len() > MAX_HELD {
            self.skip_gap(out);
        }
        Pushed::Taken
    }

    /// Skips gaps whose first held packet waited the window; releases what follows.
    pub(crate) fn poll(&mut self, now: Instant, out: &mut Vec<P>) {
        while let Some((_, h)) = self.held.first_key_value() {
            if now.saturating_duration_since(h.at) < self.window {
                break;
            }
            self.skip_gap(out);
        }
    }

    /// When the oldest held packet's wait ends (`None`: nothing waits for a gap).
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.held.first_key_value().map(|(_, h)| h.at + self.window)
    }

    fn release(&mut self, out: &mut Vec<P>) {
        while let Some(n) = self.next {
            match self.held.remove(&n) {
                Some(h) => {
                    out.push(h.pkt);
                    self.next = Some(n + 1);
                }
                None => break,
            }
        }
    }

    fn skip_gap(&mut self, out: &mut Vec<P>) {
        if let (Some(n), Some((&first, _))) = (self.next, self.held.first_key_value()) {
            self.skipped += first.saturating_sub(n);
            self.next = Some(first);
        }
        self.release(out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn in_order_packets_pass_at_once() {
        let t = Instant::now();
        let mut r = Reorder::new();
        let mut out = Vec::new();
        for s in 100..110u64 {
            assert_eq!(r.push(s, t, s, &mut out), Pushed::Taken);
        }
        assert_eq!(out, (100..110).collect::<Vec<_>>());
        assert_eq!(r.deadline(), None);
    }

    #[test]
    fn a_swap_is_put_back_in_order() {
        let t = Instant::now();
        let mut r = Reorder::new();
        let mut out = Vec::new();
        for s in [1u64, 3, 2, 5, 4, 6] {
            r.push(s, t, s, &mut out);
        }
        assert_eq!(out, vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(r.skipped, 0);
    }

    #[test]
    fn a_gap_waits_the_window_then_is_skipped() {
        let t = Instant::now();
        let mut r = Reorder::new();
        let mut out = Vec::new();
        r.push(10, t, 10, &mut out);
        r.push(12, t + ms(5), 12, &mut out);
        r.push(13, t + ms(6), 13, &mut out);
        assert_eq!(out, vec![10]);
        assert_eq!(r.deadline(), Some(t + ms(5) + MIN_WINDOW));
        r.poll(t + ms(100), &mut out);
        assert_eq!(out, vec![10], "still within the window");
        r.poll(t + ms(155), &mut out);
        assert_eq!(out, vec![10, 12, 13]);
        assert_eq!(r.skipped, 1);
        // The missing packet arriving now is late: dropped, never released out of order.
        assert_eq!(r.push(11, t + ms(160), 11, &mut out), Pushed::Late);
        assert_eq!(out, vec![10, 12, 13]);
    }

    #[test]
    fn a_retransmission_inside_the_window_fills_the_gap() {
        let t = Instant::now();
        let mut r = Reorder::new();
        let mut out = Vec::new();
        r.push(1, t, 1, &mut out);
        r.push(3, t, 3, &mut out);
        r.push(2, t + ms(120), 2, &mut out);
        assert_eq!(out, vec![1, 2, 3]);
        assert_eq!(r.skipped, 0);
    }

    #[test]
    fn the_window_is_twice_the_rtt_but_never_under_150_ms() {
        let mut r: Reorder<u64> = Reorder::new();
        r.set_rtt(ms(40));
        assert_eq!(r.window(), MIN_WINDOW);
        r.set_rtt(ms(120));
        assert_eq!(r.window(), ms(240));
    }

    #[test]
    fn duplicates_are_dropped() {
        let t = Instant::now();
        let mut r = Reorder::new();
        let mut out = Vec::new();
        r.push(1, t, 1, &mut out);
        r.push(3, t, 3, &mut out);
        assert_eq!(r.push(3, t, 3, &mut out), Pushed::Duplicate);
        assert_eq!(r.push(1, t, 1, &mut out), Pushed::Late);
        r.push(2, t, 2, &mut out);
        assert_eq!(out, vec![1, 2, 3]);
    }

    #[test]
    fn a_full_buffer_skips_the_oldest_gap_at_once() {
        let t = Instant::now();
        let mut r = Reorder::new();
        let mut out = Vec::new();
        r.push(0, t, 0, &mut out);
        for s in 2..(MAX_HELD as u64 + 3) {
            r.push(s, t, s, &mut out);
        }
        assert_eq!(out.len(), MAX_HELD + 2);
        assert_eq!(r.skipped, 1);
    }

    #[test]
    fn a_reset_starts_from_the_next_packet() {
        let t = Instant::now();
        let mut r = Reorder::new();
        let mut out = Vec::new();
        r.push(500, t, 500, &mut out);
        r.reset();
        r.push(7, t, 7, &mut out);
        assert_eq!(out, vec![500, 7]);
    }
}
