#!/usr/bin/env bash
# M6: SUMMARIZE / CONSENSUS aggregates, EXTRACT MANY, RERANK, and |>.
#
# Default path is the mock suite. An ambient OPENROUTER_API_KEY does not
# start Postgres or spend a call.
#
# Live, against hutch-lite, with the session reading RVBBIT_HUTCH_URL:
#   RVBBIT_M6_LIVE=1 bash scripts/rvbbit_m6_demo.sh
# Requires OPENROUTER_API_KEY. Ports 55436 / 8096.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

cargo test -p rvbbit_parse -p rvbbit_dispatch -p rvbbit_hutch_lite --profile fast-profile --offline \
  --lib --test fake_openai -- \
  m6_aggregate_rerank_extract_many_and_pipe \
  agg_state_appends_parts_and_keeps_the_first_clause \
  rerank_summary_and_extract_many \
  regress_sql_is_not_rewritten
echo "m6 demo: rewrite, aggregate state, rerank, summary, and extract-many passed"

if [[ "${RVBBIT_M6_LIVE:-}" != "1" ]]; then
  echo "export RVBBIT_M6_LIVE=1 and OPENROUTER_API_KEY to run the live hutch-lite script"
  exit 0
fi
if [[ -z "${OPENROUTER_API_KEY:-}" ]]; then
  echo "RVBBIT_M6_LIVE=1 needs OPENROUTER_API_KEY" >&2
  exit 1
fi

PG_BIN=${PG_BIN:-/usr/lib/postgresql/18/bin}
PGPORT=${PGPORT:-55436}
HUTCH_PORT=${HUTCH_PORT:-8096}
PGHOST=${PGHOST:-/tmp}
PGUSER=${PGUSER:-postgres}
PGDATA=${PGDATA:-/tmp/rvbbit-m6-pg}
RUNDIR=${RUNDIR:-/tmp/rvbbit-m6-demo}
DECISION=${HUTCH_DECISION_MODEL:-typesafe/jev-1.14}
export HUTCH_DECISION_MODEL="$DECISION"
export RVBBIT_HUTCH_URL="${RVBBIT_HUTCH_URL:-http://127.0.0.1:${HUTCH_PORT}}"
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

if [[ "$RVBBIT_HUTCH_URL" == "http://127.0.0.1:${HUTCH_PORT}" ]]; then
  echo "=== hutch-lite on $RVBBIT_HUTCH_URL decision=$DECISION ==="
  : >"$HUTCH_LOG"
  ./target/fast-profile/hutch_lite "$HUTCH_PORT" >"$HUTCH_LOG" 2>&1 &
  HUTCH_PID=$!
else
  echo "=== using existing hutch at $RVBBIT_HUTCH_URL ==="
fi

initdb -D "$PGDATA" --no-locale --encoding=UTF8 --auth-local=trust --auth-host=trust -U "$PGUSER" >"$RUNDIR/initdb.log"
ulimit -s 65520
RUST_MIN_STACK=33554432 \
  ./target/fast-profile/postgres \
  -D "$PGDATA" -k "$PGHOST" -p "$PGPORT" \
  -c listen_addresses= -c io_method=sync -c max_stack_depth=60000 \
  >"$PG_LOG" 2>&1 &

ready=0
for _ in $(seq 1 50); do
  if curl -sf "${RVBBIT_HUTCH_URL%/}/stats" >/dev/null && \
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
"${PSQL[@]}" -d postgres -c "DROP DATABASE IF EXISTS rvbbit_m6" -c "CREATE DATABASE rvbbit_m6"

cat >"$RUNDIR/setup.sql" <<SQL
CREATE BACKEND live TYPE hutch URL '${RVBBIT_HUTCH_URL}';
CREATE TABLE notes (kind text, body text);
INSERT INTO notes VALUES
  ('road', 'A large hairy biped crossed the logging road'),
  ('road', 'Another tall figure on the same logging road'),
  ('lake', 'Lights hovered over the water');
SQL
"${PSQL[@]}" -d rvbbit_m6 -f "$RUNDIR/setup.sql"

echo "=== SUMMARIZE and CONSENSUS by kind ==="
"${PSQL[@]}" -d rvbbit_m6 -c \
  "SELECT kind, SUMMARIZE(body) AS summary, CONSENSUS(body, 'a large biped was seen') AS biped
   FROM notes GROUP BY kind ORDER BY kind"

echo "=== RERANK via pipe, then EXTRACT MANY ==="
"${PSQL[@]}" -d rvbbit_m6 -c \
  "SELECT kind, body |> RERANK 'logging road' AS score FROM notes ORDER BY score DESC"
"${PSQL[@]}" -d rvbbit_m6 -c \
  "SELECT EXTRACT MANY creature FROM body FROM notes"

echo "=== clause tail on a piped MEANS ==="
"${PSQL[@]}" -d rvbbit_m6 -c \
  "SELECT body |> MEANS 'a large biped was seen' WITH TAKES 2 FROM notes WHERE kind = 'road' LIMIT 1"

echo "=== rvbbit.calls ==="
"${PSQL[@]}" -d rvbbit_m6 -c \
  "SELECT op, count(*) AS calls, sum(cost) AS cost FROM rvbbit.calls GROUP BY op ORDER BY op"
echo "=== done. RVBBIT_HUTCH_URL=$RVBBIT_HUTCH_URL ==="
