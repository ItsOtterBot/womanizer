//! Wave 0 STUB — requirement DSP-04; filled in by Plan 02-06.
//!
//! Goal of the real test (per RESEARCH §Q12 / VALIDATION.md Wave 0 Requirements list):
//! generate 512 samples of white noise (no periodic structure), pass it to
//! `Yin48k::get_pitch`, and assert the return is `None`. This validates the unvoiced
//! branch — without it, YIN would emit garbage F0 readings for whisper / breath / silence
//! and the UI's `.is_nan()` "—" rendering (D-32) would never trigger.
//!
//! The companion test `dsp_yin_sine_accuracy` covers the voiced branch (220 Hz sine →
//! 220 ± 2 Hz). Together they pin both sides of the clarity threshold (0.93, D-32).
//!
//! No `assert_no_alloc::AllocDisabler` registration in THIS file — Yin48k::get_pitch is
//! covered for alloc-freedom by `dsp_assert_no_alloc_loop`; this test focuses on the
//! unvoiced-classification correctness.
//!
//! When Plan 02-06 fills in the body it MUST remove the `#[ignore]` attribute below.

// `use` keeps the dsp::Yin48k surface live so a future rename breaks this stub at
// compile time even before Plan 02-06 fills in the body.
#[allow(unused_imports)]
use womanizer_engine::dsp::Yin48k;

#[test]
#[ignore = "stub — filled in by Plan 02-06"]
fn yin_noise_unvoiced() {
    todo!("Plan 02-06 fills in the body — see RESEARCH §Q12 sketch (512 samples of white noise → Yin48k::get_pitch returns None — validates D-32 unvoiced branch)");
}
