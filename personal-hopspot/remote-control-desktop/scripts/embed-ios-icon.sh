#!/usr/bin/env bash
# Compile assets/AppIcon.appiconset into an iOS .app that `dx build --ios` produced.
# Dioxus 0.7.5 does not embed [bundle].icon into the device .app, so this fills that gap.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
app="${1:-}"
if [[ -z "$app" || ! -d "$app" ]]; then
  echo "usage: $0 /path/to/PersonalHopspotRemoteControlDesktop.app" >&2
  exit 2
fi

iconset="$root/assets/AppIcon.appiconset"
if [[ ! -f "$iconset/AppIcon.png" ]]; then
  echo "error: missing $iconset/AppIcon.png" >&2
  exit 1
fi

work="$(mktemp -d "${TMPDIR:-/tmp}/controller-ios-icon.XXXXXX")"
cleanup() { rm -rf "$work"; }
trap cleanup EXIT

mkdir -p "$work/Assets.xcassets"
cp -R "$iconset" "$work/Assets.xcassets/AppIcon.appiconset"
mkdir -p "$work/out"

xcrun actool "$work/Assets.xcassets" \
  --compile "$work/out" \
  --platform iphoneos \
  --minimum-deployment-target 15.0 \
  --app-icon AppIcon \
  --output-partial-info-plist "$work/partial.plist" \
  >/dev/null

cp "$work/out/Assets.car" "$app/Assets.car"
# Loose PNGs referenced by CFBundleIconFiles
shopt -s nullglob
for png in "$work/out"/AppIcon*.png; do
  cp "$png" "$app/$(basename "$png")"
done

# Merge CFBundleIcons* into Info.plist
python3 - "$app/Info.plist" "$work/partial.plist" <<'PY'
import plistlib
import sys
from pathlib import Path

info_path = Path(sys.argv[1])
partial_path = Path(sys.argv[2])
info = plistlib.loads(info_path.read_bytes())
partial = plistlib.loads(partial_path.read_bytes())
for key in ("CFBundleIcons", "CFBundleIcons~ipad"):
    if key in partial:
        info[key] = partial[key]
info_path.write_bytes(plistlib.dumps(info))
print(f"embedded AppIcon into {info_path.parent.name}")
PY

# Re-sign so the added resources are accepted on device.
identity="${IOS_CODESIGN_IDENTITY:-}"
if [[ -z "$identity" ]]; then
  identity="$(codesign -dv "$app" 2>&1 | awk -F= '/^Authority=Apple Development|^Authority=iPhone Developer|^Authority=Apple Distribution/{print $2; exit}')"
fi
if [[ -z "$identity" ]]; then
  identity="$(security find-identity -v -p codesigning 2>/dev/null | awk -F'\"' '/Apple Development|iPhone Developer|Apple Distribution/{print $2; exit}')"
fi
if [[ -z "$identity" ]]; then
  echo "warning: no codesigning identity found; Assets.car copied but app not re-signed" >&2
  exit 0
fi
# XML entitlements on stdout; binary --entitlements PATH dumps are not usable.
if codesign -d --entitlements :- "$app" >"$work/entitlements.plist" 2>/dev/null \
  && [[ -s "$work/entitlements.plist" ]]; then
  codesign --force --sign "$identity" --entitlements "$work/entitlements.plist" "$app"
else
  codesign --force --sign "$identity" "$app"
fi
echo "re-signed with $identity"
