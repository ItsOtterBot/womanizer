//! Wave 0 STUB — requirements DSP-01 + DSP-02 + DSP-04 + DSP-06; filled in by Plan 02-04.
//!
//! Goal of the real test (per RESEARCH §Q12 / VALIDATION.md Wave 0 Requirements list):
//! drive ~10 s of synthetic audio (≈ 1875 blocks of BLOCK=256 @ 48 kHz) through the full
//! Phase 2 DSP worker body — Gate::update → SmoothedVoiceParams::step → Stretch48k::set_*
//! → Stretch48k::process — wrapped in `assert_no_alloc(|| { ... })`, and assert that the
//! process-global `assert_no_alloc::violation_count()` does not increase. This is the
//! capstone RT-safety gate for Phase 2: it covers every per-block hot-path crossing of the
//! signalsmith-stretch FFI, the YIN F0 evaluation tick, and the wide::f32x8 RMS path.
//!
//! Boilerplate mirrors `tests/rt_safety.rs` (the Phase 1 AUDIO-10 gate):
//!   - registers `assert_no_alloc::AllocDisabler` as the `#[global_allocator]` for THIS
//!     test binary under `#[cfg(debug_assertions)]` — the `warn_debug` feature only emits
//!     violations under debug;
//!   - `#[serial_test::serial(no_alloc_violation_counter)]` because the violation counter
//!     is process-global and any other test in this binary that resets/reads it must
//!     coordinate (same group name as rt_safety.rs);
//!   - snapshots `violation_count()` BEFORE the 10 s synthetic loop and asserts the count
//!     did not increase AFTER.
//!
//! When Plan 02-04 fills in the body it MUST remove the `#[ignore]` attribute below.

#[cfg(debug_assertions)]
#[global_allocator]
static A: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

// `use` keeps the dsp surface live so a future rename of `Stretch48k` breaks this stub at
// compile time even before Plan 02-04 fills in the body.
#[allow(unused_imports)]
use womanizer_engine::dsp::Stretch48k;

#[test]
#[ignore = "stub — filled in by Plan 02-04"]
#[serial_test::serial(no_alloc_violation_counter)]
fn assert_no_alloc_loop() {
    todo!("Plan 02-04 fills in the body — see RESEARCH §Q12 sketch (10 s synthetic loop wrapped in assert_no_alloc(|| {{ ... }}), assert violation_count() delta == 0)");
}
