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
//!   4. otherwise opens/migrates/seeds the local voice database, reads the three Phase 1
//!      last-selected device-id settings (`input_device_id`, `virtual_output_device_id`,
//!      `monitor_device_id` — read-only per Phase 1's contract; writes land in Phase 4),
//!      constructs the eframe App in the Setup state with those slots populated, and runs
//!      `eframe::run_native` to open the window.
//!
//! `eframe::run_native` returns when the user closes the window or `egui::ViewportCommand::Close`
//! is sent (e.g. the setup screen's Quit button). On headless CI hosts (no display server),
//! `run_native` returns `Err(eframe::Error)` immediately; `main()` propagates that as
//! `anyhow::Error` rather than panicking.

// The App state machine + render modules live in the lib half of this crate so they can be
// unit-tested via `cargo test -p womanizer-app --lib`. The bin imports the public surface.
use womanizer_app::app;

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
    //
    // Plan 01-05 contract (CONTEXT integration-points): the --smoke branch must remain unchanged
    // and must NOT initialize the engine or eframe. This is verified by the plan's verify-block
    // running `cargo run -p womanizer-app -- --smoke` and grep'ing for "smoke harness passed".
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

    // ---- Phase 1 (Plan 01-05): read the three last-selected device-id settings ----
    //
    // Phase 1 is READ-ONLY per CONTEXT integration-points. Each read is `Ok(None)` for a
    // fresh install (the settings table is empty); on subsequent launches Phase 4 will have
    // written the user's last selection. Any error reading the settings table is logged + the
    // slot defaults to None (the engine will fall back to host defaults — same as a fresh
    // install).
    let last_input = womanizer_db::read_setting(&conn, womanizer_db::settings::KEY_INPUT_DEVICE_ID)
        .unwrap_or_else(|e| {
            tracing::warn!(error = ?e, "failed to read input_device_id from settings; using None");
            None
        });
    let last_vout =
        womanizer_db::read_setting(&conn, womanizer_db::settings::KEY_VIRTUAL_OUTPUT_DEVICE_ID)
            .unwrap_or_else(|e| {
                tracing::warn!(
                    error = ?e,
                    "failed to read virtual_output_device_id from settings; using None"
                );
                None
            });
    let last_mon = womanizer_db::read_setting(&conn, womanizer_db::settings::KEY_MONITOR_DEVICE_ID)
        .unwrap_or_else(|e| {
            tracing::warn!(
                error = ?e,
                "failed to read monitor_device_id from settings; using None"
            );
            None
        });
    tracing::info!(
        last_input = ?last_input,
        last_vout = ?last_vout,
        last_mon = ?last_mon,
        "last-selected device ids read from settings (Phase 1 read-only)",
    );

    // ---- Construct the eframe App + launch the window ----
    //
    // The App starts in the Setup state with the three device-id slots populated. eframe owns
    // the conn (we drop it explicitly here — the App does NOT hold the SQLite connection;
    // Phase 4 will re-open the DB inside the egui app to wire CRUD).
    drop(conn);

    let app = app::App::new(last_input, last_vout, last_mon);
    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "Womanizer",
        native_options,
        Box::new(|_cc| Ok(Box::new(app))),
    )
    .map_err(|e| anyhow::anyhow!("eframe::run_native failed: {e}"))?;

    tracing::info!("eframe window closed; exiting cleanly");
    Ok(())
}
