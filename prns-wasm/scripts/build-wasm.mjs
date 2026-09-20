#!/usr/bin/env node
// Build prns_wasm and run wasm-bindgen against the real cargo target dir.
//
// Honors CARGO_TARGET_DIR (and cargo's configured target directory) so
// wasm-bindgen does not look at a stale local ./target when cargo wrote
// elsewhere.
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.dirname(path.dirname(fileURLToPath(import.meta.url)));

function usage() {
  console.error(
    "usage: node scripts/build-wasm.mjs [--release] [--out-dir DIR] [--features LIST]",
  );
  process.exit(2);
}

function run(command, args) {
  const result = spawnSync(command, args, {
    cwd: root,
    stdio: "inherit",
    env: process.env,
    shell: process.platform === "win32",
  });
  if (result.status !== 0) {
    process.exit(result.status === null ? 1 : result.status);
  }
}

function cargoTargetDirectory() {
  const result = spawnSync(
    "cargo",
    ["metadata", "--no-deps", "--format-version", "1"],
    {
      cwd: root,
      encoding: "utf8",
      env: process.env,
      shell: process.platform === "win32",
    },
  );
  if (result.status !== 0) {
    process.exit(result.status === null ? 1 : result.status);
  }
  const metadata = JSON.parse(result.stdout);
  return metadata.target_directory;
}

const args = process.argv.slice(2);
let profile = "debug";
let outDir = path.join(root, "smoke", "pkg");
const features = [];

for (let i = 0; i < args.length; i += 1) {
  const arg = args[i];
  if (arg === "--release") {
    profile = "release";
  } else if (arg === "--out-dir") {
    outDir = args[++i];
    if (!outDir) {
      usage();
    }
  } else if (arg === "--features") {
    const list = args[++i];
    if (!list) {
      usage();
    }
    features.push(list);
  } else {
    usage();
  }
}

const cargoArgs = ["build", "--locked", "--target", "wasm32-unknown-unknown"];
if (profile === "release") {
  cargoArgs.push("--release");
}
for (const list of features) {
  cargoArgs.push("--features", list);
}

run("cargo", cargoArgs);

const wasm = path.join(
  cargoTargetDirectory(),
  "wasm32-unknown-unknown",
  profile,
  "prns_wasm.wasm",
);
if (!existsSync(wasm)) {
  console.error(`error: cargo did not emit ${wasm}`);
  process.exit(1);
}

mkdirSync(outDir, { recursive: true });
run("wasm-bindgen", [wasm, "--target", "web", "--out-dir", outDir]);
