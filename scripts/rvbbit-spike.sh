#!/usr/bin/env bash
# Go/no-go measurements against a running pgrust and the mock hutch.
#
# Expects:
#   PGHOST (default /tmp), PGPORT (default 55432), PGUSER (default postgres)
#   HUTCH (default http://127.0.0.1:8099) — the mock's base URL
# The server must have been started with RVBBIT_HUTCH_URL=$HUTCH/v1/means.
set -euo pipefail

PGHOST=${PGHOST:-/tmp}
PGPORT=${PGPORT:-55432}
PGUSER=${PGUSER:-postgres}
HUTCH=${HUTCH:-http://127.0.0.1:8099}
PSQL=(psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -v ON_ERROR_STOP=1 -X -q)

hutch() {
  local method=$1 path=$2
  local rest=${HUTCH#http://}
  local host=${rest%%/*}
  [[ $rest == */* ]] || host=$rest
  local body=${3:-}
  if [[ -n $body ]]; then
    curl -sS -X "$method" -H 'Content-Type: application/json' --data "$body" "$HUTCH$path"
  else
    curl -sS -X "$method" "$HUTCH$path"
  fi
}

echo "=== reset hutch ==="
hutch POST /reset
echo

echo "=== 10k MEANS, no prewarm ==="
"${PSQL[@]}" <<'SQL'
DROP TABLE IF EXISTS rvbbit_spike;
CREATE TABLE rvbbit_spike (id int, body text);
INSERT INTO rvbbit_spike
SELECT g, 'row-' || g::text || ' alpha'
FROM generate_series(1, 10000) AS g;
SQL
echo "--- EXPLAIN ---"
"${PSQL[@]}" -c "EXPLAIN SELECT count(*) FROM rvbbit_spike WHERE body MEANS 'alpha'"
echo "--- EXPLAIN ANALYZE ---"
"${PSQL[@]}" -c "EXPLAIN ANALYZE SELECT count(*) FROM rvbbit_spike WHERE body MEANS 'alpha'"
echo "--- count ---"
"${PSQL[@]}" -c "SELECT count(*) FROM rvbbit_spike WHERE body MEANS 'alpha'"
echo "--- hutch /stats (expect calls=313, rows=10000, max_batch=32) ---"
hutch GET /stats
echo

echo "=== cheap predicate first (id > 9000; expect ~32 calls) ==="
hutch POST /reset >/dev/null
echo "--- EXPLAIN ---"
"${PSQL[@]}" -c "EXPLAIN SELECT count(*) FROM rvbbit_spike WHERE id > 9000 AND body MEANS 'alpha'"
echo "--- count ---"
"${PSQL[@]}" -c "SELECT count(*) FROM rvbbit_spike WHERE id > 9000 AND body MEANS 'alpha'"
echo "--- hutch /stats ---"
hutch GET /stats
echo

echo "=== memo (identical bodies; expect far fewer than 313 calls) ==="
hutch POST /reset >/dev/null
"${PSQL[@]}" -c "SELECT count(*) FROM rvbbit_spike WHERE 'same' MEANS 'same'"
echo "--- hutch /stats ---"
hutch GET /stats
echo
