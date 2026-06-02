//! `womanizer-engine` — cpal audio I/O, the DSP worker thread, and per-OS virtual-device detection.
//!
//! Owns the cpal stream lifecycle, the DSP worker (Phase 1: memcpy passthrough; Phase 2+:
//! signalsmith Stretch), the rubato I/O-boundary resampler, the per-OS virtual-device
//! detector (VB-CABLE presence on Windows), the self-monitor
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
pub mod dsp;
pub mod event_loop;
pub mod monitor;
pub mod resampler;
pub mod worker;

// Re-export the per-OS detection entrypoint + the shared DetectionResult enum so callers
// can `use womanizer_engine::{detect, DetectionResult}` without reaching into modules. The
// concrete `detect()` function is cfg-gated and exported from the per-OS module by
// `devices/mod.rs`; the signature is finalized in Plan 01-04.
pub use devices::{detect, DetectionResult};

// Re-export the engine handle types so the UI (Plan 01-05) can
// `use womanizer_engine::{EngineHandle, EngineState, spawn}` without reaching into the
// `event_loop` module. Populated by Plan 01-02b.
pub use event_loop::{spawn as spawn_engine, EngineHandle, EngineState};

// Re-export the cpal-side UI surface: device enumeration for the Ready shell's input row
// (AUDIO-01), the BLOCK / SAMPLE_RATE_HZ engine constants for any UI surface that needs them.
pub use cpal_io::{enumerate_inputs, enumerate_outputs, BLOCK, SAMPLE_RATE_HZ};

// Re-export the DSP Preset enum (D-26 segmented-row variants) so the Ready shell (Plan
// 02-08) can `use womanizer_engine::Preset` without reaching into the `dsp` module — same
// pattern Phase 1 uses for `EngineHandle` / `DetectionResult` / `MonitorBannerState`.
pub use dsp::Preset;

// Re-export the banner-state publishers and verbatim copy constants that Plan 01-05's
// Ready shell consumes for the three yellow banners (AUDIO-04 sample-rate-mismatch,
// AUDIO-08 feedback-detected, AUDIO-09 disconnected).
pub use monitor::{MonitorBannerState, DISCONNECT_BANNER_COPY, FEEDBACK_BANNER_COPY};
pub use resampler::{render_resample_banner, SampleRateState, RESAMPLE_BANNER_TEMPLATE};

// Re-export the off-audio-thread command/event types womanizer-core publishes so the UI can
// `use womanizer_engine::{EngineCommand, EngineEvent}` without depending on womanizer-core
// directly. (The app crate still depends on womanizer-core for HotParams / Telemetry / etc.,
// but reducing the surface the UI imports for the channel types is a small ergonomic win.)
pub use womanizer_core::{EngineCommand, EngineError, EngineEvent};
