//! Ready shell — the minimal Phase 1 engine surface (AUDIO-04 / 06 / 08 / 09 + D-12 monitor).
//!
//! Renders, in vertical order:
//! 1. Three banner blocks (each gated on its predicate):
//!    - Sample-rate-mismatch (AUDIO-04, D-05 verbatim copy).
//!    - Feedback-detected (AUDIO-08, D-14 verbatim copy + `×` dismiss).
//!    - Disconnect (AUDIO-09, D-07 verbatim copy + click-to-reconnect button).
//! 2. App header + device row (input device name from `enumerate_inputs`).
//! 3. Monitor checkbox (D-12 default OFF; inline label verbatim "Self-monitor (headphones only)").
//! 4. Start / Stop horizontal row → sends `EngineCommand::Start` / `Stop` on `cmd_tx`.
//! 5. Live meters: latency_ms, input_rms, xruns — all read at the 30 Hz repaint cadence.
//!
//! Phase 4 will add the input/virtual-output/monitor device dropdowns + the voice library
//! editor; this is the Phase 1 minimal surface that lets the developer (and the manual
//! checkpoint verifier) prove the end-to-end mic → virtual device → VRChat path works.

use std::sync::atomic::Ordering;

use eframe::egui;
use womanizer_engine::{
    enumerate_inputs, enumerate_outputs, EngineCommand, DISCONNECT_BANNER_COPY,
    FEEDBACK_BANNER_COPY, RESAMPLE_BANNER_TEMPLATE,
};

use crate::app::ReadyState;

/// Render the Ready shell. Called from `app.rs::update` while in `App::Ready` arm.
pub fn render(state: &mut ReadyState, _ctx: &egui::Context, ui: &mut egui::Ui) {
    // -------- Three banner blocks --------

    // (a) Sample-rate-mismatch yellow banner (AUDIO-04, D-05 verbatim).
    // Predicate: SampleRateState::read() returns Some(native_hz).
    if let Some(hz) = state.sample_rate_state.read() {
        ui.colored_label(
            egui::Color32::YELLOW,
            RESAMPLE_BANNER_TEMPLATE.replace("{}", &hz.to_string()),
        );
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

    // -------- Start / Stop row --------
    ui.horizontal(|ui| {
        if ui.button("Start").clicked() {
            let _ = state.handle.cmd_tx.send(EngineCommand::Start);
        }
        if ui.button("Stop").clicked() {
            let _ = state.handle.cmd_tx.send(EngineCommand::Stop);
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
}
