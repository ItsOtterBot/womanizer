//! AUDIO-09 reconnect integration test — exercises the
//! `error_callback → ErrorRing → drain → EngineEvent::Error(DeviceFault) → user Start → rebuild`
//! cycle without a real device disconnect.
//!
//! Pattern: skip-on-no-device for headless CI runners. The test exits 0 either by running the
//! full reconnect cycle on a hosted dev machine (default input + default output present) OR by
//! hitting the skip branch when the first `EngineCommand::Start` cannot find devices and the
//! engine emits `EngineEvent::Error(DeviceFault)` directly from `build_streams_and_worker`.
//!
//! Mapped from `01-VALIDATION.md` row AUDIO-09:
//! `cargo test -p womanizer-engine --test reconnect` exits 0.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use serial_test::serial;
use womanizer_core::{EngineCommand, EngineError, EngineEvent, HotParams, Telemetry};
use womanizer_engine::{spawn_engine, EngineState};

/// AUDIO-09 ground truth: the error → EngineEvent::Error → user-Start → rebuild path runs
/// end-to-end on a hosted dev machine; degrades to a skip on headless CI.
///
/// `#[serial]` because the Engine instantiates cpal streams; only one host-wide cpal stream
/// per device may be acceptable on some backends (CoreAudio in particular can refuse
/// concurrent exclusive streams from the same process). All Phase 1 cpal-touching
/// integration tests share the implicit `cpal_default_host` serial group.
#[test]
#[serial(cpal_default_host)]
fn error_ring_drain_emits_engine_event_then_rebuild() {
    // ---- 1. Build the engine with empty EngineState (falls back to host defaults) ----
    let hot = Arc::new(HotParams {
        input_gain: atomic_float::AtomicF32::new(1.0),
        gate_threshold: atomic_float::AtomicF32::new(0.0),
        bypass: AtomicBool::new(false),
        monitor_enabled: AtomicBool::new(false),
    });
    let tele = Arc::new(Telemetry {
        latency_ms: atomic_float::AtomicF32::new(0.0),
        input_rms: atomic_float::AtomicF32::new(0.0),
        xruns: std::sync::atomic::AtomicU32::new(0),
        input_f0_hz: atomic_float::AtomicF32::new(f32::NAN),
        output_f0_hz: atomic_float::AtomicF32::new(f32::NAN),
    });
    let handle = spawn_engine(EngineState::default(), hot, tele);

    // ---- 2. Send the first Start ----
    handle
        .cmd_tx
        .send(EngineCommand::Start)
        .expect("cmd_tx alive");

    // ---- 3. Expect either Started (dev host) or Error(DeviceFault) (headless CI) ----
    let started_or_failed = handle
        .evt_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("first Start must emit a deterministic event within 2s");

    match started_or_failed {
        EngineEvent::Error(EngineError::DeviceFault) => {
            // Headless CI path: no default audio device. The error path itself proves
            // half of AUDIO-09 — `build_streams_and_worker` failure surfaces as
            // EngineEvent::Error(DeviceFault) via the same event_loop path the
            // error_callback drain uses. Skip the rebuild leg with a diagnostic.
            eprintln!(
                "skipping reconnect rebuild leg: first Start emitted DeviceFault \
                 (no default audio device on this host — headless CI)"
            );
            drop(handle);
            return;
        }
        EngineEvent::Started => {
            // Dev host path: real audio device present. Continue to the rebuild leg.
        }
        other => {
            panic!("first Start must emit either Started or Error(DeviceFault); got {other:?}")
        }
    }

    // ---- 4. Inject a synthetic DeviceFault and assert it arrives on evt_rx ----
    handle.inject_error(EngineError::DeviceFault);
    let injected_event = handle
        .evt_rx
        .recv_timeout(Duration::from_millis(200))
        .expect("injected DeviceFault must arrive within one 50ms tick (+slack)");
    assert_eq!(
        injected_event,
        EngineEvent::Error(EngineError::DeviceFault),
        "engine must surface injected ErrorRing entries as EngineEvent::Error"
    );

    // ---- 5. Send a second Start and assert Started arrives (the rebuild leg) ----
    handle
        .cmd_tx
        .send(EngineCommand::Start)
        .expect("cmd_tx alive (reconnect Start)");

    // The engine tears down old streams + workers, rebuilds rings + atomics + streams,
    // re-spawns workers, then emits Started. On a hosted dev machine this is < 100ms;
    // grant 1s for slack across slow CI hardware.
    //
    // Note: between the inject_error event drained at step 4 and the reconnect Started,
    // the engine may surface additional faults (e.g. xruns from the brief stop/restart).
    // We tolerate intermediate Error events and assert that Started eventually arrives.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let ev = handle
            .evt_rx
            .recv_timeout(remaining)
            .expect("reconnect Start must emit Started within 2s");
        match ev {
            EngineEvent::Started => break, // success
            EngineEvent::Stopped => {
                // Expected: the rebuild path emits Stopped (silent path is silent, but the
                // engine may still emit Stopped from intermediate handle_stop calls if
                // refactored). Tolerate it.
                continue;
            }
            EngineEvent::Error(e) => {
                // Tolerated intermediate Error from leftover error-ring drain — keep
                // waiting for the eventual Started.
                eprintln!("(tolerated intermediate error during reconnect: {e:?})");
                continue;
            }
        }
    }

    // ---- 6. Clean shutdown ----
    handle
        .cmd_tx
        .send(EngineCommand::Stop)
        .expect("cmd_tx alive (cleanup Stop)");
    // Best-effort: drain any final Stopped event then drop. The engine thread exits when
    // we drop the handle (cmd_tx Disconnected).
    let _ = handle.evt_rx.recv_timeout(Duration::from_millis(500));
    drop(handle);
}
