#!/usr/bin/env bash
# Kinetix local CI gate — a faithful mirror of .github/workflows/ci.yml.
#
# Run this BEFORE every commit/push so the remote CI is green on the first try:
#
#     scripts/ci.sh            # full gate (fmt, clippy, tests, release, smoke, bench, deny, dashboard)
#     scripts/ci.sh --fast     # everything except the release build, smoke, bench and dashboard
#
# NFR-1.8, NFR-3.7, NFR-5.5, NFR-7.1.
set -euo pipefail

cd "$(dirname "$0")/.."

FAST=0
[ "${1:-}" = "--fast" ] && FAST=1

step() { printf '\n\033[1m==> %s\033[0m\n' "$1"; }

step "cargo fmt --check"
cargo fmt --all -- --check

step "cargo clippy --all-targets"
cargo clippy --all-targets 2>&1 | tail -1

step "cargo test (unit + protocol torture/fuzz)"
cargo test --quiet

if [ "$FAST" = "1" ]; then
  step "cargo-deny check"
  if command -v cargo-deny >/dev/null 2>&1; then cargo deny check; else echo "cargo-deny not installed; skipping"; fi
  printf '\n\033[1;32m==> fast CI checks passed\033[0m\n'
  exit 0
fi

step "cargo build --release (NFR-7.1)"
cargo build --release 2>&1 | tail -1

# The dashboard bundle is embedded into the binary via rust-embed; CI builds it
# before any cargo step. Rebuild + re-embed it here so the check is faithful.
step "dashboard: npm ci + tsc + build (embedded assets)"
( cd dashboard && npm ci --silent && npx tsc --noEmit && npm run build >/dev/null )
touch src/assets.rs
cargo build 2>&1 | tail -1

step "installer source-build smoke"
bash scripts/install-smoke.sh

step "end-to-end smoke (FR-9.2, FR-9.4)"
scripts/smoke.sh 127.0.0.1:8180 2>&1 | tail -3

step "coding-agent compatibility matrix (FR-9.2, FR-9.3)"
scripts/compat-matrix.sh 127.0.0.1:8186 2>&1 | tail -5

step "benchmark matrix (NFR-1)"
scripts/bench-rust.sh "1 10 100" 500 2>&1 | tail -5

step "cargo-deny check (advisories, licenses, bans, sources)"
if command -v cargo-deny >/dev/null 2>&1; then
  cargo deny check 2>&1 | tail -1
else
  echo "cargo-deny not installed; skipping (install: cargo install cargo-deny)"
fi

printf '\n\033[1;32m==> local CI checks passed\033[0m\n'
