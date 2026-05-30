//! Per-OS virtual-device detection — `Womanizer` (rebranded BlackHole) on macOS;
//! `CABLE Input (VB-Audio Virtual Cable)` on Windows.
//!
//! D-10: auto-detect via `cfg(target_os = ...)` at compile time, render only the relevant
//! per-OS path. The bodies of `macos.rs` and `windows.rs` are populated by Plan 01-04;
//! Plan 01-01 ships stub `detect()` implementations that return `NotFound` so callers
//! compile end-to-end.
//!
//! `DetectionResult` lives here (not inside a per-OS module) so callers can pattern-match
//! the result without their own `#[cfg]` arms.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::detect;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::detect;

// Fallback `detect()` for non-{macos, windows} hosts (e.g. Linux dev machines, CI runners).
// Phase 1's target platforms are macOS Apple Silicon + Windows 10/11 only (PROJECT.md), but
// keeping the crate buildable on Linux avoids breaking developers who do code review or run
// `cargo test --lib` on a Linux box.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn detect() -> DetectionResult {
    DetectionResult::NotFound {
        reason: "platform not supported: Womanizer ships on macOS + Windows only".to_string(),
    }
}

/// Outcome of a single virtual-device detection attempt.
///
/// Returned by the per-OS `detect()` functions; the egui setup gate (Plan 01-05) renders
/// `Found` as `✓ Womanizer detected` / `✓ CABLE Input detected` (D-11) and `NotFound` as
/// `✗ Not detected — {reason}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetectionResult {
    /// A matching virtual-device was found and passes the 48 kHz / 2-ch / f32 capability check.
    Found {
        /// Human-readable device name as reported by cpal (`"Womanizer"` on macOS;
        /// `"CABLE Input (VB-Audio Virtual Cable)"` on Windows).
        device_name: String,
        /// Opaque device id the engine uses to re-open the same device on reconnect (D-21).
        device_id: String,
    },
    /// No matching virtual-device was found, or detection failed. The reason is user-facing
    /// copy rendered after `✗ Not detected — ` on the setup screen (D-11).
    NotFound {
        /// Human-readable failure reason (e.g. `"no device named 'Womanizer'"`,
        /// `"driver present but does not support 48 kHz"`).
        reason: String,
    },
}
