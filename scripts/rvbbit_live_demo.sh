#!/usr/bin/env bash
# Build pgrustic + hutch-lite and run live MEANS, score, CLASSIFY, and EXTRACT queries.
#
# Prerequisites on a fresh Ubuntu box (see docs/rvbbit/HUTCH_LITE.md):
#   sudo apt-get update
#   sudo apt-get install -y build-essential g++ pkg-config ca-certificates curl \
#     postgresql-18 postgresql-client-18
#   # rustup, then rustc 1.96.0 from rust-toolchain.toml
#   # if the linker name is missing, as root (this script will not sudo):
#   sudo ln -s libstdc++.so.6 /usr/lib/x86_64-linux-gnu/libstdc++.so
#
# /usr/bin/c++ may be clang. re2 then fails to find <algorithm>. This
# script forces CC=gcc CXX=g++.
#
# Required: OPENROUTER_API_KEY (never printed).
# Optional: HUTCH_MODEL (chat, default openai/gpt-4o-mini),
#           HUTCH_DECISION_MODEL (MEANS and fixed-label CLASSIFY,
#           default typesafe/jev-1.14), OPENROUTER_BASE_URL,
#           HUTCH_TIMEOUT_MS (default 60000 inside hutch-lite).
#
# Ports: Postgres 55433, hutch 8098. The spike's 55432 / 8099 are left alone.
# TAKES / PICK are not used (parser error until milestone M5).
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

PG_BIN=${PG_BIN:-/usr/lib/postgresql/18/bin}
PGPORT=${PGPORT:-55433}
HUTCH_PORT=${HUTCH_PORT:-8098}
PGHOST=${PGHOST:-/tmp}
PGUSER=${PGUSER:-postgres}
PGDATA=${PGDATA:-/tmp/rvbbit-live-pg}
RUNDIR=${RUNDIR:-/tmp/rvbbit-live-demo}
CHAT=${HUTCH_MODEL:-openai/gpt-4o-mini}
DECISION=${HUTCH_DECISION_MODEL:-typesafe/jev-1.14}
export HUTCH_MODEL="$CHAT"
export HUTCH_DECISION_MODEL="$DECISION"
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
echo "system c++: $(c++ --version 2>/dev/null | head -1 || echo unknown)"
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

echo "=== hutch-lite on 127.0.0.1:$HUTCH_PORT chat=$CHAT decision=$DECISION api_key=set ==="
# Inherit OPENROUTER_API_KEY. Do not echo the environment.
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

echo "=== catalog + six sightings ==="
"${PSQL[@]}" -d postgres -c "DROP DATABASE IF EXISTS rvbbit_live" -c "CREATE DATABASE rvbbit_live"

run_sql() {
  local label=$1
  local file=$2
  echo "=== $label ==="
  local start
  start=$(date +%s%3N)
  "${PSQL[@]}" -d rvbbit_live -f "$file"
  local end
  end=$(date +%s%3N)
  echo "--- $label wall_ms=$((end - start)) ---"
}

cat >"$RUNDIR/setup.sql" <<SQL
CREATE BACKEND live TYPE hutch URL 'http://127.0.0.1:${HUTCH_PORT}';
CREATE MODEL jev ON BACKEND live NAME '${DECISION//\'/\'\'}';
CREATE MODEL chat ON BACKEND live NAME '${CHAT//\'/\'\'}';
CREATE SEMANTIC FUNCTION means(text text, criterion text) RETURNS boolean
  MODEL jev
  PROMPT \$\$Does the text match the criterion?\$\$
  TEMPERATURE 0 CACHE memoize;
CREATE SEMANTIC OPERATOR means (LEFTARG = text, RIGHTARG = text, FUNCTION = means);
CREATE SEMANTIC FUNCTION means_score(text text, criterion text) RETURNS double precision
  MODEL jev
  PROMPT \$\$Does the text match the criterion?\$\$
  TEMPERATURE 0 CACHE memoize;
CREATE SEMANTIC FUNCTION kind(notes text) RETURNS text
  MODEL jev
  LABELS (
    'bigfoot' AS 'an unidentified large primate or sasquatch',
    'known animal' AS 'a recognized species such as a deer, bear, or owl',
    'unclear' AS 'the report does not identify a creature'
  )
  TEMPERATURE 0 CACHE memoize;
CREATE SEMANTIC FUNCTION creature(notes text) RETURNS text
  MODEL chat
  SYSTEM \$\$Return only a JSON object with keys creature and confidence. creature is a short lowercase noun. confidence is a number from 0 to 1. No markdown.\$\$
  PROMPT \$\$What creature is described? Text: {{input}}\$\$
  TEMPERATURE 0 MAX TOKENS 100 CACHE memoize;
CREATE TABLE sightings (id int PRIMARY KEY, notes text NOT NULL);
INSERT INTO sightings (id, notes) VALUES
  (1, 'A tall hairy figure crossed the logging road on two legs and left huge prints.'),
  (2, 'We heard wood knocks, then a dark biped stepped over the creek.'),
  (3, 'A deer bolted through the ferns at dusk.'),
  (4, 'An owl called from the ridge and nothing else moved.'),
  (5, 'Something ape-like stood at the tree line and walked upright into the timber.'),
  (6, 'Campers reported a black bear turning over a cooler.');
SQL

cat >"$RUNDIR/queries.sql" <<'SQL'
\pset pager off
SELECT id FROM sightings
WHERE notes MEANS 'a large hairy biped was seen'
ORDER BY id;
SELECT id, rvbbit.means_score(notes, 'a large hairy biped was seen') AS score
FROM sightings
ORDER BY id;
SELECT id, rvbbit.kind(notes) AS kind
FROM sightings
ORDER BY id;
SELECT id, rvbbit.creature(notes) AS extracted
FROM sightings
ORDER BY id;
SQL

run_sql "catalog and sightings (no model calls)" "$RUNDIR/setup.sql"
# Counters start at zero so pass 1 is only the two queries below.
curl -sf -X POST "http://127.0.0.1:$HUTCH_PORT/reset" >/dev/null
run_sql "pass 1 MEANS + score + CLASSIFY + EXTRACT" "$RUNDIR/queries.sql"
S1=$(curl -sf "http://127.0.0.1:$HUTCH_PORT/stats")
C1=$(curl -sf "http://127.0.0.1:$HUTCH_PORT/v1/cache/stats")
echo "=== after pass 1 ==="
echo "GET /stats $S1"
echo "GET /v1/cache/stats $C1"

run_sql "pass 2 queries (expect cache hits, no new provider calls)" "$RUNDIR/queries.sql"
S2=$(curl -sf "http://127.0.0.1:$HUTCH_PORT/stats")
C2=$(curl -sf "http://127.0.0.1:$HUTCH_PORT/v1/cache/stats")
echo "=== after pass 2 ==="
echo "GET /stats $S2"
echo "GET /v1/cache/stats $C2"

S1="$S1" S2="$S2" python3 - <<'PY'
import json, os, sys
a = json.loads(os.environ["S1"])
b = json.loads(os.environ["S2"])
print(
    f"provider_calls {a['provider_calls']} -> {b['provider_calls']}; "
    f"hits {a['hits']} -> {b['hits']}; "
    f"requests {a['requests']} -> {b['requests']}"
)
if a["provider_calls"] > 40:
    sys.exit(f"pass 1 used {a['provider_calls']} provider calls; cap is 40")
if b["provider_calls"] != a["provider_calls"]:
    sys.exit("provider_calls increased on the rerun")
if b["hits"] <= a["hits"]:
    sys.exit("cache hits did not increase on the rerun")
if b["requests"] <= a["requests"]:
    sys.exit("expected the rerun to reach hutch again")
PY

if grep -q -F -- "$OPENROUTER_API_KEY" "$HUTCH_LOG"; then
  echo "hutch log contains the API key; refusing to continue" >&2
  exit 1
fi

echo "=== done. TAKES/PICK not exercised (stubbed until M5). ==="
