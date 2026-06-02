//! Named cross-thread primitives — the cross-phase plumbing contract (INFRA-05).
//!
//! Every later phase consumes these named types rather than bare crate types, so the codebase
//! speaks in domain terms (`InputRing`, `VirtualOutRing`) and a refactor of the underlying
//! primitive stays local (Pattern 1; anti-pattern: bare `rtrb::Producer<f32>` passed around).
//!
//! Audio-path safety: the rings are wait-free `rtrb` SPSC pairs;
//! hot scalars are atomics; [`EngineCommand`]/[`EngineEvent`] travel over `crossbeam-channel`
//! and are **off-audio-thread only** (discrete commands, never a parameter stream).

use std::sync::atomic::{AtomicBool, AtomicU32};

use atomic_float::AtomicF32;
use rtrb::{Consumer, Producer};

use crate::error_ring::EngineError;

/// Ring capacity in frames. Tuned for real audio in Phase 1; a generous default here.
pub const RING_CAPACITY: usize = 8192;

/// A single mono audio sample. Phase 0 shuttles dummy data of this type only.
pub type AudioFrame = f32;

/// Capture callback → DSP worker. Synthetic frames in Phase 0; real mic audio in Phase 1.
pub type InputRing = (Producer<AudioFrame>, Consumer<AudioFrame>);
/// DSP worker → virtual-output playback callback (the device VRChat sees as a microphone).
pub type VirtualOutRing = (Producer<AudioFrame>, Consumer<AudioFrame>);
/// DSP worker → self-monitoring playback callback (the user's physical headphones).
pub type MonitorOutRing = (Producer<AudioFrame>, Consumer<AudioFrame>);

/// Continuously-tweaked hot scalars, shared as `Arc<HotParams>`.
///
/// Overwrite-latest semantics: atomics (not `triple_buffer`) are correct here because the
/// previous value is never needed. Read on the audio thread, written from the UI thread.
pub struct HotParams {
    /// Input gain multiplier.
    pub input_gain: AtomicF32,
    /// Noise-gate threshold.
    pub gate_threshold: AtomicF32,
    /// Bypass the whole effect chain (pass audio through unprocessed).
    pub bypass: AtomicBool,
    /// Self-monitor playback gate. Default false (D-12 — monitor ships OFF). Set to false by
    /// the engine event loop's feedback-loop detector on trip (D-14, AUDIO-08); the UI toggle
    /// re-sets it to true. Overwrite-latest semantics — atomic, never `Mutex`. Added by Phase 1
    /// Plan 01-01 as a cross-phase contract widening (Pattern G — fields that cross thread
    /// boundaries live in `womanizer-core/src/primitives.rs`).
    pub monitor_enabled: AtomicBool,
}

/// Live engine telemetry, shared as `Arc<Telemetry>`. Written by the engine, read by the UI
/// for live meters (latency / RMS / xrun count).
pub struct Telemetry {
    /// Measured end-to-end latency in milliseconds.
    pub latency_ms: AtomicF32,
    /// Input RMS level for the meter.
    pub input_rms: AtomicF32,
    /// Cumulative buffer underrun/overrun (xrun) count.
    pub xruns: AtomicU32,
    /// Input fundamental frequency in Hz (post-YIN). `f32::NAN` when unvoiced. Written by the
    /// DSP worker at ~30 Hz; read by the UI each repaint. Phase 2 (D-32). Added by Phase 2
    /// Plan 02-02 as a cross-phase contract widening (Pattern G — fields that cross thread
    /// boundaries live in `womanizer-core/src/primitives.rs`).
    pub input_f0_hz: AtomicF32,
    /// Output fundamental in Hz (input_f0 * smoothed pitch ratio). `f32::NAN` when input is
    /// unvoiced. Written by the DSP worker at ~30 Hz; read by the UI each repaint. Phase 2
    /// (D-32). Added by Phase 2 Plan 02-02 as a cross-phase contract widening (Pattern G —
    /// fields that cross thread boundaries live in `womanizer-core/src/primitives.rs`).
    pub output_f0_hz: AtomicF32,
}

/// Three quality-vs-latency presets exposed in the Ready shell (D-26). Low <32 ms,
/// Balanced <40 ms, Quality <50 ms total round-trip target (D-25/D-26). Lives in
/// `womanizer-core` (rather than `womanizer-engine::dsp`) so [`EngineCommand::SetPreset`]
/// can reference it without a circular crate dep (Pattern G / PATTERNS.md decision (a)).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Preset {
    /// <32 ms total round-trip target.
    Low,
    /// <40 ms total round-trip target.
    Balanced,
    /// <50 ms total round-trip target.
    Quality,
}

/// Discrete commands from the UI to the engine. **Off-audio-thread only** — sent over
/// `crossbeam-channel`, never touched from the audio callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineCommand {
    /// Start the audio engine.
    Start,
    /// Stop the audio engine.
    Stop,
    /// Switch the active voice to the given voice-library row id.
    SelectVoice(i64),
    /// Set the capture device by name. `None` means "fall back to host default".
    /// The engine updates its in-memory `EngineState.selected_input` and, if currently
    /// running, tears down + rebuilds streams so the change takes effect immediately.
    SetInput(Option<String>),
    /// Set the virtual-output device by name. Same semantics as [`Self::SetInput`].
    SetVirtualOutput(Option<String>),
    /// Set the monitor (headphones) device by name. Same semantics as [`Self::SetInput`].
    /// Monitor failures are non-fatal — the mic → virtual-output path continues regardless.
    SetMonitor(Option<String>),
    /// Switch the active quality preset (D-26 segmented row). Handled OFF the RT path:
    /// `signalsmith-stretch` exposes no in-place reconfigure, so the engine event loop
    /// constructs a fresh `Stretch48k` off-RT and hands it to the DSP worker via a
    /// `crossbeam_channel::bounded<Stretch48k>(1)` swap channel (RESEARCH §Q9). The worker
    /// drains the channel between blocks and swaps the instance in. Added by Phase 2 Plan
    /// 02-02 as a cross-phase contract widening (Pattern D — append at end, preserve derive
    /// order so the existing `#[derive(Debug, Clone, PartialEq, Eq)]` covers this variant).
    SetPreset(Preset),
}

/// Discrete events from the engine to the UI. **Off-audio-thread only.**
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineEvent {
    /// Engine has started.
    Started,
    /// Engine has stopped.
    Stopped,
    /// A non-fatal engine error occurred (drained from the [`ErrorRing`] on a non-RT thread).
    ///
    /// [`ErrorRing`]: crate::error_ring::ErrorRing
    Error(EngineError),
}
