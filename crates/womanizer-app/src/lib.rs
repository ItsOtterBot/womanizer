//! `womanizer_app` — library half of the desktop binary.
//!
//! Houses the egui App state machine and the two render modules (setup gate + Ready shell)
//! so they can be unit-tested via `cargo test -p womanizer-app --lib`. The binary entrypoint
//! (`src/main.rs`) constructs `app::App::new(...)` against this lib's public surface.
//!
//! The split exists for testability only — the binary is still the single shipped surface;
//! the lib is not published to crates.io (Cargo.toml `publish = false`).

pub mod app;
pub mod ready_shell;
pub mod setup_screen;
