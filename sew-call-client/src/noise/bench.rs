//! The noise removal stage's device bench (feature `noise-bench`; for debug builds only, never
//! a release): the microphone through kessel-audio on this platform's audio (the OS's voice
//! processing on phones, the system's devices on desktop) and the stage a call would get
//! (DeepFilterNet3 when its weights are loaded, RNNoise otherwise), for a while, with no call.
//! The report says what each block cost, the process's memory, which model ran and every
//! step-down.
//!
//! On a phone the app's audio session must be active (iOS: an `AVAudioSession` in a voice
//! category, activated; the bench reports to and waits for the [`AudioSession`] as a call
//! does).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use kessel_audio::{AudioEngine, CaptureStage, DeviceChoice};
use serde::Serialize;
use sew_noise::{Mode, StageControl};

use super::control::NoiseStage;
use super::hook;
use super::{NoiseModel, NoiseRemovalControl};
use crate::audio::{AudioBridgeError, AudioSession};

/// One step-down from DeepFilterNet3 to RNNoise.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[non_exhaustive]
pub struct StepDownEvent {
    /// Seconds from the bench's start.
    pub at_s: f64,
    /// The stage's block count then.
    pub block: u64,
    /// `slow_block`, `slow_window`, `heavy`, `fault` or `forced`.
    pub reason: &'static str,
}

/// The device's thermal state at one moment, as the OS reports it.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[non_exhaustive]
pub struct ThermalSample {
    /// Seconds from the bench's start.
    pub at_s: f64,
    /// What the OS says (for example `"zone0 41.5 C"`); `None` where this build can't read it.
    pub state: Option<String>,
}

/// What the bench measured. Its JSON is the report an app saves or sends.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[non_exhaustive]
pub struct NoiseBenchReport {
    /// `target_os` and `target_arch`.
    pub os: &'static str,
    pub arch: &'static str,
    /// The device model, where the OS gives it without native code; else `None`.
    pub device_model: Option<String>,
    /// How long the microphone ran (s).
    pub seconds: f64,
    /// Blocks the stage processed, and how many were timed.
    pub blocks: u64,
    pub timed_blocks: u64,
    /// The stage's time per 10 ms block (µs), over the timed blocks.
    pub block_us_p50: Option<f64>,
    pub block_us_p99: Option<f64>,
    pub block_us_max: Option<f64>,
    /// The share of a core at the median block (%).
    pub core_share_p50: Option<f64>,
    /// DeepFilterNet3's weights were loaded, and what that cost.
    pub dfn3_loaded: bool,
    pub rss_before_load_kib: Option<u64>,
    pub rss_after_load_kib: Option<u64>,
    pub load_ms: Option<u64>,
    /// Resident memory at the bench's start and end, and the peak (KiB; `None` where this
    /// build can't read it).
    pub rss_start_kib: Option<u64>,
    pub rss_end_kib: Option<u64>,
    pub rss_peak_kib: Option<u64>,
    /// Blocks the model ran in.
    pub model_blocks: u64,
    /// The model running at the end (`None`: the stage was off or bypassed).
    pub active_model: Option<NoiseModel>,
    /// Where DeepFilterNet3 was at the end (`idle`, `warming`, `running`, `stepped_down`,
    /// `retired`).
    pub strong_state: &'static str,
    /// Every step-down, in order.
    pub step_downs: Vec<StepDownEvent>,
    /// kessel-audio's view of the stage: strikes, whether it bypassed it (and why), its own
    /// p99 and max per block (µs).
    pub kessel_strikes: u32,
    pub kessel_bypassed: Option<String>,
    pub kessel_permanent: bool,
    pub kessel_p99_us: u32,
    pub kessel_max_us: u32,
    /// The thermal state at the start, every 10 s, and at the end.
    pub thermal: Vec<ThermalSample>,
    /// What this build could not measure here.
    pub notes: Vec<String>,
}

/// The stage, each call timed into preallocated slots (no lock, no allocation).
struct Timed {
    stage: NoiseStage,
    times: Arc<[AtomicU32]>,
    next: Arc<AtomicUsize>,
}

impl CaptureStage for Timed {
    fn process(&mut self, block: &mut [f32; 480]) {
        let t = Instant::now();
        self.stage.process(block);
        let ns = u32::try_from(t.elapsed().as_nanos()).unwrap_or(u32::MAX);
        let i = self.next.fetch_add(1, Ordering::Relaxed);
        if let Some(slot) = self.times.get(i) {
            slot.store(ns, Ordering::Relaxed);
        }
    }

    fn latency_samples(&self) -> usize {
        self.stage.latency_samples()
    }

    fn reset(&mut self) {
        self.stage.reset();
    }
}

/// Runs the bench for `duration` on this platform's default microphone and speaker (blocking:
/// call it off the UI thread). `control` gives the setting (Standard runs DeepFilterNet3 when
/// its weights were loaded, RNNoise otherwise) and the weights; `session` is the app's audio
/// session (on iOS nothing opens until it is active).
pub fn run(
    control: &NoiseRemovalControl,
    session: &AudioSession,
    duration: Duration,
) -> Result<NoiseBenchReport, AudioBridgeError> {
    let mut notes = Vec::new();
    let started = Instant::now();
    let at = |t: Instant| t.duration_since(started).as_secs_f64();
    let rss_start = super::rss_kib();
    let mut thermal = vec![ThermalSample {
        at_s: 0.0,
        state: thermal_state(),
    }];
    let load = control.dfn3_info();
    let stage = control.build_stage();
    let ctl = stage.control();
    // 10 ms blocks, and room for the warm-up.
    let slots = usize::try_from(duration.as_millis() / 10).unwrap_or(usize::MAX / 2) + 1_000;
    let times: Arc<[AtomicU32]> = (0..slots).map(|_| AtomicU32::new(0)).collect();
    let next = Arc::new(AtomicUsize::new(0));
    let mut cfg = crate::audio::platform_devices(DeviceChoice::Default, DeviceChoice::Default);
    let _ = hook::install(
        &mut cfg.processing,
        Box::new(Timed {
            stage: NoiseStage::new(stage),
            times: Arc::clone(&times),
            next: Arc::clone(&next),
        }),
    );
    let (engine, deferred) = crate::audio::start_engine(cfg)?;
    let engine = Arc::new(engine);
    let weak: std::sync::Weak<AudioEngine> = Arc::downgrade(&engine);
    let sink: std::sync::Weak<dyn crate::audio::SessionSink> = weak;
    let session_id = session.attach(sink, deferred);
    let frames = engine.capture_frames();
    let mut step_downs = Vec::new();
    let mut seen = 0u64;
    let mut last_thermal = Instant::now();
    while started.elapsed() < duration {
        std::thread::sleep(Duration::from_millis(100));
        while frames.try_recv().is_some() {}
        let st = ctl.stats();
        if st.step_downs > seen {
            seen = st.step_downs;
            step_downs.push(StepDownEvent {
                at_s: at(Instant::now()),
                block: st.last_step_down_block,
                reason: st.last_step_down.map_or("unknown", |r| r.as_str()),
            });
        }
        if last_thermal.elapsed() >= Duration::from_secs(10) {
            last_thermal = Instant::now();
            thermal.push(ThermalSample {
                at_s: at(last_thermal),
                state: thermal_state(),
            });
        }
    }
    let kessel = engine.stats().capture_stage;
    let st = ctl.stats();
    let active_model = active_model(&ctl);
    let rss_end = super::rss_kib();
    let rss_peak = super::rss_peak_kib();
    engine.shutdown();
    session.detach(session_id);
    thermal.push(ThermalSample {
        at_s: at(Instant::now()),
        state: thermal_state(),
    });

    let n = next.load(Ordering::Relaxed).min(times.len());
    let mut us: Vec<f64> = times[..n]
        .iter()
        .map(|t| f64::from(t.load(Ordering::Relaxed)) / 1_000.0)
        .collect();
    us.sort_by(f64::total_cmp);
    let q = |p: f64| (!us.is_empty()).then(|| us[((us.len() - 1) as f64 * p).round() as usize]);
    if rss_start.is_none() {
        notes.push("resident memory: not readable on this platform without native code".into());
    }
    if rss_peak.is_none() {
        notes.push("peak resident memory: only on Linux and Android (VmHWM)".into());
    }
    let device_model = device_model();
    if device_model.is_none() {
        notes.push("device model: not readable on this platform without native code".into());
    }
    if thermal.iter().all(|t| t.state.is_none()) {
        notes.push("thermal state: not readable on this platform without native code".into());
    }
    if n == 0 {
        notes.push(
            "no block was processed: the microphone did not open (permission, or the audio session not active)"
                .into(),
        );
    }
    Ok(NoiseBenchReport {
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        device_model,
        seconds: at(Instant::now()),
        blocks: st.blocks,
        timed_blocks: n as u64,
        block_us_p50: q(0.5),
        block_us_p99: q(0.99),
        block_us_max: q(1.0),
        core_share_p50: q(0.5).map(|v| v / 10_000.0 * 100.0),
        dfn3_loaded: load.0,
        rss_before_load_kib: load.1.and_then(|l| l.0),
        rss_after_load_kib: load.1.and_then(|l| l.1),
        load_ms: load.1.map(|l| l.2),
        rss_start_kib: rss_start,
        rss_end_kib: rss_end,
        rss_peak_kib: rss_peak,
        model_blocks: st.model_blocks,
        active_model,
        strong_state: st.strong_state.as_str(),
        step_downs,
        kessel_strikes: kessel.as_ref().map_or(0, |k| k.strikes),
        kessel_bypassed: kessel
            .as_ref()
            .and_then(|k| k.bypassed.as_ref())
            .map(|r| format!("{r:?}")),
        kessel_permanent: kessel.as_ref().is_some_and(|k| k.permanent),
        kessel_p99_us: kessel.as_ref().map_or(0, |k| k.p99_block_us),
        kessel_max_us: kessel.as_ref().map_or(0, |k| k.max_block_us),
        thermal,
        notes,
    })
}

fn active_model(ctl: &StageControl) -> Option<NoiseModel> {
    match ctl.active() {
        Mode::Off => None,
        Mode::Strong => Some(NoiseModel::Dfn3),
        _ => Some(NoiseModel::Rnnoise),
    }
}

/// The device model: DMI on Linux, `ro.product.model` on Android, `hw.model` on macOS.
fn device_model() -> Option<String> {
    let out = |cmd: &str, args: &[&str]| {
        std::process::Command::new(cmd)
            .args(args)
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .filter(|s| !s.is_empty())
    };
    if cfg!(target_os = "android") {
        out("getprop", &["ro.product.model"])
    } else if cfg!(target_os = "macos") {
        out("sysctl", &["-n", "hw.model"])
    } else if cfg!(target_os = "linux") {
        std::fs::read_to_string("/sys/devices/virtual/dmi/id/product_name")
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    } else {
        None
    }
}

/// The thermal state: the first thermal zone's temperature on Linux and Android (when the
/// OS lets an app read it), `pmset -g therm`'s CPU speed limit on macOS.
fn thermal_state() -> Option<String> {
    if cfg!(any(target_os = "linux", target_os = "android")) {
        let t = std::fs::read_to_string("/sys/class/thermal/thermal_zone0/temp").ok()?;
        let milli: f64 = t.trim().parse().ok()?;
        Some(format!("zone0 {:.1} C", milli / 1_000.0))
    } else if cfg!(target_os = "macos") {
        let o = std::process::Command::new("pmset")
            .args(["-g", "therm"])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&o.stdout);
        text.lines()
            .find(|l| l.contains("CPU_Speed_Limit"))
            .map(|l| l.trim().to_owned())
    } else {
        None
    }
}
