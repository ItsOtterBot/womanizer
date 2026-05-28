//! Declarative SQLite schema, driven by the `rusqlite_migration` crate (D-06).
//!
//! [`MIGRATIONS`] is an ordered, append-only list of [`M`] migrations. `to_latest` applies
//! every pending migration atomically (each in a transaction) and manages the `user_version`
//! PRAGMA under the hood — so we NEVER hand-roll `PRAGMA user_version` read/increment/match
//! stepping (the documented source of first-launch migration bugs, and VOICE-08's evolution
//! contract relies on this crate owning the version).
//!
//! Schema follows D-05:
//!   - `voices` = TYPED columns, one per editable [`womanizer_core::VoiceParams`] field
//!     (id/timestamps added). `compensate_pitch` is stored as INTEGER 0/1 (SQLite has no
//!     native bool). `created_at`/`updated_at` default to `datetime('now')`.
//!   - `settings` = key/value (`key TEXT PRIMARY KEY, value TEXT NOT NULL`); structured
//!     values (e.g. hotkey bindings) are stored as JSON-in-`value` (DIST-08).
//!
//! Migrations are append-only: to evolve the schema in a later phase, ADD a new `M::up(...)`
//! to the end of [`MIGRATIONS_SLICE`] — never edit an existing one (that would diverge already
//! migrated databases from fresh ones).

use rusqlite_migration::{Migrations, M};

/// Ordered, append-only migration list. Each entry is applied exactly once, in order, with
/// `user_version` advanced by `rusqlite_migration` after a successful, atomic apply.
///
/// Migration 1 creates the `voices` (typed) and `settings` (key/value) tables (D-05).
pub const MIGRATIONS_SLICE: &[M<'_>] = &[M::up(
    "CREATE TABLE voices (
        id                INTEGER PRIMARY KEY,
        name              TEXT NOT NULL,
        pitch_semitones   REAL NOT NULL,
        formant_semitones REAL NOT NULL,
        compensate_pitch  INTEGER NOT NULL,   -- 0/1 (SQLite has no native bool)
        breathiness       REAL NOT NULL,
        brightness_db     REAL NOT NULL,
        sibilance_tame    REAL NOT NULL,
        mix               REAL NOT NULL,
        quality_preset    TEXT NOT NULL,
        color_tag         TEXT,               -- nullable: optional UI color tag
        created_at        TEXT NOT NULL DEFAULT (datetime('now')),
        updated_at        TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE TABLE settings (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL                    -- structured values are JSON-in-value (D-05)
    );",
)];

/// The schema's migration set. `MIGRATIONS.to_latest(&mut conn)` applies all pending
/// migrations and advances `user_version` (D-06).
pub const MIGRATIONS: Migrations<'_> = Migrations::from_slice(MIGRATIONS_SLICE);

#[cfg(test)]
mod tests {
    use super::*;

    /// `rusqlite_migration` validates the migration list at runtime (e.g. catches a
    /// later edit that breaks the append-only invariant). Proven here so a malformed
    /// migration fails fast in CI rather than on a user's first launch.
    #[test]
    fn migrations_are_valid() {
        MIGRATIONS
            .validate()
            .expect("migration list must be valid (append-only, well-formed SQL)");
    }
}
