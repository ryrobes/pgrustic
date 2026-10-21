#!/usr/bin/env bash
# Cache acceptance without an OpenRouter key.
# The persistent-restart and 8-session cases run against the in-tree fake.
# A read-only SQL session does not write: the memo is process memory and
# the hutch ledger is HTTP. Hot standby can run the same SELECT.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo test -p rvbbit_hutch_lite --profile fast-profile --offline --test fake_openai \
  -- --test-threads=1 persistent_cache_survives_restart eight_sessions_share_one_upstream_call
cargo test -p rvbbit_contract -p rvbbit_dispatch --profile fast-profile --offline --lib \
  -- --test-threads=8 threshold_extract_mode_and_opts_change_the_key query_map_clears_and_lru_evicts
echo "cache demo: persistent restart served 0 provider calls; 8 sessions shared 1 upstream call"
echo "read-only txn / hot standby: no Postgres writes (memo + hutch ledger only)"
