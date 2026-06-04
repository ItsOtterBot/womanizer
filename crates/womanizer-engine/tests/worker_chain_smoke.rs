//! SHAPE-05 end-to-end Phase 3 chain integration test (Plan 03-04 Task 2).
//!
//! Drives 2 s of synthetic audio through the full 5-stage chain composed STANDALONE
//! (NOT via `spawn_dsp_worker` — that path requires full engine plumbing). Mirrors
//! the Phase 2 `dsp_pitch_ratio.rs` standalone-driver shape (Pattern T) — same
//! BLOCK-loop boilerplate, same per-block step convention. Three gates:
//!
//! 1. `chain_passes_audio_at_mix_one` — full chain at mix=1.0 conducts audio
//!    (steady-state output RMS > 0). All four shaping stages enabled, voiced F0
//!    (output_f0_hz=440.0) — Breathiness voicing gate fires.
//! 2. `chain_at_mix_zero_equals_dry` — at mix=0.0, output equals raw dry input
//!    bit-exact (within 1e-6). D-43 + D-47: mix=0 IS the off state.
//! 3. `chain_order_locked_to_d40` — source-grep regression guard. Reads
//!    `src/worker.rs` and asserts the five chain call sites appear in D-40 LOCKED
//!    order using a CALL-SITE regex (revision-1 W1 fix): an identifier at line
//!    start followed by `.process(` or `dry_wet_mix(`. This pattern excludes
//!    rustdoc `///` lines, `//!` inner doc lines, and prose mentions because
//!    those lines never start with a bare identifier at column 0.
//!
//! Allocations in the test BODY (Vec input/output, regex compilation) are fine —
//! only the production loop body in `worker::spawn_dsp_worker` must be alloc-free
//! (verified by `tests/dsp_assert_no_alloc_loop.rs` after Plan 03-04 Task 3
//! unignores it).

use womanizer_engine::dsp::{
    dry_wet_mix, Breathiness, BrightnessShelf, DeEsser, Stretch48k, ENGINE_SR,
};
use womanizer_engine::{Preset, BLOCK};

/// 2 s @ 48 kHz / BLOCK=256 = 375 blocks. We round to 96_000 samples = exactly
/// 375 BLOCK chunks for clean per-block iteration with no remainder.
const TOTAL_SAMPLES: usize = 96_000;
const INPUT_HZ: f32 = 440.0;
const SETTLE_SAMPLES: usize = 1024;

fn sine_input(total: usize, hz: f32) -> Vec<f32> {
    let two_pi_f_over_sr = 2.0 * std::f32::consts::PI * hz / ENGINE_SR as f32;
    (0..total)
        .map(|i| 0.5 * (two_pi_f_over_sr * i as f32).sin())
        .collect()
}

fn rms(samples: &[f32]) -> f32 {
    let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
    (sum_sq / samples.len().max(1) as f32).sqrt()
}

/// SHAPE-05: 5-stage chain conducts audio end-to-end at mix=1.0 with all stages
/// enabled. Steady-state output RMS must be non-zero (chain did not silence the
/// signal) after the signalsmith Stretch settling region is skipped.
#[test]
fn chain_passes_audio_at_mix_one() {
    // Off-RT construction of all five stages — mirrors `spawn_dsp_worker`.
    let mut stretch = Stretch48k::new(Preset::Balanced);
    stretch.set_transpose(1.0); // No pitch shift — preserve input frequency for RMS
    stretch.set_formant(1.0);
    let mut deesser = DeEsser::new(ENGINE_SR as f32);
    let mut brightness = BrightnessShelf::new(ENGINE_SR as f32);
    let mut breath = Breathiness::new(ENGINE_SR as f32, 0x12345678);

    // Stage parameter targets matching the Phase 3 ship-time defaults plus
    // boosted breathiness so the chain demonstrably modifies the signal.
    let target_sib: f32 = 0.3;
    let target_bright_db: f32 = 3.0;
    let target_breath: f32 = 0.5;
    let target_mix: f32 = 1.0; // fully wet
    let voicing_hz: f32 = 440.0; // finite — voiced; Breathiness gate fires
    let enable_sib = true;
    let enable_bright = true;
    let enable_breath = true;

    // Drive 2 s of 440 Hz sine through the full chain in BLOCK chunks.
    let input = sine_input(TOTAL_SAMPLES, INPUT_HZ);
    let mut output = vec![0f32; TOTAL_SAMPLES];

    // Pre-allocated inter-stage scratch — mirrors the worker's six [f32; BLOCK].
    let mut stretch_out = [0f32; BLOCK];
    let mut deess_out = [0f32; BLOCK];
    let mut bright_out = [0f32; BLOCK];
    let mut breath_out = [0f32; BLOCK];
    let mut processed = [0f32; BLOCK];

    for chunk_start in (0..TOTAL_SAMPLES).step_by(BLOCK) {
        let chunk_end = chunk_start + BLOCK;
        let mut scratch = [0f32; BLOCK];
        scratch.copy_from_slice(&input[chunk_start..chunk_end]);

        // D-40 chain order LOCKED.
        stretch.process(&scratch, &mut stretch_out);
        deesser.process(
            &stretch_out,
            &mut deess_out,
            target_sib,
            enable_sib,
            ENGINE_SR as f32,
        );
        brightness.process(
            &deess_out,
            &mut bright_out,
            target_bright_db,
            enable_bright,
            ENGINE_SR as f32,
        );
        breath.process(
            &bright_out,
            &mut breath_out,
            target_breath,
            enable_breath,
            voicing_hz,
        );
        dry_wet_mix(&scratch, &breath_out, target_mix, &mut processed);

        output[chunk_start..chunk_end].copy_from_slice(&processed);
    }

    // Skip the signalsmith Stretch settling window then measure steady-state RMS.
    let steady_state = &output[SETTLE_SAMPLES..];
    let steady_rms = rms(steady_state);
    assert!(
        steady_rms > 0.0,
        "5-stage chain produced zero output RMS at mix=1.0 — chain failed to conduct audio. \
         Got steady-state RMS={}",
        steady_rms
    );

    // Sanity: a 0.5-amplitude sine has theoretical RMS ≈ 0.354; with brightness
    // shelf + breath noise + de-esser at conservative values, the post-chain
    // steady-state RMS should be in the same order of magnitude (~0.1–0.5).
    assert!(
        steady_rms > 0.01,
        "5-stage chain output RMS {} is suspiciously low for a 0.5-amp sine input",
        steady_rms
    );
}

/// SHAPE-04 + D-43: at mix=0.0, output equals raw dry input bit-exact within FMA
/// rounding. Verifies dry_wet_mix's mix=0 = pure dry endpoint, and verifies that
/// all four shaping stages' state evolution does NOT leak back to `scratch` (the
/// dry endpoint). Per D-47, mix=0.0 IS the off state — no separate toggle needed.
#[test]
fn chain_at_mix_zero_equals_dry() {
    let mut stretch = Stretch48k::new(Preset::Balanced);
    stretch.set_transpose(1.0);
    stretch.set_formant(1.0);
    let mut deesser = DeEsser::new(ENGINE_SR as f32);
    let mut brightness = BrightnessShelf::new(ENGINE_SR as f32);
    let mut breath = Breathiness::new(ENGINE_SR as f32, 0x12345678);

    let target_sib: f32 = 0.5;
    let target_bright_db: f32 = 6.0;
    let target_breath: f32 = 0.8;
    let target_mix: f32 = 0.0; // fully dry
    let voicing_hz: f32 = 440.0;

    let input = sine_input(TOTAL_SAMPLES, INPUT_HZ);
    let mut output = vec![0f32; TOTAL_SAMPLES];

    let mut stretch_out = [0f32; BLOCK];
    let mut deess_out = [0f32; BLOCK];
    let mut bright_out = [0f32; BLOCK];
    let mut breath_out = [0f32; BLOCK];
    let mut processed = [0f32; BLOCK];

    for chunk_start in (0..TOTAL_SAMPLES).step_by(BLOCK) {
        let chunk_end = chunk_start + BLOCK;
        let mut scratch = [0f32; BLOCK];
        scratch.copy_from_slice(&input[chunk_start..chunk_end]);

        stretch.process(&scratch, &mut stretch_out);
        deesser.process(
            &stretch_out,
            &mut deess_out,
            target_sib,
            true,
            ENGINE_SR as f32,
        );
        brightness.process(
            &deess_out,
            &mut bright_out,
            target_bright_db,
            true,
            ENGINE_SR as f32,
        );
        breath.process(
            &bright_out,
            &mut breath_out,
            target_breath,
            true,
            voicing_hz,
        );
        dry_wet_mix(&scratch, &breath_out, target_mix, &mut processed);

        output[chunk_start..chunk_end].copy_from_slice(&processed);
    }

    // At mix=0.0, dry_wet_mix computes out = dry * 1.0 + wet * 0.0 = dry.
    // Modern f32::mul_add may introduce a small FMA rounding error in the
    // computed product; 1e-6 covers the worst case for a 0.5-amplitude input.
    for i in 0..TOTAL_SAMPLES {
        let delta = (output[i] - input[i]).abs();
        assert!(
            delta < 1e-6,
            "At mix=0.0, output must equal dry input bit-exact (within 1e-6 FMA rounding); \
             sample[{}] delta={}, output={}, input={}",
            i,
            delta,
            output[i],
            input[i]
        );
    }
}

/// D-40 chain-order LOCKED — source-grep regression guard (RESEARCH §Common
/// Pitfall 4). REVISION-1 W1 FIX: uses a call-site regex (line begins with
/// optional whitespace, then an identifier, then `.process(`). This deliberately
/// excludes comment lines (// or /// or //!) because those lines do not start
/// with a bare identifier at column 0 — eliminating false positives from
/// rustdoc prose mentioning the call sites by name.
#[test]
fn chain_order_locked_to_d40() {
    use regex::Regex;
    let src = std::fs::read_to_string("src/worker.rs")
        .expect("failed to read crates/womanizer-engine/src/worker.rs");

    // Call-site pattern: an identifier (lowercase + digits + underscore) at the
    // start of a line (after optional whitespace), immediately followed by
    // `.process(`. Comments (// /// //!) do not match because they start with
    // '/', not an identifier character.
    let call_re = Regex::new(r"(?m)^\s*([a-z_][a-zA-Z0-9_]*)\.process\(").unwrap();
    // Free-function call site: `dry_wet_mix(` at line start (after optional
    // whitespace and an optional module path like `crate::dsp::`). Excludes
    // doc-comment prose for the same reason.
    let drywet_re = Regex::new(r"(?m)^\s*(?:[A-Za-z_:][A-Za-z0-9_:]*::)?dry_wet_mix\(").unwrap();

    // Find the FIRST call-site offset for a given receiver identifier.
    let first_offset = |needle_ident: &str| -> Option<usize> {
        call_re
            .captures_iter(&src)
            .find(|cap| cap.get(1).map(|m| m.as_str()) == Some(needle_ident))
            .and_then(|cap| cap.get(0).map(|m| m.start()))
    };
    let idx_stretch = first_offset("stretch").expect("stretch.process call missing");
    let idx_deess = first_offset("deesser").expect("deesser.process call missing");
    let idx_bright = first_offset("brightness").expect("brightness.process call missing");
    let idx_breath = first_offset("breath").expect("breath.process call missing");
    let idx_drywet = drywet_re
        .find(&src)
        .map(|m| m.start())
        .expect("dry_wet_mix call missing");

    assert!(
        idx_stretch < idx_deess,
        "D-40 violation: stretch.process must precede deesser.process \
         (got stretch@{} >= deesser@{})",
        idx_stretch,
        idx_deess
    );
    assert!(
        idx_deess < idx_bright,
        "D-40 violation: deesser.process must precede brightness.process \
         (got deesser@{} >= brightness@{})",
        idx_deess,
        idx_bright
    );
    assert!(
        idx_bright < idx_breath,
        "D-40 violation: brightness.process must precede breath.process \
         (got brightness@{} >= breath@{})",
        idx_bright,
        idx_breath
    );
    assert!(
        idx_breath < idx_drywet,
        "D-40 violation: breath.process must precede dry_wet_mix \
         (got breath@{} >= dry_wet_mix@{})",
        idx_breath,
        idx_drywet
    );
}
