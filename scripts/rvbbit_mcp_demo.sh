#!/usr/bin/env bash
# Outbound MCP tools against the in-tree mock server.
# Postgres 55461, hutch-mcp 8112. The spike on 55432 / 8099 is not used.
# No API key. Mock mode is the gate.
#
# Optional live mode, against a public no-auth MCP server:
#   RVBBIT_MCP_LIVE=1 bash scripts/rvbbit_mcp_demo.sh
# Tries npx @modelcontextprotocol/server-everything, then uvx.
# The mock assertions are not required in live mode.
set -euo pipefail

unset PGPORT HUTCH_PORT RVBBIT_HUTCH_URL
ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

PG_BIN=${PG_BIN:-/usr/lib/postgresql/18/bin}
PGPORT=${RVBBIT_MCP_PGPORT:-55461}
HUTCH_MCP_PORT=${HUTCH_MCP_PORT:-8112}
PGHOST=${PGHOST:-/tmp}
PGUSER=${PGUSER:-postgres}
PGDATA=${PGDATA:-/tmp/rvbbit-mcp-pg}
RUNDIR=${RUNDIR:-/tmp/rvbbit-mcp}
export CC=${CC:-gcc}
export CXX=${CXX:-g++}
export PATH="$PG_BIN:$PATH"
export PGRUST_PGSHAREDIR=${PGRUST_PGSHAREDIR:-/usr/share/postgresql/18}
export PGRUST_TZDIR=${PGRUST_TZDIR:-/usr/share/zoneinfo}
export HUTCH_MCP_PORT
export RVBBIT_MCP_URL="http://127.0.0.1:${HUTCH_MCP_PORT}"

if [[ "$PGPORT" == "55432" || "$HUTCH_MCP_PORT" == "8099" ]]; then
  echo "Refusing spike ports 55432 / 8099." >&2
  exit 1
fi

for cmd in gcc g++ curl initdb psql cargo python3; do
  if ! command -v "$cmd" >/dev/null 2>&1; then
    echo "missing command: $cmd" >&2
    exit 1
  fi
done

if [[ "${RVBBIT_MCP_LIVE:-}" == "1" ]]; then
  if command -v npx >/dev/null 2>&1; then
    LIVE_CMD=(npx -y @modelcontextprotocol/server-everything)
  elif command -v uvx >/dev/null 2>&1; then
    LIVE_CMD=(uvx mcp-server-everything)
  else
    echo "RVBBIT_MCP_LIVE=1 needs npx or uvx" >&2
    exit 1
  fi
fi

port_open() {
  (echo >/dev/tcp/127.0.0.1/"$1") >/dev/null 2>&1
}

if port_open "$HUTCH_MCP_PORT"; then
  echo "127.0.0.1:$HUTCH_MCP_PORT is already in use." >&2
  exit 1
fi
if [[ -S "$PGHOST/.s.PGSQL.$PGPORT" ]]; then
  echo "Postgres socket $PGHOST/.s.PGSQL.$PGPORT is already in use." >&2
  exit 1
fi

echo "=== build (CC=$CC CXX=$CXX) ==="
env -u PGPORT -u HUTCH_PORT cargo build -p main_main -p hutch_mcp --profile fast-profile

mkdir -p "$RUNDIR"
MCP_LOG=$RUNDIR/hutch-mcp.log
PG_LOG=$RUNDIR/postgres.log
HUTCH_BIN="$ROOT/target/fast-profile/hutch-mcp"

cleanup() {
  if [[ -n "${MCP_PID:-}" ]]; then
    kill "$MCP_PID" >/dev/null 2>&1 || true
    wait "$MCP_PID" 2>/dev/null || true
  fi
  if [[ -n "${PG_PID:-}" ]]; then
    if [[ -x "$PG_BIN/pg_ctl" && -f "$PGDATA/postmaster.pid" ]]; then
      "$PG_BIN/pg_ctl" -D "$PGDATA" -m fast -w stop >/dev/null 2>&1 || true
    fi
    if [[ -f "$PGDATA/postmaster.pid" ]]; then
      kill "$(head -n 1 "$PGDATA/postmaster.pid")" >/dev/null 2>&1 || true
    fi
    wait "$PG_PID" 2>/dev/null || true
  fi
  for _ in $(seq 1 50); do
    if ! port_open "$HUTCH_MCP_PORT" && [[ ! -S "$PGHOST/.s.PGSQL.$PGPORT" ]]; then
      return 0
    fi
    sleep 0.1
  done
}
trap cleanup EXIT

rm -rf "$PGDATA"

echo "=== hutch-mcp on 127.0.0.1:$HUTCH_MCP_PORT ==="
: >"$MCP_LOG"
env -u PGPORT -u HUTCH_PORT HUTCH_MCP_PORT="$HUTCH_MCP_PORT" \
  "$HUTCH_BIN" >"$MCP_LOG" 2>&1 &
MCP_PID=$!

echo "=== initdb $PGDATA ==="
initdb -D "$PGDATA" --no-locale --encoding=UTF8 --auth-local=trust --auth-host=trust -U "$PGUSER" >"$RUNDIR/initdb.log"

echo "=== postgres on socket $PGHOST port $PGPORT ==="
ulimit -s 65520
env -u PGPORT -u HUTCH_PORT \
  RVBBIT_MCP_URL="$RVBBIT_MCP_URL" HUTCH_MCP_PORT="$HUTCH_MCP_PORT" \
  RUST_MIN_STACK=33554432 \
  "$ROOT/target/fast-profile/postgres" \
  -D "$PGDATA" -k "$PGHOST" -p "$PGPORT" \
  -c listen_addresses= -c io_method=sync -c max_stack_depth=60000 \
  >"$PG_LOG" 2>&1 &
PG_PID=$!

ready=0
for _ in $(seq 1 80); do
  if curl -sf "http://127.0.0.1:$HUTCH_MCP_PORT/health" >/dev/null && \
     psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -X -q -c 'SELECT 1' >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 0.25
done
if [[ "$ready" != 1 ]]; then
  echo "hutch-mcp or postgres did not become ready. Logs: $MCP_LOG $PG_LOG" >&2
  exit 1
fi

PSQL=(psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -v ON_ERROR_STOP=1 -X)

if [[ "${RVBBIT_MCP_LIVE:-}" == "1" ]]; then
  echo "=== live server (${LIVE_CMD[*]}) ==="
  LIVE_ARGS=$(printf '%s\n' "${LIVE_CMD[@]:1}" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read().split()))')
  "${PSQL[@]}" <<SQL
CREATE SERVER live FOREIGN DATA WRAPPER mcp OPTIONS (
  transport 'stdio',
  command '$(command -v "${LIVE_CMD[0]}")',
  args '$LIVE_ARGS'
);
IMPORT FOREIGN TOOLS FROM SERVER live INTO mcp_live;
SELECT n.nspname || '.' || p.proname
  FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
 WHERE n.nspname = 'mcp_live'
 ORDER BY 1;
SQL
  echo "live import finished"
  exit 0
fi

echo "=== import the mock stdio server ==="
"${PSQL[@]}" <<SQL 2>&1 | tee "$RUNDIR/import.out"
CREATE SERVER demo FOREIGN DATA WRAPPER mcp OPTIONS (
  transport 'stdio',
  command '${HUTCH_BIN}',
  args '--mock-stdio'
);
IMPORT FOREIGN TOOLS FROM SERVER demo INTO mcp_demo;
SQL
grep -q 'skipping MCP tool' "$RUNDIR/import.out"
SKIPPED=$("${PSQL[@]}" -tA -c "SELECT count(*) FROM mcp.tool WHERE procoid IS NULL AND import_error IS NOT NULL AND length(tool) > 63;")
echo "skipped tools=$SKIPPED"
test "$SKIPPED" = "1"
ECHO_FN=$("${PSQL[@]}" -tA -c "SELECT count(*) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname = 'mcp_demo' AND p.proname = 'echo';")
test "$ECHO_FN" = "1"

echo "=== echo, people, fail, ON ERROR ==="
ECHO=$("${PSQL[@]}" -tA -c "SELECT mcp_demo.echo('Ken Lay') = to_jsonb('Ken Lay'::text);")
echo "echo=$ECHO"
test "$ECHO" = "t"

echo "=== count(*) over a table batches a volatile tool ==="
"${PSQL[@]}" <<'SQL' | tee "$RUNDIR/aggtable.out"
CREATE TABLE mcp_agg (x text);
INSERT INTO mcp_agg SELECT 'row' FROM generate_series(1, 8);
EXPLAIN (ANALYZE, COSTS ON) SELECT count(*) FROM (SELECT mcp_demo.echo(x) FROM mcp_agg) s;
SQL
grep -q 'MCP Provider Calls' "$RUNDIR/aggtable.out"
grep -E -q 'MCP Batches: [1-9]' "$RUNDIR/aggtable.out"

RETSET=$("${PSQL[@]}" -tA -c "SELECT proretset::text FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname = 'mcp_demo' AND p.proname = 'people'")
if [[ "$RETSET" == "t" ]]; then
  "${PSQL[@]}" -c "SELECT name FROM mcp_demo.people() ORDER BY 1;" | tee "$RUNDIR/people.out"
else
  "${PSQL[@]}" -c "SELECT mcp_demo.people()::text;" | tee "$RUNDIR/people.out"
fi
grep -q 'Ada' "$RUNDIR/people.out"

"${PSQL[@]}" <<'SQL'
DO $$
BEGIN
  PERFORM mcp_demo.fail();
  RAISE EXCEPTION 'fail() should have errored';
EXCEPTION WHEN OTHERS THEN
  IF SQLERRM NOT ILIKE '%fail%' THEN
    RAISE;
  END IF;
END $$;
SELECT mcp_demo.fail() ON ERROR NULL IS NULL;
SELECT mcp_demo.fail() ON ERROR NULL IS NULL;
SQL
FAIL_HITS=$("${PSQL[@]}" -tA -c "SELECT count(*) FROM rvbbit.calls WHERE op = 'mcp' AND tool = 'fail' AND cache = 'hit';")
echo "fail hits=$FAIL_HITS"
test "$FAIL_HITS" = "0"

echo "=== cache a read-only echo ==="
"${PSQL[@]}" <<'SQL'
ALTER FOREIGN TOOL mcp_demo.echo OPTIONS (read_only 'true', cache_ttl '60s');
SELECT mcp_demo.echo('cache-me');
SELECT mcp_demo.echo('cache-me');
SQL
"${PSQL[@]}" -c "SELECT cache FROM rvbbit.calls WHERE op = 'mcp' AND tool = 'echo' AND server = 'demo' ORDER BY ts;" | tee "$RUNDIR/calls.out"
grep -q 'hit' "$RUNDIR/calls.out"

echo "=== memo hit without a gateway cache ttl ==="
"${PSQL[@]}" <<'SQL'
SELECT mcp_demo.bag('{"k":"one"}'::jsonb);
SELECT mcp_demo.bag('{"k":"one"}'::jsonb);
SQL
"${PSQL[@]}" -c "SELECT cache FROM rvbbit.calls WHERE op = 'mcp' AND tool = 'bag' AND server = 'demo' ORDER BY ts;" | tee "$RUNDIR/bag.out"
grep -q 'hit' "$RUNDIR/bag.out"

echo "=== explain and refresh ==="
# Planning loads installed tools before the lift, so a fresh session
# still prints MCP lines under an aggregate.
"${PSQL[@]}" <<'SQL' | tee "$RUNDIR/explain.out"
SELECT mcp_demo.echo('plan');
EXPLAIN (ANALYZE, COSTS ON) SELECT mcp_demo.echo(x) FROM (VALUES ('a'), ('b'), ('c'), ('d'), ('e'), ('f'), ('g'), ('h'), ('i'), ('j')) v(x);
SQL
grep -q 'MCP Provider Calls' "$RUNDIR/explain.out"
grep -E -q 'Actual MCP Calls: [1-9]' "$RUNDIR/explain.out"
grep -E -q 'MCP Batches: [1-9]' "$RUNDIR/explain.out"
# count(*) drops a STABLE target it does not read. now() is volatile, so
# the aggregate still has to run it, the same shape as the live
# convert_time subquery.
"${PSQL[@]}" <<'SQL' | tee "$RUNDIR/subquery.out"
EXPLAIN (ANALYZE, COSTS ON) SELECT count(*) FROM (SELECT mcp_demo.now() AS r FROM (VALUES (1), (2), (3), (4), (5), (6), (7), (8), (9), (10)) v(x)) s;
SQL
grep -E -q 'MCP Batches: [1-9]' "$RUNDIR/subquery.out"
# Same shape on a real table. now() stays volatile, so count(*) cannot drop it.
"${PSQL[@]}" <<'SQL' | tee "$RUNDIR/aggnow.out"
EXPLAIN (ANALYZE, COSTS ON) SELECT count(*) FROM (SELECT mcp_demo.now() FROM mcp_agg) s;
SQL
grep -q 'MCP Provider Calls' "$RUNDIR/aggnow.out"
grep -E -q 'MCP Batches: [1-9]' "$RUNDIR/aggnow.out"
"${PSQL[@]}" <<'SQL' | tee "$RUNDIR/rows.out"
CREATE TABLE mcp_rows (x text);
INSERT INTO mcp_rows SELECT 'a' FROM generate_series(1, 10);
EXPLAIN SELECT mcp_demo.now() FROM mcp_rows;
SQL
grep -q 'rows=10' "$RUNDIR/rows.out"
if grep -q 'rows=1360' "$RUNDIR/rows.out"; then
  echo "scan still estimates the 10-page floor" >&2
  exit 1
fi
"${PSQL[@]}" -c "REFRESH FOREIGN TOOLS FROM SERVER demo;" 2>&1 | tee "$RUNDIR/refresh.out"
grep -q 'unchanged' "$RUNDIR/refresh.out"

echo "=== drop ==="
"${PSQL[@]}" -c "DROP SERVER demo CASCADE;"
LEFT=$("${PSQL[@]}" -tA -c "SELECT count(*) FROM mcp.tool;")
echo "tools left=$LEFT"
test "$LEFT" = "0"
if grep -q 'schema mcp already exists' "$PG_LOG"; then
  echo "schema mcp notice leaked into the postgres log" >&2
  exit 1
fi

echo "mcp demo ok"
