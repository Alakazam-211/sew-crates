//! The calling thread's CPU time: the time it ran on a core, not the time it waited for one.
//!
//! Strong's step-down rules judge a block on this, so a block that took long only because the
//! thread was kept off a core (another thread of the app, such as a camera's capture and
//! encode, ran instead) does not look like a model too slow for the device. Linux, Android,
//! macOS and iOS have the clock (`CLOCK_THREAD_CPUTIME_ID`, through rustix's safe API); on
//! other targets, wasm32 among them, there is none and the stage judges wall time as before.

/// The calling thread's CPU time in nanoseconds, or `None` where this target has no such
/// clock (or the clock refused the call). No allocation, no lock.
#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
pub(crate) fn thread_cpu_ns() -> Option<u64> {
    use rustix::time::{ClockId, DynamicClockId, clock_gettime_dynamic};
    // The dynamic form returns an error if the clock refuses the call; this answers `None` then.
    let t = clock_gettime_dynamic(DynamicClockId::Known(ClockId::ThreadCPUTime)).ok()?;
    let secs = u64::try_from(t.tv_sec).ok()?;
    let nanos = u64::try_from(t.tv_nsec).ok()?;
    Some(secs.saturating_mul(1_000_000_000).saturating_add(nanos))
}

/// No thread CPU clock on this target.
#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
pub(crate) fn thread_cpu_ns() -> Option<u64> {
    None
}

/// Whether this target has a thread CPU clock.
pub(crate) const AVAILABLE: bool = cfg!(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple"
));

#[cfg(test)]
mod tests {
    use super::*;

    /// Where the clock exists it reads, and it moves with work, not with sleep.
    #[test]
    fn counts_work_not_sleep() {
        if !AVAILABLE {
            assert_eq!(thread_cpu_ns(), None);
            return;
        }
        let Some(a) = thread_cpu_ns() else {
            panic!("this target has a thread CPU clock but it did not read");
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        let Some(b) = thread_cpu_ns() else {
            panic!("the thread CPU clock stopped reading");
        };
        assert!(
            b.saturating_sub(a) < 20_000_000,
            "50 ms asleep counted {} ns of CPU",
            b.saturating_sub(a)
        );
        let mut x = 0u64;
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_millis(30) {
            x = std::hint::black_box(x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1));
        }
        let Some(c) = thread_cpu_ns() else {
            panic!("the thread CPU clock stopped reading");
        };
        assert!(c > b, "30 ms of work counted no CPU");
    }
}
