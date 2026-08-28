#!/usr/bin/env bash
# Build prnsd into THIS worktree's prnsd/target, regardless of CARGO_TARGET_DIR.
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/lib/repo-root.sh"
root="$(repo_root)"
prnsd_dir="$root/prnsd"
target_dir="$prnsd_dir/target"
binary="$target_dir/release/prnsd"

if [[ -n "${CARGO_TARGET_DIR:-}" && "$CARGO_TARGET_DIR" != "$target_dir" ]]; then
    echo "warning: ignoring CARGO_TARGET_DIR=$CARGO_TARGET_DIR" >&2
    echo "warning: prnsd release artifacts belong in $target_dir" >&2
fi

echo "[prnsd] building release -> $target_dir"
(
    cd "$prnsd_dir"
    cargo build --release --target-dir "$target_dir"
)

if [[ ! -x "$binary" ]]; then
    echo "error: expected executable missing: $binary" >&2
    exit 1
fi

version="$("$binary" --version 2>/dev/null || true)"
mtime="$(stat -f '%Sm' -t '%Y-%m-%d %H:%M' "$binary" 2>/dev/null || stat -c '%y' "$binary" 2>/dev/null | cut -d. -f1)"
size="$(stat -f '%z' "$binary" 2>/dev/null || stat -c '%s' "$binary")"

echo "[prnsd] ok: $binary"
echo "[prnsd]     version=$version size=${size}B mtime=$mtime"
echo "[prnsd] run: $binary --config ~/.reticulum"
