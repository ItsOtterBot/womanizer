//! Integration test for DSP-06 — `dsp::rms_simd` (Plan 02-07 Task 1) reproduces the scalar
//! `sqrt(sum_sq / len.max(1))` reference within 1e-6 across silence / sine / noise /
//! constant / remainder-path inputs.
//!
//! Mirrors the four `dsp::tests::rms_simd_*` lib unit tests but lives at the integration
//! layer (`crates/womanizer-engine/tests/dsp_simd_rms_parity.rs`) so the VALIDATION.md
//! per-requirement command surface (`cargo test --test dsp_simd_rms_parity`) reports
//! DSP-06 as covered by a top-level test binary, not only via the lib runner. The lib
//! tests are the tighter, faster-feedback gate; this integration test is the
//! production-surface gate.
//!
//! No `assert_no_alloc::AllocDisabler` registration — `rms_simd` is a pure per-call free
//! function over a caller-supplied `&[f32]`; the parity test does not touch the
//! process-global allocation counter and therefore does not need `serial_test`
//! coordination either. The alloc-free claim is verified by the worker-loop integration
//! gate in Plan 02-09's `dsp_assert_no_alloc_loop`, which exercises the full Phase 2
//! callback chain (cpal_io capture → in_rx → worker → vo_tx) under the global
//! `AllocDisabler`.

use womanizer_engine::dsp::rms_simd;

const ENGINE_SR: f32 = 48_000.0;

/// Byte-equivalent reference of the scalar RMS form that lived in `cpal_io.rs:477-479`
/// before Plan 02-07 Task 2 (`let sum_sq: f32 = mono_native.iter().map(|s| s*s).sum();
/// let rms = (sum_sq / mono_native.len().max(1) as f32).sqrt();`). Keep this body
/// IDENTICAL to that historical scalar form — drift here would silently weaken the parity
/// gate. The lib-side `scalar_rms` helper in `dsp::tests` uses the same shape.
fn scalar_rms(samples: &[f32]) -> f32 {
    let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
    (sum_sq / samples.len().max(1) as f32).sqrt()
}

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

/// Deterministic linear congruential PRNG → uniform white noise in [-1, 1]. Classic
/// glibc LCG (a=1103515245, c=12345) seeded at 12345 so the test is fully reproducible
/// without bringing in a `rand` dep. Same generator as `dsp::tests::lcg_noise`.
fn lcg_noise(len: usize) -> Vec<f32> {
    let mut state: u32 = 12345;
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        state = state.wrapping_mul(1103515245).wrapping_add(12345);
        out.push((state as i32 as f32) / (i32::MAX as f32));
    }
    out
}

#[test]
fn simd_rms_parity() {
    // Build a comprehensive set of inputs that exercise:
    //  - Zero / silence path (BLOCK=256, multiple of 8 — no remainder)
    //  - Constant non-zero path (BLOCK=256 — proves accumulator wiring)
    //  - 220 Hz voiced sine at the cpal_io capture block size (BLOCK=256)
    //  - 440 Hz higher-freq sine (BLOCK=256) — distinct sine to avoid hardcoded-220 bias
    //  - LCG white noise (BLOCK=256) — full-spectrum, no periodic structure
    //  - 257-sample buffer (NOT a multiple of 8 — `chunks_exact(8)` leaves 1 tail sample;
    //    exercises the scalar remainder path that would silently drop without the loop)
    //  - 1024-sample buffer (multiple of 8, exact 128 chunks — large-N regression vs the
    //    256-sample shapes)
    let inputs: Vec<(&str, Vec<f32>)> = vec![
        ("zero buffer (256)", vec![0.0; 256]),
        ("constant 0.5 (256)", vec![0.5; 256]),
        ("220 Hz sine amp 0.5 (256)", sine_window(220.0, 0.5, 256)),
        ("440 Hz sine amp 0.7 (256)", sine_window(440.0, 0.7, 256)),
        ("LCG noise (256)", lcg_noise(256)),
        ("440 Hz sine (257) — remainder path", {
            let mut v = sine_window(440.0, 0.7, 257);
            // Force the tail sample to a distinct non-zero value so its presence/absence
            // visibly moves the result if the remainder loop were ever omitted.
            v[256] = 0.9;
            v
        }),
        ("220 Hz sine amp 0.5 (1024)", sine_window(220.0, 0.5, 1024)),
    ];

    for (label, input) in inputs.iter() {
        let simd = rms_simd(input);
        let scalar = scalar_rms(input);
        let diff = (simd - scalar).abs();
        assert!(
            diff < 1e-6,
            "rms_simd vs scalar parity broken on {label}: simd={simd}, scalar={scalar}, diff={diff} (tol 1e-6)"
        );
    }
}
