#!/usr/bin/env bash
# M8: semantic cost model, EXPLAIN, and the pre-flight budget check.
#
# The default path is the mock math in rvbbit_cost. It does not start
# Postgres and it does not spend a provider call.
#
# Live, against whatever hutch RVBBIT_HUTCH_URL names (hutch-lite or the
# mock). When that variable is unset, this starts hutch_mock.
#   RVBBIT_M8_LIVE=1 bash scripts/rvbbit_m8_demo.sh
#   RVBBIT_M8_LIVE=1 RVBBIT_HUTCH_URL=http://127.0.0.1:8096 bash scripts/rvbbit_m8_demo.sh
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

cargo test -p rvbbit_cost --profile fast-profile --offline --lib -- --test-threads=1
echo "m8 demo: seed prices, history, knobs, and the budget check passed"

if [[ "${RVBBIT_M8_LIVE:-}" != "1" ]]; then
  echo "export RVBBIT_M8_LIVE=1 to EXPLAIN a live plan against RVBBIT_HUTCH_URL"
  exit 0
fi

PG_BIN=${PG_BIN:-/usr/lib/postgresql/18/bin}
PGPORT=${PGPORT:-55438}
HUTCH_PORT=${HUTCH_PORT:-8102}
PGHOST=${PGHOST:-/tmp}
PGUSER=${PGUSER:-postgres}
PGDATA=${PGDATA:-/tmp/rvbbit-m8-pg}
RUNDIR=${RUNDIR:-/tmp/rvbbit-m8-demo}
export CC=${CC:-gcc}
export CXX=${CXX:-g++}
export PATH="$PG_BIN:$PATH"
export PGRUST_PGSHAREDIR=${PGRUST_PGSHAREDIR:-/usr/share/postgresql/18}
export PGRUST_TZDIR=${PGRUST_TZDIR:-/usr/share/zoneinfo}

STARTED_MOCK=0
if [[ -z "${RVBBIT_HUTCH_URL:-}" ]]; then
  export RVBBIT_HUTCH_URL="http://127.0.0.1:${HUTCH_PORT}"
  STARTED_MOCK=1
fi

for cmd in gcc g++ curl python3 initdb psql cargo; do
  if ! command -v "$cmd" >/dev/null 2>&1; then
    echo "missing command: $cmd" >&2
    exit 1
  fi
done

echo "=== build ==="
if [[ "$STARTED_MOCK" == 1 ]]; then
  cargo build -p main_main -p hutch_mock --profile fast-profile
else
  cargo build -p main_main --profile fast-profile
fi

mkdir -p "$RUNDIR"
PG_LOG=$RUNDIR/postgres.log
MOCK_LOG=$RUNDIR/mock.log
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

if [[ "$STARTED_MOCK" == 1 ]]; then
  echo "=== hutch_mock on $RVBBIT_HUTCH_URL ==="
  : >"$MOCK_LOG"
  ./target/fast-profile/hutch_mock "$HUTCH_PORT" >"$MOCK_LOG" 2>&1 &
  MOCK_PID=$!
else
  echo "=== using hutch at $RVBBIT_HUTCH_URL ==="
fi

echo "=== initdb $PGDATA ==="
initdb -D "$PGDATA" --no-locale --encoding=UTF8 --auth-local=trust --auth-host=trust -U "$PGUSER" >"$RUNDIR/initdb.log"
ulimit -s 65520
RUST_MIN_STACK=33554432 \
  ./target/fast-profile/postgres \
  -D "$PGDATA" -k "$PGHOST" -p "$PGPORT" \
  -c listen_addresses= -c io_method=sync -c max_stack_depth=60000 \
  >"$PG_LOG" 2>&1 &

ready=0
for _ in $(seq 1 80); do
  if curl -sf "${RVBBIT_HUTCH_URL%/}/stats" >/dev/null && \
     psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -X -q -c 'SELECT 1' >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 0.25
done
if [[ "$ready" != 1 ]]; then
  echo "hutch or postgres did not become ready. postgres log: $PG_LOG" >&2
  exit 1
fi

PSQL=(psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -v ON_ERROR_STOP=1 -X -q)
"${PSQL[@]}" -d postgres -c "DROP DATABASE IF EXISTS rvbbit_m8" -c "CREATE DATABASE rvbbit_m8"

cat >"$RUNDIR/setup.sql" <<SQL
CREATE BACKEND m8 TYPE hutch URL '${RVBBIT_HUTCH_URL}';
CREATE TABLE notes (id int, body text);
INSERT INTO notes
SELECT g, 'row ' || g::text || ' alpha'
FROM generate_series(1, 64) AS g;
CREATE TABLE kinds (kind text PRIMARY KEY);
INSERT INTO kinds VALUES ('keep');
CREATE TABLE docs (kind text, body text);
INSERT INTO docs SELECT 'keep', 'alpha ' || g::text FROM generate_series(1, 40) AS g;
INSERT INTO docs SELECT 'drop', 'alpha ' || g::text FROM generate_series(1, 40) AS g;
ANALYZE notes;
ANALYZE kinds;
ANALYZE docs;
SQL
"${PSQL[@]}" -d rvbbit_m8 -f "$RUNDIR/setup.sql"

echo "=== EXPLAIN: cheap predicate on the scan, semantic cost on the filter ==="
"${PSQL[@]}" -d rvbbit_m8 -c \
  "EXPLAIN SELECT body FROM notes WHERE id > 32 AND body MEANS 'alpha'" \
  | tee "$RUNDIR/explain.txt"

python3 - "$RUNDIR/explain.txt" <<'PY'
import sys
text = open(sys.argv[1]).read()
for needle in (
    "Semantic Filter",
    "Semantic Provider Calls",
    "Semantic HTTP Batches",
    "Semantic USD",
    "Semantic Latency",
    "Semantic Model",
    "Semantic Selectivity",
    "Semantic Total Provider Calls",
    "Semantic Total HTTP Batches",
    "Semantic Total USD",
):
    if needle not in text:
        raise SystemExit(f"missing {needle}")
scan = text.find("Seq Scan")
cheap = text.find("id > 32")
means = text.find("Semantic Filter")
if not (means != -1 and means < scan and scan < cheap):
    raise SystemExit("cheap predicate did not stay under the semantic filter")
print("explain shape ok")
PY

echo "=== EXPLAIN: filter above a selective inner join ==="
"${PSQL[@]}" -d rvbbit_m8 -c \
  "EXPLAIN SELECT d.body FROM docs d JOIN kinds k ON d.kind = k.kind WHERE d.body MEANS 'alpha'" \
  | tee "$RUNDIR/join.txt"
python3 - "$RUNDIR/join.txt" <<'PY'
import sys
lines = open(sys.argv[1]).read().splitlines()
def indent(*labels):
    found = []
    for line in lines:
        if any(label in line for label in labels):
            found.append(len(line) - len(line.lstrip(" ")))
    if not found:
        raise SystemExit(f"missing {' or '.join(labels)}")
    return min(found)
if indent("Semantic Filter") >= indent("Join", "Nested Loop"):
    raise SystemExit("semantic filter stayed under the join")
print("join hoist ok")
PY

echo "=== EXPLAIN: project above LIMIT ==="
"${PSQL[@]}" -d rvbbit_m8 -c \
  "EXPLAIN SELECT body |> RERANK 'alpha' AS score FROM docs LIMIT 5" \
  | tee "$RUNDIR/limit.txt"
python3 - "$RUNDIR/limit.txt" <<'PY'
import sys
lines = open(sys.argv[1]).read().splitlines()
def indent(label):
    for line in lines:
        if label in line:
            return len(line) - len(line.lstrip(" "))
    raise SystemExit(f"missing {label}")
if indent("Semantic Project") >= indent("Limit"):
    raise SystemExit("semantic project stayed under LIMIT")
print("limit hoist ok")
PY

echo "=== BUDGET rejects before any call ==="
set +e
"${PSQL[@]}" -d rvbbit_m8 -c \
  "SELECT count(*) FROM notes WHERE body MEANS 'alpha' WITH BUDGET 0.0000001" \
  >"$RUNDIR/budget.txt" 2>&1
budget_status=$?
set -e
if [[ "$budget_status" == 0 ]]; then
  echo "BUDGET should have failed" >&2
  cat "$RUNDIR/budget.txt" >&2
  exit 1
fi
grep -q "exceeds budget" "$RUNDIR/budget.txt"
grep -q "0.00000010" "$RUNDIR/budget.txt"
# The error is pre-flight: hutch must not have been posted to.
if [[ "$STARTED_MOCK" == 1 ]]; then
  python3 - "${RVBBIT_HUTCH_URL%/}/stats" <<'PY'
import json, sys, urllib.request
stats = json.load(urllib.request.urlopen(sys.argv[1]))
if int(stats.get("calls", 0)) != 0:
    raise SystemExit(f"budget check posted to hutch: {stats}")
print("budget rejected before any call")
PY
fi

echo "=== semantic.max_cost rejects, semantic.cost_check = off runs ==="
set +e
"${PSQL[@]}" -d rvbbit_m8 -c \
  "SET semantic.max_cost = '0.0000001'; SELECT count(*) FROM notes WHERE body MEANS 'alpha'" \
  >"$RUNDIR/maxcost.txt" 2>&1
max_status=$?
set -e
if [[ "$max_status" == 0 ]]; then
  echo "semantic.max_cost should have failed" >&2
  cat "$RUNDIR/maxcost.txt" >&2
  exit 1
fi
grep -q "exceeds budget" "$RUNDIR/maxcost.txt"

"${PSQL[@]}" -d rvbbit_m8 -c \
  "SET semantic.cost_check = off; SET semantic.max_cost = '0.0000001'; SELECT count(*) FROM notes WHERE body MEANS 'alpha'"

echo "=== EXPLAIN ANALYZE ==="
"${PSQL[@]}" -d rvbbit_m8 -c \
  "SET semantic.cost_check = on; SET semantic.max_cost = ''; EXPLAIN ANALYZE SELECT count(*) FROM notes WHERE id > 32 AND body MEANS 'alpha'" \
  | tee "$RUNDIR/analyze.txt"
python3 - "$RUNDIR/analyze.txt" <<'PY'
import sys
text = open(sys.argv[1]).read()
for needle in (
    "Actual Semantic Calls",
    "Actual Cache Hits",
    "Actual HTTP Batches",
    "Actual Semantic USD",
    "Actual Semantic Latency",
    "Actual Semantic Model",
    "Actual Total Calls",
    "Actual Total Cache Hits",
    "Actual Total HTTP Batches",
    "Actual Total USD",
):
    if needle not in text:
        raise SystemExit(f"missing {needle}")
print("analyze ok")
PY

echo "=== done. RVBBIT_HUTCH_URL=$RVBBIT_HUTCH_URL ==="
echo "sample EXPLAIN:"
cat "$RUNDIR/explain.txt"
