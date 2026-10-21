#!/usr/bin/env bash
# Regression for the live M6 bugs: ORDER BY / LIMIT must finish, EXTRACT
# MANY must not leak e0_list, round() and a cast must batch, and
# |> MEANS_SCORE WITH TAKES must parse.
#
# Uses hutch_mock. An ambient OPENROUTER_API_KEY is not read.
# Ports 55437 / 18098, away from the spike and the M6 demo.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

PG_BIN=${PG_BIN:-/usr/lib/postgresql/18/bin}
PGPORT=${PGPORT:-55437}
HUTCH_PORT=${HUTCH_PORT:-18098}
PGHOST=${PGHOST:-/tmp}
PGUSER=${PGUSER:-postgres}
PGDATA=${PGDATA:-/tmp/rvbbit-m6-regress-pg}
RUNDIR=${RUNDIR:-/tmp/rvbbit-m6-regress}
export RVBBIT_HUTCH_URL="${RVBBIT_HUTCH_URL:-http://127.0.0.1:${HUTCH_PORT}}"
export CC=${CC:-gcc}
export CXX=${CXX:-g++}
export PATH="$PG_BIN:$PATH"
export PGRUST_PGSHAREDIR=${PGRUST_PGSHAREDIR:-/usr/share/postgresql/18}
export PGRUST_TZDIR=${PGRUST_TZDIR:-/usr/share/zoneinfo}

echo "=== build ==="
cargo build -p main_main -p hutch_mock --profile fast-profile --offline --bin postgres --bin hutch_mock

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

echo "=== hutch_mock on $RVBBIT_HUTCH_URL ==="
: >"$HUTCH_LOG"
./target/fast-profile/hutch_mock "$HUTCH_PORT" >"$HUTCH_LOG" 2>&1 &
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
"${PSQL[@]}" -d postgres -c "DROP DATABASE IF EXISTS rvbbit_regress" -c "CREATE DATABASE rvbbit_regress"

cat >"$RUNDIR/setup.sql" <<SQL
CREATE BACKEND live TYPE hutch URL '${RVBBIT_HUTCH_URL}';
CREATE MODEL mock ON BACKEND live NAME 'mock';
CREATE SEMANTIC FUNCTION means_score(text text, criterion text) RETURNS double precision
  MODEL mock
  PROMPT \$\$Does the text match the criterion?\$\$
  TEMPERATURE 0 CACHE memoize;
CREATE TABLE notes (id int, body text);
INSERT INTO notes VALUES
  (1, 'A large hairy biped crossed the logging road'),
  (2, 'Another tall figure on the same logging road'),
  (3, 'Lights hovered over the water'),
  (4, 'A truck passed the logging road'),
  (5, 'Nothing unusual by the lake');
SQL
"${PSQL[@]}" -d rvbbit_regress -f "$RUNDIR/setup.sql"

echo "=== ORDER BY the rerank score ==="
order=$({ timeout 20 "${PSQL[@]}" -d rvbbit_regress -At -c \
  "SELECT id || ':' || score FROM (SELECT id, body |> RERANK 'logging road' AS score FROM notes ORDER BY score DESC) s"; })
echo "order: $order"
# Ties keep heap order. The three logging rows outrank the other two.
printf '%s\n' "$order" | awk -F: '{ n++ } NR<=3 { if ($2+0 < 0.5) bad=1 } END { if (n != 5) bad=1; exit bad+0 }'

echo "=== ORDER BY id LIMIT 3 ==="
limited=$({ timeout 20 "${PSQL[@]}" -d rvbbit_regress -At -c \
  "SELECT id FROM (SELECT id, body |> RERANK 'logging road' AS score FROM notes ORDER BY id LIMIT 3) s"; })
echo "limit ids: $limited"
[[ "$limited" == $'1\n2\n3' ]]

echo "=== ORDER BY score LIMIT 3 ==="
timeout 20 "${PSQL[@]}" -d rvbbit_regress -c \
  "SELECT id, body |> RERANK 'logging road' AS score FROM notes ORDER BY score DESC LIMIT 3" >/dev/null

echo "=== CLASSIFY ordered by its label ==="
timeout 20 "${PSQL[@]}" -d rvbbit_regress -c \
  "SELECT CLASSIFY(body INTO 'road', 'lake') AS label FROM notes ORDER BY label"

echo "=== round() and a cast batch one query ==="
curl -sf -X POST "${RVBBIT_HUTCH_URL%/}/reset" >/dev/null
timeout 20 "${PSQL[@]}" -d rvbbit_regress -c \
  "SELECT round(body |> RERANK 'logging road') AS score FROM notes"
stats=$(curl -sf "${RVBBIT_HUTCH_URL%/}/stats")
python3 - "$stats" <<'PY'
import json, sys
stats = json.loads(sys.argv[1])
calls = stats["calls"]
rows = stats["rows"]
max_batch = stats["max_batch"]
print(f"round() stats calls={calls} rows={rows} max_batch={max_batch}")
if calls != 1 or rows != 5 or max_batch != 5:
    raise SystemExit(f"round() did not batch: {stats}")
PY

curl -sf -X POST "${RVBBIT_HUTCH_URL%/}/reset" >/dev/null
timeout 20 "${PSQL[@]}" -d rvbbit_regress -c \
  "SELECT (body |> RERANK 'logging road')::numeric AS score FROM notes"
stats=$(curl -sf "${RVBBIT_HUTCH_URL%/}/stats")
python3 - "$stats" <<'PY'
import json, sys
stats = json.loads(sys.argv[1])
print(f"cast stats calls={stats['calls']} rows={stats['rows']} max_batch={stats['max_batch']}")
if stats["calls"] != 1 or stats["rows"] != 5 or stats["max_batch"] != 5:
    raise SystemExit(f"cast did not batch: {stats}")
PY

echo "=== EXTRACT MANY hides e0_list and keeps a kind noun ==="
cols=$("${PSQL[@]}" -d rvbbit_regress -At -c \
  "SELECT string_agg(key, ',' ORDER BY key) FROM (
     SELECT json_object_keys(to_json(s)) AS key FROM (
       SELECT EXTRACT MANY creature FROM body FROM notes WHERE id = 1
     ) s
   ) k")
echo "extract columns: $cols"
case ",$cols," in
  *",e0_list,"*|*",src,"*) echo "helper column leaked: $cols" >&2; exit 1 ;;
esac
case ",$cols," in
  *",creature,"*) ;;
  *) echo "creature column missing: $cols" >&2; exit 1 ;;
esac
got=$("${PSQL[@]}" -d rvbbit_regress -At -c \
  "SELECT creature FROM (
     SELECT EXTRACT MANY creature FROM body FROM notes WHERE id = 1
   ) s")
echo "creature values: $got"
printf '%s\n' "$got" | grep -qx 'biped'

echo "=== |> MEANS_SCORE WITH TAKES ==="
timeout 20 "${PSQL[@]}" -d rvbbit_regress -c \
  "SELECT body |> MEANS_SCORE 'logging' WITH TAKES 3 AS s FROM notes WHERE id = 1"

echo "=== regress passed ==="
