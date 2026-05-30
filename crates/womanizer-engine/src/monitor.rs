//! Self-monitor playback stream + feedback-loop detector.
//!
//! Populated by Plan 01-03. Default OFF (D-12). The detector trips on five consecutive
//! 50 ms RMS windows each rising by ≥ 6 dB (D-13, AUDIO-08 500 ms total budget); on trip
//! it sets `HotParams::monitor_enabled` to false (D-14) and the UI shows the persistent
//! yellow banner: "Self-monitor disabled — feedback detected. Use headphones, not speakers."
