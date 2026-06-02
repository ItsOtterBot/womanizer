//! Wave 0 STUB — requirement DSP-02; filled in by Plan 02-05.
//!
//! Goal of the real test (per RESEARCH §Q12 / VALIDATION.md Wave 0 Requirements list):
//! when `Gate::update(raw_rms)` is below the close threshold (−50 dBFS ≈ 0.003162) for
//! longer than the 50 ms hold-open (2400 samples), the gate is closed AND the worker emits
//! true digital silence — every sample BYTE-EXACTLY `0.0_f32` (D-29). Drive a synthetic
//! sub-threshold buffer for ≥ 50 ms and assert the output slice is `[0.0; N]` verbatim
//! (no near-zero floating-point residue from the gate's release envelope leaking through).
//!
//! Boilerplate mirrors `tests/rt_safety.rs`:
//!   - registers `assert_no_alloc::AllocDisabler` as the `#[global_allocator]` for THIS
//!     test binary under `#[cfg(debug_assertions)]` because Gate hysteresis is part of the
//!     worker hot path and the assertion is meaningless without the global allocator live;
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
fn gate_closed_silence() {
    todo!("Plan 02-05 fills in the body — see RESEARCH §Q12 sketch (drive Gate::update with sub-threshold RMS for >50 ms, assert output buffer == [0.0; N] byte-exact per D-29)");
}
