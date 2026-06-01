//! Full-window first-run setup gate (UI-11, DEVICE-05, D-08).
//!
//! Per-OS install instructions are rendered via `#[cfg(target_os = ...)]` at COMPILE time
//! (D-10 — not a runtime conditional). Engine controls are NEVER addressable here; the only
//! interactive widgets are `Test detection` and `Quit`.
//!
//! ## D-11 success-flash transition
//! On a `Found` detection result, [`SetupState::flash_until`] is set to `now + 1s`. The
//! green `✓ {device_name} detected` label is shown for that full second; then `app.rs::update`
//! observes `should_transition_now` is true and constructs the engine handle + transitions
//! to Ready.

use std::time::{Duration, Instant};

use eframe::egui;
use womanizer_engine::DetectionResult;

use crate::app::SetupState;

/// Render the setup gate. Called from `app.rs::update` while in [`App::Setup`] arm.
///
/// [`App::Setup`]: crate::app::App::Setup
pub fn render(state: &mut SetupState, ctx: &egui::Context, ui: &mut egui::Ui) {
    ui.heading("Welcome to Womanizer");
    ui.label("To route audio to VRChat, Womanizer needs a virtual audio device.");
    ui.separator();

    // --- Per-OS install instructions (D-10: compile-time cfg, not runtime) ---
    #[cfg(target_os = "macos")]
    {
        ui.label("On macOS, install the Womanizer audio driver:");
        ui.code("cd drivers/macos-blackhole && ./install.sh install");
        ui.label("Then approve the Womanizer device in:");
        ui.label("System Settings → Privacy & Security");
        ui.label("System Settings → Sound → Output (Womanizer should appear)");
    }
    #[cfg(target_os = "windows")]
    {
        ui.label("On Windows, install VB-CABLE from the official download:");
        ui.hyperlink_to("Download VB-CABLE", "https://vb-audio.com/Cable/");
        ui.label("After install, Windows will register CABLE Input as a playback device.");
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        ui.label(
            "Note: Womanizer ships only for macOS Apple Silicon + Windows 10/11. This is \
             likely a developer build on an unsupported platform; detection will return \
             NotFound.",
        );
    }

    ui.separator();

    // --- Two-button row: Test detection (left) + Quit (right) ---
    ui.horizontal(|ui| {
        if ui.button("Test detection").clicked() {
            let r = womanizer_engine::detect();
            // On Found, arm the 1-second success-flash timer (D-11). The app.rs update loop
            // observes the elapsed timer next frame and transitions to Ready.
            if matches!(r, DetectionResult::Found { .. }) {
                state.flash_until = Some(Instant::now() + Duration::from_secs(1));
            } else {
                // NotFound clears any previous flash timer so the user can re-test cleanly.
                state.flash_until = None;
            }
            state.last_detection = Some(r);
        }
        if ui.button("Quit").clicked() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    });

    // --- Inline status (D-11 verbatim copy) ---
    if let Some(r) = &state.last_detection {
        match r {
            DetectionResult::Found { device_name, .. } => {
                ui.colored_label(egui::Color32::GREEN, format!("✓ {device_name} detected"));
            }
            DetectionResult::NotFound { reason } => {
                ui.colored_label(egui::Color32::RED, format!("✗ Not detected — {reason}"));
            }
        }
    }

    // --- Manual-pick fallback (D-11 escape hatch) ---
    //
    // Shown only after a failed detection so first-run users see the strict-regex path first
    // (DEVICE-05 + D-08). If the user is on a multi-cable installer, or cpal's WASAPI endpoint
    // names diverge from the friendly names shown in the Windows audio panel, they can pick
    // their virtual output device directly. The pick is moved into EngineState on transition,
    // bypassing the host-default fallback.
    if matches!(state.last_detection, Some(DetectionResult::NotFound { .. })) {
        ui.separator();
        ui.label("Or pick the virtual output device manually:");
        let outputs = womanizer_engine::enumerate_outputs();
        if outputs.is_empty() {
            ui.colored_label(
                egui::Color32::YELLOW,
                "(no output devices enumerated — cpal returned an empty list)",
            );
        } else {
            let current = state
                .picked_vout
                .clone()
                .unwrap_or_else(|| "— choose a device —".to_string());
            egui::ComboBox::from_id_salt("manual-vout-pick")
                .selected_text(current)
                .show_ui(ui, |ui| {
                    for name in &outputs {
                        ui.selectable_value(&mut state.picked_vout, Some(name.clone()), name);
                    }
                });
            ui.horizontal(|ui| {
                let can_advance = state.picked_vout.is_some();
                if ui
                    .add_enabled(can_advance, egui::Button::new("Use this device"))
                    .clicked()
                {
                    // Skip the success flash on the manual path — the user has already
                    // explicitly picked, so transition on the next frame.
                    state.flash_until = Some(Instant::now());
                }
            });
        }
    }
}
