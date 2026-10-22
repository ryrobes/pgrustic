#!/usr/bin/env bash
# Function-call shims and rvbbit.flow against hutch_mock.
# Operator MEANS, rvbbit.means, rvbbit.means_score, rvbbit.classify,
# rvbbit.extract, rvbbit.semantic_similarity, rvbbit.rerank, rvbbit.decide,
# and a stored workflow all resolve. No OpenRouter key.
#
# Ports: Postgres 55443, mock 8107. The spike on 55432 / 8099 is not used.
# Run with `env -u PGPORT -u HUTCH_PORT` so a leftover spike port is not picked up.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

PG_BIN=${PG_BIN:-/usr/lib/postgresql/18/bin}
PGPORT=${PGPORT:-55443}
HUTCH_PORT=${HUTCH_PORT:-8107}
PGHOST=${PGHOST:-/tmp}
PGUSER=${PGUSER:-postgres}
PGDATA=${PGDATA:-/tmp/rvbbit-m9-pg}
RUNDIR=${RUNDIR:-/tmp/rvbbit-m9}
export CC=${CC:-gcc}
export CXX=${CXX:-g++}
export PATH="$PG_BIN:$PATH"
export PGRUST_PGSHAREDIR=${PGRUST_PGSHAREDIR:-/usr/share/postgresql/18}
export PGRUST_TZDIR=${PGRUST_TZDIR:-/usr/share/zoneinfo}
export RVBBIT_HUTCH_URL="http://127.0.0.1:${HUTCH_PORT}"

if [[ "$PGPORT" == "55432" || "$HUTCH_PORT" == "8099" ]]; then
  echo "Refusing spike ports 55432 / 8099. Use the defaults (55443 / 8107)." >&2
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

PSQL=(psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -v ON_ERROR_STOP=1 -X -q -A -t)

echo "=== bootstrap and shims ==="
"${PSQL[@]}" <<'SQL' >"$RUNDIR/out.txt"
SELECT 'hello alpha' MEANS 'alpha';
SELECT rvbbit.means('hello alpha', 'alpha');
SELECT rvbbit.means('invoice 100', 'alpha');
SELECT rvbbit.means_score('hello alpha', 'alpha') > 0.5;
SELECT rvbbit.means_score('invoice 100', 'alpha') < 0.5;
SELECT rvbbit.classify('a hairy biped crossed', 'biped,invoice');
SELECT rvbbit.extract('a hairy biped crossed', 'creature') IS NOT NULL;
SELECT rvbbit.semantic_similarity('a hairy biped', 'biped') > rvbbit.semantic_similarity('invoice 100', 'biped');
SELECT rvbbit.rerank('the logging road', 'road') > 0.5;
CREATE DECISION TREE m9_leaf (notes text) AS 'kept';
SELECT rvbbit.decide('m9_leaf', 'ignored');
CREATE WORKFLOW m9_route (ticket text) RETURNS text VERSION 1 AS $$
  CLASSIFY({{ticket}} INTO 'biped', 'invoice')
$$;
CREATE WORKFLOW m9_route (ticket text) RETURNS text VERSION 2 AS $$
  SELECT 'v2'
$$;
SELECT rvbbit.flow('m9_route', 'a hairy biped crossed');
SELECT rvbbit.flow('m9_route', 'a hairy biped crossed', 1);
SELECT rvbbit.flow('m9_route', 'ignored', 2);
SELECT ('hello alpha' MEANS 'alpha') = rvbbit.means('hello alpha', 'alpha');
SELECT count(*) FROM pg_proc p
  JOIN pg_namespace n ON n.oid = p.pronamespace
  WHERE n.nspname = 'rvbbit'
    AND p.proname IN (
      'means', 'means_score', 'extract', 'classify', 'decide',
      'summarize', 'consensus', 'rerank', 'semantic_similarity', 'flow'
    );
SQL

echo "=== results ==="
cat "$RUNDIR/out.txt"

python3 - "$RUNDIR/out.txt" <<'PY'
import sys
lines = [ln.strip() for ln in open(sys.argv[1]) if ln.strip()]
print("lines", lines)
# Latest workflow version is 2, so the unqualified flow call returns v2.
# The version-1 call is the CLASSIFY body. The last row is the pg_proc count:
# means, means_score, extract, classify x2, decide, summarize, consensus,
# rerank, semantic_similarity, flow x2.
expected = ["t", "t", "f", "t", "t", "biped", "t", "t", "t", "kept", "v2", "biped", "v2", "t"]
if lines[:-1] != expected:
    sys.exit(f"shim rows:\n  got {lines[:-1]}\n  want {expected}")
count = int(lines[-1])
if count < 12:
    sys.exit(f"expected at least 12 common pg_proc rows, got {count}")
print("m9 shims passed")
PY

echo "=== rvbbit.calls ==="
"${PSQL[@]}" -c "SELECT op, count(*), round(sum(cost)::numeric, 8) FROM rvbbit.calls GROUP BY op ORDER BY op"
echo "m9 shim demo passed"
