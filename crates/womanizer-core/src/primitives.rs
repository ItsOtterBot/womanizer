//! Named cross-thread primitives — the cross-phase plumbing contract (INFRA-05).
//!
//! Every later phase consumes these named types rather than bare crate types, so the codebase
//! speaks in domain terms (`InputRing`, `VirtualOutRing`) and a refactor of the underlying
//! primitive stays local (Pattern 1; anti-pattern: bare `rtrb::Producer<f32>` passed around).
//!
//! Audio-path safety (the project spec "What NOT to Use"): the rings are wait-free `rtrb` SPSC pairs;
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
