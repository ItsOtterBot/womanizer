//! Integration test for INFRA-04 (Success Criterion #4): first launch creates the SQLite DB,
//! applies migrations, and seeds EXACTLY ONE "Default" voice — idempotently.
//!
//! The test never touches the real `ProjectDirs` app-data location: it points the
//! path-injectable [`womanizer_db::open_at`] at a unique temp-dir file (no temp-dir crate —
//! `std::env::temp_dir().join(unique)` keeps the dep graph minimal and license-clean), and
//! also drives the identical migrate+seed path on an in-memory connection via
//! [`womanizer_db::init_conn`].

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::Connection;

/// Build a process-unique temp path so parallel test runs never collide with each other or
/// with the developer's real app-data DB.
fn unique_temp_db_path() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let unique = format!("womanizer-test-{pid}-{n}.sqlite3");
    std::env::temp_dir().join(unique)
}

/// Count rows named 'Default' via a parameterized query (no string-concat SQL).
fn default_voice_count(conn: &Connection) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM voices WHERE name = ?1",
        ["Default"],
        |row| row.get(0),
    )
    .expect("count query should succeed against migrated schema")
}

#[test]
fn first_launch_creates_and_seeds() {
    let db_path = unique_temp_db_path();
    // Guard against a stale file from a previous aborted run.
    let _ = std::fs::remove_file(&db_path);

    // ---- First launch: create + migrate + seed ----
    let conn = womanizer_db::open_at(&db_path).expect("first launch open_at should succeed");

    // The DB file was actually created on disk.
    assert!(
        db_path.exists(),
        "first launch must create the SQLite DB file at {}",
        db_path.display()
    );

    // The migration applied the schema: settings table exists too (D-05).
    let settings_table: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            ["settings"],
            |row| row.get(0),
        )
        .expect("schema query should succeed");
    assert_eq!(
        settings_table, 1,
        "migration must create the settings table"
    );

    // Exactly one Default voice seeded.
    assert_eq!(
        default_voice_count(&conn),
        1,
        "first launch must seed exactly one Default voice"
    );

    // ---- Idempotency: seeding again on the same DB does NOT duplicate ----
    womanizer_db::seed_default_if_empty(&conn).expect("second seed call should succeed");
    assert_eq!(
        default_voice_count(&conn),
        1,
        "a second seed must not duplicate the Default voice (idempotent)"
    );

    // ---- Re-open the same DB file (simulates a second app launch) ----
    drop(conn);
    let conn2 = womanizer_db::open_at(&db_path).expect("second launch open_at should succeed");
    assert_eq!(
        default_voice_count(&conn2),
        1,
        "re-opening the existing DB must not re-seed the Default voice"
    );

    // ---- The seeded row carries the D-07 values (read back via parameterized query) ----
    let (pitch, formant, compensate, mix, preset): (f64, f64, i64, f64, String) = conn2
        .query_row(
            "SELECT pitch_semitones, formant_semitones, compensate_pitch, mix, quality_preset
             FROM voices WHERE name = ?1",
            ["Default"],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .expect("seeded Default row should be readable");

    // f32 -> SQLite REAL (f64) widening: compare with a small epsilon.
    assert!(
        (pitch - 8.7).abs() < 1e-4,
        "D-07 pitch must be 8.7 semitones, got {pitch}"
    );
    assert!(
        (formant - 2.9).abs() < 1e-4,
        "D-07 formant must be 2.9 semitones, got {formant}"
    );
    assert_eq!(compensate, 1, "D-07 compensate_pitch must be 1 (true)");
    assert!((mix - 1.0).abs() < 1e-4, "D-07 mix must be 1.0, got {mix}");
    assert_eq!(preset, "Balanced", "D-07 quality_preset must be 'Balanced'");

    // Cleanup the temp DB (and its WAL/journal siblings if any).
    drop(conn2);
    let _ = std::fs::remove_file(&db_path);
}

/// The in-memory connection drives the SAME migrate+seed code path as a file DB, proving the
/// path logic and the seed logic are independent and the idempotency guard holds without any
/// filesystem involvement at all.
#[test]
fn in_memory_init_is_idempotent() {
    let mut conn = Connection::open_in_memory().expect("in-memory connection");
    womanizer_db::init_conn(&mut conn).expect("first init_conn should migrate + seed");
    assert_eq!(
        default_voice_count(&conn),
        1,
        "first init seeds one Default"
    );

    // Re-running the seed path must not duplicate.
    womanizer_db::seed_default_if_empty(&conn).expect("re-seed should be a no-op");
    assert_eq!(
        default_voice_count(&conn),
        1,
        "in-memory seed must be idempotent"
    );
}
