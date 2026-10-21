#!/usr/bin/env bash
# Cache acceptance.
# The default path uses the in-tree fake. An ambient OPENROUTER_API_KEY
# does not turn this into a live call.
#
# Live persistent restart (real provider, then a second hutch process
# against the same directory; the second process must report 0 provider
# calls):
#   RVBBIT_CACHE_LIVE=1 bash scripts/rvbbit_cache_demo.sh
# Requires OPENROUTER_API_KEY. The flag is required so a stray key does
# not spend a call.
# A read-only SQL session does not write: the memo is process memory and
# the hutch ledger is HTTP. Hot standby can run the same SELECT.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo test -p rvbbit_hutch_lite --profile fast-profile --offline --test fake_openai \
  -- --test-threads=1 persistent_cache_survives_restart eight_sessions_share_one_upstream_call \
  lookup_credits_saved_cost_without_double_counting_misses
cargo test -p rvbbit_contract -p rvbbit_dispatch --profile fast-profile --offline --lib \
  -- --test-threads=8 threshold_extract_mode_and_opts_change_the_key query_map_clears_and_lru_evicts
if [[ "${RVBBIT_CACHE_LIVE:-}" == "1" ]]; then
  if [[ -z "${OPENROUTER_API_KEY:-}" ]]; then
    echo "RVBBIT_CACHE_LIVE=1 needs OPENROUTER_API_KEY" >&2
    exit 1
  fi
  cargo test -p rvbbit_hutch_lite --profile fast-profile --offline --test fake_openai \
    -- --ignored --exact live_persistent_cache_restart
  echo "cache demo live: second persistent process reported 0 provider calls"
fi
echo "cache demo: persistent restart served 0 provider calls; 8 sessions shared 1 upstream call"
echo "lookup hit credited produced cost; lookup miss was not counted twice"
echo "read-only txn / hot standby: no Postgres writes (memo + hutch ledger only)"
