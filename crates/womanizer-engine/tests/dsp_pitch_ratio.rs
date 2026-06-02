//! Wave 0 STUB — requirement DSP-01; filled in by Plan 02-04.
//!
//! Goal of the real test (per RESEARCH §Q12 / VALIDATION.md Wave 0 Requirements list):
//! generate a pure-tone sine at a known input frequency (e.g. 220 Hz), drive it through
//! `Stretch48k::process` with `set_transpose(m)` set to a known ratio (e.g. 1.5×), run an
//! FFT on the output, and assert the dominant frequency bin equals `220 * m` within a
//! tolerance dictated by the FFT bin width. Verifies the pitch-shifting half of the
//! signalsmith-stretch wrapper end-to-end without trusting upstream math blindly.
//!
//! Pattern: mirrors the sine-injection helper in `crates/womanizer-engine/src/resampler.rs`
//! (lines 282-298) — same `(i as f32 * f * 2π / SR).sin()` generator pattern; same FFT-of-
//! output → argmax-bin → frequency assertion shape. PATTERNS.md identifies the resampler
//! as the canonical analog.
//!
//! No `assert_no_alloc::AllocDisabler` registration in THIS file — this is a behavioral
//! smoke test (set-up + offline FFT analysis), not an RT-safety gate. The companion
//! `dsp_assert_no_alloc_loop` test covers the alloc invariant.
//!
//! When Plan 02-04 fills in the body it MUST remove the `#[ignore]` attribute below.

// `use` keeps the dsp::Stretch48k surface live so a future rename breaks this stub at
// compile time even before Plan 02-04 fills in the body.
#[allow(unused_imports)]
use womanizer_engine::dsp::Stretch48k;

#[test]
#[ignore = "stub — filled in by Plan 02-04"]
fn pitch_ratio() {
    todo!("Plan 02-04 fills in the body — see RESEARCH §Q12 sketch (220 Hz sine → set_transpose(1.5) → FFT-of-output dominant bin == 330 Hz ± bin-width)");
}
