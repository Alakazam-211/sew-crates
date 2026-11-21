//! A measuring stage (cargo feature `probe`; for measurement builds only, never a release).
//!
//! [`ProbeStage`] wraps a [`Stage`] and records, for every block, what the stage's `process`
//! call cost on the wall clock and in the calling thread's CPU time, which core the thread was
//! on and what the model ran. Beside the stage it runs two fixed loops on the same thread, so
//! the thread's speed can be told apart from the stage's work:
//! - the cache loop, arithmetic that stays in the smallest cache ([`Kernels::cache_loop`]), after
//!   the stage in every [`CACHE_LOOP_EVERY`]th block;
//! - the memory loop, one pass over memory larger than any cache ([`Kernels::memory_loop`]), after
//!   the stage in every [`MEMORY_LOOP_EVERY`]th block. That block and the one after it are left out
//!   of the stage's figures (the memory loop empties the caches the stage's next block would have
//!   used).
//!
//! [`reference()`] runs the same two loops on the calling thread after a spin, for a figure of
//! what the device does on a thread that asks the OS for its highest ordinary priority.
//!
//! The stage's output is the wrapped stage's, sample for sample. The step-down rules of the wrapped
//! stage are turned off for its life, so the model keeps running however slow a block is. The two
//! loops add time to the blocks they run in (the memory loop the most),
//! which an audio pipeline's own timing of the call sees: a probed call can glitch.
//!
//! `process` allocates nothing and takes no lock: the records go into slots made at
//! construction. [`ProbeHandle::report`] reads them from any thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::thread_clock::thread_cpu_ns;
use crate::{FRAME, Stage, StageControl};

/// The model ran in the block.
pub const RAN_MODEL: u8 = 1;
/// The model's ERB decoder ran in the block.
pub const RAN_ERB: u8 = 1 << 1;
/// The model's deep-filter decoder ran in the block.
pub const RAN_DF: u8 = 1 << 2;
/// Some of the block's output is the model's.
pub const STRONG_OUT: u8 = 1 << 3;
/// The memory loop ran after the stage in this block, or in the block before: left out of the
/// figures.
const LEFT_OUT: u8 = 1 << 4;
/// The thread CPU clock read for this record.
const CPU_TIMED: u8 = 1 << 5;

/// The cache loop runs after the stage in every block whose number is one less than a multiple of
/// this.
pub const CACHE_LOOP_EVERY: u64 = 10;
/// The memory loop runs after the stage in every block whose number is one less than a multiple of
/// this (every 5 s).
pub const MEMORY_LOOP_EVERY: u64 = 500;
/// Floats in each of the cache loop's two arrays.
pub const CACHE_LOOP_FLOATS: usize = 4096;
/// The cache loop's passes over its arrays.
pub const CACHE_LOOP_PASSES: usize = 200;
/// Floats in the memory loop's array (32 MiB).
pub const MEMORY_LOOP_FLOATS: usize = 8 * 1024 * 1024;
/// Blocks a report gives raw, from the first.
pub const RAW_BLOCKS: usize = 200;
/// Blocks a [`ProbeStage::new`] stage has room for (5 minutes); later ones are counted as
/// dropped.
pub const DEFAULT_BLOCKS: usize = 30_000;

/// How long [`reference()`] spins before it measures.
pub const REFERENCE_SPIN: Duration = Duration::from_millis(300);
/// The cache loop runs in a [`reference()`] run.
pub const REFERENCE_CACHE_LOOP_RUNS: usize = 20;
/// The memory loop runs in a [`reference()`] run.
pub const REFERENCE_MEMORY_LOOP_RUNS: usize = 5;

/// The core the calling thread is on: `sched_getcpu` on Linux and Android, `None` elsewhere
/// (a caller on another system gives [`ProbeStage::new`] its own function).
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn core_number() -> Option<u32> {
    u32::try_from(rustix::thread::sched_getcpu()).ok()
}

/// The core the calling thread is on: `sched_getcpu` on Linux and Android, `None` elsewhere
/// (a caller on another system gives [`ProbeStage::new`] its own function).
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub fn core_number() -> Option<u32> {
    None
}

/// A sum of products over two runs of floats, in 16 partial sums.
fn dot(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0.0f32; 16];
    for (ca, cb) in a.as_chunks::<16>().0.iter().zip(b.as_chunks::<16>().0) {
        for ((s, x), y) in acc.iter_mut().zip(ca).zip(cb) {
            *s += x * y;
        }
    }
    acc.iter().sum()
}

/// The two fixed loops' data. Building it allocates (32 MiB and a little); the loops do not.
pub struct Kernels {
    a: Box<[f32]>,
    b: Box<[f32]>,
    big: Box<[f32]>,
}

impl std::fmt::Debug for Kernels {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kernels").finish_non_exhaustive()
    }
}

impl Default for Kernels {
    fn default() -> Self {
        Self::new()
    }
}

impl Kernels {
    /// The arrays, filled with a fixed pseudo-random sequence in `[-1, 1)`.
    pub fn new() -> Self {
        let mut seed = 0x9e37_79b9_u32;
        let mut fill = |n: usize| -> Box<[f32]> {
            (0..n)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    // 24 bits, so the value is exact in a float.
                    (seed >> 8) as f32 / 8_388_608.0 - 1.0
                })
                .collect()
        };
        let a = fill(CACHE_LOOP_FLOATS);
        let b = fill(CACHE_LOOP_FLOATS);
        let big = fill(MEMORY_LOOP_FLOATS);
        Self { a, b, big }
    }

    /// The cache loop: [`CACHE_LOOP_PASSES`] sums of products over two arrays of
    /// [`CACHE_LOOP_FLOATS`] floats (32 KiB in all, which stays in the smallest cache). The same
    /// work every call.
    pub fn cache_loop(&self) -> f32 {
        let mut sum = 0.0f32;
        for _ in 0..CACHE_LOOP_PASSES {
            sum += dot(std::hint::black_box(&self.a), &self.b);
        }
        std::hint::black_box(sum)
    }

    /// The memory loop: one pass of sums of products over [`MEMORY_LOOP_FLOATS`] floats (32 MiB,
    /// read once, in order) against the cache loop's second array. The same work every call.
    pub fn memory_loop(&self) -> f32 {
        let mut sum = 0.0f32;
        for chunk in self.big.as_chunks::<CACHE_LOOP_FLOATS>().0 {
            sum += dot(chunk, &self.b);
        }
        std::hint::black_box(sum)
    }
}

/// Nanoseconds as 32 bits (a record's times; 4.29 s and more reads as the largest).
fn ns32(ns: u64) -> u64 {
    ns.min(u64::from(u32::MAX))
}

/// A timed span: wall nanoseconds and thread CPU nanoseconds (`None` without the clock).
#[derive(Clone, Copy)]
struct Span {
    started: Instant,
    cpu: Option<u64>,
}

impl Span {
    fn start() -> Self {
        Self {
            started: Instant::now(),
            cpu: thread_cpu_ns(),
        }
    }

    /// The span as a record's time word and its CPU flag.
    fn end(self) -> (u64, u8) {
        let cpu = self
            .cpu
            .and_then(|t0| Some(thread_cpu_ns()?.saturating_sub(t0)));
        let wall = u64::try_from(self.started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        (
            (ns32(wall) << 32) | ns32(cpu.unwrap_or(0)),
            if cpu.is_some() { CPU_TIMED } else { 0 },
        )
    }
}

/// One table of records: a time word and a meta word each, and how many are written.
struct Table {
    times: Box<[AtomicU64]>,
    meta: Box<[AtomicU64]>,
    len: AtomicUsize,
}

impl Table {
    fn new(cap: usize) -> Self {
        Self {
            times: (0..cap).map(|_| AtomicU64::new(0)).collect(),
            meta: (0..cap).map(|_| AtomicU64::new(0)).collect(),
            len: AtomicUsize::new(0),
        }
    }

    /// Adds a record (the one writer's call). False: the table is full.
    fn push(&self, times: u64, meta: u64) -> bool {
        let i = self.len.load(Ordering::Relaxed);
        let (Some(t), Some(m)) = (self.times.get(i), self.meta.get(i)) else {
            return false;
        };
        t.store(times, Ordering::Relaxed);
        m.store(meta, Ordering::Relaxed);
        self.len.store(i + 1, Ordering::Release);
        true
    }

    /// The records written so far.
    fn read(&self) -> Vec<Record> {
        let n = self.len.load(Ordering::Acquire);
        self.times
            .iter()
            .zip(self.meta.iter())
            .take(n)
            .map(|(t, m)| Record::unpack(t.load(Ordering::Relaxed), m.load(Ordering::Relaxed)))
            .collect()
    }
}

/// A record as written: a block of the stage, or a run of the cache loop or the memory loop.
#[derive(Clone, Copy)]
struct Record {
    wall_ns: u32,
    cpu_ns: Option<u32>,
    core: Option<u32>,
    flags: u8,
    phase: u8,
}

impl Record {
    /// The meta word: the core plus one in the low 32 bits (0: unknown), then the flags, then
    /// the phase.
    fn meta(core: Option<u32>, flags: u8, phase: u8) -> u64 {
        let core = core.map_or(0, |c| u64::from(c.min(u32::MAX - 1)) + 1);
        core | (u64::from(flags) << 32) | (u64::from(phase) << 40)
    }

    fn unpack(times: u64, meta: u64) -> Self {
        let flags = (meta >> 32) as u8;
        let core = (meta & 0xffff_ffff) as u32;
        Self {
            wall_ns: (times >> 32) as u32,
            cpu_ns: (flags & CPU_TIMED != 0).then_some(times as u32),
            core: core.checked_sub(1),
            flags,
            phase: (meta >> 40) as u8,
        }
    }
}

struct Shared {
    blocks: Table,
    cache_loop: Table,
    memory_loop: Table,
    dropped: AtomicU64,
    phase: AtomicU8,
}

/// A [`Stage`] that records what its blocks cost. See the module's text.
pub struct ProbeStage {
    stage: Stage,
    kernels: Kernels,
    shared: Arc<Shared>,
    core: fn() -> Option<u32>,
    /// Blocks processed.
    n: u64,
    /// The memory loop ran after the last block.
    after_memory_loop: bool,
}

impl std::fmt::Debug for ProbeStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProbeStage")
            .field("blocks", &self.n)
            .finish_non_exhaustive()
    }
}

impl ProbeStage {
    /// Wraps `stage`, with room for [`DEFAULT_BLOCKS`] blocks. `core` answers the core the
    /// calling thread is on ([`core_number`], or the caller's own where that has none); it
    /// runs on the audio thread once a block and after each loop, so it must not allocate or
    /// block. Turns the stage's step-down rules off. Allocates (the loops' 32 MiB and the
    /// records); build it off the audio thread.
    pub fn new(stage: Stage, core: fn() -> Option<u32>) -> Self {
        Self::with_capacity(stage, core, DEFAULT_BLOCKS)
    }

    /// [`Self::new`] with room for `blocks` blocks.
    pub fn with_capacity(stage: Stage, core: fn() -> Option<u32>, blocks: usize) -> Self {
        stage.control().set_step_down_rules(false);
        let per = |every: u64| blocks / usize::try_from(every).unwrap_or(usize::MAX) + 1;
        Self {
            stage,
            kernels: Kernels::new(),
            shared: Arc::new(Shared {
                blocks: Table::new(blocks),
                cache_loop: Table::new(per(CACHE_LOOP_EVERY)),
                memory_loop: Table::new(per(MEMORY_LOOP_EVERY)),
                dropped: AtomicU64::new(0),
                phase: AtomicU8::new(0),
            }),
            core,
            n: 0,
            after_memory_loop: false,
        }
    }

    /// The handle that sets the phase and reads the report, from any thread.
    pub fn handle(&self) -> ProbeHandle {
        ProbeHandle {
            shared: self.shared.clone(),
        }
    }

    /// The wrapped stage's control.
    pub fn control(&self) -> StageControl {
        self.stage.control()
    }

    /// [`Stage::LATENCY`].
    pub const fn latency_samples(&self) -> usize {
        Stage::LATENCY
    }

    /// [`Stage::reset`]. The records are kept.
    pub fn reset(&mut self) {
        self.stage.reset();
    }

    /// [`Stage::process`], recorded; then the cache loop or the memory loop when the block's number
    /// says so.
    pub fn process(&mut self, block: &mut [f32; FRAME]) {
        let phase = self.shared.phase.load(Ordering::Relaxed);
        let run_memory_loop = self.n % MEMORY_LOOP_EVERY == MEMORY_LOOP_EVERY - 1;
        let run_cache_loop = !run_memory_loop && self.n % CACHE_LOOP_EVERY == CACHE_LOOP_EVERY - 1;

        let span = Span::start();
        self.stage.process(block);
        let (times, timed) = span.end();
        let core = (self.core)();
        let mut flags = self.stage.probe_flags() | timed;
        if run_memory_loop || self.after_memory_loop {
            flags |= LEFT_OUT;
        }
        if !self
            .shared
            .blocks
            .push(times, Record::meta(core, flags, phase))
        {
            self.shared.dropped.fetch_add(1, Ordering::Relaxed);
        }
        self.after_memory_loop = run_memory_loop;
        self.n += 1;

        if run_cache_loop {
            let span = Span::start();
            self.kernels.cache_loop();
            let (times, timed) = span.end();
            self.shared
                .cache_loop
                .push(times, Record::meta((self.core)(), timed, phase));
        } else if run_memory_loop {
            let span = Span::start();
            self.kernels.memory_loop();
            let (times, timed) = span.end();
            self.shared
                .memory_loop
                .push(times, Record::meta((self.core)(), timed, phase));
        }
    }
}

#[cfg(feature = "capture-stage")]
impl crate::capture_stage::CaptureStage for ProbeStage {
    fn process(&mut self, block: &mut [f32; FRAME]) {
        ProbeStage::process(self, block);
    }

    fn latency_samples(&self) -> usize {
        Stage::LATENCY
    }

    fn reset(&mut self) {
        ProbeStage::reset(self);
    }
}

/// A [`ProbeStage`]'s phase and report, from any thread.
#[derive(Clone)]
pub struct ProbeHandle {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for ProbeHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProbeHandle").finish_non_exhaustive()
    }
}

impl ProbeHandle {
    /// Marks the blocks from now on as phase `phase` (0 at the start). The number is the
    /// caller's: the report groups the figures by it.
    pub fn set_phase(&self, phase: u8) {
        self.shared.phase.store(phase, Ordering::Relaxed);
    }

    /// The phase blocks are marked with now.
    pub fn phase(&self) -> u8 {
        self.shared.phase.load(Ordering::Relaxed)
    }

    /// Blocks recorded so far.
    pub fn blocks(&self) -> u64 {
        self.shared.blocks.len.load(Ordering::Acquire) as u64
    }

    /// The figures so far. Allocates; not for the audio thread.
    pub fn report(&self) -> ProbeReport {
        let blocks = self.shared.blocks.read();
        let k1 = self.shared.cache_loop.read();
        let k2 = self.shared.memory_loop.read();
        let mut order: Vec<u8> = Vec::new();
        for r in &blocks {
            if !order.contains(&r.phase) {
                order.push(r.phase);
            }
        }
        let phases = order
            .into_iter()
            .map(|p| phase_report(p, &blocks, &k1, &k2))
            .collect();
        ProbeReport {
            blocks: blocks.len() as u64,
            dropped: self.shared.dropped.load(Ordering::Relaxed),
            cpu_clock: blocks.iter().any(|r| r.cpu_ns.is_some()),
            phases,
            first_blocks: blocks.iter().take(RAW_BLOCKS).map(RawBlock::of).collect(),
        }
    }
}

fn phase_report(phase: u8, blocks: &[Record], k1: &[Record], k2: &[Record]) -> PhaseReport {
    let mine: Vec<&Record> = blocks.iter().filter(|r| r.phase == phase).collect();
    let kept: Vec<&Record> = mine
        .iter()
        .copied()
        .filter(|r| r.flags & LEFT_OUT == 0)
        .collect();
    let full: Vec<&Record> = kept
        .iter()
        .copied()
        .filter(|r| r.flags & RAN_ERB != 0 && r.flags & RAN_DF != 0)
        .collect();
    let count = |bit: u8| kept.iter().filter(|r| r.flags & bit != 0).count() as u64;
    let mut cores: Vec<CoreCount> = Vec::new();
    let mut core_unknown = 0u64;
    for r in &mine {
        match r.core {
            None => core_unknown += 1,
            Some(core) => match cores.iter_mut().find(|c| c.core == core) {
                Some(c) => c.blocks += 1,
                None => cores.push(CoreCount { core, blocks: 1 }),
            },
        }
    }
    cores.sort_by_key(|c| c.core);
    let of_phase =
        |t: &[Record]| -> Vec<Record> { t.iter().filter(|r| r.phase == phase).copied().collect() };
    let (k1, k2) = (of_phase(k1), of_phase(k2));
    let refs = |t: &[Record]| -> (Quantiles, Option<Quantiles>) {
        let v: Vec<&Record> = t.iter().collect();
        (wall(&v), cpu(&v))
    };
    let (k1_wall, k1_cpu) = refs(&k1);
    let (k2_wall, k2_cpu) = refs(&k2);
    PhaseReport {
        phase,
        blocks: mine.len() as u64,
        left_out: (mine.len() - kept.len()) as u64,
        model_blocks: count(RAN_MODEL),
        erb_blocks: count(RAN_ERB),
        df_blocks: count(RAN_DF),
        strong_out_blocks: count(STRONG_OUT),
        stage_wall: wall(&kept),
        stage_cpu: cpu(&kept),
        full_wall: wall(&full),
        full_cpu: cpu(&full),
        k1_wall,
        k1_cpu,
        k2_wall,
        k2_cpu,
        cores,
        core_unknown,
    }
}

fn wall(v: &[&Record]) -> Quantiles {
    Quantiles::of(v.iter().map(|r| u64::from(r.wall_ns)).collect())
}

/// The CPU times' figures; `None` when no record has one.
fn cpu(v: &[&Record]) -> Option<Quantiles> {
    let t: Vec<u64> = v.iter().filter_map(|r| r.cpu_ns.map(u64::from)).collect();
    (!t.is_empty()).then(|| Quantiles::of(t))
}

/// The median, the 99th percentile and the largest of some times (nearest rank).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct Quantiles {
    /// How many times went in.
    pub n: u64,
    /// The median, nanoseconds (0 of none).
    pub p50_ns: u64,
    /// The 99th percentile, nanoseconds.
    pub p99_ns: u64,
    /// The largest, nanoseconds.
    pub max_ns: u64,
}

impl Quantiles {
    fn of(mut v: Vec<u64>) -> Self {
        v.sort_unstable();
        // Nearest rank: the smallest value with at least p of the values at or under it.
        let rank = |num: usize, den: usize| -> u64 {
            let k = (v.len() * num).div_ceil(den).max(1);
            v.get(k - 1).copied().unwrap_or(0)
        };
        Self {
            n: v.len() as u64,
            p50_ns: rank(1, 2),
            p99_ns: rank(99, 100),
            max_ns: v.last().copied().unwrap_or(0),
        }
    }
}

/// Blocks a thread spent on one core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct CoreCount {
    /// The core's number, as the system counts them.
    pub core: u32,
    /// Blocks that ended on it.
    pub blocks: u64,
}

/// One phase's figures.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PhaseReport {
    /// The phase's number ([`ProbeHandle::set_phase`]).
    pub phase: u8,
    /// Blocks in the phase.
    pub blocks: u64,
    /// Of those, the blocks left out of the figures below (the memory loop ran in them or just
    /// before).
    pub left_out: u64,
    /// Of the blocks kept: those the model ran in, those its ERB decoder ran in, those its
    /// deep-filter decoder ran in, and those whose output was the model's.
    pub model_blocks: u64,
    pub erb_blocks: u64,
    pub df_blocks: u64,
    pub strong_out_blocks: u64,
    /// The stage's `process` call over the blocks kept: wall time, and the thread's CPU time
    /// (`None` without that clock).
    pub stage_wall: Quantiles,
    pub stage_cpu: Option<Quantiles>,
    /// The same over the blocks in which both of the model's decoders ran (its full work).
    pub full_wall: Quantiles,
    pub full_cpu: Option<Quantiles>,
    /// The cache loop's runs in the phase (`k1_*`).
    pub k1_wall: Quantiles,
    pub k1_cpu: Option<Quantiles>,
    /// The memory loop's runs in the phase (`k2_*`).
    pub k2_wall: Quantiles,
    pub k2_cpu: Option<Quantiles>,
    /// Blocks per core, by core number (every block of the phase, read after the stage ran).
    pub cores: Vec<CoreCount>,
    /// Blocks whose core is not known.
    pub core_unknown: u64,
}

/// One block as recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct RawBlock {
    pub phase: u8,
    /// The stage's `process` call: wall nanoseconds, and thread CPU nanoseconds.
    pub wall_ns: u32,
    pub cpu_ns: Option<u32>,
    /// The core the thread was on after the stage ran.
    pub core: Option<u32>,
    /// The model ran; its ERB decoder ran; its deep-filter decoder ran; the output was the
    /// model's.
    pub model: bool,
    pub erb: bool,
    pub df: bool,
    pub strong_out: bool,
    /// Left out of the phase's figures (the memory loop ran in this block or just before).
    pub left_out: bool,
}

impl RawBlock {
    fn of(r: &Record) -> Self {
        Self {
            phase: r.phase,
            wall_ns: r.wall_ns,
            cpu_ns: r.cpu_ns,
            core: r.core,
            model: r.flags & RAN_MODEL != 0,
            erb: r.flags & RAN_ERB != 0,
            df: r.flags & RAN_DF != 0,
            strong_out: r.flags & STRONG_OUT != 0,
            left_out: r.flags & LEFT_OUT != 0,
        }
    }
}

/// What a [`ProbeStage`] recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ProbeReport {
    /// Blocks recorded, and blocks that came after the records were full.
    pub blocks: u64,
    pub dropped: u64,
    /// The thread CPU clock read on this system.
    pub cpu_clock: bool,
    /// The phases, in the order they began.
    pub phases: Vec<PhaseReport>,
    /// The first [`RAW_BLOCKS`] blocks.
    pub first_blocks: Vec<RawBlock>,
}

/// The cache loop and the memory loop on a thread the OS has had time to speed up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReferenceRun {
    /// The core the thread was on after the spin, and at the end.
    pub core_start: Option<u32>,
    pub core_end: Option<u32>,
    /// The cache loop over [`REFERENCE_CACHE_LOOP_RUNS`] runs (`k1_*`).
    pub k1_wall: Quantiles,
    pub k1_cpu: Option<Quantiles>,
    /// The memory loop over [`REFERENCE_MEMORY_LOOP_RUNS`] runs (`k2_*`).
    pub k2_wall: Quantiles,
    pub k2_cpu: Option<Quantiles>,
}

/// Spins for [`REFERENCE_SPIN`], then runs the cache loop [`REFERENCE_CACHE_LOOP_RUNS`] times and
/// the memory loop [`REFERENCE_MEMORY_LOOP_RUNS`] times on the calling thread: the loops a
/// [`ProbeStage`] runs, the same code. Call it on a thread that asks the OS for its highest
/// ordinary priority, away from the audio thread. Blocks for the spin ([`REFERENCE_SPIN`]) and
/// the loops' runs, and holds 32 MiB.
pub fn reference(core: fn() -> Option<u32>) -> ReferenceRun {
    let kernels = Kernels::new();
    let start = Instant::now();
    let mut x = 0u64;
    while start.elapsed() < REFERENCE_SPIN {
        x = std::hint::black_box(x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1));
    }
    let core_start = core();
    let run = |n: usize, f: &dyn Fn() -> f32| -> Vec<Record> {
        (0..n)
            .map(|_| {
                let span = Span::start();
                f();
                let (times, timed) = span.end();
                Record::unpack(times, Record::meta(None, timed, 0))
            })
            .collect()
    };
    let k1 = run(REFERENCE_CACHE_LOOP_RUNS, &|| kernels.cache_loop());
    let k2 = run(REFERENCE_MEMORY_LOOP_RUNS, &|| kernels.memory_loop());
    let figures = |t: &[Record]| -> (Quantiles, Option<Quantiles>) {
        let v: Vec<&Record> = t.iter().collect();
        (wall(&v), cpu(&v))
    };
    let (k1_wall, k1_cpu) = figures(&k1);
    let (k2_wall, k2_cpu) = figures(&k2);
    ReferenceRun {
        core_start,
        core_end: core(),
        k1_wall,
        k1_cpu,
        k2_wall,
        k2_cpu,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(seed: &mut u32, block: &mut [f32; FRAME]) {
        for s in block.iter_mut() {
            *seed ^= *seed << 13;
            *seed ^= *seed >> 17;
            *seed ^= *seed << 5;
            *s = ((*seed >> 8) as f32 / 8_388_608.0 - 1.0) * 0.1;
        }
    }

    /// The loops are the same work every call, and the memory loop reads its whole array.
    #[test]
    fn the_loops_are_fixed_work() {
        let k = Kernels::new();
        assert_eq!(k.cache_loop().to_bits(), k.cache_loop().to_bits());
        assert_eq!(k.memory_loop().to_bits(), k.memory_loop().to_bits());
        assert_eq!(
            k.cache_loop().to_bits(),
            Kernels::new().cache_loop().to_bits()
        );
        assert!(k.cache_loop().is_finite() && k.cache_loop() != 0.0);
        assert!(k.memory_loop().is_finite() && k.memory_loop() != 0.0);
        assert_eq!(
            k.big.len() % CACHE_LOOP_FLOATS,
            0,
            "the memory loop leaves no tail unread"
        );
        assert_eq!(CACHE_LOOP_FLOATS % 16, 0, "the sums leave no tail unread");
    }

    #[test]
    fn quantiles_are_nearest_rank() {
        assert_eq!(Quantiles::of(vec![]), Quantiles::default());
        let q = Quantiles::of(vec![5]);
        assert_eq!((q.n, q.p50_ns, q.p99_ns, q.max_ns), (1, 5, 5, 5));
        let q = Quantiles::of((1..=100).collect());
        assert_eq!((q.p50_ns, q.p99_ns, q.max_ns), (50, 99, 100));
        let q = Quantiles::of((1..=200).rev().collect());
        assert_eq!((q.p50_ns, q.p99_ns, q.max_ns), (100, 198, 200));
    }

    #[test]
    fn a_record_packs_and_unpacks() {
        let r = Record::unpack(
            (7u64 << 32) | 9,
            Record::meta(Some(0), RAN_MODEL | CPU_TIMED, 3),
        );
        assert_eq!(
            (r.wall_ns, r.cpu_ns, r.core, r.phase),
            (7, Some(9), Some(0), 3)
        );
        let r = Record::unpack(ns32(u64::MAX) << 32, Record::meta(None, 0, 255));
        assert_eq!(
            (r.wall_ns, r.cpu_ns, r.core, r.phase),
            (u32::MAX, None, None, 255)
        );
        let r = Record::unpack(0, Record::meta(Some(u32::MAX), 0, 0));
        assert_eq!(
            r.core,
            Some(u32::MAX - 1),
            "the largest core number still reads"
        );
    }

    /// The probed stage's output is the plain stage's, sample for sample; every block is recorded;
    /// the cache loop and the memory loop run when the block's number says so; the blocks around
    /// the memory loop are left out; the phases split the figures.
    #[test]
    fn the_stage_is_recorded_and_unchanged() {
        let blocks = usize::try_from(MEMORY_LOOP_EVERY).map_or(0, |k| k * 2 + 20);
        let mut plain = Stage::new();
        let mut probe = ProbeStage::with_capacity(Stage::new(), core_number, blocks);
        let handle = probe.handle();
        assert_eq!(probe.latency_samples(), Stage::LATENCY);
        let (mut s1, mut s2) = (1u32, 1u32);
        let (mut a, mut b) = ([0.0f32; FRAME], [0.0f32; FRAME]);
        let half = blocks / 2;
        for i in 0..blocks {
            if i == half {
                handle.set_phase(7);
            }
            noise(&mut s1, &mut a);
            noise(&mut s2, &mut b);
            plain.process(&mut a);
            probe.process(&mut b);
            assert!(
                a.iter().zip(&b).all(|(x, y)| x.to_bits() == y.to_bits()),
                "block {i} differs from the plain stage's"
            );
        }
        let r = handle.report();
        assert_eq!(handle.blocks(), blocks as u64);
        assert_eq!((r.blocks, r.dropped), (blocks as u64, 0));
        assert_eq!(r.first_blocks.len(), RAW_BLOCKS);
        assert_eq!(r.cpu_clock, crate::thread_clock::AVAILABLE);
        assert_eq!(r.phases.len(), 2);
        let (p0, p7) = (&r.phases[0], &r.phases[1]);
        assert_eq!((p0.phase, p7.phase), (0, 7));
        assert_eq!(p0.blocks + p7.blocks, blocks as u64);
        // The memory loop ran after blocks 499 and 999: those and the next are left out.
        assert_eq!(p0.left_out + p7.left_out, 4);
        assert_eq!(p0.k2_wall.n + p7.k2_wall.n, 2);
        // The cache loop ran in every 10th block but the memory loop's.
        assert_eq!(
            p0.k1_wall.n + p7.k1_wall.n,
            blocks as u64 / CACHE_LOOP_EVERY - 2
        );
        assert_eq!(p0.stage_wall.n, p0.blocks - p0.left_out);
        assert!(p0.stage_wall.p50_ns > 0 && p0.k1_wall.p50_ns > 0 && p7.k2_wall.p50_ns > 0);
        assert!(p0.stage_wall.p50_ns <= p0.stage_wall.p99_ns);
        assert_eq!(p0.stage_cpu.is_some(), crate::thread_clock::AVAILABLE);
        // No model in this stage.
        assert_eq!((p0.model_blocks, p0.erb_blocks, p0.df_blocks), (0, 0, 0));
        assert_eq!(p0.full_wall.n, 0);
        let on_cores: u64 = p0.cores.iter().map(|c| c.blocks).sum();
        assert_eq!(on_cores + p0.core_unknown, p0.blocks);
        if cfg!(any(target_os = "linux", target_os = "android")) {
            assert_eq!(p0.core_unknown, 0, "sched_getcpu reads on this system");
        }
        assert!(!r.first_blocks[0].left_out && !r.first_blocks[0].model);
    }

    /// Blocks past the room are counted, not written.
    #[test]
    fn a_full_table_counts_the_rest() {
        let mut probe = ProbeStage::with_capacity(Stage::new(), || Some(3), 5);
        let handle = probe.handle();
        let mut block = [0.0f32; FRAME];
        for _ in 0..8 {
            probe.process(&mut block);
        }
        let r = handle.report();
        assert_eq!((r.blocks, r.dropped), (5, 3));
        assert_eq!(r.phases[0].cores, vec![CoreCount { core: 3, blocks: 5 }]);
    }

    /// The probe turns the step-down rules off, so a slow block does not stop the model.
    #[test]
    fn the_rules_are_off() {
        let stage = Stage::new();
        let control = stage.control();
        let _probe = ProbeStage::with_capacity(stage, core_number, 1);
        assert!(!control.step_down_rules());
    }

    #[test]
    fn the_reference_run_measures_both_loops() {
        let r = reference(core_number);
        assert_eq!(r.k1_wall.n, REFERENCE_CACHE_LOOP_RUNS as u64);
        assert_eq!(r.k2_wall.n, REFERENCE_MEMORY_LOOP_RUNS as u64);
        assert!(r.k1_wall.p50_ns > 0 && r.k2_wall.p50_ns > 0);
        assert_eq!(r.k1_cpu.is_some(), crate::thread_clock::AVAILABLE);
        if cfg!(any(target_os = "linux", target_os = "android")) {
            assert!(r.core_start.is_some() && r.core_end.is_some());
        }
    }
}
