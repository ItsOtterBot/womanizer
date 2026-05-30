//! macOS virtual-device detection — rebranded BlackHole presented as `Womanizer`.
//!
//! Populated in Plan 01-04. The implementation:
//!
//! 1. Enumerates `cpal::default_host().output_devices()`.
//! 2. Finds the device whose name is exactly `"Womanizer"` (set by `kDriver_Name` in the
//!    BlackHole rebrand — D-16 + FINDING-2).
//! 3. Runs a 48 kHz / stereo / f32 capability check via `supported_output_configs()` —
//!    NOT by opening a stream (opening can fail when other software holds the device).
//! 4. Returns the typed `DetectionResult`.
//!
//! ## When this is called
//!
//! Per RESEARCH Pitfall #13 + cpal issue #901, this MUST be called only AFTER the user clicks
//! the `Test detection` button in the Plan 01-05 setup gate. Calling it at app launch can
//! trigger the TCC mic-permission prompt prematurely on macOS — the enumeration alone is
//! enough to engage the privacy machinery on some macOS versions. The UI in Plan 01-05
//! enforces this ordering: the setup gate is a pure pre-engine view, and `detect()` only
//! runs on the explicit user click.

use cpal::traits::{DeviceTrait, HostTrait};
use cpal::SampleRate;

use super::DetectionResult;

/// Detect the rebranded BlackHole HAL plugin presented as `Womanizer` (DEVICE-01).
///
/// Returns `Found` when a cpal output device named exactly `"Womanizer"` is enumerated AND
/// its `supported_output_configs()` advertises at least one range that satisfies 48 kHz
/// stereo f32 output (the engine's virtual-output contract per `cpal_io.rs`).
///
/// Returns `NotFound` with a user-facing reason in every other case (no enumeration, no
/// matching device, device present but capability check fails). All reasons are safe to
/// render verbatim after `✗ Not detected — ` in the setup gate (D-11).
pub fn detect() -> DetectionResult {
    // Per RESEARCH Pitfall #13 + cpal issue #901, this MUST be called only after the user
    // clicks Test detection — calling it at app launch can trigger the TCC mic prompt
    // prematurely on macOS. The UI in Plan 01-05 enforces this ordering.
    let host = cpal::default_host();

    let devices: Vec<cpal::Device> = host
        .output_devices()
        .ok()
        .map(|it| it.collect())
        .unwrap_or_default();

    if devices.is_empty() {
        let result = DetectionResult::NotFound {
            reason: "cpal::output_devices() returned no devices".into(),
        };
        tracing::info!(?result, "macOS device detection");
        return result;
    }

    // cpal 0.17 deprecated DeviceTrait::name() in favor of description().name(); use the
    // non-deprecated path to match Plan 01-02a's enumerate_inputs() shape.
    let matched = devices.into_iter().find_map(|d| {
        let name = d.description().ok()?.name().to_string();
        if name == "Womanizer" {
            Some((d, name))
        } else {
            None
        }
    });

    let Some((device, name)) = matched else {
        let result = DetectionResult::NotFound {
            reason: "no device named 'Womanizer' (install the rebranded driver — see \
                     drivers/macos-blackhole/README.md)"
                .into(),
        };
        tracing::info!(?result, "macOS device detection");
        return result;
    };

    // 48 kHz / stereo / f32 capability check via supported_output_configs() — NOT by opening
    // a stream (per CONTEXT D-19 rationale: opening might fail when VRChat already has the
    // device active; capability metadata is read-only and is the safe pre-flight).
    let target: SampleRate = 48_000;
    let supports_48k_stereo_f32 = device
        .supported_output_configs()
        .ok()
        .into_iter()
        .flatten()
        .any(|range| {
            range.channels() == 2
                && range.sample_format() == cpal::SampleFormat::F32
                && range.min_sample_rate() <= target
                && range.max_sample_rate() >= target
        });

    if !supports_48k_stereo_f32 {
        let result = DetectionResult::NotFound {
            reason: "Womanizer driver present but does not support 48 kHz stereo f32 output"
                .into(),
        };
        tracing::info!(?result, "macOS device detection");
        return result;
    }

    // cpal 0.17.3 does not expose CoreAudio AudioDeviceID via DeviceTrait — using the
    // user-visible name as the id placeholder; Phase 5 may surface the underlying CoreAudio
    // ID via coreaudio-sys if needed (e.g. for sleep/wake reconnect by stable id).
    let result = DetectionResult::Found {
        device_name: name.clone(),
        device_id: name,
    };
    tracing::info!(?result, "macOS device detection");
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Host-dependent sanity check: `detect()` must not panic and must return a structurally
    /// valid `DetectionResult`. Whether the outcome is `Found` or `NotFound` depends on
    /// whether the Womanizer driver is installed on the test host — we deliberately do NOT
    /// assert which variant.
    ///
    /// DEVICE-01 full collision verification is manual per VALIDATION.md (the on-Mac
    /// stock-BlackHole side-by-side check listed in `## Manual-Only Verifications`).
    #[test]
    fn detect_returns_a_valid_variant() {
        let result = detect();
        assert!(matches!(
            result,
            DetectionResult::Found { .. } | DetectionResult::NotFound { .. }
        ));
    }
}
