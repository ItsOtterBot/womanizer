//! DSP-04 integration test — 512 samples of deterministic white noise (no periodic
//! structure) must flow through `Yin48k::get_pitch` and return `None`. Validates the
//! unvoiced branch (clarity < 0.85 per RESEARCH §Q4). Without this gate, YIN would emit
//! garbage F0 readings for whisper / breath / silence and the UI's `.is_nan()` "—"
//! rendering (D-32) would never trigger.
//!
//! Activated by Plan 02-06 (ignore attribute removed). The lib unit test
//! `dsp::tests::yin48k_returns_none_for_white_noise` exercises the same body; the
//! integration version exists for VALIDATION.md's per-requirement command surface
//! (`cargo test -p womanizer-engine --test dsp_yin_noise_unvoiced`).
//!
//! Noise generator: a deterministic linear congruential PRNG (classic glibc LCG, a =
//! 1103515245, c = 12345, seed = 12345) — no `rand` dep needed, fully reproducible.

use womanizer_engine::dsp::Yin48k;

/// 512-sample YIN window per D-32 (~10 ms @ 48 kHz).
const WINDOW: usize = 512;

#[test]
fn yin_noise_unvoiced() {
    // Deterministic LCG white noise → uniform in [-1, 1]. No periodic structure.
    let mut state: u32 = 12345;
    let mut window = Vec::with_capacity(WINDOW);
    for _ in 0..WINDOW {
        state = state.wrapping_mul(1103515245).wrapping_add(12345);
        window.push((state as i32 as f32) / (i32::MAX as f32));
    }

    let mut yin = Yin48k::new();
    let result = yin.get_pitch(&window);
    assert!(
        result.is_none(),
        "Yin48k::get_pitch must return None for white noise (no periodic structure); \
         got {result:?} — clarity threshold may be too lenient or wrapper is misconfigured"
    );
}
