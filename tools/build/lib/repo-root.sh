#!/usr/bin/env bash
# Resolve the repository root from tools/build/lib/repo-root.sh.
repo_root() {
    cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd
}
