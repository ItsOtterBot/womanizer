//! macOS virtual-device detection — rebranded BlackHole presented as `Womanizer`.
//!
//! Populated by Plan 01-04. Implementation will enumerate `cpal::default_host().output_devices()`,
//! match on the literal name `"Womanizer"` (set by `kDriver_Name` in the BlackHole rebrand —
//! D-16), and run a 48 kHz / 2-ch / f32 capability check via `supported_output_configs()`.
//!
//! Plan 01-01 stub returns `NotFound` unconditionally so the lib.rs `pub use` resolves and
//! `cargo build` succeeds end-to-end. Real implementation lands in Plan 01-04.

pub fn detect() -> super::DetectionResult {
    super::DetectionResult::NotFound {
        reason: "not implemented in Plan 01-01 — wired in Plan 01-04".to_string(),
    }
}
