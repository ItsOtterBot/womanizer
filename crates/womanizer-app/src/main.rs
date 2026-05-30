//! `womanizer` — the single shipped binary.
//!
//! This is the only crate that registers a `#[global_allocator]` and the only place the GUI
//! framework lives, keeping the library crates allocator-agnostic and unit-testable.
//!
//! On startup the binary:
//!   1. (debug builds only) installs the no-allocation guard as the global allocator,
//!   2. initializes structured logging to stderr,
//!   3. if `--smoke` was passed, runs the reusable cross-thread plumbing smoke harness and
//!      exits — the database is NOT touched, so the harness can verify plumbing even if the
//!      user's app-data directory is unwritable or the SQLite file is corrupt,
//!   4. otherwise opens/migrates/seeds the local voice database.
//!
//! No real audio engine or UI surface exists yet — normal launch opens the database and exits
//! cleanly. The window and engine are wired in later phases.

// Register the no-allocation guard as the global allocator in debug builds ONLY.
//
// In debug, this makes any heap allocation inside an `assert_no_alloc(|| ...)` region observable,
// so the real-time-safety contract is enforced from day one. The guard is deliberately compiled
// out of release builds: the registration is gated on `debug_assertions` so release binaries ship
// the platform's default system allocator with zero added hot-path overhead.
#[cfg(debug_assertions)]
#[global_allocator]
static GLOBAL_ALLOC: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

fn main() -> anyhow::Result<()> {
    // Structured logging to stderr. This is an offline, single-user desktop app: no network sink,
    // no telemetry. `RUST_LOG` controls verbosity; default to `info` when it is unset. Logging
    // is initialized BEFORE any branch (smoke or normal) because both paths benefit from it and
    // it has no side effects on the filesystem or database.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // The cross-thread plumbing smoke harness is a self-contained diagnostic of the cross-thread
    // primitives — it must run even if the user's app-data directory is unwritable or the
    // SQLite database is corrupt or locked. Branch on `--smoke` BEFORE opening the database so a
    // field triage of the plumbing is never coupled to the health of the on-disk DB.
    if std::env::args().any(|arg| arg == "--smoke") {
        tracing::info!("running cross-thread plumbing smoke harness");
        womanizer_core::smoke::run_smoke_test()?;
        tracing::info!("smoke harness passed");
        return Ok(());
    }

    // Normal launch path. Open (creating on first launch), migrate, and seed the local voice
    // database. The path is resolved internally from the canonical app-data location — never
    // from user input.
    let conn = womanizer_db::open_and_init()?;
    let db_path: String = conn
        .path()
        .map(|p| p.to_string())
        .unwrap_or_else(|| "<in-memory>".to_string());
    tracing::info!(db_path = %db_path, "voice database opened, migrated, and seeded");

    // The database is ready. The real-time engine and the GUI window land in later phases — for
    // now the binary exits cleanly so it is safe to run on headless hosts.
    tracing::info!("startup complete; no engine or UI in this build — exiting");
    Ok(())
}
