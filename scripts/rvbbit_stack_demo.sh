#!/usr/bin/env bash
# M5 clause tail.
#
# No OPENROUTER_API_KEY: run the scripted fake (winners, consensus,
# retry, fallback, budget abort, identical takes). That is the default
# in this environment.
#
# With a key: stack the UFO explanation node (TAKES 3 and two phrasings)
# on two rows, print identical takes, then abort a MEANS call on BUDGET.
# Ports 55435 / 8097 so the tree demo on 55433 / 8098 can stay up.
#
#   bash scripts/rvbbit_stack_demo.sh
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

if [[ -z "${OPENROUTER_API_KEY:-}" ]]; then
  cargo test -p rvbbit_dispatch -p rvbbit_parse --profile fast-profile --offline --lib \
    -- clauses_sql_scripted_fake call_beats_function_beats_guc means_clause_tail_is_apply_clauses plain_means_stays
  echo "stack demo: scripted fake passed (no API key, no upstream calls)"
  echo "export OPENROUTER_API_KEY and rerun to stack the UFO explanation node and abort on BUDGET"
  exit 0
fi

PG_BIN=${PG_BIN:-/usr/lib/postgresql/18/bin}
PGPORT=${PGPORT:-55435}
HUTCH_PORT=${HUTCH_PORT:-8097}
PGHOST=${PGHOST:-/tmp}
PGUSER=${PGUSER:-postgres}
PGDATA=${PGDATA:-/tmp/rvbbit-stack-pg}
RUNDIR=${RUNDIR:-/tmp/rvbbit-stack-demo}
DECISION=${HUTCH_DECISION_MODEL:-typesafe/jev-1.14}
export HUTCH_DECISION_MODEL="$DECISION"
export RVBBIT_HUTCH_URL="http://127.0.0.1:${HUTCH_PORT}"
export CC=${CC:-gcc}
export CXX=${CXX:-g++}
export PATH="$PG_BIN:$PATH"
export PGRUST_PGSHAREDIR=${PGRUST_PGSHAREDIR:-/usr/share/postgresql/18}
export PGRUST_TZDIR=${PGRUST_TZDIR:-/usr/share/zoneinfo}

for cmd in gcc g++ curl python3 initdb psql cargo; do
  if ! command -v "$cmd" >/dev/null 2>&1; then
    echo "missing command: $cmd" >&2
    exit 1
  fi
done

port_open() {
  (echo >/dev/tcp/127.0.0.1/"$1") >/dev/null 2>&1
}
if port_open "$HUTCH_PORT"; then
  echo "127.0.0.1:$HUTCH_PORT is already in use" >&2
  exit 1
fi
if [[ -S "$PGHOST/.s.PGSQL.$PGPORT" ]]; then
  echo "Postgres socket $PGHOST/.s.PGSQL.$PGPORT is already in use" >&2
  exit 1
fi

echo "=== build ==="
cargo build -p main_main -p rvbbit_hutch_lite --profile fast-profile

mkdir -p "$RUNDIR"
HUTCH_LOG=$RUNDIR/hutch.log
PG_LOG=$RUNDIR/postgres.log
cleanup() {
  if [[ -n "${HUTCH_PID:-}" ]]; then
    kill "$HUTCH_PID" >/dev/null 2>&1 || true
  fi
  if [[ -f "$PGDATA/postmaster.pid" ]]; then
    kill "$(head -n 1 "$PGDATA/postmaster.pid")" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT
rm -rf "$PGDATA"

echo "=== hutch-lite on 127.0.0.1:$HUTCH_PORT decision=$DECISION ==="
: >"$HUTCH_LOG"
./target/fast-profile/hutch_lite "$HUTCH_PORT" >"$HUTCH_LOG" 2>&1 &
HUTCH_PID=$!
initdb -D "$PGDATA" --no-locale --encoding=UTF8 --auth-local=trust --auth-host=trust -U "$PGUSER" >"$RUNDIR/initdb.log"
ulimit -s 65520
RUST_MIN_STACK=33554432 \
  ./target/fast-profile/postgres \
  -D "$PGDATA" -k "$PGHOST" -p "$PGPORT" \
  -c listen_addresses= -c io_method=sync -c max_stack_depth=60000 \
  >"$PG_LOG" 2>&1 &

ready=0
for _ in $(seq 1 50); do
  if curl -sf "http://127.0.0.1:$HUTCH_PORT/stats" >/dev/null && \
     psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -X -q -c 'SELECT 1' >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 0.2
done
if [[ "$ready" != 1 ]]; then
  echo "hutch or postgres did not become ready" >&2
  exit 1
fi

PSQL=(psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -v ON_ERROR_STOP=1 -X)
"${PSQL[@]}" -d postgres -c "DROP DATABASE IF EXISTS rvbbit_stack" -c "CREATE DATABASE rvbbit_stack"

cat >"$RUNDIR/setup.sql" <<SQL
CREATE BACKEND live TYPE hutch URL 'http://127.0.0.1:${HUTCH_PORT}';
CREATE MODEL jev ON BACKEND live NAME '${DECISION//\'/\'\'}';
CREATE DECISION TREE ufo_stack (notes text) MODEL jev AS
  ASK 'Does the report describe a light or an object in the sky?'
    YES: CHOOSE 'What is the most likely explanation?'
      TAKES 3
      PHRASINGS (
        'What is the most likely explanation?',
        'Which ordinary cause fits this sky report?'
      )
      WHEN 'aircraft' MEANING 'an airplane, helicopter, or other aircraft' THEN 'aircraft'
      WHEN 'satellite' MEANING 'a satellite or the International Space Station' THEN 'satellite'
      WHEN 'astronomical' MEANING 'a star, planet, meteor, or other astronomical object' THEN 'astronomical'
      WHEN 'unexplained' MEANING 'not a known aircraft, satellite, or astronomical object' THEN 'unexplained'
    NO: 'not a sky object';
CREATE TABLE ufo (
  id int PRIMARY KEY,
  source_id int,
  city text,
  state text,
  shape text,
  comments text
);
\copy ufo FROM '${ROOT}/crates/rvbbit/fixtures/ufo_sightings.csv' WITH (FORMAT csv, HEADER true)
CREATE TABLE ufo_sample AS
  SELECT id, comments AS notes FROM ufo WHERE id <= 2 ORDER BY id;
SQL

echo "=== two-row UFO stack (explanation node: 3 takes x 2 phrasings) ==="
"${PSQL[@]}" -d rvbbit_stack -f "$RUNDIR/setup.sql"
"${PSQL[@]}" -d rvbbit_stack -c "SELECT id, rvbbit.decide('ufo_stack', notes) FROM ufo_sample ORDER BY id"
"${PSQL[@]}" -d rvbbit_stack -c \
  "SELECT node, identical, count(*) AS calls, round(avg(score)::numeric, 3) AS score, sum(cost) AS cost
   FROM rvbbit.calls GROUP BY node, identical ORDER BY node, identical"

echo "=== BUDGET abort (real cost, cap far below one call) ==="
set +e
BUDGET_OUT=$("${PSQL[@]}" -d rvbbit_stack -c \
  "SELECT notes MEANS 'a light or an object in the sky' BUDGET 0.0000001 FROM ufo_sample LIMIT 1" 2>&1)
BUDGET_RC=$?
set -e
echo "$BUDGET_OUT"
if [[ "$BUDGET_RC" -eq 0 ]]; then
  echo "BUDGET did not abort" >&2
  exit 1
fi
if ! grep -q "budget exceeded" <<<"$BUDGET_OUT"; then
  echo "BUDGET error did not name the cap" >&2
  exit 1
fi
echo "=== done. explanation node stacked; BUDGET aborted. ==="
