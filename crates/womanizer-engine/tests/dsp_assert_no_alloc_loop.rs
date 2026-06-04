//! Phase 2 capstone RT-safety gate (Pitfall #1 + DSP-01 + DSP-02 + DSP-04 + DSP-06).
//!
//! ## RESOLVED IN PLAN 03-04 TASK 3 (revision-1, 2026-06-03, user decision Option A on B2)
//!
//! **Status:** unignored, passing. Plan 03-04 Task 3 landed the YIN allocation carve-out
//! per Path 3 of the three documented fix paths (the chosen path is the most pragmatic
//! against the single-task budget; the other two paths — hand-roll YIN over a cached
//! `realfft::RealFftPlanner` (~200 LOC + new workspace dep) and fork pitch-detection
//! upstream (~30 LOC patch + workspace git override maintenance) — are stronger long-term
//! options but were over-budget for the single Task 3.
//!
//! **Path 3 (chosen):** the worker (`crates/womanizer-engine/src/worker.rs`) wraps
//! `yin.get_pitch` in `assert_no_alloc::permit_alloc(...)`. This test does the same below.
//! The carve-out is bounded: the F0 tick fires at ~30 Hz off the per-block (188 Hz)
//! Stretch hot path; one FftPlanner construction per voiced get_pitch call is two
//! `Arc<dyn Fft<f32>>` allocations per ~33 ms tick — well below the per-block budget.
//!
//! Plan 03-04 Task 3 documents the carve-out in `dsp.rs::Yin48k` doc-comment and the
//! worker.rs F0 tick site. The four Phase 3 shaping stages (DeEsser, BrightnessShelf,
//! Breathiness, dry_wet_mix) run inside the strict no-alloc block alongside Phase 2's
//! Stretch / Gate / SmoothedVoiceParams surface — proving SHAPE-05 ("pre-allocated
//! scratch + assert_no_alloc holds with full pipeline") and Phase 3 SC4 ("assert_no_alloc
//! continues to pass with the full pipeline") are empirically verified — NOT silently
//! degraded.
//!
//! **Root cause (empirical, Plan 02-09 execution):** Running the full Phase 2 worker body
//! through the 1880-iteration loop in debug surfaces allocations inside
//! `pitch_detection::detector::yin::YINDetector::get_pitch` → `internals::windowed_autocorrelation`
//! → `rustfft::FftPlanner::new()` and `plan_fft_forward()` / `plan_fft_inverse()`. These
//! planner calls heap-allocate fresh `Arc<dyn Fft<f32>>` instances on every voiced get_pitch
//! tick rather than caching the planner inside the detector. The smoke test
//! `yin48k_get_pitch_alloc_free_smoke` in `src/dsp.rs` did NOT catch this because it runs
//! `get_pitch` 100 times in a single `assert_no_alloc(|| { … })` wrap that aborts on the
//! first violation in non-debug; the full integration loop here exposes the regression.
//!
//! **Why this is NOT a Plan 02-09 bug:** the `Yin48k::new(512, 0)` wrapper, the worker's
//! subsample tick, the f0_window plumbing, and the Telemetry F0 publish are all correct
//! per D-32. The allocation surface lives in pitch-detection's own per-call FftPlanner
//! construction — fixing it requires either:
//!   1. **Hand-roll YIN per D-32 fallback** — implement Cheveigné & Kawahara 2002 §3 over
//!      a `realfft::RealFftPlanner` that we cache on `Yin48k` at construction. ~200 LOC
//!      of DSP. Cleanest long-term fix.
//!   2. **Fork pitch-detection** — patch the YIN path to cache a planner on `YINDetector`
//!      itself. Smaller diff (~30 LOC) but introduces a maintained fork; license is MIT
//!      so it's permitted.
//!   3. **`permit_alloc` wrap** — wrap the `yin.get_pitch` call site in the worker with
//!      `assert_no_alloc::permit_alloc` to silence the gate. Cheapest but defeats the
//!      RT-safety contract for the F0 tick path; only acceptable if pitch-detection
//!      publishes a fixed release we can pin to within the Phase 2 window.
//!
//! **Decision (user, 2026-06-02):** Defer to Plan 02-10. Phase 2 closes on the explicit
//! understanding that the F0 path is allocation-heavy in debug builds (debug-only stderr
//! noise from `assert_no_alloc::warn_debug`) and that Plan 02-10 owns the fix. Production
//! release builds compile out the `assert_no_alloc::AllocDisabler` global allocator
//! entirely, so user-facing impact is bounded to debug-build stderr; the actual heap
//! allocations still happen in release mode but never crash the worker — they just risk
//! occasional jitter under memory pressure, which the F0 tick (30 Hz, off the per-block
//! hot path) tolerates better than the per-block `Stretch::process` path would.
//!
//! Plan 02-10 owns the unignore. See `.planning/phases/02-dsp-core/02-09-SUMMARY.md` and
//! `STATE.md` for the deferral record propagated forward.
//!
//! ## Intended behavior (when Plan 02-10 lands)
//!
//! Drives ~10 s of synthetic audio (1880 blocks of BLOCK=256 @ 48 kHz → 9.83 s) through a
//! body that mirrors the Phase 2 `worker::spawn_dsp_worker` inner `assert_no_alloc(|| { ... })`
//! block VERBATIM: `SmoothedVoiceParams::step` → `Stretch48k::set_transpose` →
//! `Stretch48k::set_formant` → `Gate::update` on `Telemetry::input_rms` → `Stretch48k::process`
//! → conditional `processed.fill(0.0)` on gate-closed (D-29 true digital silence) → subsample
//! YIN tick at ~30 Hz (`Yin48k::get_pitch` only when `samples_since_f0 >= F0_INTERVAL_SAMPLES`)
//! → Telemetry F0 atomic stores.
//!
//! Asserts `assert_no_alloc::violation_count()` does not increase across the full 1880-iteration
//! run. This is THE Phase 2 RT-safety integration test per VALIDATION.md — it covers every
//! per-block hot-path crossing of the signalsmith-stretch FFI, the YIN F0 evaluation tick, and
//! all of the worker's stack-local arithmetic.
//!
//! Boilerplate mirrors `tests/rt_safety.rs` (the Phase 1 AUDIO-10 gate):
//!   - registers `assert_no_alloc::AllocDisabler` as the `#[global_allocator]` for THIS test
//!     binary under `#[cfg(debug_assertions)]` — the `warn_debug` feature only emits
//!     violations under debug;
//!   - `#[serial_test::serial(no_alloc_violation_counter)]` because the violation counter is
//!     process-global and any other test in this binary that resets/reads it must coordinate
//!     (same group name as rt_safety.rs);
//!   - snapshots `violation_count()` BEFORE the 10 s synthetic loop and asserts the count did
//!     not increase AFTER.
//!
//! ## Synthetic signal shape (mirrors RESEARCH §Q12 sketch)
//!
//! - Iterations 0..940 (voiced segment): 440 Hz pure sine at amplitude 0.5 — exercises both
//!   the Gate-open branch (RMS well above the −45 dBFS open threshold) AND the voiced YIN
//!   branch (sine yields a confident F0).
//! - Iterations 940..1880 (silence segment): all-zero buffer — exercises the Gate-closed
//!   branch (RMS == 0 → eventually trips close hysteresis after the 50 ms hold-open) AND the
//!   unvoiced YIN branch (zeros yield no confident F0). The `processed.fill(0.0)` D-29 write
//!   fires here.
//!
//! Together these two phases hit every code path inside the worker's inner block — that's
//! the gate. A regression that adds allocation to ANY of (Stretch::process, Stretch::set_*,
//! Yin48k::get_pitch, Gate::update, SmoothedVoiceParams::step, atomic stores, slice fills)
//! trips this test.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use assert_no_alloc::{assert_no_alloc, reset_violation_count, violation_count};
use womanizer_core::{HotParams, Preset, Telemetry, VoiceParams};
use womanizer_engine::dsp::{
    dry_wet_mix, Breathiness, BrightnessShelf, DeEsser, Gate, SmoothedVoiceParams, Stretch48k,
    Yin48k, ENGINE_SR,
};
use womanizer_engine::BLOCK;

#[cfg(debug_assertions)]
#[global_allocator]
static A: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

/// 10 s @ 48 kHz / 256-sample blocks = 1875 blocks. We round up to 1880 (= 940 voiced + 940
/// silence) so each phase is exactly half the loop and the gate-open / gate-closed branches
/// each run for ~5 s of synthetic audio.
const ITERATIONS: usize = 1880;

/// Number of iterations spent on the voiced (440 Hz sine) signal before switching to silence.
/// = ITERATIONS / 2.
const VOICED_ITERATIONS: usize = 940;

/// Subsample interval matching worker.rs's `F0_INTERVAL_SAMPLES` constant (1600 ≈ 30 Hz @
/// 48 kHz). Kept in lock-step with the worker — if a future plan retunes the F0 cadence,
/// update both sites together.
const F0_INTERVAL_SAMPLES: usize = 1600;

/// Default Phase 2 voice (D-22): pitch ≈ 1.65×, formant ≈ 1.18×. Same ratios the worker
/// boots with; SmoothedVoiceParams converges instantly because target == current.
const TARGET_PITCH_RATIO: f32 = 1.65;
const TARGET_FORMANT_RATIO: f32 = 1.18;

/// Build a `[f32; BLOCK]` chunk of a continuous 440 Hz sine at amplitude 0.5, advancing the
/// caller-owned phase in radians. Stack-local arithmetic only; no allocation.
#[inline]
fn fill_sine_block(buf: &mut [f32; BLOCK], phase: &mut f32) {
    const FREQ_HZ: f32 = 440.0;
    const AMPLITUDE: f32 = 0.5;
    const SAMPLE_RATE: f32 = 48_000.0;
    let phase_step = 2.0 * std::f32::consts::PI * FREQ_HZ / SAMPLE_RATE;
    for s in buf.iter_mut() {
        *s = AMPLITUDE * phase.sin();
        *phase += phase_step;
        if *phase > 2.0 * std::f32::consts::PI {
            *phase -= 2.0 * std::f32::consts::PI;
        }
    }
}

#[cfg(debug_assertions)]
#[test]
#[serial_test::serial(no_alloc_violation_counter)]
fn assert_no_alloc_loop() {
    // --- Pre-allocate every primitive the worker owns (worker spawn equivalent). ---
    // All `new()` calls happen BEFORE the `reset_violation_count()` snapshot below; any
    // construction-time allocation is irrelevant to the hot-path contract this test
    // validates (the worker also constructs these off-RT during spawn).
    let mut stretch = Stretch48k::new(Preset::Balanced);
    stretch.set_transpose(TARGET_PITCH_RATIO);
    stretch.set_formant(TARGET_FORMANT_RATIO);

    // Plan 03-01: SmoothedVoiceParams::new widened to take `&VoiceParams`. Build a
    // VoiceParams whose pitch_semitones/formant_semitones decode to TARGET_PITCH_RATIO and
    // TARGET_FORMANT_RATIO via the `12 * log2(ratio)` inverse of `semitones_to_ratio`.
    // Other Phase 3 shaping fields stay at VoiceParams::default() — the test still has
    // an effective constant `target == current` smoother for the four new params (this
    // test's gate is allocation-freedom, not behavior; constant smoothed values are fine).
    // The placeholder targets fed to `smoothed.step` later use the same `default_voice`
    // pattern so the smoothed values stay constant and the test continues to gate
    // allocation only.
    let default_voice = VoiceParams {
        pitch_semitones: 12.0 * TARGET_PITCH_RATIO.log2(),
        formant_semitones: 12.0 * TARGET_FORMANT_RATIO.log2(),
        ..VoiceParams::default()
    };
    let mut smoothed = SmoothedVoiceParams::new(&default_voice, BLOCK, 30.0);
    let mut gate = Gate::new();
    let mut yin = Yin48k::new();

    // Phase 3 Plan 03-04: four shaping stages constructed off-RT, mirroring the
    // worker. Seed 0x12345678 per D-50. The four shaping stages all live inside
    // the strict no-alloc block alongside the Phase 2 surface; Task 3's YIN fix
    // is what allows the assertion below to PASS.
    let mut deesser = DeEsser::new(ENGINE_SR as f32);
    let mut brightness = BrightnessShelf::new(ENGINE_SR as f32);
    let mut breath = Breathiness::new(ENGINE_SR as f32, 0x12345678);

    // Stack-allocated scratch + processed + f0_window buffers — same shape as the worker.
    let mut scratch: [f32; BLOCK] = [0.0; BLOCK];
    let mut processed: [f32; BLOCK] = [0.0; BLOCK];
    // Phase 3 Plan 03-04: four NEW inter-stage scratch buffers mirroring the
    // worker's stack layout (RESEARCH §Stack Scratch Buffer Layout).
    let mut stretch_out: [f32; BLOCK] = [0.0; BLOCK];
    let mut deess_out: [f32; BLOCK] = [0.0; BLOCK];
    let mut bright_out: [f32; BLOCK] = [0.0; BLOCK];
    let mut breath_out: [f32; BLOCK] = [0.0; BLOCK];
    let mut f0_window: [f32; 512] = [0.0; 512];
    let mut samples_since_f0: usize = 0;

    // Phase-state for the sine generator. Stack-local f32; updated each block.
    let mut sine_phase: f32 = 0.0;

    // Telemetry + HotParams: same atomic-only primitives the worker reads/writes. NaN
    // sentinels for F0 init per D-32 / Plan 02-06 contract.
    let tele = Arc::new(Telemetry {
        latency_ms: atomic_float::AtomicF32::new(0.0),
        input_rms: atomic_float::AtomicF32::new(0.0),
        xruns: std::sync::atomic::AtomicU32::new(0),
        input_f0_hz: atomic_float::AtomicF32::new(f32::NAN),
        output_f0_hz: atomic_float::AtomicF32::new(f32::NAN),
    });
    let hot = Arc::new(HotParams {
        input_gain: atomic_float::AtomicF32::new(1.0),
        gate_threshold: atomic_float::AtomicF32::new(0.0),
        bypass: AtomicBool::new(false),
        monitor_enabled: AtomicBool::new(false),
    });

    // --- Reset + snapshot the process-global allocation violation counter. ---
    // Reset prevents cross-test pollution; the before snapshot still gives delta-precision
    // diagnostics if the assertion fires.
    reset_violation_count();
    let before = violation_count();

    // --- The 1880-iteration synthetic-audio loop. ---
    for iter in 0..ITERATIONS {
        // Decide voiced vs silence phase OUTSIDE assert_no_alloc; fill scratch with the
        // appropriate signal. Both branches are O(BLOCK) stack writes — no allocation
        // either side, but keeping the branch outside the strict no-alloc block keeps the
        // mirror to the real worker (which receives raw frames from rtrb::Consumer
        // chunk.as_slices() outside its own assert_no_alloc wrap and copies into scratch
        // there).
        if iter < VOICED_ITERATIONS {
            fill_sine_block(&mut scratch, &mut sine_phase);
        } else {
            scratch.fill(0.0);
        }

        // Maintain the worker's circular 512-sample f0_window (D-32 / Plan 02-06). Shift
        // the oldest BLOCK samples out the front, copy the latest BLOCK samples in at the
        // tail. `copy_within` is `std::slice`, alloc-free.
        f0_window.copy_within(BLOCK..512, 0);
        f0_window[512 - BLOCK..].copy_from_slice(&scratch);

        // Compute the raw RMS the worker would have read from `Telemetry::input_rms`. The
        // capture callback writes this atomically; we publish here so the gate sees a
        // realistic value. (The actual capture-callback RMS uses `dsp::rms_simd` which is
        // independently tested for alloc-freedom in Plan 02-07 — replicating it here is
        // overkill; a stack-local scalar reduction suffices for the gate's input.)
        let mut sum_sq: f32 = 0.0;
        for s in scratch.iter() {
            sum_sq += s * s;
        }
        let raw_rms = (sum_sq / scratch.len() as f32).sqrt();
        tele.input_rms.store(raw_rms, Ordering::Relaxed);

        // Cache the target ratios outside the assert_no_alloc block (mirrors the worker,
        // which caches them from `snap_out.read()` before entering the wrap). In this
        // synthetic test the targets are constant — but caching matches the worker's
        // borrow-ends-before-wrap pattern documented in worker.rs.
        let target_pitch = TARGET_PITCH_RATIO;
        let target_formant = TARGET_FORMANT_RATIO;

        // --- THE STRICT NO-ALLOC HOT PATH (mirrors worker.rs Plan 03-04 reshape). ---
        // Phase 3 Plan 03-04 Task 2 extension: full 5-stage chain runs inside the
        // strict block. Targets for the four shaping stages use the D-44..D-47
        // ship-time defaults from `default_voice` (effectively constant — this
        // test gates allocation, not behavior). All enables=true.
        assert_no_alloc(|| {
            smoothed.step(
                target_pitch,
                target_formant,
                default_voice.breathiness,
                default_voice.brightness_db,
                default_voice.sibilance_tame,
                default_voice.mix,
            );
            stretch.set_transpose(smoothed.pitch());
            stretch.set_formant(smoothed.formant());

            let raw_rms_inner = tele.input_rms.load(Ordering::Relaxed);
            let gate_open = gate.update(raw_rms_inner);

            // D-40 chain order LOCKED.
            stretch.process(&scratch, &mut stretch_out);
            deesser.process(
                &stretch_out,
                &mut deess_out,
                smoothed.sibilance(),
                default_voice.sibilance_tame_enabled,
                ENGINE_SR as f32,
            );
            brightness.process(
                &deess_out,
                &mut bright_out,
                smoothed.brightness_db(),
                default_voice.brightness_enabled,
                ENGINE_SR as f32,
            );
            breath.process(
                &bright_out,
                &mut breath_out,
                smoothed.breathiness(),
                default_voice.breathiness_enabled,
                tele.output_f0_hz.load(Ordering::Relaxed),
            );

            // D-29 + D-51 gate-closed: hard zero AFTER stages, dry/wet mix on
            // gate-open. D-43 endpoints: dry=scratch (raw), wet=breath_out.
            if !gate_open {
                processed.fill(0.0);
            } else {
                dry_wet_mix(&scratch, &breath_out, smoothed.mix(), &mut processed);
            }

            // YIN subsample tick — only fires once per F0_INTERVAL_SAMPLES (≈ 30 Hz). At
            // BLOCK=256 the counter crosses every 7th iteration; across 1880 iterations
            // that's ~268 YIN calls, of which half are voiced (sine — return Some) and
            // half are unvoiced (silence — return None).
            //
            // Plan 03-04 Task 3 (Path 3, revision-1, 2026-06-03): the upstream
            // pitch-detection 0.3.0 crate re-constructs a fresh `rustfft::FftPlanner`
            // inside `windowed_autocorrelation` on every voiced get_pitch call (two
            // `Arc<dyn Fft<f32>>` allocs per call). We wrap the call in `permit_alloc`
            // exactly as the production worker does (see worker.rs F0 tick site for the
            // matching pattern + full rationale). The carve-out is documented in
            // `dsp.rs::Yin48k` doc-comment. The four Phase 3 shaping stages above run
            // OUTSIDE the carve-out and are alloc-free — proving SC4 holds.
            samples_since_f0 = samples_since_f0.saturating_add(BLOCK);
            if samples_since_f0 >= F0_INTERVAL_SAMPLES {
                samples_since_f0 = 0;
                let pitch_result =
                    assert_no_alloc::permit_alloc(|| yin.get_pitch(&f0_window));
                match pitch_result {
                    Some(f0) => {
                        tele.input_f0_hz.store(f0, Ordering::Relaxed);
                        tele.output_f0_hz
                            .store(f0 * smoothed.pitch(), Ordering::Relaxed);
                    }
                    None => {
                        tele.input_f0_hz.store(f32::NAN, Ordering::Relaxed);
                        tele.output_f0_hz.store(f32::NAN, Ordering::Relaxed);
                    }
                }
            }

            // Bypass-aware output selection mirror — the worker pushes to rtrb here. This
            // test omits the ring push (the rtrb push path is independently verified
            // alloc-free in `tests/rt_safety.rs`) and instead just chooses the slice and
            // keeps it live via `black_box` outside the closure.
            let _to_push: &[f32] = if hot.bypass.load(Ordering::Relaxed) {
                &scratch
            } else {
                &processed
            };
        });
    }

    // --- Assertion: no allocation across 1880 iterations of the full Phase 2 worker body. ---
    let after = violation_count();
    assert_eq!(
        after,
        before,
        "Phase 1+2+3 worker body allocated {} times across {} iterations of synthetic audio — \
         Pitfall #1 regression. One or more of (Stretch::process, Stretch::set_transpose/formant, \
         SmoothedVoiceParams::step, Gate::update, DeEsser::process, BrightnessShelf::process, \
         Breathiness::process, dry_wet_mix, Telemetry atomic stores, slice fills) introduced \
         a heap allocation on the hot path. Note: Yin48k::get_pitch is INTENTIONALLY excluded \
         via `permit_alloc` per Plan 03-04 Task 3 Path 3 (revision-1) — see module doc-comment.",
        after.saturating_sub(before),
        ITERATIONS
    );

    // Keep the buffers + atomics live so the optimizer cannot elide the work above.
    std::hint::black_box(&scratch);
    std::hint::black_box(&processed);
    std::hint::black_box(&stretch_out);
    std::hint::black_box(&deess_out);
    std::hint::black_box(&bright_out);
    std::hint::black_box(&breath_out);
    std::hint::black_box(&f0_window);
    std::hint::black_box(&tele);
    std::hint::black_box(&hot);
}
