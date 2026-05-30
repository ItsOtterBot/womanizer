//! Read-only access to the key/value `settings` table (D-05).
//!
//! Phase 1's contract per `.planning/phases/01-audio-i-o-passthrough-virtual-device-setup/01-CONTEXT.md`
//! ("integration-points") is read-only: the app's first launch reads the three last-selected
//! device-id keys ([`KEY_INPUT_DEVICE_ID`], [`KEY_VIRTUAL_OUTPUT_DEVICE_ID`],
//! [`KEY_MONITOR_DEVICE_ID`]) to populate the engine's [`EngineState`] before constructing the
//! eframe app. Writes land in Phase 4 GUI (when the user selects a device from a dropdown).
//!
//! [`EngineState`]: https://docs.rs/womanizer-engine/0.1.0/womanizer_engine/struct.EngineState.html
//!
//! ## Security
//! The SELECT is PARAMETERIZED (`?N` placeholder, never string-concatenated). Same T-00-05
//! pattern as `seed.rs` — established from day one so future write paths inherit it.
//!
//! ## Phase 1 keys
//! - `"input_device_id"`: the user-visible cpal device name (matches
//!   `cpal::Device::description().name()`) for the mic input.
//! - `"virtual_output_device_id"`: the user-visible name of the virtual-audio device VRChat
//!   sees as a microphone (the rebranded BlackHole `"Womanizer"` on macOS; the
//!   regex-matched-canonical `"CABLE Input (VB-Audio Virtual Cable)"` on Windows).
//! - `"monitor_device_id"`: the user-visible name of the headphone output the self-monitor
//!   stream targets when `HotParams::monitor_enabled == true`.

use anyhow::{Context, Result};
use rusqlite::Connection;

/// Settings key for the last-selected mic input device.
pub const KEY_INPUT_DEVICE_ID: &str = "input_device_id";

/// Settings key for the last-selected virtual-output device (Womanizer / CABLE Input).
pub const KEY_VIRTUAL_OUTPUT_DEVICE_ID: &str = "virtual_output_device_id";

/// Settings key for the last-selected self-monitor headphone device.
pub const KEY_MONITOR_DEVICE_ID: &str = "monitor_device_id";

/// Read a single value from the `settings` table.
///
/// Returns:
/// - `Ok(Some(value))` if a row with the given `key` exists.
/// - `Ok(None)` if no row exists (the `QueryReturnedNoRows` rusqlite error is mapped to `None`).
/// - `Err(_)` only for genuine failures (corrupt DB, locked connection, schema mismatch).
///
/// Parameterized: the `key` is passed as a `?1` placeholder bind value, never interpolated
/// into SQL text. Safe for arbitrary key strings (including single quotes, backslashes, etc.).
///
/// Read-only by design for Phase 1 (CONTEXT integration-points). Writes are Phase 4's remit.
pub fn read_setting(conn: &Connection, key: &str) -> Result<Option<String>> {
    match conn.query_row(
        "SELECT value FROM settings WHERE key = ?1",
        rusqlite::params![key],
        |row| row.get::<_, String>(0),
    ) {
        Ok(value) => Ok(Some(value)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e).with_context(|| format!("read_setting({key}) failed")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// Build a migrated in-memory DB for a settings test. Mirrors the `init.rs` integration
    /// test pattern; reusing the public `init_conn` path means the test exercises the same
    /// migrate path production uses.
    fn open_migrated_in_memory() -> Connection {
        let mut conn = Connection::open_in_memory().expect("open_in_memory");
        crate::init_conn(&mut conn).expect("init_conn (migrate + seed)");
        conn
    }

    /// A missing key must return `Ok(None)` — `QueryReturnedNoRows` is mapped to `None`, not
    /// surfaced as an error. The Phase 1 main.rs relies on this for first-launch behavior
    /// (the settings table is empty so all three reads return `None` and the engine falls back
    /// to host defaults — matches the AUDIO-09 in-memory fallback contract in event_loop.rs).
    #[test]
    fn read_setting_returns_none_for_missing_key() {
        let conn = open_migrated_in_memory();
        let v = read_setting(&conn, KEY_INPUT_DEVICE_ID).expect("read_setting must not error");
        assert!(
            v.is_none(),
            "missing key must map to Ok(None), got {v:?}"
        );
    }

    /// A present key must return `Ok(Some(value))`. Inserts a row via parameterized INSERT
    /// then reads it back.
    #[test]
    fn read_setting_returns_value_for_present_key() {
        let conn = open_migrated_in_memory();
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)",
            rusqlite::params![KEY_INPUT_DEVICE_ID, "Test Mic"],
        )
        .expect("INSERT into settings");
        let v = read_setting(&conn, KEY_INPUT_DEVICE_ID).expect("read_setting must not error");
        assert_eq!(
            v,
            Some("Test Mic".to_string()),
            "present key must round-trip the stored value"
        );
    }

    /// The SELECT must be parameterized — a key containing a single quote must NOT cause a
    /// SQL parse error / injection. Proves the `?1` bind contract is in place (T-00-05).
    #[test]
    fn read_setting_uses_parameterized_query() {
        let conn = open_migrated_in_memory();
        // A key with characters that would break string-concatenated SQL: single quote,
        // semicolon, comment marker.
        let nasty_key = "foo'bar; -- drop table settings";
        // Reading a missing nasty key must return Ok(None), not an error and not panic.
        let v = read_setting(&conn, nasty_key)
            .expect("parameterized SELECT must handle quotes/semicolons without erroring");
        assert!(
            v.is_none(),
            "nasty key has no row; must return Ok(None), got {v:?}"
        );
        // Inserting the same nasty key and reading it back must round-trip cleanly.
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)",
            rusqlite::params![nasty_key, "intact-value"],
        )
        .expect("parameterized INSERT must handle quotes/semicolons");
        let v2 = read_setting(&conn, nasty_key).expect("post-insert read");
        assert_eq!(
            v2,
            Some("intact-value".to_string()),
            "nasty-key round-trip must return the inserted value"
        );
        // And the settings table is still alive (the `-- drop table` substring was data,
        // not SQL).
        let still_alive: i64 = conn
            .query_row("SELECT COUNT(*) FROM settings", [], |row| row.get(0))
            .expect("settings table must still be queryable");
        assert!(
            still_alive >= 1,
            "settings table must still exist after the nasty-key round-trip"
        );
    }
}
