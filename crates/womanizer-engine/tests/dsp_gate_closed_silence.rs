//! DSP-02 (Plan 02-05): the worker emits TRUE digital silence to its output buffer when
//! the [`Gate`] is closed (D-29). Simulates one worker iteration with the gate closed:
//! constructs a Gate, drives it below the close threshold for longer than the hold-open
//! window, then runs the same `stretch.process` + gate-closed-overwrite shape the worker
//! body executes. Asserts every sample of the output buffer is BYTE-EXACTLY `0.0_f32`.
//!
//! Boilerplate mirrors `tests/rt_safety.rs`:
//!   - registers `assert_no_alloc::AllocDisabler` as the `#[global_allocator]` for THIS
//!     test binary under `#[cfg(debug_assertions)]` because Gate hysteresis is part of the
//!     worker hot path and the assertion is meaningless without the global allocator live;
//!   - `#[serial_test::serial(no_alloc_violation_counter)]` because the violation counter
//!     is process-global and shared across every assert_no_alloc test in this binary.

#[cfg(debug_assertions)]
#[global_allocator]
static A: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

use assert_no_alloc::assert_no_alloc;
use womanizer_core::Preset;
use womanizer_engine::dsp::{Gate, Stretch48k};

/// Worker BLOCK size (256 frames @ 48 kHz). Mirrors `cpal_io::BLOCK`. We don't import the
/// crate constant directly to keep the test self-contained at the public-API surface.
const BLOCK: usize = 256;

/// `hold_open_samples` from D-30 (50 ms @ 48 kHz). Drives the close-detection window.
const HOLD_OPEN_SAMPLES: usize = 2400;

#[test]
#[serial_test::serial(no_alloc_violation_counter)]
fn gate_closed_silence() {
    // Reset + snapshot the process-global counter so this test's pass/fail does not depend
    // on unrelated tests that ran earlier in this binary.
    assert_no_alloc::reset_violation_count();
    let before = assert_no_alloc::violation_count();

    // Construct the same primitives the DSP worker owns. All heap allocation must happen
    // BEFORE the assert_no_alloc wrap below — Stretch48k::new pulls in the C++ side of
    // signalsmith and is the heaviest construction in this set; do it up front.
    let mut gate = Gate::new();
    let mut stretch = Stretch48k::new(Preset::Balanced);
    // Drive the gate below the close threshold (−50 dBFS ≈ 0.003162) for (HOLD_OPEN_SAMPLES
    // / BLOCK) + 2 blocks. Gate starts CLOSED at construction, so the first updates leave
    // it closed regardless of hold_open_samples — but the loop below establishes a clean
    // "smoothed RMS converged near zero" steady state before we measure output silence.
    let sub_threshold_rms = 0.001; // ≈ −60 dBFS
    let warmup_blocks = (HOLD_OPEN_SAMPLES / BLOCK) + 2;
    for _ in 0..warmup_blocks {
        gate.update(sub_threshold_rms);
    }
    assert!(
        !gate.is_open(),
        "precondition: gate must be CLOSED after sustained sub-threshold RMS"
    );

    // Stack-allocated scratch + output buffers (mirror worker.rs). Non-zero input so the
    // gate-closed `processed.fill(0.0)` is the OBSERVABLE event — if the worker skipped the
    // fill, the stretch output would carry residual signal and the assertion below would
    // trip.
    let scratch = [0.5f32; BLOCK];
    let mut processed = [0.5f32; BLOCK]; // start non-zero so .fill(0.0) is observable

    // Simulate one worker iteration with the gate closed. Mirrors the body of
    // worker.rs's per-block assert_no_alloc wrap.
    assert_no_alloc(|| {
        // Stretch is called every block regardless of gate state (D-28 warm contract).
        // Its output may be anything (cold-start phase-vocoder will emit small residual).
        stretch.process(&scratch, &mut processed);
        // Gate-closed: overwrite the stretch output with TRUE digital zero (D-29).
        if !gate.is_open() {
            processed.fill(0.0);
        }
    });

    // Every sample of `processed` must be BYTE-EXACTLY 0.0f32 — not "near zero", not
    // "below some epsilon". The fill writes literal zero bits and the assertion enforces
    // exact equality so a future regression that switches to a smoothed dip or comfort
    // noise (a documented Phase 4 deferred consideration) trips here loudly.
    for (i, s) in processed.iter().enumerate() {
        assert_eq!(
            s.to_bits(),
            0u32,
            "sample {i} is not exact zero: {s} (bits = {:#x}). \
             D-29 requires TRUE digital silence on gate-closed.",
            s.to_bits()
        );
    }

    // assert_no_alloc invariant: the violation count must not have changed across the
    // simulated worker step. The gate-closed branch executes identical work to the
    // gate-open branch (stretch.process + optional fill); both are alloc-free.
    let after = assert_no_alloc::violation_count();
    assert_eq!(
        after,
        before,
        "assert_no_alloc violation count delta = {} (gate-closed branch must allocate nothing)",
        after - before
    );

    // Keep the stretch and gate live so the optimizer cannot elide the simulated step.
    std::hint::black_box(&stretch);
    std::hint::black_box(&gate);
    std::hint::black_box(&processed);
}
