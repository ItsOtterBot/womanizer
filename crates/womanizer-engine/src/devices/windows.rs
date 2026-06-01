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
//! Multi-cable installer variants (`CABLE-A Input`, etc.) do not match the strict regex by
//! design — if a future phase needs a chooser, enumerate against a broader pattern there.

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
/// design), the first one is selected.
pub fn detect() -> DetectionResult {
    let host = cpal::default_host();
    tracing::info!(host_id = ?host.id(), "WINDOWS DEBUG: cpal host selected");

    // Probe every cpal enumeration path so we can see exactly what the Windows backend
    // returns. Each Test detection click prints one block of these `WINDOWS DEBUG:` lines.
    match host
        .default_input_device()
        .and_then(|d| d.description().ok())
    {
        Some(d) => tracing::info!(name = %d.name(), "WINDOWS DEBUG: host default INPUT device"),
        None => tracing::warn!("WINDOWS DEBUG: host has NO default input device"),
    }
    match host
        .default_output_device()
        .and_then(|d| d.description().ok())
    {
        Some(d) => tracing::info!(name = %d.name(), "WINDOWS DEBUG: host default OUTPUT device"),
        None => tracing::warn!("WINDOWS DEBUG: host has NO default output device"),
    }
    match host.input_devices() {
        Ok(iter) => {
            let names: Vec<String> = iter
                .filter_map(|d| d.description().ok().map(|x| x.name().to_string()))
                .collect();
            tracing::info!(
                count = names.len(),
                ?names,
                "WINDOWS DEBUG: all INPUT devices"
            );
        }
        Err(e) => tracing::error!(error = ?e, "WINDOWS DEBUG: host.input_devices() FAILED"),
    }
    match host.output_devices() {
        Ok(iter) => {
            let names: Vec<String> = iter
                .filter_map(|d| d.description().ok().map(|x| x.name().to_string()))
                .collect();
            tracing::info!(
                count = names.len(),
                ?names,
                "WINDOWS DEBUG: all OUTPUT devices"
            );
        }
        Err(e) => tracing::error!(error = ?e, "WINDOWS DEBUG: host.output_devices() FAILED"),
    }

    let matched: Vec<(cpal::Device, String)> = host
        .output_devices()
        .into_iter()
        .flatten()
        .filter_map(|d| {
            // cpal 0.17 deprecated name() in favor of description().name() — use the
            // non-deprecated path to match Plan 01-02a's enumerate_inputs() shape.
            let name = d.description().ok()?.name().to_string();
            let matched = CABLE_RE.is_match(&name);
            tracing::info!(name = %name, matched, len = name.len(), "enumerated output device");
            if matched {
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
