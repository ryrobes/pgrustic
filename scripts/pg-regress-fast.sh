#!/usr/bin/env bash
# Drive upstream pg_regress against an already-running pgrust server.
#
# The README names this script; it was not in the tree. This recreation does
# not start the server (pgrust has no initdb of its own, and pg_regress
# --temp-instance would launch PostgreSQL's postmaster from --bindir).
#
#   initdb -D "$PGDATA" --no-locale --encoding=UTF8 --auth-local=trust --auth-host=trust -U postgres
#   ulimit -s 65520
#   PGRUST_PGSHAREDIR=/usr/share/postgresql/18 PGRUST_TZDIR=/usr/share/zoneinfo \
#     RUST_MIN_STACK=33554432 \
#     target/fast-profile/postgres -D "$PGDATA" -k /tmp -p "$PGPORT" \
#       -c listen_addresses= -c io_method=sync -c max_stack_depth=60000
#   PGPORT=55432 scripts/pg-regress-fast.sh
#   PGPORT=55432 scripts/pg-regress-fast.sh boolean int4
#
# Extra arguments are test names (pg_regress EXTRA-TEST). With no extra
# arguments the full parallel_schedule runs.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
REGRESS_IN=${REGRESS_IN:-$ROOT/crates/postgres-18.6-reference/src/test/regress}
PG_BIN=${PG_BIN:-/usr/lib/postgresql/18/bin}
PG_REGRESS=${PG_REGRESS:-/usr/lib/postgresql/18/lib/pgxs/src/test/regress/pg_regress}
PGHOST=${PGHOST:-/tmp}
PGPORT=${PGPORT:-55432}
PGUSER=${PGUSER:-postgres}
OUT=${REGRESS_OUT:-/tmp/pgrust-regress}
SCHEDULE=${SCHEDULE:-parallel_schedule}

mkdir -p "$OUT"
export PATH="$PG_BIN:$PATH"

args=(
  --use-existing
  --bindir="$PG_BIN"
  --host="$PGHOST"
  --port="$PGPORT"
  --user="$PGUSER"
  --dbname="${PGDATABASE:-regression}"
  --inputdir="$REGRESS_IN"
  --outputdir="$OUT"
)
if [[ $# -eq 0 ]]; then
  args+=(--schedule="$SCHEDULE")
fi

echo "pg_regress ${args[*]} $*"
"$PG_REGRESS" "${args[@]}" "$@"
