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
    /// DSP quality-vs-latency preset.
    pub quality_preset: QualityPreset,
    /// Optional UI color tag for the voice library.
    pub color_tag: Option<String>,
}

impl Default for VoiceParams {
    /// The seeded "Default" voice (D-07): the spec's M→F starting point.
    ///
    /// pitch +8.7 st (≈1.65×), formant +2.9 st (≈1.18×), compensate true, all shaping at 0,
    /// full wet mix, Balanced preset. These are ear-tuned in Phases 2–3; no DSP consumes them
    /// in Phase 0.
    fn default() -> Self {
        Self {
            pitch_semitones: 8.7,   // ≈ 1.651×
            formant_semitones: 2.9, // ≈ 1.184×
            compensate_pitch: true,
            breathiness: 0.0,
            brightness_db: 0.0,
            sibilance_tame: 0.0,
            mix: 1.0,
            quality_preset: QualityPreset::Balanced,
            color_tag: None,
        }
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
