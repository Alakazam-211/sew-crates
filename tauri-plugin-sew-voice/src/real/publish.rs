//! The real engine's camera and screen publish behind the plugin's video seam (`video-core`):
//! `sew-call-client`'s `VideoBridge` over the app's kessel-video backend.
//!
//! - **Camera.** `camera {on, device?}` is `VideoBridge::set_camera`: the policy, the OS
//!   permission, the lease, the camera (through the bridge's `VideoCapture`, taken over from a
//!   running preview), the codec and encoders, the publication. Off unpublishes, closes the
//!   camera and releases the lease. `set_video_device` during a call is
//!   `VideoBridge::switch_camera` (`real.rs`).
//! - **Screen.** `screen {on, source?}` opens the source through the capture
//!   (`real/capture.rs`: the listed source or the OS picker, with the app window as its parent
//!   on Windows; at most 1080p15 and the room's `policy.screen`; the screen content hint) and
//!   publishes it through `VideoBridge::publish_capture`, under the `self_stream` lease.
//! - **Errors** are the engine's `VideoError` codes (`video_not_allowed`, `camera_denied`,
//!   `video_slots_full`, `media_busy` with `{retry_after}`, `codec_unavailable`, …) and the
//!   capture's (`screen_denied`, `picker_cancelled`, `screen_not_found`, `capture_failed`, …).
//! - **Events** (`real.rs`, `real/video.rs`): the engine's `LocalVideo` becomes the plugin's
//!   `local_video` (camera and screen on or off, with the reason); the sources' pauses (a
//!   hidden window, another app took the camera) make it `paused`; its `MediaPolicy` is the
//!   `policy` event.

use sew_call_client::{
    Call, CameraRequest as EngineCamera, VideoBridge, VideoError, VideoSource as EngineSource,
};

use super::capture::EngineCapture;
use crate::engine::EngineError;
use crate::video::{CameraRequest, ScreenRequest};
use crate::wire::{DeviceChoice, ErrorInfo};

/// The engine's refusal as the plugin's `{code, detail}`.
pub(super) fn error(e: &VideoError) -> EngineError {
    let detail = match e {
        VideoError::MediaBusy {
            retry_after: Some(s),
        } => serde_json::json!({ "retry_after": s }),
        other => serde_json::Value::String(other.to_string()),
    };
    EngineError::Failed(ErrorInfo::with_detail(e.code(), detail))
}

/// `camera {on, device?}`; without `device`, the camera `set_video_device` chose (`chosen`),
/// else the OS default. An off with `off_reason` says it as `local_video`'s reason (else
/// `unpublished`).
pub(super) async fn camera(
    bridge: &VideoBridge,
    call: &Call,
    req: &CameraRequest,
    chosen: Option<String>,
    off_reason: Option<&str>,
) -> Result<(), EngineError> {
    let mut r = if req.on {
        EngineCamera::on()
    } else {
        EngineCamera::off()
    };
    if let Some(why) = off_reason {
        r = r.with_reason(why);
    }
    match &req.device {
        Some(DeviceChoice::Id(id)) => r = r.with_device(id),
        Some(DeviceChoice::Default) => {}
        None => {
            if let Some(id) = chosen {
                r = r.with_device(&id);
            }
        }
    }
    bridge
        .set_camera(call, r)
        .await
        .map(|_| ())
        .map_err(|e| error(&e))
}

/// `screen {on, source?}`: the policy first (no picker for a share that can only be refused),
/// then the source, then the publication.
pub(super) async fn screen(
    bridge: &VideoBridge,
    capture: &EngineCapture,
    call: &Call,
    req: &ScreenRequest,
) -> Result<(), EngineError> {
    if !req.on {
        return bridge
            .stop_source(call, EngineSource::ScreenShare)
            .await
            .map_err(|e| error(&e));
    }
    let policy = call
        .media_policy()
        .ok_or_else(|| error(&VideoError::NotAllowed))?;
    if !policy.video_enabled || !policy.screen_allowed {
        return Err(error(&VideoError::NotAllowed));
    }
    let hint = &policy.screen;
    let src = capture
        .open_screen(req, Some((hint.max_width, hint.max_height, hint.max_fps)))
        .await?;
    bridge
        .publish_capture(call, EngineSource::ScreenShare, src.into_capture(), None)
        .await
        .map(|_| ())
        .map_err(|e| error(&e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_carry_their_codes() {
        let e = error(&VideoError::MediaBusy {
            retry_after: Some(30),
        })
        .to_info();
        assert_eq!(e.code, "media_busy");
        assert_eq!(e.detail, serde_json::json!({"retry_after": 30}));
        assert_eq!(error(&VideoError::NoLease).to_info().code, "no_lease");
    }
}
