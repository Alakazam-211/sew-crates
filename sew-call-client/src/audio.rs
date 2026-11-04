//! The kessel-audio bridge (feature `audio-core`; `audio` adds the system devices): the
//! capture path's `EncodedFrame`s go to the call's send queue (the engine encrypts them), and
//! the call's decrypted, verified frames go to kessel-audio's `push` / `lost`, with
//! `add_participant` first, `remove_participant` when a track ends, and the call's mute and
//! deafen as `set_capture` / `set_deafened`.
//!
//! kessel-audio never sees a key or a ciphertext; the engine never touches PCM.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use kessel_audio::{AudioEngine, EngineConfig, Level, RecvFrame};
use tokio::task::JoinHandle;

use crate::call::{Call, FrameSender, FrameStream, OpusFrame, RecvItem};
use crate::stats::{PlayoutStats, PlayoutTotals};

/// How often the bridge moves frames (well under one 20 ms frame).
const TICK: Duration = Duration::from_millis(5);

/// Why the bridge could not start.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AudioBridgeError {
    /// The call's frame stream was already taken.
    #[error("the call's frames are already taken")]
    FramesTaken,
    /// kessel-audio did not start.
    #[error("audio: {0}")]
    Audio(#[from] kessel_audio::AudioError),
}

/// A running bridge between one call and a kessel-audio engine.
pub struct AudioBridge {
    engine: Arc<AudioEngine>,
    identities: Identities,
    task: JoinHandle<()>,
}

/// kessel-audio's participant ids → the publishers' engine identities.
type Identities = Arc<Mutex<HashMap<u64, String>>>;

/// kessel-audio's latest levels (about every 50 ms) by engine identity
/// ([`AudioBridge::levels`]).
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct BridgeLevels {
    /// This device's processed capture (silent while muted).
    pub local: Level,
    /// Each remote track's decoded audio, by its publisher's engine identity.
    pub remotes: Vec<(String, Level)>,
}

impl std::fmt::Debug for AudioBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioBridge").finish_non_exhaustive()
    }
}

impl Drop for AudioBridge {
    fn drop(&mut self) {
        self.task.abort();
        self.engine.shutdown();
    }
}

impl AudioBridge {
    /// Starts kessel-audio with `cfg` and bridges it to `call` (on the current tokio runtime).
    pub fn start(call: &Call, cfg: EngineConfig) -> Result<AudioBridge, AudioBridgeError> {
        let frames = call.frames().ok_or(AudioBridgeError::FramesTaken)?;
        let engine = Arc::new(AudioEngine::start(cfg)?);
        // The beacon's played and concealed audio are kessel-audio's own counters.
        call.set_playout_stats(Arc::new(KesselPlayout(Arc::downgrade(&engine))));
        let identities = Identities::default();
        let task = tokio::spawn(bridge(
            engine.clone(),
            identities.clone(),
            call.frame_sender(),
            frames,
        ));
        Ok(AudioBridge {
            engine,
            identities,
            task,
        })
    }

    /// The kessel-audio engine (levels, stats, devices).
    pub fn engine(&self) -> &AudioEngine {
        &self.engine
    }

    /// kessel-audio's latest levels, the remote ones by engine identity (a track whose
    /// identity the bridge has not seen yet is left out).
    pub fn levels(&self) -> BridgeLevels {
        let levels = self.engine.levels();
        let ids = self
            .identities
            .lock()
            .map(|m| m.clone())
            .unwrap_or_default();
        BridgeLevels {
            local: levels.local,
            remotes: levels
                .participants
                .iter()
                .filter_map(|(p, l)| ids.get(p).map(|i| (i.clone(), *l)))
                .collect(),
        }
    }
}

/// kessel-audio's per-participant playout counters for the stats windows (none once the
/// bridge is gone).
struct KesselPlayout(Weak<AudioEngine>);

impl PlayoutStats for KesselPlayout {
    fn playout_totals(&self) -> Vec<(u64, PlayoutTotals)> {
        let Some(engine) = self.0.upgrade() else {
            return Vec::new();
        };
        engine
            .stats()
            .participants
            .iter()
            .map(|(id, p)| {
                (
                    *id,
                    PlayoutTotals::new(p.frames_decoded, p.frames_fec, p.frames_concealed),
                )
            })
            .collect()
    }
}

async fn bridge(
    engine: Arc<AudioEngine>,
    identities: Identities,
    send: FrameSender,
    mut frames: FrameStream,
) {
    let capture = engine.capture_frames();
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        while let Some(f) = capture.try_recv() {
            send.push_opus(OpusFrame {
                payload: f.payload,
                ts48k: f.ts48k,
                vad: f.vad,
                level_dbov: f.level_dbov,
            });
        }
        loop {
            match frames.0.try_recv() {
                Ok(item) => apply(&engine, &identities, item),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return,
            }
        }
    }
}

fn apply(engine: &AudioEngine, identities: &Identities, item: RecvItem) {
    match item {
        RecvItem::Added {
            participant,
            identity,
        } => {
            if let Ok(mut m) = identities.lock() {
                m.insert(participant, identity);
            }
            engine.add_participant(participant);
        }
        RecvItem::Removed { participant } => {
            if let Ok(mut m) = identities.lock() {
                m.remove(&participant);
            }
            engine.remove_participant(participant);
        }
        RecvItem::Frame(f) => engine.push(RecvFrame {
            participant: f.participant,
            payload: f.payload,
            seq: f.seq,
            ts48k: f.ts48k,
            arrival: f.arrival,
        }),
        RecvItem::Lost { participant, seq } => engine.lost(participant, seq),
        RecvItem::Capture(on) => engine.set_capture(on),
        RecvItem::Deafened(on) => engine.set_deafened(on),
    }
}
