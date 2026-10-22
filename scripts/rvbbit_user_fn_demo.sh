#!/usr/bin/env bash
# User semantic functions against hutch_mock.
# A 1-arg template filters and projects in batches, a decision-tree
# function returns the leaf, and a pipe function inlines into Semantic
# Project. No OpenRouter key.
#
# Live, against hutch-lite (spends provider calls):
#   RVBBIT_USER_FN_LIVE=1 bash scripts/rvbbit_user_fn_demo.sh
# Requires OPENROUTER_API_KEY. The key is not printed. Mock match counts
# are not required in live mode; the queries and the batching checks are.
#
# Ports: Postgres 55441, mock 8105. The spike on 55432 / 8099 is not used.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

PG_BIN=${PG_BIN:-/usr/lib/postgresql/18/bin}
PGPORT=${PGPORT:-55441}
HUTCH_PORT=${HUTCH_PORT:-8105}
PGHOST=${PGHOST:-/tmp}
PGUSER=${PGUSER:-postgres}
PGDATA=${PGDATA:-/tmp/rvbbit-user-fn-pg}
RUNDIR=${RUNDIR:-/tmp/rvbbit-user-fn}
ROWS=${ROWS:-40}
export CC=${CC:-gcc}
export CXX=${CXX:-g++}
export PATH="$PG_BIN:$PATH"
export PGRUST_PGSHAREDIR=${PGRUST_PGSHAREDIR:-/usr/share/postgresql/18}
export PGRUST_TZDIR=${PGRUST_TZDIR:-/usr/share/zoneinfo}
export RVBBIT_HUTCH_URL="http://127.0.0.1:${HUTCH_PORT}"

if [[ "$PGPORT" == "55432" || "$HUTCH_PORT" == "8099" ]]; then
  echo "Refusing spike ports 55432 / 8099." >&2
  exit 1
fi

HUTCH_CRATE=hutch_mock
HUTCH_BIN=hutch_mock
if [[ "${RVBBIT_USER_FN_LIVE:-}" == "1" ]]; then
  if [[ -z "${OPENROUTER_API_KEY:-}" ]]; then
    echo "RVBBIT_USER_FN_LIVE=1 needs OPENROUTER_API_KEY" >&2
    exit 1
  fi
  HUTCH_CRATE=rvbbit_hutch_lite
  HUTCH_BIN=hutch_lite
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
cargo build -p main_main -p "$HUTCH_CRATE" --profile fast-profile

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

echo "=== $HUTCH_BIN on 127.0.0.1:$HUTCH_PORT ==="
: >"$MOCK_LOG"
./target/fast-profile/"$HUTCH_BIN" "$HUTCH_PORT" >"$MOCK_LOG" 2>&1 &
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

PSQL=(psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -v ON_ERROR_STOP=1 -X -q)

echo "=== bootstrap means, then user functions ==="
"${PSQL[@]}" <<SQL
SELECT 'a large hairy biped crossed the road' MEANS 'biped';

CREATE SEMANTIC FUNCTION is_biped(notes text) RETURNS bool
  AS 'large hairy biped'
  USING MODEL default
  DEFAULT CLAUSES ('WITH TAKES 1');

CREATE SEMANTIC FUNCTION how_biped(notes text) RETURNS score
  AS 'large hairy biped'
  USING MODEL default;

CREATE SEMANTIC FUNCTION kind_of(notes text) RETURNS label
  LABELS ('biped', 'other')
  USING MODEL default;

CREATE DECISION TREE sighting (notes text) MODEL default AS
  ASK 'large hairy biped' YES 'cryptid' NO 'other';

CREATE SEMANTIC FUNCTION sighting_of(notes text) RETURNS text
  AS DECISION TREE sighting
  USING MODEL default;

CREATE SEMANTIC FUNCTION creature_of(notes text) RETURNS text
  AS PIPE \$\$ {{notes}} |> EXTRACT creature \$\$;
SQL

echo "=== load $ROWS notes ==="
"${PSQL[@]}" <<SQL
DROP TABLE IF EXISTS notes;
CREATE TABLE notes (id int, body text);
INSERT INTO notes
SELECT g,
       CASE WHEN g % 5 = 0
            THEN 'invoice ' || g::text
            ELSE 'a large hairy biped crossed row ' || g::text
       END
FROM generate_series(1, ${ROWS}) AS g;
SQL

echo "=== EXPLAIN filter ==="
FILTER_PLAN=$("${PSQL[@]}" -c "EXPLAIN SELECT count(*) FROM notes WHERE rvbbit.is_biped(body)")
echo "$FILTER_PLAN"
printf '%s\n' "$FILTER_PLAN" | grep -q "Semantic Filter"
printf '%s\n' "$FILTER_PLAN" | grep -q "Semantic USD"

curl -sf -X POST "http://127.0.0.1:$HUTCH_PORT/reset" >/dev/null
echo "=== filter ==="
FILTER_N=$("${PSQL[@]}" -tAc "SELECT count(*) FROM notes WHERE rvbbit.is_biped(body)")
echo "filter count=$FILTER_N"
STATS=$(curl -sf "http://127.0.0.1:$HUTCH_PORT/stats")
echo "$STATS"
STATS="$STATS" ROWS="$ROWS" FILTER_N="$FILTER_N" python3 - <<'PY'
import json, os, sys
stats = json.loads(os.environ["STATS"])
rows = int(os.environ["ROWS"])
calls = stats["calls"]
got = int(os.environ["FILTER_N"])
expect = rows - rows // 5
live = os.environ.get("RVBBIT_USER_FN_LIVE") == "1"
print(f"filter calls={calls} rows={stats['rows']} max_batch={stats['max_batch']} matches={got}")
if not live and got != expect:
    sys.exit(f"expected {expect} biped rows, got {got}")
if calls >= rows:
    sys.exit(f"filter was not batched: {calls} calls for {rows} rows")
if stats["max_batch"] > 32:
    sys.exit(f"batch larger than 32: {stats['max_batch']}")
print("filter batched")
PY

echo "=== EXPLAIN project ==="
PROJECT_PLAN=$("${PSQL[@]}" -c "EXPLAIN SELECT rvbbit.is_biped(body) FROM notes")
echo "$PROJECT_PLAN"
printf '%s\n' "$PROJECT_PLAN" | grep -q "Semantic Project"

curl -sf -X POST "http://127.0.0.1:$HUTCH_PORT/reset" >/dev/null
echo "=== project ==="
PROJ_N=$("${PSQL[@]}" -tAc "SELECT count(*) FROM (SELECT rvbbit.is_biped(body) AS y FROM notes) s WHERE y")
echo "project trues=$PROJ_N"
STATS=$(curl -sf "http://127.0.0.1:$HUTCH_PORT/stats")
echo "$STATS"
STATS="$STATS" ROWS="$ROWS" PROJ_N="$PROJ_N" python3 - <<'PY'
import json, os, sys
stats = json.loads(os.environ["STATS"])
rows = int(os.environ["ROWS"])
calls = stats["calls"]
got = int(os.environ["PROJ_N"])
expect = rows - rows // 5
live = os.environ.get("RVBBIT_USER_FN_LIVE") == "1"
print(f"project calls={calls} rows={stats['rows']} max_batch={stats['max_batch']} trues={got}")
if not live and got != expect:
    sys.exit(f"expected {expect} true projections, got {got}")
if calls >= rows:
    sys.exit(f"project was not batched: {calls} calls for {rows} rows")
print("project batched")
PY

echo "=== score and label ==="
"${PSQL[@]}" -c "SELECT rvbbit.how_biped(body) > 0.5 AS yes, rvbbit.kind_of(body) FROM notes WHERE id IN (1, 5) ORDER BY id"

echo "=== decision tree ==="
curl -sf -X POST "http://127.0.0.1:$HUTCH_PORT/reset" >/dev/null
TREE_N=$("${PSQL[@]}" -tAc "SELECT count(*) FROM (SELECT rvbbit.sighting_of(body) AS y FROM notes) s WHERE y = 'cryptid'")
echo "tree cryptid=$TREE_N"
STATS=$(curl -sf "http://127.0.0.1:$HUTCH_PORT/stats")
echo "$STATS"
STATS="$STATS" ROWS="$ROWS" TREE_N="$TREE_N" python3 - <<'PY'
import json, os, sys
stats = json.loads(os.environ["STATS"])
rows = int(os.environ["ROWS"])
got = int(os.environ["TREE_N"])
expect = rows - rows // 5
calls = stats["calls"]
live = os.environ.get("RVBBIT_USER_FN_LIVE") == "1"
print(f"tree calls={calls} cryptid={got}")
if not live and got != expect:
    sys.exit(f"expected {expect} cryptid leaves, got {got}")
if calls >= rows:
    sys.exit(f"tree was not batched: {calls} calls for {rows} rows")
print("tree batched")
PY

echo "=== pipe workflow ==="
PIPE_PLAN=$("${PSQL[@]}" -c "EXPLAIN SELECT rvbbit.creature_of(body) FROM notes")
echo "$PIPE_PLAN"
printf '%s\n' "$PIPE_PLAN" | grep -q "Semantic Project"
curl -sf -X POST "http://127.0.0.1:$HUTCH_PORT/reset" >/dev/null
"${PSQL[@]}" -c "SELECT count(*) FROM (SELECT rvbbit.creature_of(body) AS y FROM notes) s"
STATS=$(curl -sf "http://127.0.0.1:$HUTCH_PORT/stats")
echo "$STATS"
STATS="$STATS" ROWS="$ROWS" python3 - <<'PY'
import json, os, sys
stats = json.loads(os.environ["STATS"])
rows = int(os.environ["ROWS"])
calls = stats["calls"]
print(f"pipe calls={calls} rows={stats['rows']} max_batch={stats['max_batch']}")
if calls >= rows:
    sys.exit(f"pipe was not batched: {calls} calls for {rows} rows")
if calls < 1:
    sys.exit("pipe made no HTTP call")
print("pipe batched")
PY

echo "=== pipe syntax ==="
"${PSQL[@]}" -c "EXPLAIN SELECT body |> is_biped FROM notes WHERE id < 3"

echo "user semantic function demo passed"
