#!/usr/bin/env bash
# Build Hopspot Android JNI libs into THIS worktree's jniLibs tree.
set -euo pipefail

source "$(dirname "${BASH_SOURCE[0]}")/lib/repo-root.sh"
root="$(repo_root)"
rust_dir="$root/personal-hopspot/mobile/android/rust"
jni_dir="$root/personal-hopspot/mobile/android/app/src/main/jniLibs"
target_dir="$rust_dir/target"
abi="${1:-arm64-v8a}"

case "$abi" in
    arm64-v8a) ndk_target="arm64-v8a" ;;
    armeabi-v7a) ndk_target="armeabi-v7a" ;;
    *)
        echo "usage: hopspot-android-jni.sh [arm64-v8a|armeabi-v7a]" >&2
        exit 1
        ;;
esac

if [[ -n "${CARGO_TARGET_DIR:-}" && "$CARGO_TARGET_DIR" != "$target_dir" ]]; then
    echo "warning: ignoring CARGO_TARGET_DIR=$CARGO_TARGET_DIR" >&2
    echo "warning: Hopspot Android JNI target dir is $target_dir" >&2
fi

echo "[hopspot-android] building $ndk_target -> $jni_dir"
(
    cd "$rust_dir"
    export CARGO_TARGET_DIR="$target_dir"
    if [[ "$ndk_target" == "armeabi-v7a" ]]; then
        cargo ndk -t "$ndk_target" -P 21 -o "$jni_dir" build --release
    else
        cargo ndk -t "$ndk_target" -o "$jni_dir" build --release
    fi
)

so="$jni_dir/$ndk_target/libpersonal_hopspot_android.so"
if [[ ! -f "$so" ]]; then
    echo "error: expected shared library missing: $so" >&2
    exit 1
fi

mtime="$(stat -f '%Sm' -t '%Y-%m-%d %H:%M' "$so" 2>/dev/null || stat -c '%y' "$so" 2>/dev/null | cut -d. -f1)"
size="$(stat -f '%z' "$so" 2>/dev/null || stat -c '%s' "$so")"
echo "[hopspot-android] ok: $so size=${size}B mtime=$mtime"
