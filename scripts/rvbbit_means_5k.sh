#!/usr/bin/env bash
# 5000-row WHERE ... MEANS against hutch_mock. No prewarm.
# Expect ceil(5000/32) = 157 HTTP calls, max batch 32, 5000 rows.
#
# Ports: Postgres 55434, mock 8100. The spike on 55432 / 8099 is not used.
# No OpenRouter key. The mock is the counter.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

PG_BIN=${PG_BIN:-/usr/lib/postgresql/18/bin}
PGPORT=${PGPORT:-55434}
HUTCH_PORT=${HUTCH_PORT:-8100}
PGHOST=${PGHOST:-/tmp}
PGUSER=${PGUSER:-postgres}
PGDATA=${PGDATA:-/tmp/rvbbit-means-5k-pg}
RUNDIR=${RUNDIR:-/tmp/rvbbit-means-5k}
export CC=${CC:-gcc}
export CXX=${CXX:-g++}
export PATH="$PG_BIN:$PATH"
export PGRUST_PGSHAREDIR=${PGRUST_PGSHAREDIR:-/usr/share/postgresql/18}
export PGRUST_TZDIR=${PGRUST_TZDIR:-/usr/share/zoneinfo}
export RVBBIT_HUTCH_URL="http://127.0.0.1:${HUTCH_PORT}"

if [[ "$PGPORT" == "55432" || "$HUTCH_PORT" == "8099" ]]; then
  echo "Refusing spike ports 55432 / 8099. Use the defaults (55434 / 8100)." >&2
  exit 1
fi

for cmd in gcc g++ curl initdb psql cargo python3; do
  if ! command -v "$cmd" >/dev/null 2>&1; then
    echo "missing command: $cmd" >&2
    exit 1
  fi
done

port_open() {
  (echo >/dev/tcp/127.0.0.1/"$1") >/dev/null 2>&1
}

if port_open "$HUTCH_PORT"; then
  echo "127.0.0.1:$HUTCH_PORT is already in use." >&2
  exit 1
fi
if [[ -S "$PGHOST/.s.PGSQL.$PGPORT" ]]; then
  echo "Postgres socket $PGHOST/.s.PGSQL.$PGPORT is already in use." >&2
  exit 1
fi

echo "=== build (CC=$CC CXX=$CXX) ==="
cargo build -p main_main -p hutch_mock --profile fast-profile

mkdir -p "$RUNDIR"
MOCK_LOG=$RUNDIR/mock.log
PG_LOG=$RUNDIR/postgres.log

cleanup() {
  if [[ -n "${MOCK_PID:-}" ]]; then
    kill "$MOCK_PID" >/dev/null 2>&1 || true
  fi
  if [[ -f "$PGDATA/postmaster.pid" ]]; then
    kill "$(head -n 1 "$PGDATA/postmaster.pid")" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

rm -rf "$PGDATA"

echo "=== hutch_mock on 127.0.0.1:$HUTCH_PORT ==="
: >"$MOCK_LOG"
./target/fast-profile/hutch_mock "$HUTCH_PORT" >"$MOCK_LOG" 2>&1 &
MOCK_PID=$!

echo "=== initdb $PGDATA ==="
initdb -D "$PGDATA" --no-locale --encoding=UTF8 --auth-local=trust --auth-host=trust -U "$PGUSER" >"$RUNDIR/initdb.log"

echo "=== postgres on socket $PGHOST port $PGPORT ==="
ulimit -s 65520
RUST_MIN_STACK=33554432 \
  ./target/fast-profile/postgres \
  -D "$PGDATA" -k "$PGHOST" -p "$PGPORT" \
  -c listen_addresses= -c io_method=sync -c max_stack_depth=60000 \
  >"$PG_LOG" 2>&1 &

ready=0
for _ in $(seq 1 80); do
  if curl -sf "http://127.0.0.1:$HUTCH_PORT/stats" >/dev/null && \
     psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -X -q -c 'SELECT 1' >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 0.25
done
if [[ "$ready" != 1 ]]; then
  echo "mock or postgres did not become ready. Logs: $MOCK_LOG $PG_LOG" >&2
  exit 1
fi

curl -sf -X POST "http://127.0.0.1:$HUTCH_PORT/reset" >/dev/null

PSQL=(psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -v ON_ERROR_STOP=1 -X -q)
echo "=== load 5000 rows ==="
"${PSQL[@]}" <<'SQL'
DROP TABLE IF EXISTS means_5k;
CREATE TABLE means_5k (id int, body text);
INSERT INTO means_5k
SELECT g, 'row-' || g::text || ' alpha'
FROM generate_series(1, 5000) AS g;
SQL

echo "=== EXPLAIN (no execution) ==="
"${PSQL[@]}" -c "EXPLAIN SELECT count(*) FROM means_5k WHERE body MEANS 'alpha'"

echo "=== count, no prewarm ==="
"${PSQL[@]}" -c "SELECT count(*) FROM means_5k WHERE body MEANS 'alpha'"

STATS=$(curl -sf "http://127.0.0.1:$HUTCH_PORT/stats")
echo "=== hutch_mock /stats ==="
echo "$STATS"
STATS="$STATS" python3 - <<'PY'
import json, os, sys
stats = json.loads(os.environ["STATS"])
calls = stats["calls"]
rows = stats["rows"]
max_batch = stats["max_batch"]
print(f"calls={calls} rows={rows} max_batch={max_batch}")
if calls != 157:
    sys.exit(f"expected 157 HTTP calls (ceil(5000/32)), got {calls}")
if rows != 5000:
    sys.exit(f"expected 5000 rows counted by the mock, got {rows}")
if max_batch != 32:
    sys.exit(f"expected max batch 32, got {max_batch}")
print("5k MEANS batch count matches")
PY
