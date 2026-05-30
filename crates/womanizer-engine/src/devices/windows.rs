//! Windows virtual-device detection — VB-CABLE strict-name regex + capability check.
//!
//! Populated in Plan 01-04. The implementation:
//!
//! 1. Enumerates `cpal::default_host().output_devices()`.
//! 2. Case-insensitive whole-string match against the D-19 strict regex
//!    `^CABLE Input \(VB-Audio Virtual Cable\)$`. VoiceMeeter VAIO / AUX explicitly DO NOT
//!    match (Pitfall #6 — VoiceMeeter ships under the same `VB-Audio` vendor and the loose
//!    `VB-Audio` substring match would false-positive).
//! 3. Runs a 48 kHz / stereo / f32 capability check via `supported_output_configs()` —
//!    NOT by opening a stream (opening on Windows can fail when VRChat already has the
//!    device active for capture; capability metadata is read-only and is the safe pre-flight).
//! 4. Returns the typed `DetectionResult`.
//!
//! Sibling helper [`enumerate_matched_cables`] exposes the candidate list when multiple
//! devices match (D-19 multi-cable case) so Plan 01-05's UI dropdown can present a chooser.

use std::sync::LazyLock;

use cpal::traits::{DeviceTrait, HostTrait};
use cpal::SampleRate;

use super::{DetectionResult, VB_CABLE_REGEX};

/// Compiled once per process (LazyLock is stable since Rust 1.80; the workspace MSRV is 1.92
/// per Phase 0 D — well above). The regex string is validated at compile-time-of-thought via
/// the `regex_tests` module in `mod.rs`; an `expect()` here is appropriate because a panic
/// here means the constant itself is malformed, which is a build-time bug.
static CABLE_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(VB_CABLE_REGEX).expect("VB-CABLE regex is well-formed at compile time")
});

/// Detect the VB-CABLE virtual audio cable (DEVICE-04).
///
/// Returns `Found` when a cpal output device whose name matches the D-19 strict regex is
/// enumerated AND advertises 48 kHz stereo f32 output via `supported_output_configs()`.
/// Returns `NotFound` with a user-facing reason in every other case.
///
/// Multi-cable handling: if multiple devices match (uncommon — VB-Audio's multi-cable
/// installer ships `CABLE-A Input` / `CABLE-B Input` which do NOT match the strict regex by
/// design), the first one is selected. Plan 01-05's UI uses [`enumerate_matched_cables`] to
/// present a chooser dropdown when more than one matches.
pub fn detect() -> DetectionResult {
    let host = cpal::default_host();

    let matched: Vec<(cpal::Device, String)> = host
        .output_devices()
        .into_iter()
        .flatten()
        .filter_map(|d| {
            // cpal 0.17 deprecated name() in favor of description().name() — use the
            // non-deprecated path to match Plan 01-02a's enumerate_inputs() shape.
            let name = d.description().ok()?.name().to_string();
            if CABLE_RE.is_match(&name) {
                Some((d, name))
            } else {
                None
            }
        })
        .collect();

    if matched.is_empty() {
        let result = DetectionResult::NotFound {
            reason: "no device matched 'CABLE Input (VB-Audio Virtual Cable)' — install \
                     VB-CABLE from https://vb-audio.com/Cable/"
                .into(),
        };
        tracing::info!(?result, "Windows VB-CABLE detection");
        return result;
    }

    let (device, name) = matched.into_iter().next().unwrap();

    // Capability check identical in spirit to devices/macos.rs::detect — read-only metadata
    // probe, NOT a build_output_stream + drop. Opening the device might fail when other
    // software (VRChat, Discord) already holds it active for capture.
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
            reason: "VB-CABLE present but does not support 48 kHz stereo f32 output".into(),
        };
        tracing::info!(?result, "Windows VB-CABLE detection");
        return result;
    }

    // cpal 0.17.3 does not expose WASAPI endpoint IDs via DeviceTrait beyond the name —
    // using the user-visible name as the id placeholder. Phase 5 may surface the underlying
    // WASAPI endpoint id via windows-rs if needed (e.g. for sleep/wake reconnect by stable id).
    let result = DetectionResult::Found {
        device_name: name.clone(),
        device_id: name,
    };
    tracing::info!(?result, "Windows VB-CABLE detection");
    result
}

/// Return the list of output-device names matching the VB-CABLE strict regex.
///
/// Plan 01-05's UI dropdown calls this when offering a chooser between multiple matched
/// cables. The strict regex `^CABLE Input \(VB-Audio Virtual Cable\)$` only matches the
/// single canonical VB-CABLE; multi-cable installer variants like `CABLE-A Input
/// (VB-Audio Cable A)` do NOT match by design (D-19, Pitfall #6).
///
/// If multi-cable support is needed in a future phase, add a sibling regex
/// `^CABLE-[A-Z]? Input \(VB-Audio Cable [A-Z]?\)$` and union the results.
pub fn enumerate_matched_cables() -> Vec<String> {
    let host = cpal::default_host();
    host.output_devices()
        .into_iter()
        .flatten()
        .filter_map(|d| {
            let name = d.description().ok()?.name().to_string();
            if CABLE_RE.is_match(&name) {
                Some(name)
            } else {
                None
            }
        })
        .collect()
}
