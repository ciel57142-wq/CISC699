#!/usr/bin/env bash
# Reproducibility pass. Copies the package to a fresh temporary directory
# (no target/, no .venv/, no data directories), then follows the README
# quick start there, step by step, and records everything in a log.
#
#   scripts/clean_run.sh                 full pass, including the driver check
#   scripts/clean_run.sh --skip-drivers  offline pass (no Python, no PyPI)
#
# The log is written to docs/evidence/clean-run-<UTC date>.log.
set -uo pipefail
src="$(cd "$(dirname "$0")/.." && pwd)"
skip_drivers=0
[ "${1:-}" = "--skip-drivers" ] && skip_drivers=1

stamp="$(date -u +%Y-%m-%d)"
log="$src/docs/evidence/clean-run-$stamp.log"
mkdir -p "$src/docs/evidence"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

failed=0
step() {  # step "title" command...
  local title="$1"; shift
  echo
  echo "### $title"
  echo "\$ $*"
  local start=$SECONDS
  if "$@"; then
    echo "--> ok ($((SECONDS - start)) s)"
  else
    echo "--> FAILED with exit code $? ($((SECONDS - start)) s)"
    failed=1
  fi
}

{
  echo "polydb clean-run log"
  echo "started (UTC): $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "source package: $src"
  echo "fresh copy:     $work/polydb"

  mkdir "$work/polydb"
  (cd "$src" && tar cf - --exclude=./target --exclude=./.venv --exclude=./.git \
      --exclude='./*-data' --exclude=./docs/evidence .) | (cd "$work/polydb" && tar xf -)
  cd "$work/polydb"
  export CARGO_TARGET_DIR="$work/polydb/target"

  step "0. Fresh copy contains no build output" test ! -e target -a ! -e .venv
  step "1. Check prerequisites (README: Prerequisites)" bash scripts/check_env.sh
  step "2. Build (README: Quick start, step 2)" cargo build --release --locked
  step "3. Run the test suite (README: Quick start, step 3)" cargo test --release --locked
  step "4. Smoke test against golden output (README: Quick start, step 4)" bash scripts/smoke.sh
  if [ "$skip_drivers" = 1 ]; then
    echo; echo "### 5. Driver check SKIPPED (--skip-drivers)"
  else
    step "5. Driver check through redis-py, psycopg2, pymongo (README: Driver check)" bash scripts/driver_checks.sh
  fi
  step "6. Same tests in an unoptimised build (larger stack frames; guards the DEFECT-01 fix)" cargo test --locked

  echo
  echo "finished (UTC): $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  if [ "$failed" = 0 ]; then echo "RESULT: CLEAN RUN PASSED"; else echo "RESULT: CLEAN RUN FAILED"; fi
} 2>&1 | tee "$log"

echo "log written to docs/evidence/clean-run-$stamp.log"
grep -q "RESULT: CLEAN RUN PASSED" "$log"
