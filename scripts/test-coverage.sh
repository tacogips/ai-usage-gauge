#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$project_root"

if command -v cargo-llvm-cov >/dev/null 2>&1; then
  cargo llvm-cov --locked -p ccusage-core -p ccusage-cli --summary-only
else
  echo "cargo-llvm-cov is not installed. Install it with 'cargo install cargo-llvm-cov' to measure Rust line coverage." >&2
  exit 1
fi
