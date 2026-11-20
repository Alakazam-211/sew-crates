//! The camera publish while a phone app is in the background (`video-core`).
//!
//! The plugin tells the call its combined scene ([`crate::engine::CallHandle::app_scene`]:
//! background while the app **or** the video backend says so). In the background the camera
//! **pauses** rather than unpublishing: its pipeline stops capturing and encoding
//! (`VideoBridge::pause_camera`) and its track is muted at the media server
//! (`Call::set_video_muted`), so the lease and the tile stay, and `local_video` says
//! `{camera: "paused", reason: "background"}`. Back in front it unmutes and resumes, every
//! encoder making a keyframe before its first frame, and `local_video` says `on` again. After
//! [`BACKGROUND_UNPUBLISH`] in the background the camera turns off (unpublished, closed, the
//! lease released) with `{camera: "off", reason: "background"}`; it stays off on the return.
//! A camera turned on while in the background pauses at once. Audio is never touched.
//!
//! The OS may pause the camera's source for the background on its own (iOS interrupts the
//! capture session) and say when that is over. The pipeline takes the source's first frame
//! after the return as the end of that pause too; [`HELD_CHECK`] after the return, a camera
//! still paused that way with no frame since opens again (`VideoBridge::reopen_held_camera`),
//! so the app's return alone brings the camera back. Never while the app is in the
//! background.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::task::JoinHandle;

use super::lock;
use crate::engine::BoxFuture;

/// How long a paused camera stays published in the background.
pub(crate) const BACKGROUND_UNPUBLISH: Duration = Duration::from_secs(300);

/// How long after the return a camera the OS still holds paused for the background waits
/// before it opens again.
pub(super) const HELD_CHECK: Duration = Duration::from_secs(3);

/// What the pause acts on (the real call's bridge and `Call`; a recorder in tests).
pub(super) trait CameraOps: Send + Sync + 'static {
    /// Pauses (`true`) or resumes the camera's pipeline and mutes or unmutes its track;
    /// `false`: no camera publishes.
    fn pause(&self, paused: bool) -> BoxFuture<'_, bool>;
    /// The camera turns off after the background's time limit.
    fn unpublish(&self) -> BoxFuture<'_, ()>;
    /// Back in front: the camera opens again if the OS still holds it paused for the
    /// background (`true`: it did).
    fn reopen_held(&self) -> BoxFuture<'_, bool>;
    /// `local_video` changes: the camera paused (`true`) or running again.
    fn paused_changed(&self, paused: bool);
}

#[derive(Default)]
struct Applied {
    /// The scene the camera follows now.
    background: bool,
    /// The camera is paused by the background.
    paused: bool,
    /// The background's time limit, or the check after the return.
    timer: Option<JoinHandle<()>>,
    /// Bumped by every change, so a stale timer does nothing.
    epoch: u64,
}

/// One call's camera pause (see the module docs).
pub(super) struct ScenePause {
    ops: Arc<dyn CameraOps>,
    /// The runtime the steps run on (a scene change may come from any thread).
    rt: tokio::runtime::Handle,
    /// The scene the plugin asked for last.
    want: Mutex<bool>,
    /// What was done; held across the steps, so changes apply one at a time, in order.
    applied: tokio::sync::Mutex<Applied>,
}

impl ScenePause {
    pub(super) fn new(ops: Arc<dyn CameraOps>, rt: tokio::runtime::Handle) -> Arc<ScenePause> {
        Arc::new(ScenePause {
            ops,
            rt,
            want: Mutex::new(false),
            applied: tokio::sync::Mutex::new(Applied::default()),
        })
    }

    /// The plugin's scene changed. Never blocks: the steps run on a task.
    pub(super) fn scene(self: &Arc<Self>, background: bool) {
        *lock(&self.want) = background;
        let me = Arc::clone(self);
        self.rt.spawn(async move { me.follow().await });
    }

    /// Applies the latest scene.
    async fn follow(self: Arc<Self>) {
        let mut a = self.applied.lock().await;
        let want = *lock(&self.want);
        if want == a.background {
            return;
        }
        a.background = want;
        a.epoch += 1;
        if let Some(t) = a.timer.take() {
            t.abort();
        }
        if want {
            self.pause(&mut a).await;
            return;
        }
        if a.paused {
            a.paused = false;
            self.ops.pause(false).await;
            self.ops.paused_changed(false);
        }
        let epoch = a.epoch;
        let me = Arc::clone(&self);
        a.timer = Some(self.rt.spawn(async move {
            tokio::time::sleep(HELD_CHECK).await;
            let a = me.applied.lock().await;
            if a.epoch != epoch || a.background {
                return;
            }
            // Held under the lock: a change waits for the reopen, then applies.
            me.ops.reopen_held().await;
        }));
    }

    /// Pauses a running camera and starts the time limit.
    async fn pause(self: &Arc<Self>, a: &mut Applied) {
        if a.paused || !self.ops.pause(true).await {
            return;
        }
        a.paused = true;
        self.ops.paused_changed(true);
        let epoch = a.epoch;
        let me = Arc::clone(self);
        a.timer = Some(self.rt.spawn(async move {
            tokio::time::sleep(BACKGROUND_UNPUBLISH).await;
            let mut a = me.applied.lock().await;
            if a.epoch != epoch || !a.paused {
                return;
            }
            a.paused = false;
            a.timer = None;
            me.ops.unpublish().await;
        }));
    }

    /// The camera went on (`camera {on}` answered): paused at once in the background.
    pub(super) async fn camera_started(self: &Arc<Self>) {
        let mut a = self.applied.lock().await;
        if a.background {
            self.pause(&mut a).await;
        }
    }

    /// The camera went off (`camera {off}`, or the call's end): nothing is paused any more.
    pub(super) async fn camera_stopped(&self) {
        let mut a = self.applied.lock().await;
        a.epoch += 1;
        if let Some(t) = a.timer.take() {
            t.abort();
        }
        a.paused = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Recorder {
        camera: Mutex<bool>,
        log: Mutex<Vec<String>>,
    }

    impl Recorder {
        fn log(&self) -> Vec<String> {
            lock(&self.log).clone()
        }
    }

    impl CameraOps for Recorder {
        fn pause(&self, paused: bool) -> BoxFuture<'_, bool> {
            let on = *lock(&self.camera);
            lock(&self.log).push(format!("pause {paused} (camera {on})"));
            Box::pin(std::future::ready(on))
        }
        fn unpublish(&self) -> BoxFuture<'_, ()> {
            *lock(&self.camera) = false;
            lock(&self.log).push("unpublish".into());
            Box::pin(std::future::ready(()))
        }
        fn reopen_held(&self) -> BoxFuture<'_, bool> {
            let on = *lock(&self.camera);
            lock(&self.log).push(format!("reopen held (camera {on})"));
            Box::pin(std::future::ready(on))
        }
        fn paused_changed(&self, paused: bool) {
            lock(&self.log).push(format!("local paused {paused}"));
        }
    }

    async fn settle() {
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_background_pauses_the_camera_and_the_return_resumes_it() {
        let r = Arc::new(Recorder::default());
        *lock(&r.camera) = true;
        let p = ScenePause::new(r.clone(), tokio::runtime::Handle::current());
        p.scene(true);
        settle().await;
        p.scene(true);
        settle().await;
        assert_eq!(r.log(), ["pause true (camera true)", "local paused true"]);
        tokio::time::advance(BACKGROUND_UNPUBLISH - Duration::from_secs(1)).await;
        p.scene(false);
        settle().await;
        assert_eq!(
            r.log()[2..],
            ["pause false (camera true)", "local paused false"]
        );
        // The old time limit is gone; the check after the return runs once.
        tokio::time::advance(Duration::from_secs(600)).await;
        settle().await;
        assert_eq!(
            r.log()[4..],
            ["reopen held (camera true)"],
            "no unpublish after the return"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_return_checks_for_a_held_camera_once_and_never_in_the_background() {
        let r = Arc::new(Recorder::default());
        *lock(&r.camera) = true;
        let p = ScenePause::new(r.clone(), tokio::runtime::Handle::current());
        p.scene(true);
        settle().await;
        p.scene(false);
        settle().await;
        assert_eq!(r.log().len(), 4);
        tokio::time::advance(HELD_CHECK - Duration::from_millis(10)).await;
        settle().await;
        assert_eq!(r.log().len(), 4, "not before the check's time");
        // Back to the background before the check: no check, the camera pauses.
        p.scene(true);
        settle().await;
        tokio::time::advance(Duration::from_secs(10)).await;
        settle().await;
        assert!(
            !r.log().iter().any(|l| l.starts_with("reopen")),
            "no reopen while in the background: {:?}",
            r.log()
        );
        // In front again: the check runs after its time, once.
        p.scene(false);
        settle().await;
        tokio::time::advance(HELD_CHECK + Duration::from_millis(10)).await;
        settle().await;
        tokio::time::advance(Duration::from_secs(60)).await;
        settle().await;
        let reopens = r.log().iter().filter(|l| l.starts_with("reopen")).count();
        assert_eq!(reopens, 1, "{:?}", r.log());
    }

    /// A scene that never left the front checks nothing.
    #[tokio::test(start_paused = true)]
    async fn a_scene_that_never_left_the_front_checks_nothing() {
        let r = Arc::new(Recorder::default());
        *lock(&r.camera) = true;
        let p = ScenePause::new(r.clone(), tokio::runtime::Handle::current());
        p.scene(false);
        settle().await;
        tokio::time::advance(Duration::from_secs(60)).await;
        settle().await;
        assert!(r.log().is_empty(), "{:?}", r.log());
    }

    #[tokio::test(start_paused = true)]
    async fn five_minutes_in_the_background_turn_the_camera_off() {
        let r = Arc::new(Recorder::default());
        *lock(&r.camera) = true;
        let p = ScenePause::new(r.clone(), tokio::runtime::Handle::current());
        p.scene(true);
        settle().await;
        tokio::time::advance(BACKGROUND_UNPUBLISH - Duration::from_millis(10)).await;
        settle().await;
        assert!(!r.log().contains(&"unpublish".to_owned()));
        tokio::time::advance(Duration::from_millis(20)).await;
        settle().await;
        assert_eq!(r.log().last().map(String::as_str), Some("unpublish"));
        // Back in front: nothing to resume, the camera stays off (the check finds none).
        p.scene(false);
        settle().await;
        assert_eq!(r.log().last().map(String::as_str), Some("unpublish"));
        tokio::time::advance(HELD_CHECK * 2).await;
        settle().await;
        assert_eq!(
            r.log().last().map(String::as_str),
            Some("reopen held (camera false)")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn no_camera_nothing_paused_and_a_camera_started_in_the_background_pauses() {
        let r = Arc::new(Recorder::default());
        let p = ScenePause::new(r.clone(), tokio::runtime::Handle::current());
        p.scene(true);
        settle().await;
        assert_eq!(r.log(), ["pause true (camera false)"], "no camera runs");
        *lock(&r.camera) = true;
        p.camera_started().await;
        assert_eq!(
            r.log()[1..],
            ["pause true (camera true)", "local paused true"]
        );
        // Off by hand: no time limit, nothing to resume.
        p.camera_stopped().await;
        *lock(&r.camera) = false;
        tokio::time::advance(BACKGROUND_UNPUBLISH * 2).await;
        p.scene(false);
        settle().await;
        assert_eq!(r.log().len(), 3);
    }

    /// The time limit counts from the last move to the background: a return halfway cancels
    /// the first one.
    #[tokio::test(start_paused = true)]
    async fn the_time_limit_counts_from_the_last_background() {
        let r = Arc::new(Recorder::default());
        *lock(&r.camera) = true;
        let p = ScenePause::new(r.clone(), tokio::runtime::Handle::current());
        p.scene(true);
        settle().await;
        tokio::time::advance(BACKGROUND_UNPUBLISH / 2).await;
        p.scene(false);
        settle().await;
        p.scene(true);
        settle().await;
        // The first limit would end here: not unpublished.
        tokio::time::advance(BACKGROUND_UNPUBLISH / 2 + Duration::from_secs(1)).await;
        settle().await;
        assert!(!r.log().contains(&"unpublish".to_owned()), "{:?}", r.log());
        tokio::time::advance(BACKGROUND_UNPUBLISH / 2).await;
        settle().await;
        assert_eq!(r.log().last().map(String::as_str), Some("unpublish"));
    }
}
