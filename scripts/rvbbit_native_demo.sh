#!/usr/bin/env bash
# Live native MEANS / ABOUT / CLASSIFY / EXTRACT on 8 fixture rows.
#
# Expected provider calls on pass 1: 32
#   8 tight MEANS noul + 8 loose ABOUT noul + 8 CLASSIFY choice + 8 EXTRACT chat.
# The script aborts above 60. Pass 2 must not add provider calls or cost.
#
# Ports: Postgres 55433, hutch 8098 (same as the other live demos; this
# script exits if they are taken). The spike on 55432 / 8099 is left alone.
#
# Required: OPENROUTER_API_KEY (never printed, never written into SQL).
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

PG_BIN=${PG_BIN:-/usr/lib/postgresql/18/bin}
PGPORT=${PGPORT:-55433}
HUTCH_PORT=${HUTCH_PORT:-8098}
PGHOST=${PGHOST:-/tmp}
PGUSER=${PGUSER:-postgres}
PGDATA=${PGDATA:-/tmp/rvbbit-native-pg}
RUNDIR=${RUNDIR:-/tmp/rvbbit-native-demo}
CHAT=${HUTCH_MODEL:-openai/gpt-4o-mini}
DECISION=${HUTCH_DECISION_MODEL:-typesafe/jev-1.14}
export HUTCH_MODEL="$CHAT"
export HUTCH_DECISION_MODEL="$DECISION"
export CC=${CC:-gcc}
export CXX=${CXX:-g++}
export PATH="$PG_BIN:$PATH"
export PGRUST_PGSHAREDIR=${PGRUST_PGSHAREDIR:-/usr/share/postgresql/18}
export PGRUST_TZDIR=${PGRUST_TZDIR:-/usr/share/zoneinfo}

if [[ "$PGPORT" == "55432" || "$HUTCH_PORT" == "8099" ]]; then
  echo "Refusing spike ports 55432 / 8099." >&2
  exit 1
fi

if [[ -z "${OPENROUTER_API_KEY:-}" ]]; then
  echo "OPENROUTER_API_KEY is not set. Export it in this shell; this script does not print it." >&2
  exit 1
fi

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
  echo "127.0.0.1:$HUTCH_PORT is already in use. Stop that process or set HUTCH_PORT." >&2
  exit 1
fi
if [[ -S "$PGHOST/.s.PGSQL.$PGPORT" ]]; then
  echo "Postgres socket $PGHOST/.s.PGSQL.$PGPORT is already in use." >&2
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
for _ in $(seq 1 80); do
  if curl -sf "http://127.0.0.1:$HUTCH_PORT/stats" >/dev/null && \
     psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -X -q -c 'SELECT 1' >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 0.25
done
if [[ "$ready" != 1 ]]; then
  echo "hutch or postgres did not become ready. Logs: $HUTCH_LOG $PG_LOG" >&2
  exit 1
fi

if grep -q -F -- "$OPENROUTER_API_KEY" "$HUTCH_LOG"; then
  echo "hutch log contains the API key; refusing to continue" >&2
  exit 1
fi

PSQL=(psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -v ON_ERROR_STOP=1 -X)
"${PSQL[@]}" -d postgres -c "DROP DATABASE IF EXISTS rvbbit_native" -c "CREATE DATABASE rvbbit_native"

cat >"$RUNDIR/setup.sql" <<SQL
CREATE BACKEND live TYPE hutch URL 'http://127.0.0.1:${HUTCH_PORT}';
CREATE MODEL jev ON BACKEND live NAME '${DECISION//\'/\'\'}';
CREATE SEMANTIC FUNCTION means(text text, criterion text) RETURNS boolean
  MODEL jev
  PROMPT \$\$Does the text match the criterion?\$\$
  TEMPERATURE 0 CACHE memoize;
CREATE SEMANTIC OPERATOR means (LEFTARG = text, RIGHTARG = text, FUNCTION = means);
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
SQL

cat >"$RUNDIR/queries.sql" <<'SQL'
\pset pager off
-- 8-row copy. WHERE MEANS / ABOUT cannot scan the 200-row fixture.
SELECT id, class FROM sightings
WHERE notes MEANS 'a large hairy biped was seen'
ORDER BY id;
SELECT id, class FROM sightings
WHERE notes ABOUT 'a sound, a dark shape, or any unusual event was noticed'
ORDER BY id;
SELECT id,
       CLASSIFY(notes INTO 'bigfoot', 'known animal', 'unclear') AS label
FROM sightings
ORDER BY id;
SELECT id,
       EXTRACT(creature FROM notes) ON ERROR 'unknown' AS creature
FROM sightings
ORDER BY id;
SQL

echo "=== catalog and 8-row sightings ==="
"${PSQL[@]}" -d rvbbit_native -f "$RUNDIR/setup.sql"

echo "=== EXPLAIN project batch ==="
EXPLAIN=$("${PSQL[@]}" -d rvbbit_native -c "EXPLAIN SELECT id, CLASSIFY(notes INTO 'bigfoot', 'known animal', 'unclear') AS label FROM sightings")
echo "$EXPLAIN"
if ! grep -q "Semantic Project" <<<"$EXPLAIN"; then
  echo "EXPLAIN did not name the node Semantic Project" >&2
  exit 1
fi
if ! grep -q "Batch Size" <<<"$EXPLAIN"; then
  echo "EXPLAIN did not print Batch Size" >&2
  exit 1
fi

curl -sf -X POST "http://127.0.0.1:$HUTCH_PORT/reset" >/dev/null
echo "=== pass 1 ==="
"${PSQL[@]}" -d rvbbit_native -f "$RUNDIR/queries.sql"
S1=$(curl -sf "http://127.0.0.1:$HUTCH_PORT/stats")
echo "=== after pass 1 ==="
echo "GET /stats $S1"

echo "=== pass 2 ==="
"${PSQL[@]}" -d rvbbit_native -f "$RUNDIR/queries.sql"
S2=$(curl -sf "http://127.0.0.1:$HUTCH_PORT/stats")
echo "=== after pass 2 ==="
echo "GET /stats $S2"

if grep -q -F -- "$OPENROUTER_API_KEY" "$HUTCH_LOG"; then
  echo "hutch log contains the API key; refusing to continue" >&2
  exit 1
fi

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
if not a.get("total_cost_usd"):
    sys.exit("pass 1 recorded no provider cost")
if abs(b["total_cost_usd"] - a["total_cost_usd"]) > 1e-9:
    sys.exit("total_cost_usd changed on the rerun")
if b.get("cost_saved_usd", 0) <= a.get("cost_saved_usd", 0):
    sys.exit("cost_saved_usd did not increase on the rerun")
if b["hits"] <= a["hits"]:
    sys.exit("cache hits did not increase on the rerun")
print("native demo counters match")
PY
