//! DSP-01 verification — `Stretch48k` produces the expected pitch ratio.
//!
//! Generates a pure 440 Hz sine, drives it through `Stretch48k::process` with a 1.5×
//! transpose factor in BLOCK=256 chunks, runs an FFT on the steady-state tail of the
//! output, and asserts the dominant magnitude bin maps to 660 ± 5 Hz.
//!
//! Tolerance derivation: the FFT bin width for an 8192-sample analysis window at 48 kHz
//! is 48000 / 8192 ≈ 5.86 Hz. Asserting ±5 Hz keeps the test within a single bin's
//! resolution while leaving headroom for the upstream vocoder's interpolation precision.
//!
//! Skip-first-N-samples rationale: signalsmith's Stretch fills its internal latency
//! before producing valid output. We discard `2 * stretch.latency_samples()` from the
//! head of the output — the leading invalid region plus a settling margin — then
//! analyze the next steady-state window.
//!
//! Allocations in the test BODY (Vec for output, the FFT scratch) are fine — only the
//! production loop body in `worker::spawn_dsp_worker` must be alloc-free (verified by
//! `tests/rt_safety.rs` and Plan 02-09's `dsp_assert_no_alloc_loop.rs`).

use num_complex::Complex;
use rustfft::FftPlanner;
use womanizer_engine::dsp::{Stretch48k, ENGINE_SR};
use womanizer_engine::Preset;

const BLOCK: usize = 256;
const INPUT_HZ: f32 = 440.0;
const PITCH_RATIO: f32 = 1.5;
const EXPECTED_HZ: f32 = INPUT_HZ * PITCH_RATIO; // 660.0
const TOLERANCE_HZ: f32 = 5.0;
const FFT_SIZE: usize = 8192;

#[test]
fn pitch_ratio() {
    // --- Construct + configure the Stretch instance off the audio path. ---
    let mut stretch = Stretch48k::new(Preset::Balanced);
    stretch.set_transpose(PITCH_RATIO);
    stretch.set_formant(1.0);

    // --- Generate ~1 s of a 440 Hz sine at amplitude 0.5. ---
    // Length is rounded up to a multiple of BLOCK so every chunk feeds a full 256 samples;
    // we make it generous (2 s) so steady-state analysis still has plenty of headroom
    // after discarding the upstream-latency leading region.
    const TOTAL_SAMPLES: usize = 96_000; // 2 s @ 48 kHz, divisible by BLOCK=256.
    let two_pi_f_over_sr = 2.0 * std::f32::consts::PI * INPUT_HZ / ENGINE_SR as f32;
    let input: Vec<f32> = (0..TOTAL_SAMPLES)
        .map(|i| 0.5 * (two_pi_f_over_sr * i as f32).sin())
        .collect();

    // --- Process in BLOCK=256 chunks. ---
    let mut output = vec![0f32; TOTAL_SAMPLES];
    let mut scratch_out = [0f32; BLOCK];
    let num_blocks = TOTAL_SAMPLES / BLOCK;
    for b in 0..num_blocks {
        let start = b * BLOCK;
        let end = start + BLOCK;
        stretch.process(&input[start..end], &mut scratch_out);
        output[start..end].copy_from_slice(&scratch_out);
    }

    // --- Skip the upstream-latency leading region, then take an FFT_SIZE window
    //     from a steady-state slice. ---
    // Discard 2× latency_samples (leading invalid region + settling margin). The
    // remainder must be at least FFT_SIZE samples for the analysis window.
    let lead_skip = 2 * stretch.latency_samples();
    assert!(
        TOTAL_SAMPLES > lead_skip + FFT_SIZE,
        "test signal too short: TOTAL_SAMPLES={TOTAL_SAMPLES} must exceed lead_skip+FFT_SIZE \
         ={skip_plus_fft} (latency_samples={lat})",
        skip_plus_fft = lead_skip + FFT_SIZE,
        lat = stretch.latency_samples(),
    );
    // Take the analysis window from a slice well after the leading skip — center it in the
    // remaining usable region so we have settling margin on both sides.
    let usable_start = lead_skip;
    let usable_end = TOTAL_SAMPLES;
    let window_start = usable_start + (usable_end - usable_start - FFT_SIZE) / 2;
    let window = &output[window_start..window_start + FFT_SIZE];

    // --- FFT magnitude spectrum → peak bin → Hz. ---
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FFT_SIZE);
    let mut buf: Vec<Complex<f32>> = window.iter().map(|&s| Complex { re: s, im: 0.0 }).collect();
    fft.process(&mut buf);

    // Real input → spectrum is conjugate-symmetric; only bins 0..N/2 carry independent
    // information. Skip bin 0 (DC) so any residual DC offset doesn't get picked as the peak.
    let mut peak_bin = 1usize;
    let mut peak_mag = buf[1].norm_sqr();
    for (i, c) in buf.iter().enumerate().take(FFT_SIZE / 2).skip(2) {
        let m = c.norm_sqr();
        if m > peak_mag {
            peak_mag = m;
            peak_bin = i;
        }
    }
    let peak_hz = peak_bin as f32 * ENGINE_SR as f32 / FFT_SIZE as f32;

    // --- Assertion: dominant bin maps to ~660 Hz. ---
    assert!(
        (peak_hz - EXPECTED_HZ).abs() <= TOLERANCE_HZ,
        "DSP-01 violated: 440 Hz × {PITCH_RATIO}× should peak near {EXPECTED_HZ} Hz; \
         got {peak_hz} Hz (peak_bin={peak_bin}, FFT_SIZE={FFT_SIZE}, bin_width={bw:.3} Hz)",
        bw = ENGINE_SR as f32 / FFT_SIZE as f32,
    );
}
