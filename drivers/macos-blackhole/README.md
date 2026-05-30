# Womanizer macOS Virtual Audio Driver

The macOS virtual-audio device that VRChat sees as a microphone. Womanizer's main app
writes processed audio into this device; VRChat (or any CoreAudio client) selects the device
named `Womanizer` as its mic input and receives the converted audio stream.

This directory is a **rebrand** of Existential Audio's
[BlackHole](https://github.com/ExistentialAudio/BlackHole) HAL plugin, built via the
officially-documented preprocessor-define mechanism. No BlackHole source is modified —
we override five build-time macros so the produced bundle is collision-safe with stock
BlackHole installs.

---

## License (GPL-3.0)

The BlackHole source code is licensed under **GPL-3.0** (Existential Audio LLC,
[license text](https://github.com/ExistentialAudio/BlackHole/blob/master/LICENSE.md)).

Per GPL-3.0 §6 ("Conveying Non-Source Forms"), the full source of this driver is published
in this directory after vendoring (see "Vendoring the upstream source" below). The Womanizer
distribution that includes this `.driver` bundle ships this `drivers/macos-blackhole/`
subtree intact so end-users have access to the corresponding source.

### GPL boundary

The Womanizer Rust application binary (the `womanizer` crate and its workspace members) is
licensed under MIT / Apache-2.0 and **does NOT link this driver**. The `.driver` bundle
produced here is loaded directly by macOS `coreaudiod` as an out-of-process HAL plugin —
the Womanizer binary only talks to it through the public CoreAudio API, the same way any
unrelated CoreAudio client (VRChat, QuickTime, Discord) would.

Concretely:

- `drivers/macos-blackhole/` is **outside** the Cargo workspace (`Cargo.toml`'s `members`
  list does not include it). `cargo build` never reads any file under this directory.
- The build artifact is a separate `.driver` bundle that lives in
  `/Library/Audio/Plug-Ins/HAL/Womanizer.driver`, not inside the Womanizer `.app` bundle.
- `cargo deny check licenses` never sees BlackHole's GPL header because no Cargo crate
  depends on this directory.

This process-level boundary is the Phase 0 D-02 decision and the DEVICE-03 contract.

---

## Vendoring the upstream source

This directory ships build infrastructure (`build.sh`, `install.sh`, this README) but does
**not** include the BlackHole source code itself — that is a one-time manual vendoring step
the developer performs. This keeps the GPL boundary deliberate (the act of pulling GPL
source into the tree is an explicit decision, not a `git clone --recursive` side-effect)
and avoids carrying a stale snapshot of upstream in this repo.

To vendor:

```sh
cd drivers/macos-blackhole
git clone --branch v0.6.1 https://github.com/ExistentialAudio/BlackHole.git BlackHole-src
mv BlackHole-src/BlackHole .
mv BlackHole-src/BlackHole.xcodeproj .
cp BlackHole-src/LICENSE.md .
rm -rf BlackHole-src
```

`v0.6.1` (released 2025-02-08) is the version Phase 1 RESEARCH §3 was written against and
is the latest release as of planning time. Pinning to a tag keeps the build reproducible —
a `main` checkout would change behavior over time as upstream commits land.

After vendoring, the directory looks like:

```
drivers/macos-blackhole/
├── README.md            (this file)
├── build.sh             (xcodebuild wrapper — already present)
├── install.sh           (install/uninstall/status — already present)
├── BlackHole/           (vendored — upstream source)
├── BlackHole.xcodeproj/ (vendored — upstream Xcode project)
└── LICENSE.md           (vendored — upstream GPL-3.0 text)
```

The `build.sh` script exits with a clear error if any of the vendored items are missing.

---

## Building

```sh
cd drivers/macos-blackhole
./build.sh
```

Produces `Womanizer.driver` in this directory (the `.driver` bundle is a folder under
macOS). The build invokes `xcodebuild` with **all five** collision-safety preprocessor
defines per FINDING-2 (so the produced bundle does not conflict with stock BlackHole when
installed side-by-side):

- `kDriver_Name="Womanizer"`
- `kPlugIn_BundleID="com.otterbot.womanizer.driver"`
- `kNumber_Of_Channels=2`
- `kBox_UID="WomanizerBox_UID"`
- `kDevice_UID="WomanizerDevice_UID"`
- `kDevice_ModelUID="WomanizerModel_UID"`

If `Resources/Womanizer.icns` exists, `build.sh` also passes `kPlugIn_Icon="Womanizer.icns"`
(D-17 — the icon override is conditional so the build works without a vendored icon asset).

### Requirements

- macOS (Apple Silicon or Intel) with Xcode Command Line Tools installed
  (`xcode-select --install`).
- The vendored upstream source (see above).

---

## Installing

```sh
./install.sh install   # default verb
./install.sh status    # check what's installed (Womanizer + stock BlackHole)
./install.sh uninstall # remove ONLY the Womanizer driver
```

`install` copies the built `Womanizer.driver` bundle to
`/Library/Audio/Plug-Ins/HAL/Womanizer.driver` (`sudo` prompt) and runs
`sudo killall coreaudiod` so macOS rescans the HAL plugin directory. After install, approve
the device in `System Settings → Privacy & Security` (if macOS prompts) and confirm it
appears in `System Settings → Sound → Output` and in `Audio MIDI Setup`.

`uninstall` removes **only** the Womanizer driver — the `rm -rf` path is hardcoded to
`/Library/Audio/Plug-Ins/HAL/Womanizer.driver` and the script never touches stock
BlackHole at `/Library/Audio/Plug-Ins/HAL/BlackHole*.driver` (FINDING-2 collision-safety).

`status` prints the install state of both Womanizer and stock BlackHole 2ch — useful for
debugging side-by-side installs.

---

## Why a rebrand and not a fork

We use BlackHole's officially-documented
[Running Multiple BlackHole Drivers](https://github.com/ExistentialAudio/BlackHole/wiki/Running-Multiple-BlackHole-Drivers)
preprocessor-define rebrand mechanism. This lets us ship a user-facing-distinct `Womanizer`
device WITHOUT modifying the BlackHole source. The GPL boundary stays clean:

- We carry an **unmodified** vendored upstream source tree.
- All Womanizer-specific changes are passed as **build-time macros** to `xcodebuild`.
- Stock BlackHole users can have both `Womanizer` and `BlackHole 2ch` registered
  simultaneously with no driver conflict.

A fork would obligate us to maintain a divergent BlackHole codebase forever and would
complicate the GPL distribution obligation. The rebrand approach is one Xcode invocation
plus five macros.

---

## GPL Compliance Checklist (for Phase 6 distribution)

Before shipping a release that includes the Womanizer driver:

- [ ] Vendored upstream source is present in this directory (BlackHole/, BlackHole.xcodeproj/).
- [ ] `LICENSE.md` from upstream is present in this directory and unmodified.
- [ ] This `README.md` is present and includes the GPL-3.0 notice + source location.
- [ ] The shipped `.dmg` includes `drivers/macos-blackhole/` intact (build, install
      scripts, vendored source, LICENSE.md, README.md).
- [ ] Build instructions in this README are reproducible from a clean macOS install with
      only Xcode Command Line Tools.
- [ ] The Womanizer Rust binary's license posture is unchanged (MIT / Apache-2.0); the
      driver lives outside the Cargo workspace.
