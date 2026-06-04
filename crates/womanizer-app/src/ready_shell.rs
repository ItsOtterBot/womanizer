//! Ready shell — engine surface for Phase 1 (AUDIO-04 / 06 / 08 / 09 + D-12 monitor) plus
//! the Phase 2 (Plan 02-08) pitch / formant / preset / F0 readout additions.
//!
//! Renders, in vertical order:
//! 1. Three banner blocks (each gated on its predicate):
//!    - Sample-rate-mismatch (AUDIO-04, D-05 verbatim copy).
//!    - Feedback-detected (AUDIO-08, D-14 verbatim copy + `×` dismiss).
//!    - Disconnect (AUDIO-09, D-07 verbatim copy + click-to-reconnect button).
//! 2. App header + device row (input device name from `enumerate_inputs`).
//! 3. Device pickers (input mic, virtual output, monitor).
//! 4. Monitor checkbox (D-12 default OFF; inline label verbatim "Self-monitor (headphones only)").
//! 5. Phase 2: pitch slider (`1.20..=2.00`) + formant slider (`1.00..=1.40`) — D-23 + D-35;
//!    slider on-change publishes via `state.publish_voice_params()` → `EngineHandle::snap_in`.
//! 6. Start / Stop horizontal row → sends `EngineCommand::Start` / `Stop` on `cmd_tx`.
//! 7. Phase 2: three-button preset row (`[Low latency] [Balanced] [Quality]`) — D-26;
//!    clicks send `EngineCommand::SetPreset(p)` over `cmd_tx`.
//! 8. Live meters: latency_ms, input_rms, xruns — all read at the 30 Hz repaint cadence.
//! 9. Phase 2: F0 readout (`Pitch: <input_hz> → <output_hz>`) — D-32 + D-33; renders
//!    "—" when YIN reports unvoiced (NaN sentinel).
//!
//! Phase 4 will add the voice library editor; this is the minimal surface that lets the
//! developer (and the manual checkpoint verifier) prove the end-to-end mic → virtual device
//! → VRChat path works with the Phase 2 DSP affordances.

use std::sync::atomic::Ordering;

use eframe::egui;
use womanizer_engine::{
    enumerate_inputs, enumerate_outputs, render_resample_banner, EngineCommand, Preset,
    DISCONNECT_BANNER_COPY, FEEDBACK_BANNER_COPY,
};

use crate::app::ReadyState;

/// Render the Ready shell. Called from `app.rs::update` while in `App::Ready` arm.
pub fn render(state: &mut ReadyState, _ctx: &egui::Context, ui: &mut egui::Ui) {
    // -------- Three banner blocks --------

    // (a) Sample-rate-mismatch yellow banner (AUDIO-04, D-05 verbatim).
    // Predicate: SampleRateState::read() returns Some(native_hz).
    if let Some(hz) = state.sample_rate_state.read() {
        ui.colored_label(egui::Color32::YELLOW, render_resample_banner(hz));
    }

    // (b) Feedback-detected yellow banner (AUDIO-08, D-14 verbatim) with `×` dismiss.
    // Predicate: monitor_banner.is_feedback_detected().
    // On `×` click: clear the flag (allows the detector to re-arm via the false→true edge of
    // monitor_enabled). The user can also re-enable the monitor toggle to clear; we expose
    // the explicit dismiss so the banner doesn't linger after the user has acknowledged it.
    if state.monitor_banner.is_feedback_detected() {
        ui.horizontal(|ui| {
            ui.colored_label(egui::Color32::YELLOW, FEEDBACK_BANNER_COPY);
            if ui.button("×").clicked() {
                state.monitor_banner.clear_feedback_detected();
            }
        });
    }

    // (c) Disconnect yellow banner (AUDIO-09, D-07 verbatim) with click-to-reconnect.
    // Predicate: monitor_banner.is_disconnected() && !disconnect_dismissed.
    // The dismiss flag prevents the banner from re-appearing on the same fault after the
    // user clicks reconnect — App::drain_events_into_banners resets it on a fresh
    // EngineEvent::Error(DeviceFault) arrival.
    if state.monitor_banner.is_disconnected() && !state.disconnect_dismissed {
        ui.horizontal(|ui| {
            ui.colored_label(egui::Color32::YELLOW, DISCONNECT_BANNER_COPY);
            if ui.button("Click to reconnect").clicked() {
                // Send a fresh Start — the engine's idempotent reconnect path drops old
                // streams + workers, then rebuilds (matches AUDIO-09 contract).
                let _ = state.handle.cmd_tx.send(EngineCommand::Start);
                // Dismiss the banner immediately — the engine will clear
                // monitor_banner.disconnected on successful rebuild (via
                // build_streams_and_worker's W8 wiring). If the rebuild fails, the
                // engine will re-set the flag and a fresh Error event will reset
                // disconnect_dismissed.
                state.disconnect_dismissed = true;
            }
        });
    }

    ui.separator();

    // -------- App header --------
    ui.heading("Womanizer");

    // -------- Device pickers (input mic + virtual output) --------
    //
    // Picker changes send `EngineCommand::SetInput` / `SetVirtualOutput` on `cmd_tx`. The
    // engine event loop updates its in-memory `EngineState` and, if streams are running,
    // tears down + rebuilds atomically so the new device takes effect without a Stop click.
    // Phase 4 will replace this with persistent dropdowns backed by the settings table.

    ui.horizontal(|ui| {
        ui.label("Input mic:");
        let inputs = enumerate_inputs();
        let current = state
            .selected_input
            .clone()
            .unwrap_or_else(|| "(host default)".to_string());
        let mut new_pick: Option<Option<String>> = None;
        egui::ComboBox::from_id_salt("ready-input-pick")
            .selected_text(current)
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(state.selected_input.is_none(), "(host default)")
                    .clicked()
                {
                    new_pick = Some(None);
                }
                for name in &inputs {
                    if ui
                        .selectable_label(
                            state.selected_input.as_deref() == Some(name.as_str()),
                            name,
                        )
                        .clicked()
                    {
                        new_pick = Some(Some(name.clone()));
                    }
                }
            });
        if let Some(p) = new_pick {
            state.selected_input = p.clone();
            let _ = state.handle.cmd_tx.send(EngineCommand::SetInput(p));
        }
    });

    ui.horizontal(|ui| {
        ui.label("Virtual output:");
        let outputs = enumerate_outputs();
        let current = state
            .selected_vout
            .clone()
            .unwrap_or_else(|| "(host default)".to_string());
        let mut new_pick: Option<Option<String>> = None;
        egui::ComboBox::from_id_salt("ready-vout-pick")
            .selected_text(current)
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(state.selected_vout.is_none(), "(host default)")
                    .clicked()
                {
                    new_pick = Some(None);
                }
                for name in &outputs {
                    if ui
                        .selectable_label(
                            state.selected_vout.as_deref() == Some(name.as_str()),
                            name,
                        )
                        .clicked()
                    {
                        new_pick = Some(Some(name.clone()));
                    }
                }
            });
        if let Some(p) = new_pick {
            state.selected_vout = p.clone();
            let _ = state.handle.cmd_tx.send(EngineCommand::SetVirtualOutput(p));
        }
    });

    ui.horizontal(|ui| {
        ui.label("Monitor (headphones):");
        let outputs = enumerate_outputs();
        let current = state
            .selected_monitor
            .clone()
            .unwrap_or_else(|| "(host default)".to_string());
        let mut new_pick: Option<Option<String>> = None;
        egui::ComboBox::from_id_salt("ready-monitor-pick")
            .selected_text(current)
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(state.selected_monitor.is_none(), "(host default)")
                    .clicked()
                {
                    new_pick = Some(None);
                }
                for name in &outputs {
                    if ui
                        .selectable_label(
                            state.selected_monitor.as_deref() == Some(name.as_str()),
                            name,
                        )
                        .clicked()
                    {
                        new_pick = Some(Some(name.clone()));
                    }
                }
            });
        if let Some(p) = new_pick {
            state.selected_monitor = p.clone();
            let _ = state.handle.cmd_tx.send(EngineCommand::SetMonitor(p));
        }
    });

    // -------- Monitor checkbox (D-12 verbatim inline label) --------
    let mut mon = state.handle.hot.monitor_enabled.load(Ordering::Relaxed);
    if ui
        .checkbox(&mut mon, "Self-monitor (headphones only)")
        .changed()
    {
        state
            .handle
            .hot
            .monitor_enabled
            .store(mon, Ordering::Relaxed);
    }

    // -------- Phase 2 Plan 02-08 INSERTION 1: pitch + formant sliders (above Start/Stop) --
    //
    // Slider on-change publishes via `state.publish_voice_params()` which writes a fresh
    // `VoiceParams` snapshot through `EngineHandle::snap_in` (triple_buffer Input). This is
    // a high-frequency parameter stream per Pattern E — slider drags MUST NOT route through
    // `cmd_tx` (the bounded channel discipline; cmd_tx is for discrete commands only).
    // Ranges are conservative M→F sweet spot per D-23; SmoothedVoiceParams (30 ms ramp, D-35)
    // on the worker side prevents zipper noise (CONTEXT Pitfall #7 mitigation).
    ui.horizontal(|ui| {
        ui.label("Pitch:");
        let mut pitch = state.pitch_slider;
        if ui
            .add(egui::Slider::new(&mut pitch, 1.20..=2.00).text("×"))
            .changed()
        {
            state.pitch_slider = pitch;
            state.publish_voice_params();
        }
    });
    ui.horizontal(|ui| {
        ui.label("Formant:");
        let mut formant = state.formant_slider;
        if ui
            .add(egui::Slider::new(&mut formant, 1.00..=1.40).text("×"))
            .changed()
        {
            state.formant_slider = formant;
            state.publish_voice_params();
        }
    });

    // -------- Start / Stop row --------
    ui.horizontal(|ui| {
        if ui.button("Start").clicked() {
            let _ = state.handle.cmd_tx.send(EngineCommand::Start);
        }
        if ui.button("Stop").clicked() {
            let _ = state.handle.cmd_tx.send(EngineCommand::Stop);
        }
    });

    // -------- Phase 2 Plan 02-08 INSERTION 2: preset segmented row (below Start/Stop) ------
    //
    // Three-button segmented row per D-26 verbatim copy ("Low latency" / "Balanced" /
    // "Quality"). Clicks send `EngineCommand::SetPreset(p)` over `cmd_tx` (discrete command,
    // off-RT path) — the engine event-loop's SetPreset handler (Plan 02-09) constructs a
    // fresh `Stretch48k` off-RT and hands it to the DSP worker via a bounded swap channel.
    // The current selection is highlighted via egui's `SelectableLabel` state.
    ui.horizontal(|ui| {
        for (preset, label) in [
            (Preset::Low, "Low latency"),
            (Preset::Balanced, "Balanced"),
            (Preset::Quality, "Quality"),
        ] {
            if ui
                .selectable_label(state.current_preset == preset, label)
                .clicked()
            {
                state.current_preset = preset;
                let _ = state.handle.cmd_tx.send(EngineCommand::SetPreset(preset));
            }
        }
    });

    // -------- Phase 3 Plan 03-05: shaping toggle + slider rows (D-37 layout, D-44..D-47) --
    //
    // Inserted BETWEEN the preset segmented row (above) and the live meters block (below)
    // per D-37. Each row: [☐ enable] [stage name] [slider] [value label]. On .changed() the
    // row updates state and calls publish_voice_params() — slider drags ride the existing
    // triple_buffer<VoiceParams> publish path (Pattern E channel discipline preserved —
    // never cmd_tx for high-frequency parameter streams). The DSP worker's
    // SmoothedVoiceParams (D-35 30 ms tau, widened by Plan 03-01 to cover the four new
    // continuous params) prevents zipper noise. The three bool enables are NOT smoothed —
    // D-42 warm-off on the worker handles the transient.
    //
    // D-47 note: dry/wet has NO toggle (mix=0 IS off). Three checkbox rows + one
    // toggle-less row. Phase 4's voice editor (VOICE-03) absorbs these widgets directly;
    // Phase 3 ships them as the temporary in-place tuning row matching the D-23/D-26
    // precedent (CONTEXT D-36).
    //
    // Slider value labels per D-38: plain 0–1 for breathiness / sibilance / dry-wet
    // (text(""), no unit), "dB" for brightness — UI self-documents against REQUIREMENTS.md.

    // Breathiness row (D-45 default 0.20 ON; range 0..=1.0)
    ui.horizontal(|ui| {
        let mut enabled = state.breathiness_enabled;
        if ui.checkbox(&mut enabled, "Breathiness").changed() {
            state.breathiness_enabled = enabled;
            state.publish_voice_params();
        }
        let mut amount = state.breathiness;
        if ui
            .add(egui::Slider::new(&mut amount, 0.0..=1.0).text(""))
            .changed()
        {
            state.breathiness = amount;
            state.publish_voice_params();
        }
    });

    // Brightness row (D-44 default +3.0 dB ON; range -6.0..=12.0 dB)
    ui.horizontal(|ui| {
        let mut enabled = state.brightness_enabled;
        if ui.checkbox(&mut enabled, "Brightness").changed() {
            state.brightness_enabled = enabled;
            state.publish_voice_params();
        }
        let mut db = state.brightness_db;
        if ui
            .add(egui::Slider::new(&mut db, -6.0..=12.0).text("dB"))
            .changed()
        {
            state.brightness_db = db;
            state.publish_voice_params();
        }
    });

    // Sibilance-tame row (D-46 default 0.30 ON; range 0..=1.0)
    ui.horizontal(|ui| {
        let mut enabled = state.sibilance_tame_enabled;
        if ui.checkbox(&mut enabled, "Sibilance-tame").changed() {
            state.sibilance_tame_enabled = enabled;
            state.publish_voice_params();
        }
        let mut amount = state.sibilance_tame;
        if ui
            .add(egui::Slider::new(&mut amount, 0.0..=1.0).text(""))
            .changed()
        {
            state.sibilance_tame = amount;
            state.publish_voice_params();
        }
    });

    // Dry/Wet row (D-47 default 1.0; range 0..=1.0; NO toggle — mix=0 IS off)
    ui.horizontal(|ui| {
        ui.label("Dry/Wet:");
        let mut mix = state.mix;
        if ui
            .add(egui::Slider::new(&mut mix, 0.0..=1.0).text(""))
            .changed()
        {
            state.mix = mix;
            state.publish_voice_params();
        }
    });

    // -------- Live meters (read each repaint at 30 Hz) --------
    ui.label(format!(
        "Latency: {:.1} ms",
        state.handle.tele.latency_ms.load(Ordering::Relaxed)
    ));
    ui.label(format!(
        "Input RMS: {:.3}",
        state.handle.tele.input_rms.load(Ordering::Relaxed)
    ));
    ui.label(format!(
        "Xruns: {}",
        state.handle.tele.xruns.load(Ordering::Relaxed)
    ));

    // -------- Phase 2 Plan 02-08 INSERTION 3: F0 readout (below Xruns) -------------------
    //
    // D-33 verbatim copy "Pitch: <input> → <output>" — input + output F0 in Hz. The
    // DSP worker writes `Telemetry::input_f0_hz` / `output_f0_hz` at ~30 Hz from YIN
    // (Plan 02-06); read each repaint with Relaxed ordering. D-32 unvoiced sentinel:
    // YIN reports NaN when no confident pitch is detected → render "—" not "0 Hz" (0 Hz
    // is physically nonsensical for a human voice; "—" is the user-facing convention).
    let fmt_hz = |hz: f32| -> String {
        if hz.is_nan() {
            "—".to_string()
        } else {
            format!("{hz:.0} Hz")
        }
    };
    ui.label(format!(
        "Pitch: {} → {}",
        fmt_hz(state.handle.tele.input_f0_hz.load(Ordering::Relaxed)),
        fmt_hz(state.handle.tele.output_f0_hz.load(Ordering::Relaxed)),
    ));
}
