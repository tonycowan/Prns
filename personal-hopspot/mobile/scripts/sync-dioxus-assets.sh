#!/usr/bin/env bash
# Build the Dioxus web UI and sync it into the Hopspot APK assets folder.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
UI="$ROOT/dioxus-android"
DST="$ROOT/android/app/src/main/assets/dioxus"

cd "$UI"
dx build --platform web --release

# dx 0.7 writes under CARGO_TARGET_DIR; fall back to the usual local target.
CANDIDATES=(
  "${CARGO_TARGET_DIR:-}/dx/personal-hopspot-dioxus-android/release/web/public"
  "$UI/target/dx/personal-hopspot-dioxus-android/release/web/public"
)
SRC=""
for candidate in "${CANDIDATES[@]}"; do
  if [[ -n "$candidate" && -f "$candidate/index.html" ]]; then
    SRC="$candidate"
    break
  fi
done
if [[ -z "$SRC" ]]; then
  # Cursor / sandbox cargo target cache
  SRC="$(find /var/folders -path '*personal-hopspot-dioxus-android/release/web/public/index.html' 2>/dev/null | head -1 | xargs dirname 2>/dev/null || true)"
fi
if [[ -z "$SRC" || ! -f "$SRC/index.html" ]]; then
  echo "could not locate dx web public output" >&2
  exit 1
fi

mkdir -p "$DST"
rsync -a --delete "$SRC/" "$DST/"

# file:// WebView cannot resolve absolute /assets/... paths.
python3 - <<PY
from pathlib import Path
import re
root = Path("$DST")
index = root / "index.html"
text = index.read_text()
text = text.replace('"/./assets/', '"./assets/').replace('"/assets/', '"./assets/')
text = text.replace("'/./assets/", "'./assets/").replace("'/assets/", "'./assets/")
index.write_text(text)
assets = root / "assets"
for js in assets.glob("*.js"):
    body = js.read_text()
    # Prefer same-directory wasm next to the JS bundle.
    body = re.sub(
        r'module_or_path:"/\.?/assets/(personal-hopspot-dioxus-android_bg-[^"]+\.wasm)"',
        r'module_or_path:"./\1"',
        body,
    )
    body = body.replace('"/./assets/', '"./assets/').replace('"/assets/', '"./assets/')
    js.write_text(body)
print(f"synced dioxus assets from {root}")
PY
