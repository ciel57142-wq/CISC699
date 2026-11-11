#!/usr/bin/env bash
# Start the polydb server using config/polydb.env (or the defaults in
# config/polydb.env.example if you have not made a copy).
set -euo pipefail
cd "$(dirname "$0")/.."

config="config/polydb.env"
[ -f "$config" ] || config="config/polydb.env.example"
echo "using $config"
set -a
# shellcheck disable=SC1090
. "$config"
set +a

[ -x target/release/polydb ] || cargo build --release --locked

args=(serve)
if [ -n "${POLYDB_DATA_DIR:-}" ]; then args+=("$POLYDB_DATA_DIR"); else args+=(--memory); fi
args+=(--host "${POLYDB_HOST:-127.0.0.1}"
       --port "${POLYDB_PORT:-7878}"
       --redis-port "${POLYDB_REDIS_PORT:-6380}"
       --pg-port "${POLYDB_PG_PORT:-5433}"
       --mongo-port "${POLYDB_MONGO_PORT:-27018}")
echo "running: polydb ${args[*]}"
exec ./target/release/polydb "${args[@]}"
