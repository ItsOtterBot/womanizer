//! Virtual-device detection — `CABLE Input (VB-Audio Virtual Cable)` on Windows.
//!
//! Womanizer is Windows-only (rescoped after Phase 1: VRChat does not ship on macOS).
//! Non-Windows builds (Linux dev hosts, CI runners) compile against the fallback `detect()`
//! below so the crate is still usable for code review and unit testing.
//!
//! `DetectionResult` lives here (not inside a per-OS module) so callers can pattern-match
//! the result without their own `#[cfg]` arms.

/// Strict regex per D-19. Whole-string anchors and case-insensitive. VoiceMeeter VAIO/AUX do
/// NOT match by design (Pitfall #6). Lifted to `mod.rs` (no cfg) so the regex assertion
/// tests run on any host OS — i.e. the Linux dev box and the CI runners.
///
/// The Windows detection module (`devices/windows.rs`) builds a `LazyLock<Regex>` from this
/// constant. The multi-cable installer variants (`CABLE-A Input`, `CABLE-B Input`, …) do NOT
/// match this strict regex by design; if multi-cable support is needed in a future phase,
/// add a sibling regex `^CABLE-[A-Z]? Input \(VB-Audio Cable [A-Z]?\)$` and union the results.
pub const VB_CABLE_REGEX: &str = r"(?i)^CABLE Input(?: \(VB-Audio Virtual Cable\))?$";

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::detect;

// Fallback `detect()` for non-Windows hosts (Linux / macOS dev machines, CI runners).
// Womanizer ships on Windows 10/11 only; keeping the crate buildable on other platforms
// avoids breaking developers who do code review or run `cargo test --lib` off-target.
#[cfg(not(target_os = "windows"))]
pub fn detect() -> DetectionResult {
    DetectionResult::NotFound {
        reason: "platform not supported: Womanizer ships on Windows only".to_string(),
    }
}

/// Outcome of a single virtual-device detection attempt.
///
/// Returned by `detect()`; the egui setup gate (Plan 01-05) renders `Found` as
/// `✓ CABLE Input detected` (D-11) and `NotFound` as `✗ Not detected — {reason}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetectionResult {
    /// A matching virtual-device was found and passes the 48 kHz / 2-ch / f32 capability check.
    Found {
        /// Human-readable device name as reported by cpal (`"CABLE Input (VB-Audio Virtual Cable)"`).
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

/// Regex-string assertion tests. Compiled and run on every host OS (the regex string itself
/// is host-independent) — the `regex` crate is a `dev-dependency` of `womanizer-engine` so
/// this test module builds on Linux and macOS dev hosts even though the production
/// `devices/windows.rs` only compiles on Windows.
///
/// VALIDATION.md row DEVICE-04 (revision B2) points at `devices::regex_tests::regex_matches`
/// as the named test for the VB-CABLE strict-match contract — this module provides it.
#[cfg(test)]
mod regex_tests {
    use super::VB_CABLE_REGEX;

    /// DEVICE-04 (revision B2 alias). VALIDATION.md row DEVICE-04 points here. Sole
    /// assertion: the canonical VB-CABLE name matches AND the VoiceMeeter VAIO false-positive
    /// does NOT match.
    #[test]
    fn regex_matches() {
        let re = regex::Regex::new(VB_CABLE_REGEX).unwrap();
        assert!(
            re.is_match("CABLE Input (VB-Audio Virtual Cable)"),
            "canonical VB-CABLE name must match"
        );
        assert!(
            !re.is_match("VoiceMeeter Input (VB-Audio VoiceMeeter VAIO)"),
            "VoiceMeeter VAIO must NOT match (Pitfall #6)"
        );
    }

    #[test]
    fn regex_matches_canonical_cable_name() {
        let re = regex::Regex::new(VB_CABLE_REGEX).unwrap();
        assert!(re.is_match("CABLE Input (VB-Audio Virtual Cable)"));
    }

    #[test]
    fn regex_matches_case_insensitive() {
        let re = regex::Regex::new(VB_CABLE_REGEX).unwrap();
        assert!(
            re.is_match("cable input (vb-audio virtual cable)"),
            "lowercase variant must match (case-insensitive flag)"
        );
        assert!(
            re.is_match("CABLE INPUT (VB-AUDIO VIRTUAL CABLE)"),
            "uppercase variant must match (case-insensitive flag)"
        );
    }

    #[test]
    fn regex_rejects_voicemeeter_vaio() {
        let re = regex::Regex::new(VB_CABLE_REGEX).unwrap();
        assert!(
            !re.is_match("VoiceMeeter Input (VB-Audio VoiceMeeter VAIO)"),
            "VoiceMeeter VAIO must NOT match (Pitfall #6)"
        );
    }

    #[test]
    fn regex_rejects_voicemeeter_aux() {
        let re = regex::Regex::new(VB_CABLE_REGEX).unwrap();
        assert!(
            !re.is_match("VoiceMeeter AUX Input (VB-Audio VoiceMeeter AUX VAIO)"),
            "VoiceMeeter AUX must NOT match (Pitfall #6)"
        );
    }

    #[test]
    fn regex_matches_cpal_truncated_name() {
        // cpal 0.17 on Windows returns just `"CABLE Input"` (PKEY_Device_FriendlyName) —
        // NOT the composited `"CABLE Input (VB-Audio Virtual Cable)"` the Windows audio
        // control panel shows. The strict regex was rewritten to accept both forms.
        let re = regex::Regex::new(VB_CABLE_REGEX).unwrap();
        assert!(
            re.is_match("CABLE Input"),
            "cpal-truncated name must match the strict regex"
        );
        assert!(
            !re.is_match("CABLE In 16ch"),
            "16-channel premium variant must NOT match (wrong device for our 48k/stereo pipeline)"
        );
    }

    #[test]
    fn regex_rejects_padded_match() {
        let re = regex::Regex::new(VB_CABLE_REGEX).unwrap();
        assert!(
            !re.is_match("Some Prefix - CABLE Input (VB-Audio Virtual Cable) - suffix"),
            "padded match must fail the whole-string anchors"
        );
    }
}
