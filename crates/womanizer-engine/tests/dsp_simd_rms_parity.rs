//! Wave 0 STUB — requirement DSP-06; filled in by Plan 02-07.
//!
//! Goal of the real test (per RESEARCH §Q12 / VALIDATION.md Wave 0 Requirements list):
//! the SIMD-accelerated `rms_simd(samples)` (`wide::f32x8` lanes) returns a result that
//! matches a straight-scalar `sqrt((sum sample^2) / len)` reference computation within
//! 1e-6 absolute tolerance across representative buffer shapes (silence, full-scale sine,
//! white noise, edge sizes: 1, 7, 8, 9, 256, 1024). Drives D-34's SIMD acceleration without
//! any behavioral drift from the existing scalar `.map(|s| s*s).sum()` pattern in
//! `cpal_io::capture` and `Gate::update`.
//!
//! No `assert_no_alloc::AllocDisabler` registration in THIS file — `rms_simd` is a pure
//! per-call free function over a caller-supplied `&[f32]`, so the parity test does not
//! exercise the process-global allocation counter and does not need `serial_test`
//! coordination either.
//!
//! When Plan 02-07 fills in the body it MUST remove the `#[ignore]` attribute below.

// `use` keeps the dsp::rms_simd surface live so a future rename breaks this stub at
// compile time even before Plan 02-07 fills in the body.
#[allow(unused_imports)]
use womanizer_engine::dsp::rms_simd;

#[test]
#[ignore = "stub — filled in by Plan 02-07"]
fn simd_rms_parity() {
    todo!("Plan 02-07 fills in the body — see RESEARCH §Q12 sketch (rms_simd vs scalar sqrt((sum sq) / len) parity within 1e-6 across silence/sine/noise/edge sizes)");
}
