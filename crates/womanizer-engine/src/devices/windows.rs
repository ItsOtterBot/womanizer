//! Windows virtual-device detection — VB-CABLE strict-name regex + capability check.
//!
//! Populated by Plan 01-04. Implementation will enumerate `cpal::default_host().output_devices()`,
//! filter on the D-19 strict regex `^CABLE Input \(VB-Audio Virtual Cable\)$` (case-insensitive,
//! VoiceMeeter VAIO/AUX explicitly do NOT match), and run a 48 kHz / 2-ch / f32 capability
//! check via `supported_output_configs()`.
//!
//! Plan 01-01 stub returns `NotFound` unconditionally so the lib.rs `pub use` resolves and
//! `cargo build` succeeds end-to-end. Real implementation lands in Plan 01-04.

pub fn detect() -> super::DetectionResult {
    super::DetectionResult::NotFound {
        reason: "not implemented in Plan 01-01 — wired in Plan 01-04".to_string(),
    }
}
