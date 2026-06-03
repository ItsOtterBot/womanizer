//! DSP-04 integration test — 220 Hz pure sine over a 512-sample window at 48 kHz must
//! flow through `Yin48k::get_pitch` and return `Some(f)` with `|f - 220.0| < 2.0`.
//!
//! Activated by Plan 02-06 (ignore attribute removed). The lib unit test
//! `dsp::tests::yin48k_returns_some_for_220hz_sine` exercises the same body; the
//! integration version exists because VALIDATION.md row DSP-04 names the per-requirement
//! command as `cargo test -p womanizer-engine --test dsp_yin_sine_accuracy` (the public-
//! facing per-test surface the verifier matches against).
//!
//! Tolerance of ±2 Hz accommodates YIN's parabolic-interpolation quantization (RESEARCH
//! §Q4) without masking a real regression in the wrapper's clarity-threshold (0.85) or
//! power-threshold (0.0) constants.

use womanizer_engine::dsp::Yin48k;

/// Engine sample rate — fixed at 48 kHz (D-05). The integration test re-declares the
/// constant so it stays self-contained at the public-API surface (no crate-internal
/// constant import needed).
const ENGINE_SR: f32 = 48_000.0;

/// 512-sample YIN window per D-32 (~10 ms @ 48 kHz).
const WINDOW: usize = 512;

#[test]
fn yin_sine_accuracy() {
    // 220 Hz pure sine, amplitude 0.5, 512 samples — same generator pattern as
    // `resampler.rs::tests` lines 282-298 (PATTERNS.md identifies as the canonical
    // sine-injection analog).
    let f_hz = 220.0f32;
    let amplitude = 0.5f32;
    let phase_step = 2.0 * std::f32::consts::PI * f_hz / ENGINE_SR;
    let mut phase = 0.0f32;
    let mut window = vec![0f32; WINDOW];
    for s in window.iter_mut() {
        *s = amplitude * phase.sin();
        phase += phase_step;
        if phase > 2.0 * std::f32::consts::PI {
            phase -= 2.0 * std::f32::consts::PI;
        }
    }

    let mut yin = Yin48k::new();
    let result = yin.get_pitch(&window);
    let f = result.expect(
        "Yin48k::get_pitch must return Some for a clean 220 Hz sine — None here would \
         indicate the clarity threshold (0.85 per RESEARCH §Q4) is too strict for this signal",
    );
    let err = (f - f_hz).abs();
    assert!(
        err < 2.0,
        "Yin48k::get_pitch returned {f} Hz for a {f_hz} Hz sine; error {err} Hz exceeds \
         the 2 Hz YIN parabolic-interpolation tolerance"
    );
}
