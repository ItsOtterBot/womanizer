//! [`run_smoke_test`] — the reusable end-to-end plumbing harness (D-12, INFRA-05).
//!
//! Instantiates every one of the nine named cross-thread primitives and shuttles synthetic
//! frames `InputRing → stub DSP → VirtualOutRing` / `MonitorOutRing`, then exercises the
//! snapshot, hot atomics, telemetry atomics, command/event channels, wake handle, and error
//! ring. No real audio device, DSP, or UI runs — Phase 0 shuttles dummy data only.
//!
//! Written as a standalone `pub fn` so a later phase can promote it to a `--selftest` binary
//! mode without rework (D-12; the subcommand itself is deferred). The `stub_dsp_callback` is
//! written *as if* it ran on the RT thread (no allocation, no lock, no channel, no log) and
//! is wrapped in [`assert_no_alloc`](assert_no_alloc::assert_no_alloc).
//!
//! In debug builds the harness ENFORCES the no-allocation contract: it snapshots
//! [`assert_no_alloc::violation_count`] before the RT-shaped region and fails loudly if the
//! counter increased after. The workspace builds `assert_no_alloc` with the `warn_debug`
//! feature, which makes a violation warn-and-continue rather than panic — so the explicit
//! before/after delta is what turns detection into a hard failure. The before snapshot
//! (rather than a reset) also keeps the result robust against unrelated prior violations
//! that may sit in the process-global counter when the harness is invoked from a test.
//!
//! In release builds the no-alloc enforcement is intentionally compiled out — `warn_debug`
//! does not activate `violation_count` in release, and `AllocDisabler` is only registered as
//! the `#[global_allocator]` under `#[cfg(debug_assertions)]`. The harness still shuttles
//! every primitive end-to-end in release; it just cannot verify the no-alloc contract there.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use assert_no_alloc::assert_no_alloc;
use crossbeam_channel::unbounded;

use crate::error_ring::{EngineError, ErrorRing};
use crate::params::VoiceParams;
use crate::primitives::{
    EngineCommand, EngineEvent, HotParams, InputRing, MonitorOutRing, Telemetry, VirtualOutRing,
    RING_CAPACITY,
};
use crate::wake::DspWakeHandle;

/// Frames per synthetic block shuttled through the rings.
const SMOKE_BLOCK: usize = 256;

/// RT-safe stub DSP callback: copies `input → out` using only stack/slice ops.
///
/// No allocation, no lock, no channel, no logging — written exactly as the real DSP callback
/// must be on the RT thread. The whole body runs inside `assert_no_alloc(|| ...)` so a future
/// allocation here would be caught by the guardrail (INFRA-03).
fn stub_dsp_callback(input: &[f32], out: &mut [f32]) {
    assert_no_alloc(|| {
        let n = input.len().min(out.len());
        out[..n].copy_from_slice(&input[..n]);
    });
}

/// Reusable harness (D-12): instantiate every named primitive and shuttle dummy data
/// end-to-end. Returns `Ok(())` when every primitive shuttled its data successfully.
///
/// A later phase can promote this to a `womanizer --selftest` binary mode unchanged.
pub fn run_smoke_test() -> anyhow::Result<()> {
    // --- rtrb rings: InputRing, VirtualOutRing, MonitorOutRing, ErrorRing ---
    let (mut in_tx, mut in_rx): InputRing = rtrb::RingBuffer::new(RING_CAPACITY);
    let (mut vo_tx, vo_rx): VirtualOutRing = rtrb::RingBuffer::new(RING_CAPACITY);
    let (mut mo_tx, mo_rx): MonitorOutRing = rtrb::RingBuffer::new(RING_CAPACITY);
    let (mut err_tx, mut err_rx) = ErrorRing::new(64);

    // --- triple_buffer snapshot: ActiveVoiceSnapshot ---
    let (mut snap_in, mut snap_out) = triple_buffer::triple_buffer(&VoiceParams::default());

    // --- hot atomics + telemetry atomics (shared as Arc) ---
    let hot = Arc::new(HotParams {
        input_gain: atomic_float::AtomicF32::new(1.0),
        gate_threshold: atomic_float::AtomicF32::new(0.0),
        bypass: std::sync::atomic::AtomicBool::new(false),
    });
    let tele = Arc::new(Telemetry {
        latency_ms: atomic_float::AtomicF32::new(0.0),
        input_rms: atomic_float::AtomicF32::new(0.0),
        xruns: std::sync::atomic::AtomicU32::new(0),
    });

    // --- off-audio-thread command/event channels ---
    let (cmd_tx, cmd_rx) = unbounded::<EngineCommand>();
    let (evt_tx, evt_rx) = unbounded::<EngineEvent>();

    // --- DSP wake handle bound to a worker thread (drive synchronously in the harness) ---
    let wake = DspWakeHandle::new(std::thread::current());

    // ---- shuttle synthetic frames: InputRing -> stub DSP -> VirtualOut + MonitorOut ----
    let frame = [0.25f32; SMOKE_BLOCK];
    in_tx
        .push_entire_slice(&frame)
        .map_err(|_| anyhow::anyhow!("input ring full pushing synthetic frame"))?;

    // Pop the block out of InputRing into a stack scratch buffer, then run the RT-safe stub
    // DSP. `read_chunk` borrows the ring's slots; `commit_all` releases them after the copy.
    let mut scratch = [0f32; SMOKE_BLOCK];
    {
        let chunk = in_rx
            .read_chunk(SMOKE_BLOCK)
            .map_err(|e| anyhow::anyhow!("input ring read_chunk failed: {e:?}"))?;
        let (a, b) = chunk.as_slices();
        scratch[..a.len()].copy_from_slice(a);
        scratch[a.len()..a.len() + b.len()].copy_from_slice(b);
        chunk.commit_all();
    }
    let mut processed = [0f32; SMOKE_BLOCK];
    // Snapshot the assert_no_alloc violation counter BEFORE the RT-shaped region. The counter
    // is process-global, so comparing the delta (rather than resetting) keeps the harness
    // robust against unrelated prior violations recorded by other tests in the same process.
    // After the region, if the counter advanced, the stub allocated inside an RT-forbidden
    // path — that is a hard failure for this harness.
    //
    // The check is gated on `debug_assertions` because `assert_no_alloc`'s `warn_debug`
    // feature only compiles `violation_count()` in debug builds (and `AllocDisabler` is
    // only registered as the `#[global_allocator]` in debug — see app/main.rs). In release
    // builds the no-alloc enforcement is intentionally compiled out, so the harness still
    // shuttles primitives end-to-end but cannot verify the no-alloc contract.
    #[cfg(debug_assertions)]
    let no_alloc_before = assert_no_alloc::violation_count();
    stub_dsp_callback(&scratch, &mut processed);
    #[cfg(debug_assertions)]
    if assert_no_alloc::violation_count() > no_alloc_before {
        anyhow::bail!(
            "stub DSP allocated inside an RT-forbidden region (violation counter advanced)"
        );
    }

    vo_tx
        .push_entire_slice(&processed)
        .map_err(|_| anyhow::anyhow!("virtual-out ring full"))?;
    mo_tx
        .push_entire_slice(&processed)
        .map_err(|_| anyhow::anyhow!("monitor-out ring full"))?;

    // Both output rings must report the full block as readable end-to-end.
    if vo_rx.slots() != SMOKE_BLOCK {
        anyhow::bail!(
            "VirtualOutRing has {} readable slots, expected {SMOKE_BLOCK}",
            vo_rx.slots()
        );
    }
    if mo_rx.slots() != SMOKE_BLOCK {
        anyhow::bail!(
            "MonitorOutRing has {} readable slots, expected {SMOKE_BLOCK}",
            mo_rx.slots()
        );
    }

    // ---- exercise + assert the remaining primitives ----

    // ActiveVoiceSnapshot (triple_buffer): write latest VoiceParams, read it back.
    let published = VoiceParams {
        pitch_semitones: 12.0,
        ..VoiceParams::default()
    };
    snap_in.write(published.clone());
    let latest = snap_out.read();
    if latest.pitch_semitones != published.pitch_semitones {
        anyhow::bail!(
            "snapshot round-trip mismatch: read {} expected {}",
            latest.pitch_semitones,
            published.pitch_semitones
        );
    }

    // HotParams atomic round-trip.
    hot.bypass.store(true, Ordering::Relaxed);
    if !hot.bypass.load(Ordering::Relaxed) {
        anyhow::bail!("HotParams.bypass did not round-trip");
    }

    // Telemetry atomic round-trip.
    tele.xruns.store(7, Ordering::Relaxed);
    if tele.xruns.load(Ordering::Relaxed) != 7 {
        anyhow::bail!("Telemetry.xruns did not round-trip");
    }

    // EngineCommand channel: send Start -> recv Start.
    cmd_tx.send(EngineCommand::Start)?;
    if cmd_rx.recv()? != EngineCommand::Start {
        anyhow::bail!("EngineCommand::Start did not round-trip on the command channel");
    }

    // EngineEvent channel: send Started -> recv Started.
    evt_tx.send(EngineEvent::Started)?;
    if evt_rx.recv()? != EngineEvent::Started {
        anyhow::bail!("EngineEvent::Started did not round-trip on the event channel");
    }

    // ErrorRing: push Xrun -> pop equal.
    err_tx
        .push(EngineError::Xrun)
        .map_err(|_| anyhow::anyhow!("error ring full pushing Xrun"))?;
    match err_rx.pop() {
        Ok(EngineError::Xrun) => {}
        other => anyhow::bail!("ErrorRing did not return Xrun, got {other:?}"),
    }

    // DspWakeHandle: prove wake() is callable (a single atomic store + unpark, no allocation).
    wake.wake();

    Ok(())
}
