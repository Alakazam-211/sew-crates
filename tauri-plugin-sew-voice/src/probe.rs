//! The noise removal stage's probe (feature `noise-probe`; measurement builds only): the core
//! number of the calling thread on this system, and the reference run on a thread of its own.
//! On macOS and iOS the core number is `pthread_cpu_number_np`, linked strongly: a probe build
//! needs iOS 14.2 or macOS 11 or later.

use crate::wire::ErrorInfo;

/// The core the calling thread is on, on Apple: `pthread_cpu_number_np`, the system's own
/// call for it (`unsafe`, as the thread QoS class call below and the native sinks' are).
#[cfg(any(target_os = "ios", target_os = "macos"))]
#[allow(unsafe_code)]
pub(crate) fn core_number() -> Option<u32> {
    let mut cpu: libc::size_t = 0;
    // SAFETY: `pthread_cpu_number_np` writes the calling thread's CPU number through its one
    // pointer, which is a live local of the type it takes; it keeps nothing.
    let r = unsafe { libc::pthread_cpu_number_np(&raw mut cpu) };
    if r == 0 {
        u32::try_from(cpu).ok()
    } else {
        None
    }
}

/// The reference run's thread asks for the user-interactive QoS class on Apple.
#[cfg(any(target_os = "ios", target_os = "macos"))]
#[allow(unsafe_code)]
fn raise_thread_priority() {
    // SAFETY: `pthread_set_qos_class_self_np` changes the calling thread's own QoS class; it
    // takes two plain values and no pointer.
    let r = unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0)
    };
    if r != 0 {
        tracing::warn!(
            error = r,
            "sew-voice: the noise probe's reference thread kept its QoS class"
        );
    }
}

#[cfg(not(any(target_os = "ios", target_os = "macos")))]
fn raise_thread_priority() {}

/// The core the calling thread is on: sew-call-client's default (`sched_getcpu` on Linux and
/// Android, none elsewhere).
#[cfg(not(any(target_os = "ios", target_os = "macos")))]
pub(crate) fn core_number() -> Option<u32> {
    sew_call_client::noise_probe::default_core_number()
}

/// sew-call-client's reference run on a thread of its own, as JSON.
pub(crate) async fn reference() -> Result<serde_json::Value, ErrorInfo> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("sew-noise-probe-ref".into())
        .spawn(move || {
            raise_thread_priority();
            let _ = tx.send(sew_call_client::noise_probe::reference(Some(core_number)));
        })
        .map_err(|e| ErrorInfo::with_detail("engine_stopped", e.to_string()))?;
    let run = rx
        .await
        .map_err(|e| ErrorInfo::with_detail("engine_stopped", e.to_string()))?;
    serde_json::to_value(run).map_err(|e| ErrorInfo::with_detail("engine_stopped", e.to_string()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[tokio::test]
    async fn the_reference_run_answers_json() {
        let json = reference().await.unwrap();
        assert_eq!(json["schema"], "sew-noise-probe/2");
        assert!(json["k1_wall"]["p50_ns"].as_u64().unwrap() > 0);
        assert!(json["k2_wall"]["p50_ns"].as_u64().unwrap() > 0);
        if cfg!(any(target_os = "linux", target_os = "android")) {
            assert!(json["core_start"].is_u64());
            assert!(core_number().is_some());
        }
    }
}
