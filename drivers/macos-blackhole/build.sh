#!/usr/bin/env bash
# Build the Womanizer-rebranded BlackHole HAL plugin via xcodebuild.
#
# FINDING-2: stock BlackHole and any rebrand MUST differ in FIVE preprocessor defines
# (kDriver_Name, kPlugIn_BundleID, kBox_UID, kDevice_UID, kDevice_ModelUID) for
# collision-safe side-by-side install. kNumber_Of_Channels=2 reflects D-16 (stereo).
#
# This script does NOT vendor the upstream BlackHole source — see README.md
# "Vendoring the upstream source" for the one-time `git clone` step the developer runs
# before this script will succeed.

set -euo pipefail

if [[ ! -d "BlackHole.xcodeproj" ]]; then
  echo "ERROR: upstream BlackHole source is not vendored." >&2
  echo "       See README.md 'Vendoring the upstream source' for the one-time setup." >&2
  exit 64
fi

# Always clean to avoid stale builds from stale macro values. Stale builds are the most
# common rebrand pitfall — Xcode caches object files keyed on source paths, not on
# preprocessor-macro changes.
xcodebuild -project BlackHole.xcodeproj -configuration Release clean

# Conditional kPlugIn_Icon override (D-17): include the icon macro only if the .icns asset
# is vendored at Resources/Womanizer.icns. Building without it is supported (the bundle
# uses the upstream default icon).
ICON_DEF=""
if [[ -f "Resources/Womanizer.icns" ]]; then
  ICON_DEF=' kPlugIn_Icon=\"Womanizer.icns\"'
fi

# All FIVE collision-safety preprocessor defines per FINDING-2. The `\"...\"` escaping is
# required because GCC_PREPROCESSOR_DEFINITIONS values are themselves consumed by the
# preprocessor — string literals must arrive at the C compiler quoted.
xcodebuild -project BlackHole.xcodeproj -configuration Release \
  GCC_PREPROCESSOR_DEFINITIONS="\$(inherited) kDriver_Name=\\\"Womanizer\\\" kPlugIn_BundleID=\\\"com.otterbot.womanizer.driver\\\" kNumber_Of_Channels=2 kBox_UID=\\\"WomanizerBox_UID\\\" kDevice_UID=\\\"WomanizerDevice_UID\\\" kDevice_ModelUID=\\\"WomanizerModel_UID\\\"${ICON_DEF}"

BUILT_BUNDLE="build/Release/BlackHole.driver"
if [[ ! -d "$BUILT_BUNDLE" ]]; then
  echo "ERROR: xcodebuild succeeded but did not produce $BUILT_BUNDLE" >&2
  exit 70
fi

# Rename to the user-facing brand. install.sh expects ./Womanizer.driver in this directory.
rm -rf Womanizer.driver
mv "$BUILT_BUNDLE" Womanizer.driver
echo "Built Womanizer.driver — run ./install.sh install to register with CoreAudio."
