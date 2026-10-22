#!/usr/bin/env bash
# EXTRACT MANY recall: proper nouns, descriptive phrases, and synonyms.
#
# The default path checks the prompt, temperature 0, and the JSON schema.
# It does not start Postgres and it does not spend a provider call.
#
# Live, against hutch-lite (a real chat model, not the mock):
#   RVBBIT_EXTRACT_LIVE=1 bash scripts/rvbbit_extract_recall.sh
# Requires OPENROUTER_API_KEY. Two runs with semantic.cache off must agree.
# Row 3605 must include Bigfoot. Row 23987 must include gray. A note with
# neither stays empty.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

cargo test -p rvbbit_dispatch -p rvbbit_hutch_lite --profile fast-profile --offline \
  --lib --test fake_openai -- \
  extract_many_asks_only_for_the_requested_kind \
  extract_many_keeps_only_strings_and_allows_an_empty_list \
  extract_many_chat_sends_temperature_zero_and_a_strict_schema \
  extract_many_structured_output_unwraps_mentions_and_retries_without_schema \
  rerank_summary_and_extract_many
echo "extract recall: prompt examples, temperature 0, and structured output passed"

if [[ "${RVBBIT_EXTRACT_LIVE:-}" != "1" ]]; then
  echo "export RVBBIT_EXTRACT_LIVE=1 and OPENROUTER_API_KEY to score the Bigfoot and gray rows"
  exit 0
fi
if [[ -z "${OPENROUTER_API_KEY:-}" ]]; then
  echo "RVBBIT_EXTRACT_LIVE=1 needs OPENROUTER_API_KEY" >&2
  exit 1
fi

# Fail before initdb when the chat base is a dead proxy or the key is rejected.
CHAT_BASE=${OPENROUTER_BASE_URL:-https://openrouter.ai/api/v1}
CHAT_BASE=${CHAT_BASE%/}
CHAT_MODEL=${HUTCH_MODEL:-openai/gpt-4o-mini}
echo "=== probe $CHAT_MODEL at $CHAT_BASE ==="
probe_body=$(mktemp)
probe_code=$(curl -sS -o "$probe_body" -w '%{http_code}' --max-time 20 \
  "$CHAT_BASE/chat/completions" \
  -H "Authorization: Bearer ${OPENROUTER_API_KEY}" \
  -H 'Content-Type: application/json' \
  -d "{\"model\":\"${CHAT_MODEL}\",\"temperature\":0,\"max_tokens\":1,\"messages\":[{\"role\":\"user\",\"content\":\"ping\"}]}" \
  || true)
if [[ -z "$probe_code" || "$probe_code" == "000" ]]; then
  echo "chat probe failed at $CHAT_BASE: connection failed" >&2
  echo "Live recall needs a reachable chat model. Check OPENROUTER_BASE_URL and OPENROUTER_API_KEY." >&2
  rm -f "$probe_body"
  exit 1
fi
python3 - "$probe_code" "$probe_body" "$CHAT_BASE" <<'PY'
import json, sys
code, path, base = sys.argv[1], sys.argv[2], sys.argv[3]
if code == "200":
    raise SystemExit(0)
detail = ""
try:
    err = json.load(open(path)).get("error")
    if isinstance(err, dict):
        detail = str(err.get("message", ""))[:200]
except Exception:
    detail = open(path, errors="replace").read()[:200]
print(f"chat probe failed at {base}: HTTP {code} {detail}", file=sys.stderr)
print("Live recall needs a reachable chat model. Check OPENROUTER_BASE_URL and OPENROUTER_API_KEY.", file=sys.stderr)
raise SystemExit(1)
PY
rm -f "$probe_body"

PG_BIN=${PG_BIN:-/usr/lib/postgresql/18/bin}
# Ignore an ambient PGPORT so this does not attach to another postmaster.
EXTRACT_PORT=${RVBBIT_EXTRACT_PGPORT:-55440}
HUTCH_PORT=${RVBBIT_EXTRACT_HUTCH_PORT:-8104}
PGHOST=${PGHOST:-/tmp}
PGUSER=${PGUSER:-postgres}
PGDATA=${PGDATA:-/tmp/rvbbit-extract-pg}
RUNDIR=${RUNDIR:-/tmp/rvbbit-extract-recall}
DECISION=${HUTCH_DECISION_MODEL:-typesafe/jev-1.14}
export HUTCH_DECISION_MODEL="$DECISION"
export RVBBIT_HUTCH_URL="${RVBBIT_HUTCH_URL:-http://127.0.0.1:${HUTCH_PORT}}"
export CC=${CC:-gcc}
export CXX=${CXX:-g++}
export PATH="$PG_BIN:$PATH"
export PGRUST_PGSHAREDIR=${PGRUST_PGSHAREDIR:-/usr/share/postgresql/18}
export PGRUST_TZDIR=${PGRUST_TZDIR:-/usr/share/zoneinfo}
unset PGPORT || true
PGPORT=$EXTRACT_PORT
export PGPORT

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
STARTED_HUTCH=0
cleanup() {
  if [[ "$STARTED_HUTCH" == 1 && -n "${HUTCH_PID:-}" ]]; then
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
  STARTED_HUTCH=1
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
for _ in $(seq 1 80); do
  if curl -sf "${RVBBIT_HUTCH_URL%/}/stats" >/dev/null && \
     psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -X -q -c 'SELECT 1' >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 0.25
done
if [[ "$ready" != 1 ]]; then
  echo "hutch or postgres did not become ready" >&2
  tail -n 40 "$HUTCH_LOG" >&2 || true
  tail -n 40 "$PG_LOG" >&2 || true
  exit 1
fi

PSQL=(psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -v ON_ERROR_STOP=1 -X)
"${PSQL[@]}" -d postgres -c "DROP DATABASE IF EXISTS rvbbit_extract" -c "CREATE DATABASE rvbbit_extract"

cat >"$RUNDIR/setup.sql" <<SQL
CREATE BACKEND live TYPE hutch URL '${RVBBIT_HUTCH_URL}';
CREATE TABLE notes (id int PRIMARY KEY, body text);
INSERT INTO notes VALUES
  (3605, 'My story of Bigfoot...'),
  (23987, 'Large gray triangular aircraft'),
  (1, 'The invoice was paid on Tuesday.'),
  (2, 'A sasquatch crossed the road.');
SQL
"${PSQL[@]}" -d rvbbit_extract -f "$RUNDIR/setup.sql"

cat >"$RUNDIR/quality.sql" <<'SQL'
SET statement_timeout = '180s';
SET semantic.cache = 'off';
SELECT 'CREATURE';
SELECT id::text || E'\t' || coalesce(string_agg(lower(creature), '|' ORDER BY lower(creature)), '')
FROM (
  SELECT id, EXTRACT MANY creature FROM body FROM notes WHERE id IN (3605, 1, 2)
) s
GROUP BY id
ORDER BY id;
SELECT 'COLOR';
SELECT id::text || E'\t' || coalesce(string_agg(lower(color), '|' ORDER BY lower(color)), '')
FROM (
  SELECT id, EXTRACT MANY color FROM body FROM notes WHERE id IN (23987, 1)
) s
GROUP BY id
ORDER BY id;
SQL

echo "=== first pass ==="
"${PSQL[@]}" -d rvbbit_extract -Atq -f "$RUNDIR/quality.sql" | tee "$RUNDIR/pass1.txt"
echo "=== second pass (cache off, must match) ==="
"${PSQL[@]}" -d rvbbit_extract -Atq -f "$RUNDIR/quality.sql" | tee "$RUNDIR/pass2.txt"

python3 - "$RUNDIR/pass1.txt" "$RUNDIR/pass2.txt" <<'PY'
import sys
first = [line for line in open(sys.argv[1]).read().splitlines() if line.strip()]
second = [line for line in open(sys.argv[2]).read().splitlines() if line.strip()]
print("pass1:", first)
print("pass2:", second)
if first != second:
    raise SystemExit("two cache-off passes disagreed")

def section(lines, name):
    if name not in lines:
        raise SystemExit(f"missing {name} marker: {lines}")
    start = lines.index(name) + 1
    rest = lines[start:]
    end = len(rest)
    for marker in ("CREATURE", "COLOR"):
        if marker in rest:
            end = min(end, rest.index(marker))
    out = {}
    for line in rest[:end]:
        if "\t" not in line:
            raise SystemExit(f"bad {name} row {line!r}")
        id_, mentions = line.split("\t", 1)
        out[id_] = mentions
    return out

creature = section(first, "CREATURE")
color = section(first, "COLOR")

def parts(text):
    return [p for p in text.split("|") if p]

def require(label, text, needle):
    got = parts(text)
    if not any(needle in p for p in got):
        raise SystemExit(f"{label} missing {needle!r}: {got}")

def forbid(label, text, banned):
    got = parts(text)
    for p in got:
        for word in banned:
            if word in p.split():
                raise SystemExit(f"{label} included {word!r} in {p!r}")

def empty(label, text):
    got = parts(text)
    if got:
        raise SystemExit(f"{label} should be empty, got {got}")

if set(creature) != {"1", "2", "3605"}:
    raise SystemExit(f"creature ids: {sorted(creature)}")
if set(color) != {"1", "23987"}:
    raise SystemExit(f"color ids: {sorted(color)}")

require("3605 creature", creature["3605"], "bigfoot")
forbid("3605 creature", creature["3605"], ["story", "road", "gray", "grey", "aircraft", "sasquatch"])
require("2 creature", creature["2"], "sasquatch")
forbid("2 creature", creature["2"], ["road", "crossed"])
empty("1 creature", creature["1"])

require("23987 color", color["23987"], "gray")
forbid("23987 color", color["23987"], ["aircraft", "triangular", "large", "bigfoot", "story"])
empty("1 color", color["1"])
print("extract recall live: Bigfoot, gray, sasquatch, and the empty note agreed across two passes")
PY

echo "=== done. RVBBIT_HUTCH_URL=$RVBBIT_HUTCH_URL ==="
