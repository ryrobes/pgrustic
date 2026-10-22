#!/usr/bin/env bash
# Build pgrustic + hutch-lite and run the bigfoot and UFO decision trees
# on an 8-row slice of the committed fixtures.
#
# Same machine setup as scripts/rvbbit_live_demo.sh. Same ports
# (Postgres 55433, hutch 8098); this script exits if those are taken.
# Do not run the two demos at once.
#
# Required: OPENROUTER_API_KEY (never printed, never written to SQL).
# Optional: HUTCH_DECISION_MODEL (default typesafe/jev-1.14).
#
# Worst case is 40 provider calls: 3 bigfoot levels and 2 UFO levels,
# 8 rows each. The script aborts above 60. The rerun must not add
# provider calls or cost.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

PG_BIN=${PG_BIN:-/usr/lib/postgresql/18/bin}
PGPORT=${PGPORT:-55433}
HUTCH_PORT=${HUTCH_PORT:-8098}
PGHOST=${PGHOST:-/tmp}
PGUSER=${PGUSER:-postgres}
PGDATA=${PGDATA:-/tmp/rvbbit-tree-pg}
RUNDIR=${RUNDIR:-/tmp/rvbbit-tree-demo}
DECISION=${HUTCH_DECISION_MODEL:-typesafe/jev-1.14}
export HUTCH_DECISION_MODEL="$DECISION"
# rvbbit.calls and the bootstrap backend use this URL.
export RVBBIT_HUTCH_URL="http://127.0.0.1:${HUTCH_PORT}"
export CC=${CC:-gcc}
export CXX=${CXX:-g++}
export PATH="$PG_BIN:$PATH"
export PGRUST_PGSHAREDIR=${PGRUST_PGSHAREDIR:-/usr/share/postgresql/18}
export PGRUST_TZDIR=${PGRUST_TZDIR:-/usr/share/zoneinfo}

if [[ -z "${OPENROUTER_API_KEY:-}" ]]; then
  echo "OPENROUTER_API_KEY is not set. Export it in this shell; this script does not print it." >&2
  exit 1
fi

for cmd in gcc g++ curl python3 initdb psql cargo; do
  if ! command -v "$cmd" >/dev/null 2>&1; then
    echo "missing command: $cmd" >&2
    echo "See docs/rvbbit/HUTCH_LITE.md for the Ubuntu package list." >&2
    exit 1
  fi
done

if ! printf '#include <algorithm>\n' | g++ -fsyntax-only -x c++ - >/dev/null 2>&1; then
  echo "g++ cannot compile <algorithm>. Install g++ (package build-essential)." >&2
  exit 1
fi

LIBDIR=/usr/lib/x86_64-linux-gnu
if [[ ! -e "$LIBDIR/libstdc++.so" ]]; then
  echo "Missing $LIBDIR/libstdc++.so. The link needs the unversioned name." >&2
  echo "As root, then rerun this script:" >&2
  echo "  ln -s libstdc++.so.6 $LIBDIR/libstdc++.so" >&2
  exit 1
fi

if [[ ! -d "$PGRUST_PGSHAREDIR" ]]; then
  echo "Missing $PGRUST_PGSHAREDIR. Install postgresql-18." >&2
  exit 1
fi

port_open() {
  (echo >/dev/tcp/127.0.0.1/"$1") >/dev/null 2>&1
}

if [[ -f "$PGDATA/postmaster.pid" ]]; then
  old=$(head -n 1 "$PGDATA/postmaster.pid")
  kill "$old" >/dev/null 2>&1 || true
  for _ in $(seq 1 25); do
    kill -0 "$old" >/dev/null 2>&1 || break
    sleep 0.2
  done
fi

if port_open "$HUTCH_PORT"; then
  echo "127.0.0.1:$HUTCH_PORT is already in use. Stop that process or set HUTCH_PORT." >&2
  exit 1
fi
if [[ -S "$PGHOST/.s.PGSQL.$PGPORT" ]]; then
  echo "Postgres socket $PGHOST/.s.PGSQL.$PGPORT is already in use. Stop that server or set PGPORT." >&2
  echo "The spike server on 55432 is not this port." >&2
  exit 1
fi

echo "=== build (CC=$CC CXX=$CXX) ==="
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

echo "=== hutch-lite on 127.0.0.1:$HUTCH_PORT decision=$DECISION api_key=set ==="
: >"$HUTCH_LOG"
./target/fast-profile/hutch_lite "$HUTCH_PORT" >"$HUTCH_LOG" 2>&1 &
HUTCH_PID=$!

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
for _ in $(seq 1 50); do
  if curl -sf "http://127.0.0.1:$HUTCH_PORT/stats" >/dev/null && \
     psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -X -q -c 'SELECT 1' >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 0.2
done
if [[ "$ready" != 1 ]]; then
  echo "hutch or postgres did not become ready. Logs: $HUTCH_LOG $PG_LOG" >&2
  exit 1
fi

if grep -q -F -- "$OPENROUTER_API_KEY" "$HUTCH_LOG"; then
  echo "hutch log contains the API key; refusing to continue" >&2
  exit 1
fi
echo "hutch startup: $(head -n 1 "$HUTCH_LOG")"

PSQL=(psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -v ON_ERROR_STOP=1 -X)
"${PSQL[@]}" -d postgres -c "DROP DATABASE IF EXISTS rvbbit_tree" -c "CREATE DATABASE rvbbit_tree"

cat >"$RUNDIR/setup.sql" <<SQL
CREATE BACKEND live TYPE hutch URL 'http://127.0.0.1:${HUTCH_PORT}';
CREATE MODEL jev ON BACKEND live NAME '${DECISION//\'/\'\'}';
CREATE DECISION TREE bigfoot_triage (notes text) MODEL jev AS
  ASK 'Does the report describe an unknown upright hairy figure, tracks, or knocks?'
    YES: CHOOSE 'Which report class fits?'
      WHEN 'class a' MEANING 'a witness saw a large upright figure' THEN
        ASK 'Does this class A report need a follow-up interview?'
          YES: TRUE
          NO: FALSE
      WHEN 'class b' MEANING 'tracks, knocks, or other signs without a clear sighting' THEN 'class b'
      WHEN 'class c' MEANING 'secondhand or very weak' THEN 'class c'
    NO: 'not a sighting';
CREATE DECISION TREE ufo_triage (notes text) MODEL jev AS
  ASK 'Does the report describe a light or an object in the sky?'
    YES: CHOOSE 'What is the most likely explanation?'
      WHEN 'aircraft' MEANING 'an airplane, helicopter, or other aircraft' THEN 'aircraft'
      WHEN 'satellite' MEANING 'a satellite or the International Space Station' THEN 'satellite'
      WHEN 'astronomical' MEANING 'a star, planet, meteor, or other astronomical object' THEN 'astronomical'
      WHEN 'unexplained' MEANING 'not a known aircraft, satellite, or astronomical object' THEN 'unexplained'
    NO: 'not a sky object';
CREATE TABLE bigfoot (
  id int PRIMARY KEY,
  bfroid int,
  class text,
  title text,
  state text,
  county text,
  season text,
  observed text,
  environment text
);
\copy bigfoot FROM '${ROOT}/crates/rvbbit/fixtures/bigfoot_sightings.csv' WITH (FORMAT csv, HEADER true)
CREATE TABLE sightings AS
  SELECT id, observed AS notes, class, title, state
  FROM bigfoot
  WHERE id <= 8
  ORDER BY id;
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
  SELECT id, comments AS notes, city, state, shape
  FROM ufo
  WHERE id <= 8
  ORDER BY id;
SQL

cat >"$RUNDIR/queries.sql" <<'SQL'
\pset pager off
-- 8-row copies. A boolean leaf is the text true or false.
-- class a continues into the follow-up question. class b and class c
-- are the leaf labels. USING / COMBINE is not used here: each extra
-- model would be another provider call per row.
SELECT id, class, rvbbit.decide('bigfoot_triage', notes) AS triage
FROM sightings
ORDER BY id;
SELECT id, shape, rvbbit.decide('ufo_triage', notes) AS triage
FROM ufo_sample
ORDER BY id;
SELECT cache, op, node,
       count(*) AS calls,
       round(avg(score)::numeric, 3) AS score,
       sum(cost) AS cost,
       avg(provider_latency_ms) AS provider_ms
FROM rvbbit.calls
GROUP BY cache, op, node
ORDER BY cache, op, node;
SQL

echo "=== catalog and fixtures ==="
"${PSQL[@]}" -d rvbbit_tree -f "$RUNDIR/setup.sql"

echo "=== EXPLAIN (no execution) ==="
"${PSQL[@]}" -d rvbbit_tree -c "EXPLAIN SELECT id, rvbbit.decide('bigfoot_triage', notes) FROM sightings ORDER BY id" \
  | tee "$RUNDIR/explain.txt"
if ! grep -q "Decision Tree" "$RUNDIR/explain.txt"; then
  echo "EXPLAIN did not name the node Decision Tree" >&2
  exit 1
fi
if ! grep -q "Batch Size" "$RUNDIR/explain.txt"; then
  echo "EXPLAIN did not show Batch Size" >&2
  exit 1
fi

curl -sf -X POST "http://127.0.0.1:$HUTCH_PORT/reset" >/dev/null
echo "=== pass 1 ==="
"${PSQL[@]}" -d rvbbit_tree -f "$RUNDIR/queries.sql"
S1=$(curl -sf "http://127.0.0.1:$HUTCH_PORT/stats")
echo "=== after pass 1 ==="
echo "GET /stats $S1"

echo "=== pass 2 (expect cache hits, no new provider calls) ==="
"${PSQL[@]}" -d rvbbit_tree -f "$RUNDIR/queries.sql"
S2=$(curl -sf "http://127.0.0.1:$HUTCH_PORT/stats")
echo "=== after pass 2 ==="
echo "GET /stats $S2"

S1="$S1" S2="$S2" python3 - <<'PY'
import json, os, sys
a = json.loads(os.environ["S1"])
b = json.loads(os.environ["S2"])
print(
    f"provider_calls {a['provider_calls']} -> {b['provider_calls']}; "
    f"hits {a['hits']} -> {b['hits']}"
)
print(
    f"total_cost_usd {a.get('total_cost_usd')} -> {b.get('total_cost_usd')}; "
    f"cost_saved_usd {a.get('cost_saved_usd')} -> {b.get('cost_saved_usd')}"
)
if a["provider_calls"] > 60:
    sys.exit(f"pass 1 used {a['provider_calls']} provider calls; cap is 60")
if a["provider_calls"] < 1:
    sys.exit("pass 1 made no provider calls")
if b["provider_calls"] != a["provider_calls"]:
    sys.exit("provider_calls increased on the rerun")
if b["hits"] <= a["hits"]:
    sys.exit("cache hits did not increase on the rerun")
if not a.get("total_cost_usd"):
    sys.exit("pass 1 recorded no provider cost")
if abs(b["total_cost_usd"] - a["total_cost_usd"]) > 1e-9:
    sys.exit("total_cost_usd changed on the rerun")
if b.get("cost_saved_usd", 0) <= a.get("cost_saved_usd", 0):
    sys.exit("cost_saved_usd did not increase on the rerun")
PY

if grep -q -F -- "$OPENROUTER_API_KEY" "$HUTCH_LOG"; then
  echo "hutch log contains the API key; refusing to continue" >&2
  exit 1
fi

echo "=== done. bigfoot_triage and ufo_triage on 8 rows each. ==="
