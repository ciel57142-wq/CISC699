#!/usr/bin/env bash
# Optional compatibility check: talk to polydb through unmodified redis-py,
# psycopg2 and pymongo. Starts an in-memory server, runs tests/drivers.py,
# stops the server. Needs python3 and network access to PyPI on first run.
#
# tests/drivers.py uses fixed ports 6380, 5433 and 27018, so those must be free.
set -euo pipefail
cd "$(dirname "$0")/.."

command -v python3 >/dev/null || { echo "python3 not found: cannot run the driver check" >&2; exit 1; }
[ -x target/release/polydb ] || cargo build --release --locked

port_open() { (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; }

for port in 6380 5433 27018; do
  if port_open "$port"; then
    echo "port $port is already in use; stop whatever is listening there (a real Redis, PostgreSQL or MongoDB, or another polydb) and retry" >&2
    exit 1
  fi
done

if [ ! -d .venv ]; then
  python3 -m venv .venv
fi
.venv/bin/python -m pip install --quiet --disable-pip-version-check -r tests/requirements-drivers.txt

log="$(mktemp)"
./target/release/polydb serve --memory --port off > "$log" 2>&1 &
server=$!
cleanup() { kill "$server" 2>/dev/null || true; wait "$server" 2>/dev/null || true; rm -f "$log"; }
trap cleanup EXIT

for _ in $(seq 1 100); do
  if port_open 6380 && port_open 5433 && port_open 27018; then break; fi
  if ! kill -0 "$server" 2>/dev/null; then echo "server exited early:" >&2; cat "$log" >&2; exit 1; fi
  sleep 0.1
done
port_open 27018 || { echo "server did not open its ports within 10 s:" >&2; cat "$log" >&2; exit 1; }

.venv/bin/python tests/drivers.py
