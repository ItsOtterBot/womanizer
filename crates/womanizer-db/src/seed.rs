//! Idempotent first-launch seed of the single "Default" voice (D-07).
//!
//! The seed is deliberately NOT a migration: putting the INSERT in [`crate::schema`] would
//! re-create the Default voice every time the user deleted it (a migration runs once per DB,
//! but a re-seed-on-empty would not — and we want the user to be able to delete Default
//! permanently). Instead [`seed_default_if_empty`] runs on every open, guarded by a
//! `COUNT(*) == 0` check, so:
//!   - first launch (empty `voices`) → exactly one Default row is inserted;
//!   - any later launch with ≥1 voice → no-op (idempotent, never duplicates).
//!
//! The seed values come from [`womanizer_core::VoiceParams::default()`] — the single source of
//! truth for the D-07 starting point — so the DB seed can never drift from the in-memory
//! contract. The INSERT is PARAMETERIZED (`?N` placeholders, never string-concatenated SQL)
//! to establish the no-SQL-injection pattern from day one (Security Domain T-00-05); Phase-4
//! CRUD inherits this pattern.

use anyhow::Result;
use rusqlite::Connection;
use womanizer_core::params::{QualityPreset, VoiceParams};

/// SQL text for the [`QualityPreset`] enum, stored as a stable TEXT discriminant in the
/// `quality_preset` column. Kept as an explicit match (not `Debug`) so the persisted form is
/// a deliberate contract, not an accident of derive formatting.
fn quality_preset_str(preset: &QualityPreset) -> &'static str {
    match preset {
        QualityPreset::Quality => "Quality",
        QualityPreset::Balanced => "Balanced",
        QualityPreset::LowLatency => "LowLatency",
    }
}

/// Seed exactly one "Default" voice if the `voices` table is empty; otherwise do nothing.
///
/// Idempotent: safe to call on every open. Uses a parameterized INSERT (`?N`) — there is no
/// string-concatenated SQL anywhere in this crate (T-00-05). The row's values are taken from
/// [`VoiceParams::default()`] (the D-07 contract).
pub fn seed_default_if_empty(conn: &Connection) -> Result<()> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM voices", [], |row| row.get(0))?;
    if count == 0 {
        let p = VoiceParams::default();
        conn.execute(
            "INSERT INTO voices
                 (name, pitch_semitones, formant_semitones, compensate_pitch,
                  breathiness, brightness_db, sibilance_tame, mix, quality_preset, color_tag)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                "Default",
                p.pitch_semitones,
                p.formant_semitones,
                p.compensate_pitch as i64, // bool -> 0/1 INTEGER
                p.breathiness,
                p.brightness_db,
                p.sibilance_tame,
                p.mix,
                quality_preset_str(&p.quality_preset),
                p.color_tag, // Option<String> -> TEXT or NULL
            ],
        )?;
    }
    Ok(())
}
