//! The eframe App state machine: `Setup` ↔ `Ready`.
//!
//! ## State machine (UI-11 + DEVICE-05 + D-08 hard gate)
//! The app launches in [`App::Setup`] — the full-window first-run setup gate. Engine
//! controls (Start/Stop/Bypass/library/editor) are UNREACHABLE until the gate clears.
//! Clicking `Test detection` runs [`womanizer_engine::detect`] (cfg-routed to the per-OS
//! detection module by Plan 01-04). On `Found`, a 1-second success flash is shown (D-11
//! auto-dismiss) and the app transitions to [`App::Ready`].
//!
//! ## Re-detection on every launch (D-09)
//! The settings table is never queried for a `virtual_device_verified` flag. The gate runs
//! detection on every Test detection click; a device that disappears between launches
//! re-blocks the user behind the gate.
//!
//! ## Ready shell (AUDIO-04 / AUDIO-06 / AUDIO-08 / AUDIO-09 wiring)
//! Once Ready, the [`ReadyState`] owns the [`EngineHandle`] returned by
//! `womanizer_engine::spawn_engine`, the [`SampleRateState`] published by the resampler,
//! and a [`MonitorBannerState`] cloned from the engine handle's `monitor_banner` field.
//! The Ready shell renders three banners (sample-rate-mismatch, feedback-detected,
//! disconnected) plus Start/Stop / latency meter / monitor checkbox.

use std::time::{Duration, Instant};

use eframe::egui;
use womanizer_engine::{
    spawn_engine, DetectionResult, EngineEvent, EngineHandle, EngineState, MonitorBannerState,
    Preset, SampleRateState,
};

/// The top-level app state. Either we're behind the setup gate, or we've cleared it and the
/// engine is alive.
pub enum App {
    /// First-run / re-launch setup gate (UI-11 + DEVICE-05). Engine controls are unreachable.
    Setup(SetupState),
    /// Ready shell — engine running, latency meter live, banners + Start/Stop available.
    Ready(ReadyState),
}

/// State held while the app is showing the setup gate.
pub struct SetupState {
    /// Last-selected mic input device name read from the SQLite settings table at launch
    /// (Phase 1: read-only; Phase 4 wires the writes). Passed into [`EngineState`] on the
    /// transition to Ready so the engine's first Start picks the same device the user used
    /// previously. `None` on a fresh-install or if the settings row is absent.
    pub last_input: Option<String>,
    /// Last-selected virtual-output device name (the matched VB-CABLE Input on Windows).
    /// Same Phase 1 read-only contract as `last_input`.
    pub last_vout: Option<String>,
    /// Last-selected self-monitor headphone device. Passed into `EngineState.selected_monitor`
    /// on Setup → Ready transition; the Ready shell's headphone dropdown can change it live
    /// via `EngineCommand::SetMonitor`. Phase 4 will wire dropdown CRUD on top of this slot.
    pub last_mon: Option<String>,
    /// Outcome of the most recent `Test detection` click. `None` before the first click.
    /// Rendered inline below the button per D-11:
    /// - `Found { device_name }` → `✓ {device_name} detected` (green).
    /// - `NotFound { reason }` → `✗ Not detected — {reason}` (red).
    pub last_detection: Option<DetectionResult>,
    /// Success-flash timer (D-11). When `Found` is observed, set to
    /// `Some(Instant::now() + 1s)`; while not yet elapsed, the green `✓ detected` label is
    /// shown. Once elapsed, the app transitions to Ready on the next `update()` call.
    pub flash_until: Option<Instant>,
    /// Manual-pick fallback (D-11 escape hatch). When strict-regex detection misses the user's
    /// virtual cable (multi-cable installer variants, divergent WASAPI endpoint names), the
    /// setup screen exposes a dropdown of all enumerated output devices. Selecting one and
    /// clicking "Use this device" stores it here and triggers the same Setup → Ready
    /// transition as a successful detect(): the engine's `selected_virtual_output` is set to
    /// this name (bypassing host-default fallback) so the user's pick reaches the cpal stream.
    pub picked_vout: Option<String>,
}

impl SetupState {
    fn new(
        last_input: Option<String>,
        last_vout: Option<String>,
        last_mon: Option<String>,
    ) -> Self {
        Self {
            last_input,
            last_vout,
            last_mon,
            last_detection: None,
            flash_until: None,
            picked_vout: None,
        }
    }
}

/// State held while the app is showing the Ready shell. Owns the live `EngineHandle` plus
/// the two banner-state publishers the engine writes and the UI reads each repaint.
pub struct ReadyState {
    /// The public engine surface (cmd_tx / evt_rx / hot / tele / monitor_banner).
    pub handle: EngineHandle,
    /// Wait-free publisher for the AUDIO-04 sample-rate-mismatch yellow banner. Plan 01-05
    /// owns this struct's lifetime; Plan 01-02b's event loop will write into it when streams
    /// are built against a non-48-kHz device. Phase 1 ships the publisher; the actual
    /// inline-resampler wire-up to write `set_mismatch(hz)` is owned by a follow-up to
    /// Plan 01-02b's `build_streams_and_worker`.
    pub sample_rate_state: SampleRateState,
    /// Twin atomic flags for the AUDIO-08 (feedback) + AUDIO-09 (disconnect) banners. Cloned
    /// from `handle.monitor_banner` at construction so the UI and engine share the same
    /// `Arc<AtomicBool>` pair — when the engine sets `disconnected = true` or the
    /// FeedbackDetector sets `feedback_detected = true`, the next repaint observes it.
    pub monitor_banner: MonitorBannerState,
    /// True while the disconnect banner has been hidden by a user reconnect click. Reset to
    /// false when a fresh `EngineEvent::Error(DeviceFault)` is drained off `evt_rx` so the
    /// banner re-appears on the next disconnect. Phase 1: the simpler "banner shows iff
    /// monitor_banner.disconnected && !disconnect_dismissed" gate avoids race conditions
    /// with the engine clearing the atomic on rebuild.
    pub disconnect_dismissed: bool,
    /// UI-side mirror of the engine's `EngineState.selected_input`. The Ready shell's input
    /// dropdown reads + writes this; on change, sends `EngineCommand::SetInput` so the
    /// engine hot-swaps. `None` means "fall back to host default" (cpal's
    /// `default_input_device`).
    pub selected_input: Option<String>,
    /// UI-side mirror of the engine's `EngineState.selected_virtual_output`. Same semantics
    /// as [`Self::selected_input`].
    pub selected_vout: Option<String>,
    /// UI-side mirror of the engine's `EngineState.selected_monitor` (headphones for the
    /// self-monitor stream). Same semantics as [`Self::selected_input`]. Monitor failures
    /// are non-fatal — `None` here means "use the host default output".
    pub selected_monitor: Option<String>,
    /// Phase 2 (Plan 02-08, D-22 / D-23) UI mirror of the active pitch multiplier. Range
    /// `1.20..=2.00` (D-23 conservative M→F sweet spot ± safety margin). Default `1.65×`
    /// (D-22, seeded from `VoiceParams::default()` semitones via `2^(st/12)`). On slider
    /// drag the new value is stored here and `publish_voice_params` writes a fresh
    /// `VoiceParams` snapshot to the DSP worker via `handle.snap_in`.
    pub pitch_slider: f32,
    /// Phase 2 (Plan 02-08, D-22 / D-23) UI mirror of the active formant multiplier. Range
    /// `1.00..=1.40` (D-23). Default `1.18×` (D-22).
    pub formant_slider: f32,
    /// Phase 2 (Plan 02-08, D-26) UI mirror of the active quality preset. Default
    /// [`Preset::Balanced`] (matches the ROADMAP's "Balanced default" and the worker's
    /// boot-time `Stretch48k::new(Preset::Balanced)`). On click the UI sends
    /// `EngineCommand::SetPreset(p)` over `cmd_tx`; the off-RT rebuild handler that
    /// actually swaps the warm `Stretch48k` instance lands in Plan 02-09.
    pub current_preset: Preset,
    /// Phase 3 (Plan 03-05, D-45) UI mirror of the breathiness amount slider. Range `0..=1.0`.
    /// Seeded from `VoiceParams::default()` (D-45 ship value 0.20). On slider drag, the new
    /// value is stored here and `publish_voice_params` writes a fresh `VoiceParams` snapshot
    /// via `handle.snap_in` (triple_buffer publish path; Pattern E — no cmd_tx for sliders).
    pub breathiness: f32,
    /// Phase 3 (Plan 03-05, D-45) UI mirror of the breathiness enable toggle. Seeded from
    /// `VoiceParams::default()` (D-45 default `true`). D-42 warm-off semantics on the worker
    /// side — when `false`, the Breathiness stage's biquad + PRNG + envelope still update
    /// each block; only the noise-add to the output is bypassed.
    pub breathiness_enabled: bool,
    /// Phase 3 (Plan 03-05, D-44) UI mirror of the brightness high-shelf gain slider. Range
    /// `-6.0..=12.0` dB. Seeded from `VoiceParams::default()` (D-44 ship value +3.0 dB).
    pub brightness_db: f32,
    /// Phase 3 (Plan 03-05, D-44) UI mirror of the brightness enable toggle. Seeded from
    /// `VoiceParams::default()` (D-44 default `true`). D-42 warm-off semantics on the worker.
    pub brightness_enabled: bool,
    /// Phase 3 (Plan 03-05, D-46) UI mirror of the sibilance-tame amount slider. Range `0..=1.0`.
    /// Seeded from `VoiceParams::default()` (D-46 ship value 0.30).
    pub sibilance_tame: f32,
    /// Phase 3 (Plan 03-05, D-46) UI mirror of the sibilance-tame enable toggle. Seeded from
    /// `VoiceParams::default()` (D-46 default `true`). D-42 warm-off semantics on the worker.
    pub sibilance_tame_enabled: bool,
    /// Phase 3 (Plan 03-05, D-47) UI mirror of the dry/wet mix slider. Range `0..=1.0`. Seeded
    /// from `VoiceParams::default()` (D-47 ship value 1.0 — fully wet). NO enable toggle per
    /// D-47 (mix=0.0 IS the off state — RESEARCH §Open Question 3).
    pub mix: f32,
}

impl ReadyState {
    fn new(
        handle: EngineHandle,
        selected_input: Option<String>,
        selected_vout: Option<String>,
        selected_monitor: Option<String>,
    ) -> Self {
        let monitor_banner = handle.monitor_banner.clone();
        let sample_rate_state = handle.sample_rate_state.clone();
        // Seed slider mirrors from VoiceParams::default() so the UI ratio matches the worker's
        // boot-time semitones (D-22 — pitch ~8.7 st → ~1.65×; formant ~2.9 st → ~1.18×).
        let defaults = womanizer_core::VoiceParams::default();
        let pitch_slider = womanizer_core::semitones_to_ratio(defaults.pitch_semitones);
        let formant_slider = womanizer_core::semitones_to_ratio(defaults.formant_semitones);
        Self {
            handle,
            sample_rate_state,
            monitor_banner,
            disconnect_dismissed: false,
            selected_input,
            selected_vout,
            selected_monitor,
            pitch_slider,
            formant_slider,
            current_preset: Preset::Balanced,
            // Phase 3 (Plan 03-05) new fields seeded from VoiceParams::default() (D-44..D-47)
            // — NOT hardcoded literals — so any future tuning to ship-time defaults in
            // womanizer-core::params propagates here automatically (Plan 02-08 discipline).
            breathiness: defaults.breathiness,
            breathiness_enabled: defaults.breathiness_enabled,
            brightness_db: defaults.brightness_db,
            brightness_enabled: defaults.brightness_enabled,
            sibilance_tame: defaults.sibilance_tame,
            sibilance_tame_enabled: defaults.sibilance_tame_enabled,
            mix: defaults.mix,
        }
    }

    /// Phase 2 (Plan 02-08, D-23 + D-35): push the current slider mirror values to the DSP
    /// worker via the lock-free `triple_buffer<VoiceParams>` writer on `EngineHandle`.
    /// Called on every slider on-change event so drags publish at the egui repaint cadence
    /// (≥ 30 Hz); the worker's [`SmoothedVoiceParams`] interpolator handles the 30 ms
    /// exponential ramp (D-35) so audible zipper noise is avoided.
    ///
    /// Pitch / formant mirror values are ratios (the slider ranges are ratios per D-23);
    /// `VoiceParams` stores semitones (D-04), so we round-trip via `12 * log2(ratio)`.
    ///
    /// Phase 3 (Plan 03-05, D-37 + D-44..D-47): the four shaping continuous params
    /// (`breathiness`, `brightness_db`, `sibilance_tame`, `mix`) plus the three enable
    /// toggles (`breathiness_enabled`, `brightness_enabled`, `sibilance_tame_enabled`)
    /// ride the same per-block `triple_buffer<VoiceParams>` snapshot path — Pattern E
    /// channel discipline preserved (no `cmd_tx` for high-frequency parameter streams).
    /// The worker's `SmoothedVoiceParams` (D-35 30 ms tau, widened by Plan 03-01 to
    /// cover the four continuous fields) prevents zipper noise on slider drags; the
    /// three enables are NOT smoothed (D-42 warm-off on the worker handles the
    /// transient). The `..VoiceParams::default()` spread at the end keeps `compensate_pitch`
    /// (D-24), `quality_preset`, and `color_tag` at their default values for Phase 3 —
    /// Phase 4's voice editor owns the persistent voice library and per-field edits.
    pub fn publish_voice_params(&self) {
        let params = womanizer_core::VoiceParams {
            pitch_semitones: 12.0 * self.pitch_slider.log2(),
            formant_semitones: 12.0 * self.formant_slider.log2(),
            breathiness: self.breathiness,
            breathiness_enabled: self.breathiness_enabled,
            brightness_db: self.brightness_db,
            brightness_enabled: self.brightness_enabled,
            sibilance_tame: self.sibilance_tame,
            sibilance_tame_enabled: self.sibilance_tame_enabled,
            mix: self.mix,
            ..womanizer_core::VoiceParams::default()
        };
        self.handle.publish_voice_params(params);
    }
}

impl App {
    /// Build a fresh `App` in the Setup state with the three last-selected device-id slots
    /// pre-populated from settings (any of them may be `None` on a fresh install).
    pub fn new(
        last_input: Option<String>,
        last_vout: Option<String>,
        last_mon: Option<String>,
    ) -> Self {
        App::Setup(SetupState::new(last_input, last_vout, last_mon))
    }

    /// Pure-state helper for the UI-11 invariant test.
    #[allow(
        dead_code,
        reason = "consumed by app::tests::setup_renders_no_engine_controls (UI-11)"
    )]
    pub fn is_setup(&self) -> bool {
        matches!(self, App::Setup(_))
    }

    /// Pure-state helper for the DEVICE-05 gate-state-machine test.
    #[allow(
        dead_code,
        reason = "consumed by app::tests::setup_renders_no_engine_controls (UI-11)"
    )]
    pub fn is_ready(&self) -> bool {
        matches!(self, App::Ready(_))
    }

    /// Pure function (no egui ctx, no clock access) the DEVICE-05 test calls to verify the
    /// transition predicate: the app should transition out of Setup iff a flash timer is
    /// set AND has elapsed. Callers pass an explicit `now: Instant` (e.g. `Instant::now()`
    /// on the live update path; a controlled value in tests) so the function is genuinely
    /// pure and the test can simulate clock states without setting `flash_until` to a value
    /// derived from `Instant::now()` (WR-05 fix).
    pub fn should_transition_now(state: &SetupState, now: Instant) -> bool {
        state.flash_until.is_some_and(|t| now >= t)
    }

    /// Drain any events sitting on `handle.evt_rx` into banner-state flags. Called once per
    /// repaint while in Ready. Best-effort — if the engine sends bursts faster than the UI
    /// repaints (unlikely at the engine's 50 ms cadence vs 30 Hz repaint), we coalesce.
    fn drain_events_into_banners(state: &mut ReadyState) {
        loop {
            match state.handle.evt_rx.try_recv() {
                Ok(EngineEvent::Started) => {
                    tracing::debug!("UI observed EngineEvent::Started");
                    // A successful Started clears any dismissed-disconnect latch — the
                    // engine has rebuilt streams; future faults should re-open the banner.
                    state.disconnect_dismissed = false;
                }
                Ok(EngineEvent::Stopped) => {
                    tracing::debug!("UI observed EngineEvent::Stopped");
                }
                Ok(EngineEvent::Error(e)) => {
                    tracing::warn!(?e, "UI observed EngineEvent::Error");
                    // A fresh DeviceFault must re-show the banner even if it was previously
                    // dismissed. The engine sets monitor_banner.disconnected = true via its
                    // ErrorRing drain — we just reset the dismissal latch here.
                    state.disconnect_dismissed = false;
                }
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    tracing::warn!("engine evt channel disconnected; engine thread exited");
                    break;
                }
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // eframe 0.34: `ui` is called once per repaint and is the sole render entrypoint.
        // The supplied `Ui` has no margin or background — this matches the spec's
        // "full-window" gate. (Wrapping in CentralPanel is unnecessary because the root
        // viewport's central area is what we get here; doing so would just add a redundant
        // panel.)
        let ctx = ui.ctx().clone();

        // Phase 1: render the current arm. The Setup → Ready transition needs to mutate
        // `self`, so we split into render-then-transition.
        match self {
            App::Setup(s) => crate::setup_screen::render(s, &ctx, ui),
            App::Ready(s) => {
                App::drain_events_into_banners(s);
                crate::ready_shell::render(s, &ctx, ui);
            }
        }

        // Phase 2: transition decision.
        match self {
            App::Setup(s) => {
                if App::should_transition_now(s, Instant::now()) {
                    // Take the device-id slots OUT of SetupState before constructing
                    // EngineState — they're moved, not cloned. A manual pick (picked_vout)
                    // overrides the last-selected slot so the user's chosen device reaches
                    // the cpal stream instead of the host default.
                    let selected_input = s.last_input.take();
                    let selected_vout = s.picked_vout.take().or_else(|| s.last_vout.take());
                    let selected_monitor = s.last_mon.take();
                    let initial_state = EngineState {
                        selected_input: selected_input.clone(),
                        selected_virtual_output: selected_vout.clone(),
                        selected_monitor: selected_monitor.clone(),
                    };
                    let handle = spawn_engine(
                        initial_state,
                        std::sync::Arc::new(default_hot_params()),
                        std::sync::Arc::new(default_telemetry()),
                    );
                    // WR-04: install an event-driven repaint callback so the engine thread
                    // can nudge the UI when banner state changes (disconnect, feedback-
                    // detected, Started/Stopped). Without this, banner toggles wait up to
                    // 33 ms for the fallback request_repaint_after tick.
                    let ctx_for_repaint = ctx.clone();
                    handle.set_repaint_callback(std::sync::Arc::new(move || {
                        ctx_for_repaint.request_repaint();
                    }));
                    *self = App::Ready(ReadyState::new(
                        handle,
                        selected_input,
                        selected_vout,
                        selected_monitor,
                    ));
                    // Repaint immediately so the Ready shell renders without a one-frame
                    // gap of stale Setup content.
                    ctx.request_repaint();
                } else if s.flash_until.is_some() {
                    // Flash timer is running — keep redrawing at the 50 ms event-loop
                    // cadence so we observe expiry promptly.
                    ctx.request_repaint_after(Duration::from_millis(50));
                }
            }
            App::Ready(_) => {
                // 30 Hz repaint keeps the latency / RMS meters live without burning CPU.
                ctx.request_repaint_after(Duration::from_millis(33));
            }
        }
    }
}

/// Construct a default-shape `HotParams` for the engine.
///
/// Phase 1 contract: input_gain = 1.0, gate_threshold = 0.0, bypass = false,
/// monitor_enabled = false (D-12 default). Mirrors the shape used in the engine's unit
/// tests (e.g. `event_loop::tests::spawn_returns_a_live_handle_that_accepts_stop`).
fn default_hot_params() -> womanizer_core::HotParams {
    womanizer_core::HotParams {
        input_gain: atomic_float::AtomicF32::new(1.0),
        gate_threshold: atomic_float::AtomicF32::new(0.0),
        bypass: std::sync::atomic::AtomicBool::new(false),
        monitor_enabled: std::sync::atomic::AtomicBool::new(false),
    }
}

/// Construct a default-shape `Telemetry` for the engine. Zero-initialized; the cpal callbacks
/// populate latency_ms / input_rms / xruns as audio flows.
fn default_telemetry() -> womanizer_core::Telemetry {
    womanizer_core::Telemetry {
        latency_ms: atomic_float::AtomicF32::new(0.0),
        input_rms: atomic_float::AtomicF32::new(0.0),
        xruns: std::sync::atomic::AtomicU32::new(0),
        input_f0_hz: atomic_float::AtomicF32::new(f32::NAN),
        output_f0_hz: atomic_float::AtomicF32::new(f32::NAN),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// UI-11 (revision: state-machine invariant). A fresh `App::new(...)` is in Setup; it
    /// is NOT in Ready. The setup-screen rendering is verified by the manual checkpoint
    /// (Task 3) — this test pins the data-model invariant that engine controls are not
    /// reachable from any state where `is_setup()` is true.
    #[test]
    fn setup_renders_no_engine_controls() {
        let app = App::new(None, None, None);
        assert!(
            app.is_setup(),
            "fresh App must start in Setup state (UI-11)"
        );
        assert!(
            !app.is_ready(),
            "fresh App must NOT be in Ready state — engine controls must be unreachable"
        );
    }

    /// DEVICE-05 (revision: state-machine invariant). Once a `Found` detection result has
    /// been observed AND the flash timer has elapsed, `should_transition_now` returns true.
    /// The actual mutation happens inside `eframe::App::update`; the predicate is
    /// independently testable without an egui context.
    #[test]
    fn gate_state_machine() {
        let mut state = SetupState::new(None, None, None);
        state.last_detection = Some(DetectionResult::Found {
            device_name: "Test".into(),
            device_id: "Test".into(),
        });
        let flash_at = Instant::now();
        state.flash_until = Some(flash_at);
        // Caller passes a `now` strictly after the flash deadline.
        let now = flash_at + Duration::from_millis(1);
        assert!(
            App::should_transition_now(&state, now),
            "transition predicate must be true once flash has elapsed (DEVICE-05)"
        );
    }

    /// D-11 (revision: state-machine invariant). A flash timer in the future blocks the
    /// transition — the green `✓ detected` label is shown for the full 1-second flash window
    /// before the Ready shell appears.
    #[test]
    fn no_transition_before_flash() {
        let mut state = SetupState::new(None, None, None);
        state.last_detection = Some(DetectionResult::Found {
            device_name: "Test".into(),
            device_id: "Test".into(),
        });
        let now = Instant::now();
        // Flash deadline well in the future relative to the `now` we pass in.
        state.flash_until = Some(now + Duration::from_secs(5));
        assert!(
            !App::should_transition_now(&state, now),
            "transition predicate must be false until the flash window elapses (D-11)"
        );
    }

    /// VALIDATION.md row AUDIO-04 / revision B1. Round-trips the SampleRateState API that
    /// the Ready shell's resample-banner widget consumes. The visual rendering is covered by
    /// the manual checkpoint; this test pins the banner-state contract that the Ready shell
    /// depends on.
    #[test]
    fn banner_on_mismatch() {
        let state = SampleRateState::new();
        assert_eq!(
            state.read(),
            None,
            "fresh SampleRateState must read as no mismatch"
        );
        state.set_mismatch(44100);
        assert_eq!(
            state.read(),
            Some(44100),
            "after set_mismatch(44100), read() must return Some(44100)"
        );
        state.clear();
        assert_eq!(state.read(), None, "after clear(), read() must return None");
    }
}
