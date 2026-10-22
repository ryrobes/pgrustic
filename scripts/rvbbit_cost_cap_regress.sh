#!/usr/bin/env bash
# Cost-cap regression.
#
# Every semantic op and an MCP call, in the SELECT list, in WHERE, and
# nested under IS NOT NULL, must be estimated. semantic.max_cost=0
# rejects each one before a provider call.
#
# A seeded predict hit rate discounts the expected USD. The cap uses the
# worst case, so a budget between the two still rejects.
#
# ALTER SERVER ... OPTIONS (SET|DROP cost_usd) updates mcp.server.
#
# Ports 55467 / 18102 / 8118. The spike on 55432 / 8099 is not used.
set -euo pipefail

unset PGPORT HUTCH_PORT
ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

PG_BIN=${PG_BIN:-/usr/lib/postgresql/18/bin}
PGPORT=${PGPORT:-55467}
HUTCH_PORT=${HUTCH_PORT:-18102}
HUTCH_MCP_PORT=${HUTCH_MCP_PORT:-8118}
PGHOST=${PGHOST:-/tmp}
PGUSER=${PGUSER:-postgres}
PGDATA=${PGDATA:-/tmp/rvbbit-cost-cap-pg}
RUNDIR=${RUNDIR:-/tmp/rvbbit-cost-cap}
export RVBBIT_HUTCH_URL="http://127.0.0.1:${HUTCH_PORT}"
export RVBBIT_MCP_URL="http://127.0.0.1:${HUTCH_MCP_PORT}"
export RVBBIT_MOCK_SEED_HITS=1
export HUTCH_MCP_PORT
export CC=${CC:-gcc}
export CXX=${CXX:-g++}
export CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-4}
export PATH="$PG_BIN:$PATH"
export PGRUST_PGSHAREDIR=${PGRUST_PGSHAREDIR:-/usr/share/postgresql/18}
export PGRUST_TZDIR=${PGRUST_TZDIR:-/usr/share/zoneinfo}

if [[ "$PGPORT" == "55432" || "$HUTCH_PORT" == "8099" || "$HUTCH_MCP_PORT" == "8099" ]]; then
  echo "Refusing spike ports 55432 / 8099." >&2
  exit 1
fi

echo "=== build ==="
cargo build -p main_main -p hutch_mock -p hutch_mcp --profile fast-profile --offline \
  --bin postgres --bin hutch_mock --bin hutch-mcp

mkdir -p "$RUNDIR"
HUTCH_LOG=$RUNDIR/hutch.log
MCP_LOG=$RUNDIR/hutch-mcp.log
PG_LOG=$RUNDIR/postgres.log
HUTCH_BIN="$ROOT/target/fast-profile/hutch-mcp"

cleanup() {
  if [[ -n "${HUTCH_PID:-}" ]]; then
    kill "$HUTCH_PID" >/dev/null 2>&1 || true
  fi
  if [[ -n "${MCP_PID:-}" ]]; then
    kill "$MCP_PID" >/dev/null 2>&1 || true
  fi
  if [[ -f "$PGDATA/postmaster.pid" ]]; then
    kill "$(head -n 1 "$PGDATA/postmaster.pid")" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT
rm -rf "$PGDATA"

echo "=== hutch_mock on $RVBBIT_HUTCH_URL ==="
: >"$HUTCH_LOG"
./target/fast-profile/hutch_mock "$HUTCH_PORT" >"$HUTCH_LOG" 2>&1 &
HUTCH_PID=$!

echo "=== hutch-mcp on $RVBBIT_MCP_URL ==="
: >"$MCP_LOG"
env -u PGPORT -u HUTCH_PORT HUTCH_MCP_PORT="$HUTCH_MCP_PORT" \
  "$HUTCH_BIN" >"$MCP_LOG" 2>&1 &
MCP_PID=$!

initdb -D "$PGDATA" --no-locale --encoding=UTF8 --auth-local=trust --auth-host=trust -U "$PGUSER" >"$RUNDIR/initdb.log"
ulimit -s 65520
env -u PGPORT -u HUTCH_PORT \
  RVBBIT_HUTCH_URL="$RVBBIT_HUTCH_URL" RVBBIT_MCP_URL="$RVBBIT_MCP_URL" \
  RUST_MIN_STACK=33554432 \
  ./target/fast-profile/postgres \
  -D "$PGDATA" -k "$PGHOST" -p "$PGPORT" \
  -c listen_addresses= -c io_method=sync -c max_stack_depth=60000 \
  >"$PG_LOG" 2>&1 &

ready=0
for _ in $(seq 1 80); do
  if curl -sf "${RVBBIT_HUTCH_URL}/stats" >/dev/null && \
     curl -sf "http://127.0.0.1:${HUTCH_MCP_PORT}/health" >/dev/null && \
     psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -X -q -c 'SELECT 1' >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 0.25
done
if [[ "$ready" != 1 ]]; then
  echo "hutch, hutch-mcp, or postgres did not become ready" >&2
  exit 1
fi

PSQL=(psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -v ON_ERROR_STOP=1 -X)
"${PSQL[@]}" -d postgres -c "DROP DATABASE IF EXISTS rvbbit_regress" -c "CREATE DATABASE rvbbit_regress"

"${PSQL[@]}" -d rvbbit_regress <<SQL
CREATE BACKEND live TYPE hutch URL '${RVBBIT_HUTCH_URL}';
CREATE MODEL mock ON BACKEND live NAME 'mock';
CREATE SEMANTIC FUNCTION is_road(notes text) RETURNS bool
  AS 'road'
  USING MODEL mock;
CREATE DECISION TREE sighting (notes text) MODEL mock AS
  ASK 'road' YES 'road' NO 'other';
CREATE SEMANTIC FUNCTION sighting_of(notes text) RETURNS text
  AS DECISION TREE sighting
  USING MODEL mock;
CREATE TABLE docs (id int, body text);
INSERT INTO docs VALUES
  (1, 'A road through the trees'),
  (2, 'The lake was quiet'),
  (3, 'Another road at dusk');
CREATE SERVER demo FOREIGN DATA WRAPPER mcp OPTIONS (
  transport 'stdio',
  command '${HUTCH_BIN}',
  args '--mock-stdio',
  cost_usd '0.001'
);
IMPORT FOREIGN TOOLS FROM SERVER demo INTO mcp_demo;
SQL

calls() {
  curl -sf "${RVBBIT_HUTCH_URL}/stats" | python3 -c 'import json,sys; print(json.load(sys.stdin)["calls"])'
}

expect_capped() {
  local label="$1"
  local sql="$2"
  local before after out status
  before=$(calls)
  set +e
  out=$("${PSQL[@]}" -d rvbbit_regress <<SQL
SET semantic.max_cost = '0';
$sql
SQL
)
  status=$?
  set -e
  if [[ "$status" -eq 0 ]]; then
    echo "FAIL $label ran under max_cost=0: $out" >&2
    exit 1
  fi
  if ! grep -q 'exceeds budget' <<<"$out"; then
    echo "FAIL $label was not a budget error:" >&2
    echo "$out" >&2
    exit 1
  fi
  after=$(calls)
  if [[ "$before" != "$after" ]]; then
    echo "FAIL $label provider calls $before -> $after" >&2
    exit 1
  fi
  echo "capped $label (calls=$after)"
}

expect_planned() {
  local label="$1"
  local needle="$2"
  local sql="$3"
  local plan
  plan=$("${PSQL[@]}" -d rvbbit_regress -At -c "EXPLAIN $sql")
  if ! grep -q "$needle" <<<"$plan"; then
    echo "FAIL $label plan has no $needle:" >&2
    echo "$plan" >&2
    exit 1
  fi
  echo "planned $label"
}

echo "=== SELECT, WHERE, and nested IS NOT NULL under max_cost=0 ==="
# label, plan needle, select, where, nested
while IFS='|' read -r label needle select_sql where_sql nested_sql; do
  [[ -z "${label:-}" ]] && continue
  expect_planned "$label select" "$needle" "$select_sql"
  expect_planned "$label where" "$needle" "$where_sql"
  expect_planned "$label nested" "$needle" "$nested_sql"
  expect_capped "$label select" "$select_sql"
  expect_capped "$label where" "$where_sql"
  expect_capped "$label nested" "$nested_sql"
done <<'OPS'
means|Semantic|SELECT body MEANS 'road' FROM docs|SELECT id FROM docs WHERE body MEANS 'road'|SELECT (body MEANS 'road') IS NOT NULL FROM docs
about|Semantic|SELECT body ABOUT 'road' FROM docs|SELECT id FROM docs WHERE body ABOUT 'road'|SELECT (body ABOUT 'road') IS NOT NULL FROM docs
classify|Semantic|SELECT CLASSIFY(body INTO 'road', 'lake') FROM docs|SELECT id FROM docs WHERE CLASSIFY(body INTO 'road', 'lake') = 'road'|SELECT CLASSIFY(body INTO 'road', 'lake') IS NOT NULL FROM docs
extract|Semantic|SELECT EXTRACT(text FROM body ON ERROR '') FROM docs|SELECT id FROM docs WHERE EXTRACT(text FROM body ON ERROR '') = 'road'|SELECT EXTRACT(text FROM body ON ERROR '') IS NOT NULL FROM docs
rerank|Semantic|SELECT body |> RERANK 'road' FROM docs|SELECT id FROM docs WHERE (body |> RERANK 'road') > 0|SELECT (body |> RERANK 'road') IS NOT NULL FROM docs
similarity|Semantic|SELECT rvbbit.semantic_similarity(body, 'road') FROM docs|SELECT id FROM docs WHERE rvbbit.semantic_similarity(body, 'road') > 0|SELECT rvbbit.semantic_similarity(body, 'road') IS NOT NULL FROM docs
user_fn|Semantic|SELECT is_road(body) FROM docs|SELECT id FROM docs WHERE is_road(body)|SELECT is_road(body) IS NOT NULL FROM docs
decide|Semantic|SELECT sighting_of(body) FROM docs|SELECT id FROM docs WHERE sighting_of(body) = 'road'|SELECT sighting_of(body) IS NOT NULL FROM docs
summarize|Semantic|SELECT SUMMARIZE(body) FROM docs|SELECT id FROM docs WHERE (SELECT SUMMARIZE(body) FROM docs d2) IS NOT NULL|SELECT SUMMARIZE(body) IS NOT NULL FROM docs
OPS

echo "=== MEANS worst case is above the discounted estimate ==="
plan=$("${PSQL[@]}" -d rvbbit_regress -At -c "EXPLAIN SELECT body MEANS 'road' FROM docs;")
python3 - "$plan" <<'PY'
import sys
text = sys.argv[1]
def metric(label):
    best = None
    for line in text.splitlines():
        low = line.lower()
        if label not in low:
            continue
        rest = low.split(label, 1)[1]
        num = ""
        seen = False
        for ch in rest:
            if ch.isdigit() or ch == ".":
                num += ch
                seen = True
            elif seen:
                break
        if num:
            best = float(num)
    if best is None:
        raise SystemExit(f"missing {label}\n{text}")
    return best
# "semantic total usd" also matches the worst line's "... total usd".
# Read the worst line first, then the expected line that is not "worst".
worst = metric("semantic worst total usd")
expected = None
for line in text.splitlines():
    low = line.lower()
    if "semantic total usd" in low and "worst" not in low:
        rest = low.split("semantic total usd", 1)[1]
        num = ""
        seen = False
        for ch in rest:
            if ch.isdigit() or ch == ".":
                num += ch
                seen = True
            elif seen:
                break
        expected = float(num)
if expected is None:
    raise SystemExit(f"missing expected total\n{text}")
print(f"expected={expected} worst={worst}")
if not (worst > expected * 1.5):
    raise SystemExit(f"worst case was not above the hit-rate discount: {expected} vs {worst}")
open("/tmp/rvbbit-cost-cap/between.txt", "w").write(f"{(expected + worst) / 2:.8f}\n")
PY
BETWEEN=$(cat "$RUNDIR/between.txt")
echo "cap between expected and worst: $BETWEEN"
before=$(calls)
set +e
out=$("${PSQL[@]}" -d rvbbit_regress <<SQL
SET semantic.max_cost = '${BETWEEN}';
SELECT body MEANS 'road' FROM docs;
SQL
)
status=$?
set -e
if [[ "$status" -eq 0 ]]; then
  echo "FAIL discounted cap let MEANS run: $out" >&2
  exit 1
fi
grep -q 'exceeds budget' <<<"$out"
after=$(calls)
if [[ "$before" != "$after" ]]; then
  echo "FAIL between-cap MEANS made calls $before -> $after" >&2
  exit 1
fi
echo "between-cap rejected with calls=$after"

echo "=== nested MCP over a scan is estimated and capped ==="
expect_planned "mcp nested" "MCP" "SELECT id, mcp_demo.echo('x') IS NOT NULL FROM docs"
expect_planned "mcp select" "MCP" "SELECT id, mcp_demo.echo(body) FROM docs"
expect_planned "mcp where" "MCP" "SELECT id FROM docs WHERE mcp_demo.echo(body)::text = body"
expect_planned "mcp no table" "MCP" "SELECT mcp_demo.echo('x') IS NOT NULL"
expect_capped "mcp nested" "SELECT id, mcp_demo.echo('x') IS NOT NULL FROM docs"
expect_capped "mcp select" "SELECT id, mcp_demo.echo(body) FROM docs"
expect_capped "mcp where" "SELECT id FROM docs WHERE mcp_demo.echo(body)::text = body"
expect_capped "mcp no table" "SELECT mcp_demo.echo('x') IS NOT NULL"
mcp_plan=$("${PSQL[@]}" -d rvbbit_regress -At -c "EXPLAIN SELECT id, mcp_demo.echo('x') IS NOT NULL FROM docs;")
python3 - "$mcp_plan" <<'PY'
import sys
text = sys.argv[1].lower()
if "mcp worst total usd" not in text or "mcp total usd" not in text:
    raise SystemExit("MCP totals missing\n" + sys.argv[1])
print("mcp explain has expected and worst totals")
PY

echo "=== ALTER SERVER cost_usd ==="
"${PSQL[@]}" -d rvbbit_regress -c "ALTER SERVER demo OPTIONS (SET cost_usd '0.004');"
set_price=$("${PSQL[@]}" -d rvbbit_regress -At -c "SELECT cost_usd FROM mcp.server WHERE name = 'demo';")
python3 -c "import sys; v=float(sys.argv[1]); assert abs(v-0.004)<1e-9, v" "$set_price"
"${PSQL[@]}" -d rvbbit_regress -c "ALTER SERVER demo OPTIONS (DROP cost_usd);"
dropped=$("${PSQL[@]}" -d rvbbit_regress -At -c "SELECT cost_usd IS NULL FROM mcp.server WHERE name = 'demo';")
test "$dropped" = "t"
"${PSQL[@]}" -d rvbbit_regress -c "ALTER SERVER demo OPTIONS (ADD cost_usd '0.002');"
added=$("${PSQL[@]}" -d rvbbit_regress -At -c "SELECT cost_usd FROM mcp.server WHERE name = 'demo';")
python3 -c "import sys; v=float(sys.argv[1]); assert abs(v-0.002)<1e-9, v" "$added"
set +e
missing=$("${PSQL[@]}" -d rvbbit_regress -c "ALTER SERVER nosuch OPTIONS (SET cost_usd '0.001');" 2>&1)
status=$?
set -e
if [[ "$status" -eq 0 ]]; then
  echo "FAIL missing server was accepted: $missing" >&2
  exit 1
fi
grep -q 'MCP server "nosuch" does not exist' <<<"$missing"
echo "alter server ok"

echo "=== a high cap still reaches the provider ==="
before=$(calls)
"${PSQL[@]}" -d rvbbit_regress <<'SQL'
SET semantic.max_cost = '1';
SELECT count(*) FROM docs WHERE body MEANS 'road';
SQL
after=$(calls)
if [[ "$after" -le "$before" ]]; then
  echo "FAIL high cap did not call the provider ($before -> $after)" >&2
  exit 1
fi
echo "high cap calls $before -> $after"
echo "COST CAP REGRESS OK"
