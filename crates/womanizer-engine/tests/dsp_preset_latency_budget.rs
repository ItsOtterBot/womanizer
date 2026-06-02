//! Wave 0 STUB — requirement DSP-03; filled in by Plan 02-09.
//!
//! Goal of the real test (per RESEARCH §Q12 / VALIDATION.md Wave 0 Requirements list +
//! 02-CONTEXT.md D-25): for each [`Preset`] variant (`Low`, `Balanced`, `Quality`),
//! construct a `Stretch48k` and assert
//!
//! ```text
//! Stretch48k::latency_samples() as f32 / 48.0 < (preset_budget_ms − 12.7)
//! ```
//!
//! where 12.7 ms is the cpal capture+playback in-flight + scheduling slack overhead
//! (RESEARCH §Q2) and the per-preset budgets are 32 / 40 / 50 ms (D-25). If Plan 02-04's
//! A/B sprint tightens the Quality window, this test (and the `preset_window_hop` smoke
//! test in `dsp.rs`) MUST be updated in lock-step.
//!
//! No `assert_no_alloc::AllocDisabler` registration in THIS file — `latency_samples` is
//! a getter over already-allocated upstream buffers; no per-block hot path is exercised.
//!
//! When Plan 02-09 fills in the body it MUST remove the `#[ignore]` attribute below.

// `use` keeps the dsp::Stretch48k surface live so a future rename breaks this stub at
// compile time even before Plan 02-09 fills in the body.
#[allow(unused_imports)]
use womanizer_engine::dsp::Stretch48k;

#[test]
#[ignore = "stub — filled in by Plan 02-09"]
fn preset_latency_budget() {
    todo!("Plan 02-09 fills in the body — see RESEARCH §Q12 sketch (per-preset Stretch48k::latency_samples()/48 < (D-25 budget − 12.7 ms); Low<32, Balanced<40, Quality<50)");
}
