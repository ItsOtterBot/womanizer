# Womanizer

> Windows-only as of 2026-06-01 (VRChat does not ship a native macOS client).

A 100% local, CPU-only, real-time male→female voice converter for VRChat on Windows 10/11.
Independent pitch + formant shifting via [signalsmith-stretch](https://github.com/Signalsmith-Audio/signalsmith-stretch),
plus breathiness / brightness / de-essing shaping. Imperceptible conversation latency,
zero GPU contention, fully offline. An OtterBot product, distributed free.

See `.planning/PROJECT.md` for the full product definition, requirements, and architectural
constraints. `CLAUDE.md` documents the locked tech-stack pins.

## Build Prerequisites (Windows)

Womanizer is a pure-Rust workspace, but one transitive build dependency requires native
tooling beyond a stock Rust install. **Before running `cargo build` or `cargo test` on a
clean Windows checkout, install LLVM/Clang.**

**Why:** [`signalsmith-stretch 0.1.3`](https://crates.io/crates/signalsmith-stretch) pulls
in `bindgen ^0.70` as a build-dependency. `bindgen` parses the C++ headers it generates
Rust FFI bindings from by invoking `libclang.dll` at build time. Visual Studio Build Tools
and the MSVC toolchain alone do not ship `libclang.dll` — only the LLVM toolchain does.
Without it, the first clean build fails with `error: Unable to find libclang`.
(This is the canonical bindgen-on-Windows pitfall — RESEARCH §Pitfall 3.)

**How to install:**

- **Chocolatey (recommended):**
  ```powershell
  choco install llvm
  ```
- **Or the official LLVM Windows installer** (LLVM 16 or newer) from
  [releases.llvm.org](https://releases.llvm.org/). Pick the
  `LLVM-x.y.z-win64.exe` artifact and select "Add LLVM to the system PATH" during install.

After installation, open a fresh shell so `PATH` picks up `libclang.dll`, then proceed
with `cargo build --release` as normal. CI installs LLVM automatically on the Windows
runner via `choco install llvm` (see `.github/workflows/ci.yml`); local Windows
workstations need the one-time install above.

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
