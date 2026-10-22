#!/usr/bin/env bash
# Load N rows from a large CSV and run MEANS, CLASSIFY, and EXTRACT.
# Prints hutch /stats and rvbbit.calls (cache and cost) after each statement.
# MEANS is run twice. The filter memo is per query, and hutch_mock does not
# keep a decision cache, so the second run posts again. The delta is printed.
#
# The Enron emails CSV is about 1.4GB and is not in this tree. Point CSV
# at it. The usual Kaggle export has columns `file` and `message`.
#   CSV=/path/to/emails.csv N=5000 bash scripts/rvbbit_enron_scale.sh
#
# Ports: Postgres 55444, mock 8108. The spike on 55432 / 8099 is not used.
# Run with `env -u PGPORT -u HUTCH_PORT` so a leftover spike port is not picked up.
# No OpenRouter key. This script uses hutch_mock.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

PG_BIN=${PG_BIN:-/usr/lib/postgresql/18/bin}
PGPORT=${PGPORT:-55444}
HUTCH_PORT=${HUTCH_PORT:-8108}
PGHOST=${PGHOST:-/tmp}
PGUSER=${PGUSER:-postgres}
PGDATA=${PGDATA:-/tmp/rvbbit-enron-pg}
RUNDIR=${RUNDIR:-/tmp/rvbbit-enron}
CSV=${CSV:-}
N=${N:-1000}
COLUMN=${COLUMN:-message}
ID_COLUMN=${ID_COLUMN:-file}
CRITERION=${CRITERION:-a meeting was scheduled}
LABELS=${LABELS:-meeting,legal,personal,other}
KIND=${KIND:-person}
EXPECT_MEANS=${EXPECT_MEANS:-}
export CC=${CC:-gcc}
export CXX=${CXX:-g++}
export PATH="$PG_BIN:$PATH"
export PGRUST_PGSHAREDIR=${PGRUST_PGSHAREDIR:-/usr/share/postgresql/18}
export PGRUST_TZDIR=${PGRUST_TZDIR:-/usr/share/zoneinfo}
export RVBBIT_HUTCH_URL="http://127.0.0.1:${HUTCH_PORT}"

if [[ "$PGPORT" == "55432" || "$HUTCH_PORT" == "8099" ]]; then
  echo "Refusing spike ports 55432 / 8099. Use the defaults (55444 / 8108)." >&2
  exit 1
fi
if [[ -z "$CSV" || ! -f "$CSV" ]]; then
  echo "Set CSV to the emails file (about 1.4GB). It is not in this tree." >&2
  echo "  CSV=/path/to/emails.csv N=5000 bash scripts/rvbbit_enron_scale.sh" >&2
  exit 2
fi
if ! [[ "$N" =~ ^[1-9][0-9]*$ ]]; then
  echo "N must be a positive integer, got $N" >&2
  exit 2
fi
if ! [[ "$KIND" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]]; then
  echo "KIND must be a single identifier, got $KIND" >&2
  exit 2
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

mkdir -p "$RUNDIR"
SLICE=$RUNDIR/slice.csv
echo "=== slice $N rows from $CSV column $COLUMN ==="
CSV="$CSV" N="$N" COLUMN="$COLUMN" ID_COLUMN="$ID_COLUMN" SLICE="$SLICE" python3 - <<'PY'
import csv, os, sys
csv.field_size_limit(50_000_000)
path = os.environ["CSV"]
column = os.environ["COLUMN"]
id_column = os.environ["ID_COLUMN"]
limit = int(os.environ["N"])
out = os.environ["SLICE"]
written = 0
with open(path, newline="", encoding="utf-8", errors="replace") as src, open(out, "w", newline="", encoding="utf-8") as dst:
    reader = csv.DictReader(src)
    if not reader.fieldnames or column not in reader.fieldnames:
        names = ", ".join(reader.fieldnames or [])
        sys.exit(f"column {column!r} is not in the CSV header ({names})")
    writer = csv.writer(dst)
    writer.writerow(["id", "body"])
    for i, row in enumerate(reader, start=1):
        if written >= limit:
            break
        body = row.get(column) or ""
        if not body.strip():
            continue
        ident = (row.get(id_column) or "").strip() or str(i)
        writer.writerow([ident, body])
        written += 1
if written == 0:
    sys.exit(f"no non-empty {column!r} values in {path}")
print(f"sliced {written} rows")
PY

IFS=',' read -ra LABEL_PARTS <<< "$LABELS"
INTO=""
for part in "${LABEL_PARTS[@]}"; do
  part="${part#"${part%%[![:space:]]*}"}"
  part="${part%"${part##*[![:space:]]}"}"
  if [[ -z "$part" ]]; then
    continue
  fi
  part="${part//\'/\'\'}"
  INTO+="'$part', "
done
INTO="${INTO%, }"
if [[ -z "$INTO" || "$INTO" != *","* ]]; then
  echo "LABELS needs at least two names, got $LABELS" >&2
  exit 2
fi

echo "=== build (CC=$CC CXX=$CXX) ==="
cargo build -p main_main -p hutch_mock --profile fast-profile

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

curl -sf -X POST "http://127.0.0.1:$HUTCH_PORT/reset" >/dev/null

PSQL=(psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -v ON_ERROR_STOP=1 -X)
echo "=== load ==="
"${PSQL[@]}" -c "DROP TABLE IF EXISTS enron" -c "CREATE TABLE enron (id text, body text)"
"${PSQL[@]}" -c "COPY enron (id, body) FROM '$SLICE' WITH (FORMAT csv, HEADER true)"
"${PSQL[@]}" -c "SELECT count(*) AS loaded FROM enron"

report() {
  local title=$1
  echo "=== $title: hutch /stats ==="
  curl -sf "http://127.0.0.1:$HUTCH_PORT/stats"
  echo
  echo "=== $title: rvbbit.calls ==="
  "${PSQL[@]}" -c "SELECT op, cache, count(*) AS n, round(coalesce(sum(cost), 0)::numeric, 8) AS cost, round(coalesce(sum(cost_saved), 0)::numeric, 8) AS cost_saved FROM rvbbit.calls GROUP BY op, cache ORDER BY op, cache"
}

crit=${CRITERION//\'/\'\'}
echo "=== MEANS ==="
"${PSQL[@]}" -c "SELECT count(*) AS means_rows FROM enron WHERE body MEANS '${crit}'"
report "after MEANS"
MEANS_CALLS=$(curl -sf "http://127.0.0.1:$HUTCH_PORT/stats" | python3 -c 'import json,sys; print(json.load(sys.stdin)["calls"])')

echo "=== MEANS again ==="
"${PSQL[@]}" -c "SELECT count(*) AS means_rows FROM enron WHERE body MEANS '${crit}'"
report "after second MEANS"
MEANS_CALLS_2=$(curl -sf "http://127.0.0.1:$HUTCH_PORT/stats" | python3 -c 'import json,sys; print(json.load(sys.stdin)["calls"])')
echo "MEANS provider calls: ${MEANS_CALLS} then ${MEANS_CALLS_2}"

echo "=== CLASSIFY ==="
"${PSQL[@]}" -c "SELECT class, count(*) FROM (SELECT CLASSIFY(body INTO $INTO) AS class FROM enron) s GROUP BY class ORDER BY class"
report "after CLASSIFY"

echo "=== EXTRACT ==="
"${PSQL[@]}" -c "SELECT count(*) AS extract_rows FROM (SELECT EXTRACT($KIND FROM body) ON ERROR '' AS found FROM enron) s"
report "after EXTRACT"

if [[ -n "$EXPECT_MEANS" ]]; then
  got=$("${PSQL[@]}" -t -A -c "SELECT count(*) FROM enron WHERE body MEANS '${crit}'")
  if [[ "$got" != "$EXPECT_MEANS" ]]; then
    echo "MEANS count $got, expected $EXPECT_MEANS" >&2
    exit 1
  fi
fi

echo "enron scale script passed ($N rows requested)"
