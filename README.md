# Womanizer

> Windows-only as of 2026-06-01 (VRChat does not ship a native macOS client).

A 100% local, CPU-only, real-time male→female voice converter for VRChat on Windows 10/11.
Independent pitch + formant shifting via [signalsmith-stretch](https://github.com/Signalsmith-Audio/signalsmith-stretch),
plus breathiness / brightness / de-essing shaping. Imperceptible conversation latency,
zero GPU contention, fully offline. An OtterBot product, distributed free.

See `.planning/PROJECT.md` for the full product definition, requirements, and architectural
constraints. Locked tech-stack pin rationale is documented in this `README.md` and inline in
`Cargo.toml`.

## Build Prerequisites (Windows)

Womanizer is a pure-Rust workspace, but one transitive build dependency requires native
tooling beyond a stock Rust install. **Before running `cargo build` or `cargo test` on a
clean Windows checkout, install LLVM/Clang.**

**The fastest path: run the bundled setup script.** From the repo root, in any PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\setup-windows.ps1
```

The script is idempotent — it detects an existing LLVM install and exits cleanly. If
LLVM isn't present, it prefers `winget` (no admin required) and falls back to `choco`.
After it finishes, **close and reopen your terminal** so the new `PATH` entries become
visible, then proceed with `cargo build --release`.

**Why this is needed:** [`signalsmith-stretch 0.1.3`](https://crates.io/crates/signalsmith-stretch)
pulls in `bindgen ^0.70` as a build-dependency. `bindgen` parses C++ headers and generates
Rust FFI bindings by invoking `libclang.dll` at build time. Visual Studio Build Tools and
the MSVC toolchain alone do not ship `libclang.dll` — only the LLVM toolchain does.
Without it, the first clean build fails with `error: Unable to find libclang`.
(This is the canonical bindgen-on-Windows pitfall — RESEARCH §Pitfall 3.)

**Manual install (if you'd rather not run the script):**

- **winget:**
  ```powershell
  winget install LLVM.LLVM
  ```
- **Chocolatey:**
  ```powershell
  choco install llvm
  ```
- **Or the official LLVM Windows installer** (LLVM 16 or newer) from
  [releases.llvm.org](https://releases.llvm.org/). Pick the
  `LLVM-x.y.z-win64.exe` artifact and select "Add LLVM to the system PATH" during install.

CI installs LLVM automatically on the Windows runner via `choco install llvm` (see
`.github/workflows/ci.yml`); local Windows workstations need the one-time install above.

## Building

Once LLVM is installed:

```bash
cargo build --release
```

The first clean build adds ~5–10 seconds for the `signalsmith-stretch` C++ compile;
subsequent incremental builds are unaffected. All other dependencies (cpal, rubato,
rusqlite with `bundled`, egui, etc.) are pure Rust and need no system libraries.

## Testing

```bash
cargo test --all --features womanizer-engine/test-injection
```

The `test-injection` feature is required by the `AUDIO-09` reconnect integration test
(see `crates/womanizer-engine/Cargo.toml` for rationale). It is OFF in the shipped binary.

## License

See `LICENSE`. First-party Womanizer code is OtterBot IP; third-party crate licenses are
audited at every CI run via `cargo deny check licenses` against the allow-list in
`deny.toml` (MIT / Apache-2.0 / BSD-* / ISC / Zlib / MPL-2.0 — no GPL).
