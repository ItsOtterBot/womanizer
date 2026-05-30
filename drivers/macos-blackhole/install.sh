#!/usr/bin/env bash
# Womanizer driver install / uninstall / status verbs.
#
# Collision-safe with stock BlackHole per FINDING-2 + D-18 — `uninstall` ONLY touches the
# Womanizer driver path; it never references the stock BlackHole path passed to `rm`.
#
# All three verbs require macOS (Linux/Windows hosts produce a usage error on `install` /
# `uninstall` because the HAL plugin directory only exists on macOS). The `status` verb is
# read-only and works as a no-op on non-macOS (both `[ -d ]` checks return false).

set -euo pipefail

INSTALL_PATH="/Library/Audio/Plug-Ins/HAL/Womanizer.driver"
STOCK_PATH="/Library/Audio/Plug-Ins/HAL/BlackHole2ch.driver"
SOURCE_BUNDLE="./Womanizer.driver"

case "${1:-install}" in
  install)
    if [[ ! -d "$SOURCE_BUNDLE" ]]; then
      echo "ERROR: $SOURCE_BUNDLE not found. Run ./build.sh first." >&2
      exit 64
    fi
    echo "Installing $SOURCE_BUNDLE to $INSTALL_PATH (will prompt for sudo)..."
    sudo cp -R "$SOURCE_BUNDLE" "$INSTALL_PATH"
    # Force coreaudiod to re-scan /Library/Audio/Plug-Ins/HAL/ — without this, the new
    # driver is on disk but not visible to CoreAudio clients until the daemon restarts.
    sudo killall coreaudiod
    echo ""
    echo "Done. Approve the Womanizer device in:"
    echo "  System Settings -> Privacy & Security"
    echo "  System Settings -> Sound -> Output (Womanizer should appear)"
    ;;
  uninstall)
    # CRITICAL: bounded to the Womanizer path only — NEVER touch stock BlackHole (FINDING-2).
    # The rm -rf argument is the hardcoded $INSTALL_PATH literal above; the script accepts
    # no path argument and offers no way to redirect the rm. This is the T-01-15 mitigation.
    if [[ ! -d "$INSTALL_PATH" ]]; then
      echo "Womanizer driver not installed at $INSTALL_PATH — nothing to uninstall."
      exit 0
    fi
    echo "Removing $INSTALL_PATH (will prompt for sudo)..."
    sudo rm -rf "$INSTALL_PATH"
    sudo killall coreaudiod
    echo "Done. Stock BlackHole (if any) is untouched."
    ;;
  status)
    if [[ -d "$INSTALL_PATH" ]]; then
      echo "Womanizer: installed at $INSTALL_PATH"
    else
      echo "Womanizer: not installed"
    fi
    if [[ -d "$STOCK_PATH" ]]; then
      echo "Stock BlackHole (2ch): installed at $STOCK_PATH"
    else
      echo "Stock BlackHole (2ch): not installed"
    fi
    # FINDING-2 collision-detection: Phase 5 may add UID comparison across both Info.plists.
    ;;
  *)
    echo "Usage: $0 [install|uninstall|status]" >&2
    exit 64
    ;;
esac
