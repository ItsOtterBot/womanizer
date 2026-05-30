//! rubato `FftFixedIn` wrapper at the I/O boundary — converts capture frames to/from 48 kHz.
//!
//! Populated by Plan 01-03. Runs OFF the cpal callback per D-05; all scratch buffers
//! pre-allocated via `input_buffer_allocate(true)` / `output_buffer_allocate(true)` so
//! `process_into_buffer` performs zero allocations. Yellow banner copy when active:
//! "Resampling from {native_hz} Hz → 48 kHz. A native 48 kHz device gives best quality."
