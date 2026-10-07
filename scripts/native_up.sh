#!/usr/bin/env bash
# No-Docker fallback: starts the three databases as local processes.
#
# This exists because the cloud sandbox used to build the first slice could not pull
# Docker images (ISSUE-003 in docs/ISSUE_LOG.md). It is NOT the measurement setup:
#   - pg-fed and pg-cons run as two native PostgreSQL 16 clusters
#   - mongo-fed is FerretDB 1.24 (MongoDB wire protocol over PostgreSQL), not MongoDB 7.0
# Timing numbers from this setup are never reported. Use `make up` (compose.yaml) on the
# project laptop for anything that goes into the report.
#
# Usage: scripts/native_up.sh [start|stop]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SB="$ROOT/.sandbox"
PGBIN="${PGBIN:-/usr/lib/postgresql/16/bin}"
FERRET="${FERRET:-/opt/ferret/ferretdb}"
RUNAS="${RUNAS:-postgres}"          # initdb refuses to run as root

as_pg() { if [ "$(id -u)" = 0 ]; then su "$RUNAS" -s /bin/bash -c "$*"; else bash -c "$*"; fi; }

init_cluster() {   # name port dbname
  local dir="$SB/$1"
  if [ ! -d "$dir/data" ]; then
    mkdir -p "$dir"; chown -R "$RUNAS" "$dir" 2>/dev/null || true
    as_pg "$PGBIN/initdb -D $dir/data -U polystore --auth=trust -E UTF8 >/dev/null"
  fi
  as_pg "$PGBIN/pg_ctl -D $dir/data -l $dir/log -o '-p $2 -c shared_buffers=128MB -k /tmp' -w start >/dev/null"
  as_pg "$PGBIN/createdb -h /tmp -p $2 -U polystore $3 2>/dev/null || true"
}

case "${1:-start}" in
  start)
    mkdir -p "$SB"; chown "$RUNAS" "$SB" 2>/dev/null || true
    init_cluster pg-fed 5432 fed
    init_cluster pg-cons 5433 cons
    init_cluster ferret-backing 5435 ferretdb
    nohup "$FERRET" --listen-addr=127.0.0.1:27017 --handler=pg \
      --postgresql-url="postgres://polystore@127.0.0.1:5435/ferretdb" \
      --telemetry=disable --state-dir="$SB" --log-level=warn \
      > "$SB/ferretdb.log" 2>&1 &
    echo $! > "$SB/ferretdb.pid"
    sleep 2
    echo "pg-fed :5432  pg-cons :5433  mongo-fed (FerretDB) :27017"
    ;;
  stop)
    [ -f "$SB/ferretdb.pid" ] && kill "$(cat "$SB/ferretdb.pid")" 2>/dev/null || true
    for c in pg-fed pg-cons ferret-backing; do
      [ -d "$SB/$c/data" ] && as_pg "$PGBIN/pg_ctl -D $SB/$c/data -m fast stop >/dev/null" || true
    done
    ;;
  wipe)
    "$0" stop; rm -rf "$SB"
    ;;
esac
