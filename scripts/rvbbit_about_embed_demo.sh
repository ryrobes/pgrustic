#!/usr/bin/env bash
# ABOUT on embeddings, against hutch_mock.
# Cosine of a cached vector, default threshold 0.30, semantic_similarity,
# and an inner-join form of the same predicate. No OpenRouter key.
#
# Live, against hutch-lite (spends embedding calls):
#   RVBBIT_ABOUT_LIVE=1 bash scripts/rvbbit_about_embed_demo.sh
# Requires OPENROUTER_API_KEY. The key is not printed.
#
# Ports: Postgres 55442, mock 8106. The spike on 55432 / 8099 is not used.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

PG_BIN=${PG_BIN:-/usr/lib/postgresql/18/bin}
PGPORT=${PGPORT:-55442}
HUTCH_PORT=${HUTCH_PORT:-8106}
PGHOST=${PGHOST:-/tmp}
PGUSER=${PGUSER:-postgres}
PGDATA=${PGDATA:-/tmp/rvbbit-about-pg}
RUNDIR=${RUNDIR:-/tmp/rvbbit-about}
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
if [[ "${RVBBIT_ABOUT_LIVE:-}" == "1" ]]; then
  if [[ -z "${OPENROUTER_API_KEY:-}" ]]; then
    echo "RVBBIT_ABOUT_LIVE=1 needs OPENROUTER_API_KEY" >&2
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

echo "=== bootstrap ==="
"${PSQL[@]}" -c "SELECT 'a large hairy biped crossed the road' MEANS 'biped'"

echo "=== load $ROWS notes ==="
"${PSQL[@]}" <<SQL
DROP TABLE IF EXISTS notes;
CREATE TABLE notes (id int, body text);
INSERT INTO notes
SELECT g,
       CASE WHEN g % 5 = 0
            THEN 'invoice ' || g::text || ' for services rendered'
            ELSE 'a large hairy biped crossed row ' || g::text
       END
FROM generate_series(1, ${ROWS}) AS g;
CREATE TABLE queries (q text);
INSERT INTO queries VALUES ('biped');
SQL

echo "=== EXPLAIN filter ==="
FILTER_PLAN=$("${PSQL[@]}" -c "EXPLAIN SELECT count(*) FROM notes WHERE body ABOUT 'biped'")
echo "$FILTER_PLAN"
printf '%s\n' "$FILTER_PLAN" | grep -q "Semantic Filter"
printf '%s\n' "$FILTER_PLAN" | grep -q "Semantic USD"
printf '%s\n' "$FILTER_PLAN" | grep -q "text-embedding-3-small"

curl -sf -X POST "http://127.0.0.1:$HUTCH_PORT/reset" >/dev/null
echo "=== filter ==="
FILTER_N=$("${PSQL[@]}" -tAc "SELECT count(*) FROM notes WHERE body ABOUT 'biped'")
echo "filter count=$FILTER_N"
STATS=$(curl -sf "http://127.0.0.1:$HUTCH_PORT/stats")
echo "$STATS"
STATS="$STATS" ROWS="$ROWS" FILTER_N="$FILTER_N" python3 - <<'PY'
import json, os, sys
stats = json.loads(os.environ["STATS"])
rows = int(os.environ["ROWS"])
calls = stats.get("calls", stats.get("requests", 0))
stat_rows = stats.get("rows", 0)
max_batch = stats.get("max_batch") or 0
got = int(os.environ["FILTER_N"])
expect = rows - rows // 5
print(f"filter calls={calls} rows={stat_rows} max_batch={max_batch} matches={got}")
if got != expect:
    sys.exit(f"expected {expect} biped rows, got {got}")
if calls >= rows:
    sys.exit(f"filter was not batched: {calls} calls for {rows} rows")
if max_batch > 33:
    sys.exit(f"batch larger than 32 rows plus the query: {max_batch}")
PY

echo "=== cost ==="
COST=$("${PSQL[@]}" -tAc "SELECT COALESCE(sum(cost), 0) FROM rvbbit.calls WHERE op = 'embed'")
echo "embed cost=$COST"
python3 - <<PY
cost = float("$COST")
if cost <= 0:
    raise SystemExit(f"expected embed cost in rvbbit.calls, got {cost}")
PY

echo "=== similarity ==="
SIM=$("${PSQL[@]}" -tAc "SELECT rvbbit.semantic_similarity(body, 'biped') FROM notes WHERE id = 1")
INV=$("${PSQL[@]}" -tAc "SELECT rvbbit.semantic_similarity(body, 'biped') FROM notes WHERE id = 5")
echo "biped score=$SIM invoice score=$INV"
python3 - <<PY
sim = float("$SIM")
inv = float("$INV")
if sim < 0.30:
    raise SystemExit(f"related cosine {sim} is below 0.30")
if inv >= 0.30:
    raise SystemExit(f"invoice cosine {inv} is not below 0.30")
PY

echo "=== EXPLAIN similarity project ==="
PROJ=$("${PSQL[@]}" -c "EXPLAIN SELECT rvbbit.semantic_similarity(body, 'biped') FROM notes")
echo "$PROJ"
printf '%s\n' "$PROJ" | grep -q "Semantic Project"

echo "=== similarity join ==="
JOIN_PLAN=$("${PSQL[@]}" -c "EXPLAIN SELECT count(*) FROM notes n, queries q WHERE n.body ABOUT q.q")
echo "$JOIN_PLAN"
printf '%s\n' "$JOIN_PLAN" | grep -q "Semantic Filter"
JOIN_N=$("${PSQL[@]}" -tAc "SELECT count(*) FROM notes n, queries q WHERE n.body ABOUT q.q")
echo "join count=$JOIN_N"
if [[ "$JOIN_N" != "$FILTER_N" ]]; then
  echo "join count $JOIN_N != filter count $FILTER_N" >&2
  exit 1
fi

echo "about embedding demo passed"
