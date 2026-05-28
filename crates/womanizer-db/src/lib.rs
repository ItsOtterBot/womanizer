//! `womanizer-db` — voice-library + settings persistence (SQLite via `rusqlite`).
//!
//! Phase 0 skeleton. The schema (`voices` typed columns + `settings` key/value, D-05),
//! `rusqlite_migration` migration list (D-06), first-launch open/migrate, and the seeded
//! Default voice (D-07) land in Plan 03 against the [`womanizer_core::VoiceParams`] contract.
//!
//! This crate intentionally has no public surface yet; it exists so the dependency graph
//! (`db -> core`) and the cargo-deny license gate are real from Phase 0.
