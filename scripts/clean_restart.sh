#!/usr/bin/env bash
# HS3 self-check: "Can you reproduce your own evidence from a clean restart?"
# Wipes the databases, the virtualenv and all generated data, then rebuilds everything and
# runs every check, logging to evidence/<label>/. Run it twice and diff the two summaries.
#
#   scripts/clean_restart.sh <label> native     # sandbox / no Docker
#   scripts/clean_restart.sh <label> compose    # project laptop
set -euo pipefail
LABEL="${1:?label}"; MODE="${2:-compose}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"; cd "$ROOT"
OUT="evidence/$LABEL"; mkdir -p "$OUT"
LOG="$OUT/run.log"; : > "$LOG"
step() { echo; echo "=== $(date -u +%H:%M:%SZ) $*" | tee -a "$LOG"; }
run()  { "$@" 2>&1 | sed "/UV_NATIVE_TLS/d" | tee -a "$LOG"; return "${PIPESTATUS[0]}"; }

step "wipe: databases, venv, generated data"
if [ "$MODE" = native ]; then run scripts/native_up.sh wipe; else run docker compose down -v; fi
rm -rf .venv bench/data/out
step "environment: locked install"
run uv sync --all-extras --frozen
step "databases up ($MODE)"
if [ "$MODE" = native ]; then run scripts/native_up.sh start; else run docker compose up -d --wait; fi
[ -f .env ] || cp .env.example .env

step "unit tests + coverage (no DB)"
run make test-unit
for SCALE in 0.01 0.1; do
  step "S=$SCALE: generate twice (T-D1), validate, load, equivalence (T-D2)"
  run make data validate load equiv SEED=1 SCALE=$SCALE
  step "S=$SCALE: oracle (T-O1), arms A1 and A4"
  run make oracle SEED=1 SCALE=$SCALE
  if [ "$SCALE" = 0.01 ]; then
    step "S=0.01: DB tests incl. 500 Hypothesis examples (T-C1, T-P1), as in CI"
    PROPERTY_EXAMPLES=500 run make test-db
  fi
done
step "pilot benchmark at S=0.1"
run make pilot SEED=1 SCALE=0.1
step "environment snapshot"
run make env-snapshot

# Compact summary for diffing two clean restarts (no timings: those vary run to run).
{
  grep -E "^T-D1 |^T-D2 |^T-O1 |^validation|passed|failed" "$LOG" | sed -E 's/ in [0-9.]+s.*//'
  RUN=$(ls -d bench/runs/*pilot-W5 | tail -1); echo "pilot: $RUN"; cat "$RUN/bytes.csv"
} > "$OUT/summary.txt"
step "done: $OUT/summary.txt"
