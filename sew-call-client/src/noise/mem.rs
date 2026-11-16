//! The process's resident memory, for the DeepFilterNet3 load figures and the device bench.

/// This process's resident memory now (KiB): `/proc/self/status` on Linux and Android, `ps` on
/// macOS; `None` elsewhere (iOS, Windows: reading it needs a system call this crate does not
/// make).
#[cfg(any(feature = "dfn3", feature = "noise-bench"))]
pub(crate) fn rss_kib() -> Option<u64> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        proc_status("VmRSS:")
    }
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
    {
        None
    }
}

/// This process's peak resident memory (KiB): `/proc/self/status`'s VmHWM on Linux and
/// Android; `None` elsewhere.
#[cfg(feature = "noise-bench")]
pub(crate) fn rss_peak_kib() -> Option<u64> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        proc_status("VmHWM:")
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        None
    }
}

#[cfg(all(
    any(feature = "dfn3", feature = "noise-bench"),
    any(target_os = "linux", target_os = "android")
))]
fn proc_status(key: &str) -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/status").ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix(key))?
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .ok()
}
