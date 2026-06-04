//! Phase 3 Plan 03-01 — VoiceParams shaping foundation gate.
//!
//! Asserts the three new bool enable fields (`breathiness_enabled`, `brightness_enabled`,
//! `sibilance_tame_enabled`) and the D-44..D-47 ship-time defaults survive a JSON serde
//! round-trip and that the Phase 2 pitch/formant defaults remain unchanged (additive
//! widening only — no Phase 2 regressions).

use womanizer_core::VoiceParams;

/// D-44..D-47 ship-time defaults gate: `VoiceParams::default()` returns the Phase 3
/// shaping values (breath 0.20, brightness +3 dB, sibilance 0.30, mix 1.0, all three
/// `*_enabled` bools ON). If a future planner retunes these, this test fires before
/// any downstream code drifts from the contract.
#[test]
fn defaults_match_phase3_ship_values() {
    let v = VoiceParams::default();
    assert!(
        (v.breathiness - 0.20).abs() < 1e-6,
        "D-45 breathiness default = 0.20 (got {})",
        v.breathiness
    );
    assert!(
        (v.brightness_db - 3.0).abs() < 1e-6,
        "D-44 brightness_db default = +3.0 dB (got {})",
        v.brightness_db
    );
    assert!(
        (v.sibilance_tame - 0.30).abs() < 1e-6,
        "D-46 sibilance_tame default = 0.30 (got {})",
        v.sibilance_tame
    );
    assert!(
        (v.mix - 1.0).abs() < 1e-6,
        "D-47 mix default = 1.0 (fully wet) (got {})",
        v.mix
    );
    assert!(
        v.breathiness_enabled,
        "D-45 breathiness toggle ON by default"
    );
    assert!(v.brightness_enabled, "D-44 brightness toggle ON by default");
    assert!(
        v.sibilance_tame_enabled,
        "D-46 sibilance-tame toggle ON by default"
    );
}

/// JSON serde round-trip preserves all seven new Phase 3 fields (four floats + three bools).
/// VoiceParams derives PartialEq (params.rs); `assert_eq!` directly compares every field.
/// Future Phase 4 voice export to the SQLite voices table will rely on this round-trip
/// property to persist the shaping values without drift.
#[test]
fn voice_params_serde_round_trip_preserves_phase3_fields() {
    let v = VoiceParams::default();
    let s = serde_json::to_string(&v).expect("serde_json::to_string must succeed on default");
    let v2: VoiceParams = serde_json::from_str(&s)
        .expect("serde_json::from_str must round-trip the serialized default verbatim");
    assert_eq!(
        v, v2,
        "VoiceParams serde JSON round-trip lost one or more fields"
    );
    // Defense-in-depth: explicitly confirm the three new bools survived.
    assert_eq!(v.breathiness_enabled, v2.breathiness_enabled);
    assert_eq!(v.brightness_enabled, v2.brightness_enabled);
    assert_eq!(v.sibilance_tame_enabled, v2.sibilance_tame_enabled);
}

/// Phase 2 D-22 lock — the Default voice's pitch_semitones (8.7) and formant_semitones (2.9)
/// remain UNCHANGED. Plan 03-01 is an additive extension; if this regresses, Phase 2's DSP
/// chain has shifted its starting point silently and the user A/B baseline moves.
#[test]
fn voice_params_default_equals_phase_2_pitch_formant() {
    let v = VoiceParams::default();
    assert!(
        (v.pitch_semitones - 8.7).abs() < 1e-6,
        "Phase 2 D-22 pitch_semitones default = 8.7 (got {})",
        v.pitch_semitones
    );
    assert!(
        (v.formant_semitones - 2.9).abs() < 1e-6,
        "Phase 2 D-22 formant_semitones default = 2.9 (got {})",
        v.formant_semitones
    );
    assert!(
        v.compensate_pitch,
        "Phase 2 D-24 compensate_pitch default = true"
    );
}
