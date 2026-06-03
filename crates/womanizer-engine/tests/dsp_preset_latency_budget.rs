//! DSP-03 integration test: per-preset Stretch48k latency stays within budget.
//!
//! For each [`Preset`] variant (`Low`, `Balanced`, `Quality`), construct a `Stretch48k`
//! via the Plan 02-04 wrapper and assert
//!
//! ```text
//! Stretch48k::latency_samples() as f32 / 48.0 < (preset_budget_ms − 12.7)
//! ```
//!
//! where 12.7 ms is the RESEARCH §Q2 platform-overhead estimate (cpal capture + playback
//! in-flight + scheduling slack) and the per-preset round-trip budgets are 32 / 40 / 50 ms
//! (D-25). The construction-time bound is necessary but not sufficient: the manual
//! checkpoint in Plan 02-09 Task 3 verifies the live `Telemetry::latency_ms` stays in
//! budget against the actual measured platform overhead, which may differ from the 12.7
//! ms estimate.
//!
//! ## What this test catches
//!
//! - A future planner accidentally restoring the original (1024/256, 2048/512, 3072/768)
//!   RESEARCH §Q2 starting points whose latency_samples ≈ block_length blew Balanced and
//!   Quality.
//! - A future signalsmith-stretch upgrade where the `input_latency` /
//!   `output_latency` semantics shift (e.g. additional internal buffering not surfaced
//!   in the getters).
//! - A planner accidentally using `Stretch::preset_default` instead of the explicit
//!   `Stretch::new(1, block, interval)` constructor (RESEARCH §Pitfall 1).
//!
//! No `assert_no_alloc::AllocDisabler` registration: `latency_samples()` is a pure
//! getter; no per-block hot path is exercised. Plan 02-09's `dsp_assert_no_alloc_loop`
//! integration test (companion to this one) carries the RT-safety contract.

use womanizer_engine::dsp::{Preset, Stretch48k};

#[test]
fn preset_latency_budget() {
    // Per-preset (budget_ms, stretch_budget_ms) — stretch_budget = budget − 12.7 ms
    // platform overhead per RESEARCH §Q2 (BLOCK=256 capture + playback in-flight +
    // scheduling slack).
    for (preset, budget_ms) in [
        (Preset::Low, 32.0f32),
        (Preset::Balanced, 40.0f32),
        (Preset::Quality, 50.0f32),
    ] {
        let s = Stretch48k::new(preset);
        let lat_samples = s.latency_samples();
        let lat_ms = lat_samples as f32 / 48.0;
        let stretch_budget = budget_ms - 12.7;
        // Print the actual values so the planner / verifier reading test output sees the
        // empirical margin — useful when tuning preset_window_hop in lock-step.
        eprintln!(
            "preset {:?}: latency_samples={}, lat_ms={:.2}, stretch_budget={:.2} \
             (total budget {} ms − 12.7 ms platform overhead)",
            preset, lat_samples, lat_ms, stretch_budget, budget_ms
        );
        assert!(
            lat_ms < stretch_budget,
            "preset {:?} latency {:.2} ms exceeds Stretch budget {:.2} ms \
             (total budget {} ms − 12.7 ms platform overhead per RESEARCH §Q2). \
             Tighten preset_window_hop({:?}) in src/dsp.rs to a smaller block_length.",
            preset,
            lat_ms,
            stretch_budget,
            budget_ms,
            preset
        );
    }
}
