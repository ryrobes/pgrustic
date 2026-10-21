#!/usr/bin/env bash
# rvbbit catalog tests against an already-running pgrust server.
# Uses database rvbbit_regress so the upstream `regression` database
# stays free of schema rvbbit.
#
#   PGPORT=55432 scripts/rvbbit-regress.sh
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
PG_BIN=${PG_BIN:-/usr/lib/postgresql/18/bin}
PG_REGRESS=${PG_REGRESS:-/usr/lib/postgresql/18/lib/pgxs/src/test/regress/pg_regress}
PGHOST=${PGHOST:-/tmp}
PGPORT=${PGPORT:-55432}
PGUSER=${PGUSER:-postgres}
PGDATABASE=${PGDATABASE:-rvbbit_regress}
OUT=${RVBBIT_REGRESS_OUT:-/tmp/rvbbit-regress}
export PATH="$PG_BIN:$PATH"

if [[ "${RVBBIT_REGRESS_KEEP:-0}" != 1 ]]; then
  dropdb -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" --if-exists "$PGDATABASE"
  # Roles are cluster-wide. Drop them after the database so leftover
  # grants from a failed run are already gone.
  psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -d postgres -v ON_ERROR_STOP=1 -q -c \
    "DROP ROLE IF EXISTS rvbbit_reader; DROP ROLE IF EXISTS rvbbit_dump_role;"
  createdb -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" "$PGDATABASE"
fi
mkdir -p "$OUT"

echo "pg_regress rvbbit db=$PGDATABASE"
"$PG_REGRESS" \
  --use-existing \
  --bindir="$PG_BIN" \
  --host="$PGHOST" \
  --port="$PGPORT" \
  --user="$PGUSER" \
  --dbname="$PGDATABASE" \
  --inputdir="$ROOT/regress/rvbbit" \
  --outputdir="$OUT" \
  --schedule="$ROOT/regress/rvbbit/schedule"
