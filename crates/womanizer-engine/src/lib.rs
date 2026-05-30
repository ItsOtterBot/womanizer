//! `womanizer-engine` — cpal audio I/O, the DSP worker thread, and per-OS virtual-device detection.
//!
//! Owns the cpal stream lifecycle, the DSP worker (Phase 1: memcpy passthrough; Phase 2+:
//! signalsmith Stretch), the rubato I/O-boundary resampler, the per-OS virtual-device
//! detector (rebranded BlackHole presence on macOS / VB-CABLE on Windows), the self-monitor
//! stream + feedback-loop detector, and the off-RT event loop that drives EngineCommand /
//! EngineEvent and drains the ErrorRing.
//!
//! Stays headless: no egui, no SQLite. The egui setup gate + Ready shell live in
//! `womanizer-app::app`; SQLite settings reads live in `womanizer-app::main`.
//!
//! Plan 01-01 stands up the crate skeleton + module slots + the workspace dep graph; the
//! module bodies below are populated by later plans:
//! - `cpal_io`  — Plan 01-02a (cpal stream construction + RT-shaped capture/playback callbacks)
//! - `worker`   — Plan 01-02a (DSP worker thread: memcpy passthrough, woken by DspWakeHandle)
//! - `event_loop` — Plan 01-02b (off-RT command/event pump; drains ErrorRing; reconnect path)
//! - `resampler` — Plan 01-03 (rubato `FftFixedIn` at the I/O boundary, off the audio callback)
//! - `monitor`   — Plan 01-03 (self-monitor output stream + feedback-loop detector)
//! - `devices`   — Plan 01-04 (per-OS virtual-device detection + capability check)

pub mod cpal_io;
pub mod devices;
pub mod event_loop;
pub mod monitor;
pub mod resampler;
pub mod worker;

// Re-export the per-OS detection entrypoint + the shared DetectionResult enum so callers
// can `use womanizer_engine::{detect, DetectionResult}` without reaching into modules. The
// concrete `detect()` function is cfg-gated and exported from the per-OS module by
// `devices/mod.rs`; the signature is finalized in Plan 01-04.
pub use devices::{detect, DetectionResult};
