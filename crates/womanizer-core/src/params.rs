//! [`VoiceParams`] — the editable voice profile, the cross-phase data contract.
//!
//! Canonical unit is **semitones** (D-04): the struct, SQLite, and JSON export all store
//! semitones. Conversion to a frequency ratio happens at the engine boundary, OFF the audio
//! thread, via [`semitones_to_ratio`]. Rationale: semitones are the unit users edit in,
//! producing human-readable JSON and a single source of truth; the conversion is trivial and
//! never on the hot path.

use serde::{Deserialize, Serialize};

/// DSP quality-vs-latency tradeoff for the (future) pitch+formant engine.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum QualityPreset {
    /// Highest quality, largest analysis window, highest latency.
    Quality,
    /// Default balance of quality and latency (the seeded Default voice uses this, D-07).
    Balanced,
    /// Smallest window, lowest latency, some quality tradeoff.
    LowLatency,
}

/// A named, editable voice profile.
///
/// # Field ranges (VOICE-07 validation contract)
///
/// The doc-comment on each field states its accepted range so the future range-validator
/// (Security Domain V5 / VOICE-07) has a single authoritative contract to enforce. Phase 0
/// declares these ranges; it does not yet enforce them.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct VoiceParams {
    /// Pitch shift in semitones (canonical unit, D-04). Positive = higher.
    /// Converted to a ratio via [`semitones_to_ratio`] at the engine boundary.
    pub pitch_semitones: f32,
    /// Formant shift in semitones (canonical unit, D-04). Positive = "smaller" vocal tract.
    /// Independent of pitch — this independence is what avoids the chipmunk artifact.
    pub formant_semitones: f32,
    /// When `true`, the engine compensates formant shift so pitch is unchanged by it.
    pub compensate_pitch: bool,
    /// Breathiness / aspiration injection amount. Range: `0..=1`.
    pub breathiness: f32,
    /// Spectral-tilt / high-shelf brightness in decibels. Typically small (±few dB).
    pub brightness_db: f32,
    /// De-essing / sibilance taming amount. Range: `0..=1`.
    pub sibilance_tame: f32,
    /// Dry/wet mix. Range: `0..=1` (`1.0` = fully processed).
    pub mix: f32,
    /// Phase 3 (Plan 03-01, D-45) enable toggle for the breathiness shaping stage.
    /// When `false`, warm-off semantics per D-42 — the stage still runs each block, only the
    /// output is bypassed. Default `true` per D-45.
    /// Note: there is NO `mix_enabled` field — per D-47, dry/wet has no toggle (mix=0.0 IS the
    /// off state). The Phase 3 contract is THREE bools, not four (RESEARCH §Open Question 3).
    pub breathiness_enabled: bool,
    /// Phase 3 (Plan 03-01, D-44) enable toggle for the brightness high-shelf shaping stage.
    /// When `false`, warm-off semantics per D-42 — the stage still runs each block, only the
    /// output is bypassed. Default `true` per D-44.
    pub brightness_enabled: bool,
    /// Phase 3 (Plan 03-01, D-46) enable toggle for the de-esser / sibilance taming shaping stage.
    /// When `false`, warm-off semantics per D-42 — the stage still runs each block, only the
    /// output is bypassed. Default `true` per D-46.
    pub sibilance_tame_enabled: bool,
    /// DSP quality-vs-latency preset.
    pub quality_preset: QualityPreset,
    /// Optional UI color tag for the voice library.
    pub color_tag: Option<String>,
}

impl Default for VoiceParams {
    /// The seeded "Default" voice (D-07): the spec's M→F starting point.
    ///
    /// pitch +8.7 st (≈1.65×), formant +2.9 st (≈1.18×), compensate true, Balanced preset.
    /// Phase 3 ship-time shaping defaults (D-44..D-47): brightness +3 dB, breathiness 0.20,
    /// sibilance-tame 0.30, dry/wet 1.0, all three `*_enabled` toggles ON. Session-only per D-39.
    fn default() -> Self {
        Self {
            pitch_semitones: 8.7,   // ≈ 1.651×
            formant_semitones: 2.9, // ≈ 1.184×
            compensate_pitch: true,
            breathiness: 0.20,    // D-45 ship-time default (was 0.0 in Phase 0/1/2)
            brightness_db: 3.0,   // D-44 ship-time default (was 0.0 in Phase 0/1/2)
            sibilance_tame: 0.30, // D-46 ship-time default (was 0.0 in Phase 0/1/2)
            mix: 1.0,             // D-47 (unchanged — fully wet on first launch)
            breathiness_enabled: true, // D-45 toggle ON by default
            brightness_enabled: true, // D-44 toggle ON by default
            sibilance_tame_enabled: true, // D-46 toggle ON by default
            quality_preset: QualityPreset::Balanced,
            color_tag: None,
        }
    }
}

impl VoiceParams {
    /// Pitch multiplier as a frequency ratio for the signalsmith
    /// `Stretch::set_transpose_factor` setter. Pure-function wrapper around
    /// [`semitones_to_ratio`] reading `self.pitch_semitones`. RT-safe (zero allocation,
    /// branchless `exp2`); the DSP worker calls this per audio block via the
    /// `triple_buffer<VoiceParams>` snapshot path (Plan 02-04 / D-23 + D-35).
    #[inline]
    pub fn pitch_semitones_to_ratio(&self) -> f32 {
        semitones_to_ratio(self.pitch_semitones)
    }

    /// Formant multiplier as a frequency ratio for the signalsmith
    /// `Stretch::set_formant_factor` setter. Pure-function wrapper around
    /// [`semitones_to_ratio`] reading `self.formant_semitones`. RT-safe (zero allocation,
    /// branchless `exp2`); the DSP worker calls this per audio block via the
    /// `triple_buffer<VoiceParams>` snapshot path (Plan 02-04 / D-23 + D-35).
    ///
    /// Independent of pitch_semitones_to_ratio — that independence is what avoids the
    /// chipmunk artifact (DSP-01 / D-24 — compensate_pitch=true is locked at the
    /// Stretch48k boundary).
    #[inline]
    pub fn formant_semitones_to_ratio(&self) -> f32 {
        semitones_to_ratio(self.formant_semitones)
    }
}

/// Convert a semitone offset to a frequency ratio: `2^(st/12)`.
///
/// Engine-boundary conversion (D-04) — runs OFF the audio thread when publishing the active
/// voice snapshot. `#[inline]` so the (tiny) conversion folds into the publish path.
///
/// `semitones_to_ratio(8.7) ≈ 1.651`, `semitones_to_ratio(2.9) ≈ 1.184`.
#[inline]
pub fn semitones_to_ratio(st: f32) -> f32 {
    2f32.powf(st / 12.0)
}
