//! SHAPE-01 (Plan 03-03 Task 1): `Breathiness::process` is allocation-free on the per-block
//! hot path. Wraps a single 256-sample block of `process()` inside `assert_no_alloc(|| { ... })`
//! and asserts the violation counter delta is zero.
//!
//! This is the integration-test variant of the `breath_alloc_free_smoke` behavior. Per the
//! plan's decision tree (03-03-PLAN.md lines 234-236), `crates/womanizer-engine/src/dsp.rs`
//! does NOT register `AllocDisabler` at the test-module level (its existing
//! `yin48k_get_pitch_alloc_free_smoke` lib test runs without one and is therefore vacuously
//! true), so the alloc-free claim for Breathiness lives in a dedicated integration test that
//! DOES register `AllocDisabler` for its own test binary — same boilerplate as
//! `tests/dsp_gate_hysteresis.rs`, `tests/dsp_gate_closed_silence.rs`, `tests/rt_safety.rs`.
//!
//! Boilerplate:
//!   - `#[global_allocator] static A: AllocDisabler` under `#[cfg(debug_assertions)]` — the
//!     allocator is debug-only so release builds are zero-cost.
//!   - `#[serial_test::serial(no_alloc_violation_counter)]` because the violation counter is
//!     process-global and shared across every `assert_no_alloc` test in this binary.
//!
//! Verifies the contract a) at amount=0.0 / voicing_gate=0 (NaN F0 — early-exit-equivalent
//! path) and b) at amount=0.5 / voicing_gate=1 (full PRNG + biquad + envelope + noise-add
//! path). If the per-sample loop ever allocates (e.g. a future XorShift32 refactor accidentally
//! introduces a Vec), this gate fires.

#[cfg(debug_assertions)]
#[global_allocator]
static A: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

use assert_no_alloc::assert_no_alloc;
use womanizer_engine::dsp::Breathiness;

const ENGINE_SR: f32 = 48_000.0;
const BLOCK: usize = 256;

/// Generate a `len`-sample window of a pure sine at `f_hz` and `amplitude` at 48 kHz.
fn sine_window(f_hz: f32, amplitude: f32, len: usize) -> Vec<f32> {
    let phase_step = 2.0 * std::f32::consts::PI * f_hz / ENGINE_SR;
    let mut phase = 0.0f32;
    let mut out = vec![0f32; len];
    for s in out.iter_mut() {
        *s = amplitude * phase.sin();
        phase += phase_step;
        if phase > 2.0 * std::f32::consts::PI {
            phase -= 2.0 * std::f32::consts::PI;
        }
    }
    out
}

/// Per-block `Breathiness::process` must not allocate. Exercises BOTH the voiced
/// (full-chain) and unvoiced (gate=0) branches and the warm-off branch.
#[cfg(debug_assertions)]
#[test]
#[serial_test::serial(no_alloc_violation_counter)]
fn breath_alloc_free_smoke() {
    // Pre-allocate everything OUTSIDE the assert_no_alloc closure.
    let input = sine_window(440.0, 0.5, BLOCK);
    let mut output = vec![0f32; BLOCK];
    let mut breath = Breathiness::new(ENGINE_SR, 0x12345678);

    assert_no_alloc::reset_violation_count();
    let before = assert_no_alloc::violation_count();

    // Branch 1: voiced, enabled — exercises the full PRNG + biquad + envelope + noise-add
    // path. This is the worst case for the hot loop.
    assert_no_alloc(|| {
        breath.process(&input, &mut output, 0.5, true, 440.0);
    });

    // Branch 2: unvoiced (NaN F0) — voicing_gate = 0, so noise_add is identically zero
    // even with amount=0.5. PRNG / biquad / envelope still advance per D-42 warm contract.
    assert_no_alloc(|| {
        breath.process(&input, &mut output, 0.5, true, f32::NAN);
    });

    // Branch 3: enabled=false (warm-off) — only the output assignment differs; PRNG /
    // biquad / envelope still advance.
    assert_no_alloc(|| {
        breath.process(&input, &mut output, 0.5, false, 440.0);
    });

    let after = assert_no_alloc::violation_count();
    assert_eq!(
        after, before,
        "Breathiness::process tripped the assert_no_alloc violation counter across 3 \
         blocks (voiced+enabled, unvoiced, warm-off) — a per-sample / per-block \
         allocation was introduced. Counter delta = {}",
        after - before
    );

    std::hint::black_box(&breath);
}
