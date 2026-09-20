#!/usr/bin/env node
// Install prns-js devDependencies when typescript is missing (fresh worktrees /
// pre-push without a prior npm ci). No-op when node_modules is already present.
import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.dirname(path.dirname(fileURLToPath(import.meta.url)));
const typescriptEntry = path.join(root, "node_modules", "typescript", "package.json");

if (existsSync(typescriptEntry)) {
  process.exit(0);
}

console.error("prns-js: installing npm ci --ignore-scripts (typescript missing)");
const result = spawnSync(
  "npm",
  ["ci", "--ignore-scripts", "--no-audit", "--no-fund"],
  {
    cwd: root,
    stdio: "inherit",
    env: process.env,
    shell: process.platform === "win32",
  },
);
process.exit(result.status === null ? 1 : result.status);
