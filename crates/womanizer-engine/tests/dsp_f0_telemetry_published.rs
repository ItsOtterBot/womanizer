//! Wave 0 STUB — requirement DSP-04; filled in by Plan 02-06.
//!
//! Goal of the real test (per RESEARCH §Q12 / VALIDATION.md Wave 0 Requirements list):
//! after driving a voiced segment through the DSP worker, assert that the values written
//! to `Telemetry::input_f0_hz` / `Telemetry::output_f0_hz` (AtomicF32, Plan 02-02) match
//! the YIN-estimated input frequency and the formant-shifted output frequency within the
//! ±2 Hz accuracy budget. Validates the worker → UI cross-thread publishing path: the UI
//! repaint loop reads these atomics each frame (Plan 02-09) and renders `—` on `.is_nan()`
//! or `f Hz` on a valid reading.
//!
//! Pattern: mirrors the atomic-publisher round-trip pattern in
//! `crates/womanizer-engine/src/resampler.rs` (lines 316-336) — same `tele.field.store(v,
//! Relaxed)` / `tele.field.load(Relaxed)` round-trip with NaN-sentinel semantics for the
//! unvoiced branch. PATTERNS.md identifies the resampler as the canonical analog for
//! cross-thread atomic publishing tests.
//!
//! No `assert_no_alloc::AllocDisabler` registration in THIS file — Telemetry stores are
//! `Ordering::Relaxed` atomic writes; their alloc-freedom is part of `dsp_assert_no_alloc_loop`.
//!
//! When Plan 02-06 fills in the body it MUST remove the `#[ignore]` attribute below.

// `use` keeps the cross-thread Telemetry primitive live so a future rename or move breaks
// this stub at compile time even before Plan 02-06 fills in the body.
#[allow(unused_imports)]
use womanizer_core::primitives::Telemetry;

#[test]
#[ignore = "stub — filled in by Plan 02-06"]
fn f0_telemetry_published() {
    todo!("Plan 02-06 fills in the body — see RESEARCH §Q12 sketch (drive voiced segment → assert Telemetry::input_f0_hz / output_f0_hz round-trip from worker → UI within ±2 Hz; NaN on unvoiced)");
}
