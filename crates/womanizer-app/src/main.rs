//! `womanizer` — the single shipped binary.
//!
//! Phase 0 stub: compiles and exits cleanly. The real entry point — `#[global_allocator]`
//! allocator registration (assert_no_alloc, debug-only per D-11), `directories`-based app-data
//! path resolution, DB open/migrate/seed, and the eframe window — lands in Plan 04.

fn main() {
    // Stub: no allocator registration, no DB open, no GUI yet (Plan 04 wires those).
    println!("womanizer Phase 0 skeleton");
}
