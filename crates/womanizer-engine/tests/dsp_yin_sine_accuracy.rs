//! Wave 0 STUB — requirement DSP-04; filled in by Plan 02-06.
//!
//! Goal of the real test (per RESEARCH §Q12 / VALIDATION.md Wave 0 Requirements list):
//! generate a 220 Hz pure-tone sine over a 512-sample window (D-32) at 48 kHz, pass it to
//! `Yin48k::get_pitch`, and assert the returned `Some(f)` is within 2 Hz of 220 Hz. The
//! 2 Hz tolerance accommodates YIN's parabolic-interpolation quantization without masking
//! a real regression in the wrapper's clarity-threshold / power-threshold constants
//! (D-32: clarity 0.93, power 0.0).
//!
//! Pattern: mirrors the sine-injection helper in `crates/womanizer-engine/src/resampler.rs`
//! (lines 282-298) — same `(i as f32 * f * 2π / SR).sin()` generator. PATTERNS.md
//! identifies the resampler as the canonical analog for sine-based DSP tests.
//!
//! No `assert_no_alloc::AllocDisabler` registration in THIS file — Yin48k::get_pitch is
//! covered for alloc-freedom by `dsp_assert_no_alloc_loop`; this test focuses on numerical
//! accuracy, not the alloc invariant.
//!
//! When Plan 02-06 fills in the body it MUST remove the `#[ignore]` attribute below.

// `use` keeps the dsp::Yin48k surface live so a future rename breaks this stub at
// compile time even before Plan 02-06 fills in the body.
#[allow(unused_imports)]
use womanizer_engine::dsp::Yin48k;

#[test]
#[ignore = "stub — filled in by Plan 02-06"]
fn yin_sine_accuracy() {
    todo!("Plan 02-06 fills in the body — see RESEARCH §Q12 sketch (220 Hz sine over 512 samples → Yin48k::get_pitch returns Some(220 ± 2 Hz))");
}
