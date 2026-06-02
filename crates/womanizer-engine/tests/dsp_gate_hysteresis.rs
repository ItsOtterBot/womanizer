//! Wave 0 STUB — requirement DSP-02; filled in by Plan 02-05.
//!
//! Goal of the real test (per RESEARCH §Q12 / VALIDATION.md Wave 0 Requirements list):
//! the Gate state machine implements D-30 hysteresis precisely — it opens when smoothed
//! RMS crosses the OPEN threshold (−45 dBFS ≈ 0.005623), stays open until smoothed RMS
//! stays below the CLOSE threshold (−50 dBFS ≈ 0.003162) for the full 50 ms hold-open
//! (2400 samples), and a level that hovers in the 5 dB hysteresis band between close and
//! open MUST NOT chatter the state (no toggle once open). Drive synthetic RMS sequences
//! for each branch — `update()`'s boolean return must follow the expected open/closed
//! trajectory.
//!
//! Boilerplate mirrors `tests/rt_safety.rs`:
//!   - registers `assert_no_alloc::AllocDisabler` as the `#[global_allocator]` for THIS
//!     test binary under `#[cfg(debug_assertions)]` — Gate::update is on the worker hot
//!     path and must be alloc-free in addition to behaviorally correct;
//!   - `#[serial_test::serial(no_alloc_violation_counter)]` because the violation counter
//!     is process-global and shared across every assert_no_alloc test in this binary.
//!
//! When Plan 02-05 fills in the body it MUST remove the `#[ignore]` attribute below.

#[cfg(debug_assertions)]
#[global_allocator]
static A: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

// `use` keeps the dsp::Gate surface live so a future rename breaks this stub at compile
// time even before Plan 02-05 fills in the body.
#[allow(unused_imports)]
use womanizer_engine::dsp::Gate;

#[test]
#[ignore = "stub — filled in by Plan 02-05"]
#[serial_test::serial(no_alloc_violation_counter)]
fn gate_hysteresis() {
    todo!("Plan 02-05 fills in the body — see RESEARCH §Q12 sketch (Gate opens at −45 dBFS, closes only after 50 ms below −50 dBFS, no chatter in the hysteresis band)");
}
