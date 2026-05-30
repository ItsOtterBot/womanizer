//! cpal stream construction + RT-shaped capture/playback callbacks.
//!
//! Populated by Plan 01-02a. Mirrors the Phase 0 smoke-harness shape (every callback body
//! wrapped in `assert_no_alloc(|| { ... })`; drop-on-Full ring pushes; error_callback pushes
//! into `ErrorRing` only — no allocation, no log, no syscall on the RT path).
