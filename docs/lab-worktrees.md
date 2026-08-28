# Lab builds with multiple Git worktrees

If you keep more than one checkout of this repository (for example `Prns` on
`test/trunk-plus-prs` and `Prns-dioxus-demo` on `feat-android-dioxus-ble`),
**do not rely on plain `cargo build` for lab binaries**.

Some environments export `CARGO_TARGET_DIR` globally (Cursor often sets it to
the active workspace). Cargo then writes artifacts into that directory even when
you `cd` into a different worktree. The stale binary in the worktree's own
`target/` directory is easy to run by mistake.

## Canonical entrypoints

From any worktree root:

```bash
./tools/build/prnsd-release.sh
./tools/build/hopspot-android-jni.sh arm64-v8a
```

These scripts pin `--target-dir` / `CARGO_TARGET_DIR` to paths inside **this**
checkout and print the artifact they produced.

Equivalent cargo aliases (also override a stray `CARGO_TARGET_DIR`):

```bash
cargo prnsd-release
```

## Run the binary you just built

After `./tools/build/prnsd-release.sh`:

```bash
/path/to/this/worktree/prnsd/target/release/prnsd --config ~/.reticulum
```

Always prefer that absolute path over `./prnsd` from memory when switching
worktrees.

## Quick sanity check

```bash
stat -f '%Sm %z %N' prnsd/target/release/prnsd
./tools/build/prnsd-release.sh   # mtime should advance
```

If the timestamp does not move, you are still not building into this worktree.
