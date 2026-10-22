#!/usr/bin/env bash
# Semantic Project columns mixed with ordinary and wrapped columns.
# The scan target list used to share TargetEntry nodes with the Result,
# so a kept column was written onto a semantic column.
#
# Ports: Postgres 55450, mock 8110. The spike on 55432 / 8099 is not used.
# Run with `env -u PGPORT -u HUTCH_PORT`. No OpenRouter key.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

PG_BIN=${PG_BIN:-/usr/lib/postgresql/18/bin}
PGPORT=${PGPORT:-55450}
HUTCH_PORT=${HUTCH_PORT:-8110}
PGHOST=${PGHOST:-/tmp}
PGUSER=${PGUSER:-postgres}
PGDATA=${PGDATA:-/tmp/rvbbit-project-mix-pg}
RUNDIR=${RUNDIR:-/tmp/rvbbit-project-mix}
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

PSQL=(psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -v ON_ERROR_STOP=1 -X -q -A -t -F '|')

echo "=== bootstrap and rows ==="
"${PSQL[@]}" <<'SQL'
SELECT 'a large hairy biped crossed the road' MEANS 'biped';

CREATE SEMANTIC FUNCTION credibility(body text) RETURNS score
  AS 'credible sighting'
  USING MODEL default;

CREATE SEMANTIC FUNCTION is_credible_sighting(body text) RETURNS bool
  AS 'credible sighting'
  USING MODEL default;

DROP TABLE IF EXISTS bf;
CREATE TABLE bf (id int, class text, n numeric, body text);
INSERT INTO bf VALUES
  (1, 'cryptid', 1.25, 'a large hairy biped crossed the road and it was a credible sighting'),
  (2, 'paper', 2.5, 'invoice 12 for services rendered');
SQL

echo "=== mixed round and pipe ==="
MIX=$("${PSQL[@]}" -c "SELECT id, class, round(rvbbit.credibility(body)::numeric, 2), body |> is_credible_sighting FROM bf ORDER BY id")
echo "$MIX"

echo "=== classify then left ==="
MID=$("${PSQL[@]}" -c "SELECT id, CLASSIFY(body INTO 'biped', 'invoice'), left(body, 120) FROM bf ORDER BY id")
echo "$MID"

echo "=== classify first ==="
FIRST=$("${PSQL[@]}" -c "SELECT CLASSIFY(body INTO 'biped', 'invoice'), id, left(body, 12) FROM bf ORDER BY id")
echo "$FIRST"

echo "=== left first, classify last ==="
LAST=$("${PSQL[@]}" -c "SELECT left(body, 12), id, n, CLASSIFY(body INTO 'biped', 'invoice') FROM bf ORDER BY id")
echo "$LAST"

echo "=== wrapped score first ==="
SCORE=$("${PSQL[@]}" -c "SELECT round(rvbbit.credibility(body)::numeric, 2), id, n FROM bf ORDER BY id")
echo "$SCORE"

echo "=== pipe in the middle ==="
PIPE=$("${PSQL[@]}" -c "SELECT id, body |> is_credible_sighting, n, class FROM bf ORDER BY id")
echo "$PIPE"

echo "=== bare alias ==="
ALIAS=$("${PSQL[@]}" -c "SELECT p FROM (SELECT body |> is_credible_sighting p FROM bf ORDER BY id) s")
echo "$ALIAS"

echo "=== CLASSIFY AS label list ==="
ASLIST=$("${PSQL[@]}" -c "SELECT CLASSIFY(body AS ('biped', 'invoice')), left(body, 12) FROM bf ORDER BY id")
echo "$ASLIST"

echo "=== unanalyzed row count ==="
"${PSQL[@]}" <<'SQL'
DROP TABLE IF EXISTS raw12;
CREATE TABLE raw12 (id int, body text);
INSERT INTO raw12
SELECT g, 'a large hairy biped crossed row ' || g::text
FROM generate_series(1, 12) AS g;
SQL
PLAN=$("${PSQL[@]}" -c "EXPLAIN SELECT count(*) FROM raw12 WHERE rvbbit.is_credible_sighting(body)")
echo "$PLAN"

MIX="$MIX" MID="$MID" FIRST="$FIRST" LAST="$LAST" SCORE="$SCORE" PIPE="$PIPE" ALIAS="$ALIAS" ASLIST="$ASLIST" PLAN="$PLAN" python3 - <<'PY'
import os, sys

def rows(text):
    return [ln.split("|") for ln in text.splitlines() if ln.strip()]

mix = rows(os.environ["MIX"])
assert mix == [
    ["1", "cryptid", "0.82", "t"],
    ["2", "paper", "0.15", "f"],
], mix

mid = rows(os.environ["MID"])
assert mid[0][0] == "1" and mid[0][1] == "biped", mid
assert mid[0][2].startswith("a large hairy biped"), mid
assert mid[1][1] == "invoice", mid
assert "invoice 12" in mid[1][2], mid
assert mid[0][1] != mid[0][2], mid

first = rows(os.environ["FIRST"])
assert first == [
    ["biped", "1", "a large hair"],
    ["invoice", "2", "invoice 12 f"],
], first

last = rows(os.environ["LAST"])
assert last == [
    ["a large hair", "1", "1.25", "biped"],
    ["invoice 12 f", "2", "2.5", "invoice"],
], last

score = rows(os.environ["SCORE"])
assert score == [
    ["0.82", "1", "1.25"],
    ["0.15", "2", "2.5"],
], score

pipe = rows(os.environ["PIPE"])
assert pipe == [
    ["1", "t", "1.25", "cryptid"],
    ["2", "f", "2.5", "paper"],
], pipe

alias = rows(os.environ["ALIAS"])
assert alias == [["t"], ["f"]], alias

aslist = rows(os.environ["ASLIST"])
assert aslist[0][0] == "biped" and aslist[0][1] == "a large hair", aslist
assert aslist[1][0] == "invoice", aslist

calls = [
    ln.split(":", 1)[1].strip()
    for ln in os.environ["PLAN"].splitlines()
    if "Semantic Provider Calls" in ln
]
print("provider call estimates", calls)
if not calls:
    sys.exit("EXPLAIN has no Semantic Provider Calls")
for value in calls:
    if abs(float(value) - 12.0) > 0.01:
        sys.exit(f"expected 12 provider calls for 12 unanalyzed rows, got {value}")
print("project mix passed")
PY

echo "project mix passed"
