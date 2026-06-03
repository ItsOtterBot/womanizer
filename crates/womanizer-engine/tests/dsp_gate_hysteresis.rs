//! DSP-02 (Plan 02-05): the [`Gate`] state machine implements D-30 hysteresis precisely:
//!
//!   - Opens when smoothed RMS crosses the OPEN threshold (−45 dBFS ≈ 0.005623).
//!   - Stays open while smoothed RMS hovers in the 5 dB hysteresis band (between −50 dBFS
//!     close and −45 dBFS open) — no chatter.
//!   - Closes only after smoothed RMS sits BELOW the CLOSE threshold (−50 dBFS ≈ 0.003162)
//!     for the full 50 ms hold-open (2400 samples).
//!
//! Drives synthetic RMS sequences across each branch; each phase is wrapped in
//! `assert_no_alloc` so the state-machine transitions stay alloc-free (D-31 — the gate is
//! on the worker hot path).
//!
//! Boilerplate mirrors `tests/rt_safety.rs`:
//!   - registers `assert_no_alloc::AllocDisabler` as the `#[global_allocator]` for THIS
//!     test binary under `#[cfg(debug_assertions)]`;
//!   - `#[serial_test::serial(no_alloc_violation_counter)]` because the violation counter
//!     is process-global and shared across every assert_no_alloc test in this binary.

#[cfg(debug_assertions)]
#[global_allocator]
static A: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

use assert_no_alloc::assert_no_alloc;
use womanizer_engine::dsp::Gate;

const BLOCK: usize = 256;
const HOLD_OPEN_SAMPLES: usize = 2400;

/// Convert a dBFS level to its linear-amplitude equivalent.
fn dbfs_linear(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

#[test]
#[serial_test::serial(no_alloc_violation_counter)]
fn gate_hysteresis() {
    assert_no_alloc::reset_violation_count();
    let before = assert_no_alloc::violation_count();

    let mut gate = Gate::new();

    // -------- Phase 1: drive raw_rms = −40 dBFS (above the −45 open threshold) for ~40
    // blocks so the 10 ms attack envelope converges. Gate must open.
    let above_open = dbfs_linear(-40.0);
    assert_no_alloc(|| {
        for _ in 0..40 {
            let _ = gate.update(above_open);
        }
    });
    assert!(
        gate.is_open(),
        "Phase 1: gate failed to open after 40 blocks of −40 dBFS (above the −45 dBFS open threshold)"
    );

    // -------- Phase 2: drop raw_rms to −47 dBFS (in the 5 dB hysteresis band — below
    // open=−45 but above close=−50). Drive for ~1 second of audio (200 * 256 = 51200
    // samples ≈ 1.067 s). Gate MUST stay open the whole time — the 5 dB gap is precisely
    // the chatter-prevention surface.
    let in_band = dbfs_linear(-47.0);
    assert_no_alloc(|| {
        for _ in 0..200 {
            let _ = gate.update(in_band);
        }
    });
    assert!(
        gate.is_open(),
        "Phase 2: gate closed mid-flight in the hysteresis band — D-30's 5 dB gap is not protecting against chatter"
    );

    // -------- Phase 3: drop raw_rms to −60 dBFS (well below the close threshold). Drive
    // for (HOLD_OPEN_SAMPLES / BLOCK) + 5 blocks so both the release envelope AND the
    // hold-open counter elapse. Gate must close.
    let below_close = dbfs_linear(-60.0);
    let close_blocks = (HOLD_OPEN_SAMPLES / BLOCK) + 5;
    assert_no_alloc(|| {
        for _ in 0..close_blocks {
            let _ = gate.update(below_close);
        }
    });
    assert!(
        !gate.is_open(),
        "Phase 3: gate failed to close after {close_blocks} blocks of −60 dBFS — hold-open window did not elapse"
    );

    // Total assert_no_alloc violation count must not have changed across any of the
    // three phases — the entire gate state machine is alloc-free.
    let after = assert_no_alloc::violation_count();
    assert_eq!(
        after, before,
        "assert_no_alloc violation count delta = {} (Gate::update must be alloc-free in every branch)",
        after - before
    );

    std::hint::black_box(&gate);
}
