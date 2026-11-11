#!/usr/bin/env bash
# Smoke test: build, run the scripted session, compare with the golden output.
# Deterministic: in-memory database, fixed input, no generated ids or timestamps.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo build --release --locked
actual="$(mktemp)"
trap 'rm -f "$actual"' EXIT
./target/release/polydb --memory < tests/fixtures/smoke.commands > "$actual"

if diff -u tests/fixtures/smoke.expected "$actual"; then
  echo "SMOKE OK: output matches tests/fixtures/smoke.expected ($(wc -l < "$actual" | tr -d ' ') lines)"
else
  echo "SMOKE FAILED: output differs from tests/fixtures/smoke.expected (diff above)" >&2
  exit 1
fi
