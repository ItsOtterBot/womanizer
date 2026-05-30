//! `womanizer-db` — voice-library + settings persistence (SQLite via `rusqlite`).
//!
//! Public entrypoint: [`open_and_init`] resolves the canonical app-data path via
//! [`directories::ProjectDirs`], opens the SQLite database, applies all pending migrations
//! (D-06, via [`schema::MIGRATIONS`]), and idempotently seeds the single "Default" voice
//! (D-07, via [`seed::seed_default_if_empty`]). The schema is the typed `voices` table +
//! key/value `settings` table (D-05), against the [`womanizer_core::params::VoiceParams`]
//! contract.
//!
//! Phase 4 wires egui CRUD onto these tables; the engine reads voice params via the published
//! snapshot and never touches SQLite on the audio thread.
//!
//! ## Public read surface
//! - [`open_and_init`]: production entrypoint (canonical path, migrate, seed Default).
//! - [`open_at`] / [`init_conn`]: test seams for an explicit path / pre-opened connection.
//! - [`read_setting`]: read-only access to the key/value `settings` table (D-05); Phase 1's
//!   contract is read-only — writes land in Phase 4 (see [`settings`] module docs for the
//!   three Phase 1 keys).
//!
//! ## Path resolution and the test seam
//!
//! [`open_and_init`] takes NO user-supplied path — the DB location is resolved entirely by the
//! app via `ProjectDirs` (T-00-06: no path traversal / unexpected location). [`open_at`] is an
//! internal/test seam that points the same migrate+seed code path at an explicit file (a temp
//! dir in tests) so we never read or write the developer's real app-data DB during testing.

use std::path::Path;

use anyhow::{Context, Result};
use directories::ProjectDirs;
use rusqlite::Connection;

pub mod schema;
pub mod seed;
pub mod settings;

pub use seed::seed_default_if_empty;
pub use settings::read_setting;

/// SQLite database filename under the resolved app-data directory.
/// `.sqlite3` makes the format obvious to anyone inspecting the app-data folder.
pub const DB_FILENAME: &str = "womanizer.sqlite3";

/// Open the voice database at the canonical app-data location, migrate it to the latest
/// schema, and seed the Default voice on first launch.
///
/// Resolves the path via `ProjectDirs::from("com", "OtterBot", "Womanizer")`:
///   - macOS: `~/Library/Application Support/com.OtterBot.Womanizer/`
///   - Windows: `%APPDATA%\OtterBot\Womanizer\data\`
///
/// The directory is created if missing. Returns an open [`Connection`] ready for CRUD.
pub fn open_and_init() -> Result<Connection> {
    let dirs = ProjectDirs::from("com", "OtterBot", "Womanizer")
        .context("could not resolve a home/app-data directory for ProjectDirs")?;
    let data_dir = dirs.data_dir();
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("failed to create app-data dir {}", data_dir.display()))?;
    let db_path = data_dir.join(DB_FILENAME);
    open_at(&db_path)
}

/// Open the database at an explicit path, migrate, and seed (the path-injectable variant).
///
/// Internal/test seam: production code calls [`open_and_init`] (no caller-supplied path).
/// Tests point this at a unique temp-dir file so they exercise the real migrate+seed code path
/// without touching the canonical `ProjectDirs` location (T-00-06: the injected path is
/// internal, never external input).
pub fn open_at(db_path: &Path) -> Result<Connection> {
    let mut conn = Connection::open(db_path)
        .with_context(|| format!("failed to open SQLite DB at {}", db_path.display()))?;
    init_conn(&mut conn)?;
    Ok(conn)
}

/// Apply migrations then seed on an already-open connection.
///
/// Shared by [`open_at`] and usable directly by tests that drive an in-memory connection
/// (`Connection::open_in_memory`) through the identical migrate+seed path.
pub fn init_conn(conn: &mut Connection) -> Result<()> {
    schema::MIGRATIONS
        .to_latest(conn)
        .context("failed to apply SQLite migrations")?; // atomic; manages user_version (D-06)
    seed::seed_default_if_empty(conn).context("failed to seed Default voice")?;
    Ok(())
}
