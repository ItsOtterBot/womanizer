//! Phase 3 SC5 CPU-budget gate: 5-stage shaping chain ≤ 15% of one core at Balanced preset.
//!
//! Mirrors `tests/dsp_preset_latency_budget.rs` Pattern U (Plan 02-09). No new crate dep
//! (Option B per RESEARCH §Architectural Responsibility Map — Criterion deferred to Phase 5
//! if measured noise warrants).
//!
//! Budget math: BLOCK=256 @ 48 kHz = 5.33 ms of audio per block; 15% × 5.33 = 0.80 ms per
//! block. Phase 3's stage stack (DeEsser + BrightnessShelf + Breathiness + dry_wet_mix) is
//! well within budget per RESEARCH §Pattern 3 / 4 / 5 / 6 cost estimates — this test catches
//! regressions, not budget challenges.
//!
//! ## Standalone driver shape (revision-1 W3 acknowledgement)
//!
//! This test measures the SHAPING-STAGES-ONLY composition — it does NOT exercise
//! `spawn_dsp_worker`'s per-block YIN tick (30 Hz; ~3% of blocks have a YIN call), the
//! `SmoothedVoiceParams::step` overhead, or the `Telemetry` atomic stores. The full SC5
//! ROADMAP budget ("DSP CPU usage INCLUDING shaping stays under 15% of one core") is a
//! full-worker measurement. Three reasons the planner kept the standalone-driver shape:
//!   (i) Mirror to Phase 2 Pattern U: `dsp_preset_latency_budget.rs` uses the same shape;
//!   (ii) Driving `spawn_dsp_worker` through synthetic rings + worker handshake requires
//!        the full engine plumbing (rtrb producers/consumers, Telemetry, HotParams,
//!        EngineHandle bring-up + teardown), a significant boilerplate increase;
//!   (iii) The known per-block costs OUTSIDE the shaping chain are ~30 ns Stretch48k
//!         setters + ~30 ns Gate::update + ~30 ns SmoothedVoiceParams::step + ~3% blocks ×
//!         ~50 µs YIN tick ≈ ~1.6 µs/block average — under 0.4% of the 0.80 ms SC5 budget.
//!         The standalone-driver underestimate is bounded.
//!
//! Phase 5's perf-audit phase WILL re-verify the SC5 budget against `spawn_dsp_worker` on
//! real Windows hardware per the existing Phase 2 SC5 deferral pattern. The Plan 03-06
//! SUMMARY captures this deferral so verify-phase carries it forward.

use std::time::Instant;
use womanizer_core::VoiceParams;
use womanizer_engine::dsp::{
    dry_wet_mix, Breathiness, BrightnessShelf, DeEsser, Stretch48k, ENGINE_SR,
};
use womanizer_engine::{Preset, BLOCK};

const WARMUP_ITERS: usize = 100;
const MEASURE_ITERS: usize = 1000;
const INPUT_HZ: f32 = 440.0;
/// SC5 budget: 15% of 5.33 ms (one BLOCK at 48 kHz) = 0.80 ms per block.
const BUDGET_PER_BLOCK_MS: f32 = 0.80;

fn sine_block_iter(phase: &mut f32) -> [f32; BLOCK] {
    let two_pi_f_over_sr = 2.0 * std::f32::consts::PI * INPUT_HZ / ENGINE_SR as f32;
    let mut block = [0f32; BLOCK];
    for s in block.iter_mut() {
        *s = 0.5 * phase.sin();
        *phase += two_pi_f_over_sr;
        if *phase > 2.0 * std::f32::consts::PI {
            *phase -= 2.0 * std::f32::consts::PI;
        }
    }
    block
}

/// SC5 verification: measure median per-block CPU time of the full 5-stage shaping chain
/// at Balanced preset and assert it stays under the 0.80 ms budget.
#[test]
fn shaping_chain_per_block_cpu_under_budget() {
    let fs = ENGINE_SR as f32;
    let default_voice = VoiceParams::default();
    let mut stretch = Stretch48k::new(Preset::Balanced);
    stretch.set_transpose(default_voice.pitch_semitones_to_ratio());
    stretch.set_formant(default_voice.formant_semitones_to_ratio());
    let mut deesser = DeEsser::new(fs);
    let mut brightness = BrightnessShelf::new(fs);
    let mut breath = Breathiness::new(fs, 0x12345678);

    let mut stretch_out = [0f32; BLOCK];
    let mut deess_out = [0f32; BLOCK];
    let mut bright_out = [0f32; BLOCK];
    let mut breath_out = [0f32; BLOCK];
    let mut processed = [0f32; BLOCK];
    let mut phase = 0f32;

    // Warm-up: 100 blocks outside the timing window so envelopes settle, signalsmith fills
    // its internal STFT, CPU caches warm. Otherwise the first measured block dominates the
    // statistic with the cold-cache outlier.
    for _ in 0..WARMUP_ITERS {
        let block = sine_block_iter(&mut phase);
        stretch.process(&block, &mut stretch_out);
        deesser.process(
            &stretch_out,
            &mut deess_out,
            default_voice.sibilance_tame,
            true,
            fs,
        );
        brightness.process(
            &deess_out,
            &mut bright_out,
            default_voice.brightness_db,
            true,
            fs,
        );
        breath.process(
            &bright_out,
            &mut breath_out,
            default_voice.breathiness,
            true,
            440.0,
        );
        dry_wet_mix(&block, &breath_out, default_voice.mix, &mut processed);
    }

    // Measurement: 1000 blocks, per-block Instant snapshot.
    let mut elapsed_us = vec![0u64; MEASURE_ITERS];
    for slot in elapsed_us.iter_mut() {
        let block = sine_block_iter(&mut phase);
        let start = Instant::now();
        stretch.process(&block, &mut stretch_out);
        deesser.process(
            &stretch_out,
            &mut deess_out,
            default_voice.sibilance_tame,
            true,
            fs,
        );
        brightness.process(
            &deess_out,
            &mut bright_out,
            default_voice.brightness_db,
            true,
            fs,
        );
        breath.process(
            &bright_out,
            &mut breath_out,
            default_voice.breathiness,
            true,
            440.0,
        );
        dry_wet_mix(&block, &breath_out, default_voice.mix, &mut processed);
        *slot = start.elapsed().as_micros() as u64;
    }

    // Statistics: median + p95 from sorted per-block elapsed times.
    elapsed_us.sort();
    let median_us = elapsed_us[MEASURE_ITERS / 2] as f32;
    let p95_us = elapsed_us[(MEASURE_ITERS * 95) / 100] as f32;
    let median_ms = median_us / 1000.0;
    let p95_ms = p95_us / 1000.0;

    eprintln!(
        "shaping chain Balanced preset: median per-block = {:.3} ms (budget {:.2} ms), p95 = {:.3} ms",
        median_ms, BUDGET_PER_BLOCK_MS, p95_ms
    );

    assert!(
        median_ms < BUDGET_PER_BLOCK_MS,
        "SC5 violation: median per-block CPU {:.3} ms exceeds budget {:.2} ms ({}× over)",
        median_ms,
        BUDGET_PER_BLOCK_MS,
        median_ms / BUDGET_PER_BLOCK_MS,
    );
}

/// Per-stage diagnostic breakdown (revision-1 W3 fix). Same standalone composition, but
/// times EACH stage separately across N=200 iterations. No hard assertion — Test 1 owns
/// the budget gate; this test is diagnostic only. Informs Phase 5's SIMD-revisit decision
/// per RESEARCH §SIMD Strategy line 773.
#[test]
fn shaping_chain_per_stage_cost_breakdown() {
    let fs = ENGINE_SR as f32;
    let default_voice = VoiceParams::default();
    let mut stretch = Stretch48k::new(Preset::Balanced);
    stretch.set_transpose(default_voice.pitch_semitones_to_ratio());
    stretch.set_formant(default_voice.formant_semitones_to_ratio());
    let mut deesser = DeEsser::new(fs);
    let mut brightness = BrightnessShelf::new(fs);
    let mut breath = Breathiness::new(fs, 0x12345678);

    let mut stretch_out = [0f32; BLOCK];
    let mut deess_out = [0f32; BLOCK];
    let mut bright_out = [0f32; BLOCK];
    let mut breath_out = [0f32; BLOCK];
    let mut processed = [0f32; BLOCK];
    let mut phase = 0f32;

    // Warm-up identical to Test 1.
    for _ in 0..WARMUP_ITERS {
        let block = sine_block_iter(&mut phase);
        stretch.process(&block, &mut stretch_out);
        deesser.process(
            &stretch_out,
            &mut deess_out,
            default_voice.sibilance_tame,
            true,
            fs,
        );
        brightness.process(
            &deess_out,
            &mut bright_out,
            default_voice.brightness_db,
            true,
            fs,
        );
        breath.process(
            &bright_out,
            &mut breath_out,
            default_voice.breathiness,
            true,
            440.0,
        );
        dry_wet_mix(&block, &breath_out, default_voice.mix, &mut processed);
    }

    const STAGE_ITERS: usize = 200;
    let mut t_stretch = vec![0u64; STAGE_ITERS];
    let mut t_deess = vec![0u64; STAGE_ITERS];
    let mut t_bright = vec![0u64; STAGE_ITERS];
    let mut t_breath = vec![0u64; STAGE_ITERS];
    let mut t_drywet = vec![0u64; STAGE_ITERS];

    for i in 0..STAGE_ITERS {
        let block = sine_block_iter(&mut phase);
        let s = Instant::now();
        stretch.process(&block, &mut stretch_out);
        t_stretch[i] = s.elapsed().as_nanos() as u64;
        let s = Instant::now();
        deesser.process(
            &stretch_out,
            &mut deess_out,
            default_voice.sibilance_tame,
            true,
            fs,
        );
        t_deess[i] = s.elapsed().as_nanos() as u64;
        let s = Instant::now();
        brightness.process(
            &deess_out,
            &mut bright_out,
            default_voice.brightness_db,
            true,
            fs,
        );
        t_bright[i] = s.elapsed().as_nanos() as u64;
        let s = Instant::now();
        breath.process(
            &bright_out,
            &mut breath_out,
            default_voice.breathiness,
            true,
            440.0,
        );
        t_breath[i] = s.elapsed().as_nanos() as u64;
        let s = Instant::now();
        dry_wet_mix(&block, &breath_out, default_voice.mix, &mut processed);
        t_drywet[i] = s.elapsed().as_nanos() as u64;
    }

    let median = |v: &mut Vec<u64>| -> f32 {
        v.sort();
        v[STAGE_ITERS / 2] as f32 / 1000.0
    };
    let med_stretch = median(&mut t_stretch);
    let med_deess = median(&mut t_deess);
    let med_bright = median(&mut t_bright);
    let med_breath = median(&mut t_breath);
    let med_drywet = median(&mut t_drywet);
    let sum_med = med_stretch + med_deess + med_bright + med_breath + med_drywet;

    eprintln!("stretch:     median {:.3} µs", med_stretch);
    eprintln!("deesser:     median {:.3} µs", med_deess);
    eprintln!("brightness:  median {:.3} µs", med_bright);
    eprintln!("breath:      median {:.3} µs", med_breath);
    eprintln!("dry_wet_mix: median {:.3} µs", med_drywet);
    eprintln!("sum:         {:.3} µs", sum_med);
    // No hard assertion — Test 1 owns the budget gate. This test is diagnostic only.
}
