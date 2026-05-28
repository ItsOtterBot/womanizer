//! `womanizer-core` — cross-phase contract types and lock-free cross-thread plumbing.
//!
//! Phase 0 declares the contracts every later phase builds against without re-exploring
//! the codebase:
//! - [`params`]: [`VoiceParams`] (canonical unit = semitones, D-04) + [`QualityPreset`]
//!   + [`semitones_to_ratio`] (engine-boundary conversion, off the audio thread).
//! - [`primitives`]: the named cross-thread primitives ([`HotParams`], [`Telemetry`],
//!   the `rtrb` ring type aliases) and the off-audio-thread [`EngineCommand`] /
//!   [`EngineEvent`] contracts.
//! - [`wake`]: [`DspWakeHandle`] — allocation-free, lock-free park/unpark wake.
//! - [`error_ring`]: [`EngineError`] (Copy, no heap) + [`ErrorRing`] constructor.
//!
//! - [`smoke`]: [`run_smoke_test`](smoke::run_smoke_test) — the reusable end-to-end plumbing
//!   harness (D-12) that instantiates all nine named primitives and shuttles dummy frames.

pub mod error_ring;
pub mod params;
pub mod primitives;
pub mod smoke;
pub mod wake;

// Re-export the cross-phase contract surface at the crate root so downstream crates can
// `use womanizer_core::{VoiceParams, HotParams, ...}` without reaching into modules.
pub use error_ring::{EngineError, ErrorRing};
pub use params::{semitones_to_ratio, QualityPreset, VoiceParams};
pub use primitives::{
    AudioFrame, EngineCommand, EngineEvent, HotParams, InputRing, MonitorOutRing, Telemetry,
    VirtualOutRing, RING_CAPACITY,
};
pub use wake::DspWakeHandle;
