//! The real engine's iPhone screen share (`ios-screen-ring` with `video-core`): the broadcast
//! extension's ring ([`crate::screen_ring`]) driven into the call's `VideoBridge` as the
//! `screen` publication, under the `self_stream` lease, as a desktop screen.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};

use sew_call_client::{SourceFeed, VideoError, VideoSource as EngineSource};

use super::{CallShared, lock, publish};
use crate::engine::{BoxFuture, CallEvent, EngineError};
use crate::screen_ring::{self, RingPump, ScreenShareSink};
use crate::wire::{ErrorInfo, LocalVideoState, video_code};

/// The call's armed ring: its driver task.
#[derive(Debug, Default)]
pub(super) struct RingSlot(Mutex<Option<tokio::task::JoinHandle<()>>>);

impl Drop for RingSlot {
    fn drop(&mut self) {
        if let Some(t) = lock(&self.0).take() {
            t.abort();
        }
    }
}

/// The call's screen publication, for the ring's driver.
struct CallSink(Weak<CallShared>);

impl ScreenShareSink for CallSink {
    fn publish(&self, feed: SourceFeed) -> BoxFuture<'_, Result<(), VideoError>> {
        Box::pin(async move {
            let sh = self.0.upgrade().ok_or(VideoError::Ended)?;
            let bridge = sh.video_bridge().ok_or(VideoError::Ended)?;
            let g = sh.call.read().await;
            let call = g.as_ref().ok_or(VideoError::Ended)?;
            bridge
                .publish_source(call, EngineSource::ScreenShare, feed, None)
                .await
                .map(|_| ())
        })
    }

    fn unpublish(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let Some(sh) = self.0.upgrade() else { return };
            let Some(bridge) = sh.video_bridge() else {
                return;
            };
            let g = sh.call.read().await;
            if let Some(call) = g.as_ref()
                && let Err(e) = bridge.stop_source(call, EngineSource::ScreenShare).await
            {
                tracing::info!(target: "sew_voice", error = %e, "the screen ring's share did not unpublish");
            }
        })
    }

    fn refused(&self, error: &VideoError) {
        if let Some(sh) = self.0.upgrade() {
            sh.send(CallEvent::Error(publish::error(error).to_info()));
        }
    }

    fn alive(&self) -> bool {
        // A leave holding the write lock is taking the call away: still there until it's done.
        self.0.upgrade().is_some_and(|sh| match sh.call.try_read() {
            Ok(g) => g.is_some(),
            Err(_) => true,
        })
    }
}

/// Arms the ring in `container` for the call (replacing an armed one).
pub(super) async fn arm(shared: &Arc<CallShared>, container: PathBuf) -> Result<(), EngineError> {
    let policy = match shared.call.read().await.as_ref() {
        Some(call) => call.media_policy(),
        None => return Err(EngineError::failed(crate::wire::code::NOT_IN_CALL)),
    };
    let layout = screen_ring::layout_for(policy.as_ref());
    let pump = RingPump::create(screen_ring::ring_path(&container), layout).map_err(|e| {
        EngineError::Failed(ErrorInfo::with_detail(
            video_code::SCREEN_UNSUPPORTED,
            format!("the screen ring in {}: {e}", container.display()),
        ))
    })?;
    let sink: Arc<dyn ScreenShareSink> = Arc::new(CallSink(Arc::downgrade(shared)));
    let mut slot = lock(&shared.ring.0);
    // A ring armed before: its share (if any) is unpublished before the new ring runs.
    let replaced = slot.take().inspect(|t| t.abort()).is_some();
    let sharing = replaced && shared.video.local().screen != LocalVideoState::Off;
    *slot = Some(tokio::spawn(async move {
        if sharing {
            sink.unpublish().await;
        }
        screen_ring::drive(pump, sink).await;
    }));
    Ok(())
}
