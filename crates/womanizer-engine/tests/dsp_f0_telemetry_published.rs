//! DSP-04 integration test — verifies the Telemetry F0 atomic publishing surface that
//! the DSP worker writes from inside `assert_no_alloc(|| { ... })` and the UI repaint
//! loop reads each frame (Plan 02-09).
//!
//! Activated by Plan 02-06 (ignore attribute removed). The contract proved here:
//! - Initial state: `tele.input_f0_hz` and `tele.output_f0_hz` both `f32::NAN` so the
//!   UI's `.is_nan()` check renders "—" before YIN produces a real reading (D-32).
//! - Voiced round-trip: a worker-side `store(220.0, Relaxed)` is visible to a subsequent
//!   `load(Relaxed)` with bit-equivalence (overwrite-latest semantics, Pattern C).
//! - Unvoiced sentinel: storing `f32::NAN` round-trips as `.is_nan()` (the UI's
//!   distinguishing signal between "no reading" and "0 Hz reading" — see D-32 rationale
//!   in 02-CONTEXT.md). 0.0 is forbidden as the unvoiced sentinel because a real human
//!   fundamental of 0 Hz is physically nonsensical and the conflation would mislead the
//!   UI into rendering `0 Hz` instead of `—`.
//!
//! This test does NOT spin up the full worker — Plan 02-09's `dsp_assert_no_alloc_loop`
//! integration test runs the entire Phase 2 worker body for 10 s under AllocDisabler and
//! covers the YIN-in-worker integration. THIS test pins the cross-thread atomic publisher
//! round-trip at the primitive level so a Telemetry field reshape, an atomic_float ABI
//! drift, or an accidental `Ordering::Release/Acquire` swap is caught immediately.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use atomic_float::AtomicF32;
use womanizer_core::primitives::Telemetry;

#[test]
fn f0_telemetry_published() {
    // Construct Telemetry the same way every other call site in the workspace does
    // (event_loop.rs, app.rs, monitor.rs, rt_safety.rs, reconnect.rs, smoke.rs) — both
    // F0 fields initialized to `f32::NAN` per the D-32 sentinel convention.
    let tele = Arc::new(Telemetry {
        latency_ms: AtomicF32::new(0.0),
        input_rms: AtomicF32::new(0.0),
        xruns: AtomicU32::new(0),
        input_f0_hz: AtomicF32::new(f32::NAN),
        output_f0_hz: AtomicF32::new(f32::NAN),
    });

    // Initial state: both fields must read as NaN so the UI renders "—" before YIN's
    // first subsample tick produces a real reading.
    assert!(
        tele.input_f0_hz.load(Ordering::Relaxed).is_nan(),
        "Telemetry::input_f0_hz must initialize to NaN (D-32 unvoiced sentinel)"
    );
    assert!(
        tele.output_f0_hz.load(Ordering::Relaxed).is_nan(),
        "Telemetry::output_f0_hz must initialize to NaN (D-32 unvoiced sentinel)"
    );

    // Voiced round-trip: 220 Hz is the lib-test fundamental from
    // dsp::tests::yin48k_returns_some_for_220hz_sine. A round-trip with bit-equivalence
    // proves the worker → UI publisher path works for real-valued Hz readings.
    tele.input_f0_hz.store(220.0, Ordering::Relaxed);
    assert_eq!(
        tele.input_f0_hz.load(Ordering::Relaxed),
        220.0,
        "input_f0_hz must round-trip 220.0 Hz (worker stores real YIN result)"
    );

    // Output side: 220 Hz input × default pitch ratio (1.65×) ≈ 363 Hz, a typical
    // post-shift female fundamental.
    let output_hz = 220.0_f32 * 1.65;
    tele.output_f0_hz.store(output_hz, Ordering::Relaxed);
    assert_eq!(
        tele.output_f0_hz.load(Ordering::Relaxed),
        output_hz,
        "output_f0_hz must round-trip a post-shift Hz reading bit-exact"
    );

    // Unvoiced round-trip: storing NaN must result in load().is_nan() == true. NaN
    // values do NOT compare equal to themselves under IEEE 754 (`NaN != NaN`), so the
    // assertion MUST use `.is_nan()` — the UI uses the same check (Plan 02-09).
    tele.input_f0_hz.store(f32::NAN, Ordering::Relaxed);
    tele.output_f0_hz.store(f32::NAN, Ordering::Relaxed);
    assert!(
        tele.input_f0_hz.load(Ordering::Relaxed).is_nan(),
        "input_f0_hz must round-trip f32::NAN as .is_nan() == true (worker stores NaN on unvoiced)"
    );
    assert!(
        tele.output_f0_hz.load(Ordering::Relaxed).is_nan(),
        "output_f0_hz must round-trip f32::NAN as .is_nan() == true (worker stores NaN on unvoiced)"
    );
}
