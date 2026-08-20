#!/usr/bin/env bash
# Build the Dioxus web UI and sync it into the Hopspot APK assets folder.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
UI="$ROOT/dioxus-android"
DST="$ROOT/android/app/src/main/assets/dioxus"

cd "$UI"
dx build --platform web --release

# Prefer an explicit target dir; otherwise pick the newest dx public output.
SRC=""
if [[ -n "${CARGO_TARGET_DIR:-}" && -f "${CARGO_TARGET_DIR}/dx/personal-hopspot-dioxus-android/release/web/public/index.html" ]]; then
  SRC="${CARGO_TARGET_DIR}/dx/personal-hopspot-dioxus-android/release/web/public"
elif [[ -f "$UI/target/dx/personal-hopspot-dioxus-android/release/web/public/index.html" ]]; then
  SRC="$UI/target/dx/personal-hopspot-dioxus-android/release/web/public"
else
  SRC="$(
    find /var/folders "$HOME" -path '*/dx/personal-hopspot-dioxus-android/release/web/public/index.html' 2>/dev/null \
      | xargs -I{} dirname {} \
      | xargs -I{} stat -f '%m %N' {} 2>/dev/null \
      | sort -nr \
      | head -1 \
      | cut -d' ' -f2-
  )"
fi
if [[ -z "$SRC" || ! -f "$SRC/index.html" ]]; then
  echo "could not locate dx web public output" >&2
  exit 1
fi

echo "syncing from $SRC"
rm -rf "$DST"
mkdir -p "$DST"
rsync -a "$SRC/" "$DST/"

python3 - <<PY
from pathlib import Path
import re

root = Path("$DST")
assets = root / "assets"

def newest(pattern: str) -> Path:
    files = sorted(assets.glob(pattern), key=lambda p: p.stat().st_mtime, reverse=True)
    if not files:
        raise SystemExit(f"no assets matching {pattern}")
    return files[0]

# Drop stale hashed bundles left behind by dx's public dir.
keep = {newest("*.js"), newest("*.wasm"), newest("*.css")}
for path in assets.iterdir():
    if path.is_file() and path not in keep:
        path.unlink()

js = newest("*.js")
wasm = newest("*.wasm")
css = newest("*.css")

body = js.read_text()
body = re.sub(
    r'module_or_path:"[^"]+\.wasm"',
    f'module_or_path:"./assets/{wasm.name}"',
    body,
)
body = body.replace('"/./assets/', '"./assets/').replace('"/assets/', '"./assets/')
js.write_text(body)

(root / "index.html").write_text(
    f"""<!DOCTYPE html>
<html>
    <head>
        <title>Personal Hopspot</title>
        <meta content="text/html;charset=utf-8" http-equiv="Content-Type">
        <meta name="viewport" content="width=device-width, initial-scale=1">
        <meta charset="UTF-8">
        <link rel="stylesheet" href="./assets/{css.name}">
    </head>
    <body>
        <div id="main"></div>
        <script type="module" src="./assets/{js.name}"></script>
    </body>
</html>
"""
)
print(f"synced dioxus assets js={js.name} wasm={wasm.name} css={css.name}")
PY
