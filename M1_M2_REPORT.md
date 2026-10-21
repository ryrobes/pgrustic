# M1 and M2: catalog DDL and the hutch contract

Stacked on the MEANS spike (`cursor/rvbbit-semantic-spike-4990`, PR #1). This branch is `cursor/rvbbit-m1-m2-4990`. Measurements are from this VM on 2026-10-21, against `target/fast-profile/postgres` and `hutch_mock` on `127.0.0.1:8099`. No real model provider was called.

## Result

M1 and M2 are in. `MEANS` resolves through the catalog (operator → semantic function → model → backend). The six DDL kinds store, depend, grant, and dump. Dispatch and the mock speak `rvbbit-hutch-v1`. The spike's call count, cancel, and the upstream 217/231 gate are unchanged.

| Check | Result |
| --- | --- |
| rvbbit regress (`scripts/rvbbit-regress.sh`, database `rvbbit_regress`) | **Pass.** 6/6: unreserved, ddl, deps, grants, dump, means. |
| Dump round trip | **Pass.** `rvbbit.dump_sql()` after `restore_sql` equals the saved text. |
| Catalog-driven MEANS | **Pass.** First predicate bootstraps `hutch` → `default` → `rvbbit.means` → operator `means`. `hello alpha MEANS 'alpha'` is true. `EXPLAIN` shows `Semantic Filter` and `Filter: rvbbit.means(...)`. |
| 10k MEANS, one execution | **Pass.** 313 HTTP calls, batch 32, concurrency 4, memo hits 0, 10000 rows. Execution 28.762 ms. |
| Script `/stats` after EXPLAIN ANALYZE plus the following SELECT | 626 calls, 20000 rows. Same double-count as the spike. One execution is 313. |
| Cheap predicate `id > 9000` | 32 calls, 1000 rows, max batch 32. |
| Identical pair `'same' MEANS 'same'` | HTTP calls 4, memo hits 9996. `/stats`: calls 4, rows 4, max batch 1, batches `[1,1,1,1]`. |
| 50 ms mock latency, 10k rows | 4025.125 ms, 313 calls, max batch 32. Spike was 4030.945 ms. |
| Cancel, `statement_timeout = 500ms`, mock latency 3s | **Pass.** `ERROR: canceling statement due to statement timeout`, psql exit 1, wall 515 ms. After the in-flight posts: calls 4, rows 128, max batch 32. |
| Upstream `scripts/pg-regress-fast.sh` | **Pass vs baseline.** 217/231 in 15s. Same 14 failures: `create_index`, `select_distinct`, `subselect`, `join`, `aggregates`, `portals`, `join_hash`, `tidscan`, `incremental_sort`, `limit`, `partition_join`, `partition_prune`, `partition_aggregate`, `memoize`. |
| `regression` database has schema `rvbbit` | No. The schema is created on first MEANS or rvbbit DDL, and that suite does neither. |

## What landed

**M1.** `CREATE` / `ALTER` / `DROP` for `BACKEND`, `MODEL` (including `EMBEDDING MODEL`), `SEMANTIC FUNCTION`, `SEMANTIC OPERATOR` (word, symbol, negator), `DECISION TREE` (ASK/YES/NO and CHOOSE, stored as text plus jsonb, not executed), and `WORKFLOW` (name, version, body). Relations live in schema `rvbbit` with oids from `rvbbit.oid_seq` (starts at 910000). `pg_depend` rows use those tables' `pg_class` oids as `classid` (`deptype = 'n'`). `RESTRICT` names the dependent. `CASCADE` walks backend → model → function → operator, and model → tree. `GRANT`/`REVOKE EXECUTE` on a semantic function is a real function grant (`has_function_privilege`). `GRANT`/`REVOKE USAGE` on a backend updates the `acl` column. Comments go to the row and to `pg_description` (and `COMMENT ON FUNCTION` for semantic functions). `\dt rvbbit.*`, `\dv rvbbit.*`, and `\df rvbbit.means` work.

Secrets: `AUTH ENV` and `AUTH SECRET` both store an environment-variable name. The value is read at dispatch and is not written to the catalog.

The supported dump round trip is `rvbbit.dump_sql()` / `rvbbit.restore_sql(text)`. Stock `pg_dump` does not emit this metadata. Command tags reuse the pinned PG 18.3 tags, so psql prints `CREATE FUNCTION` for `CREATE BACKEND`. `cmdtaglist.h` was not edited.

**Grammar.** New words are tokens of `crates/rvbbit/grammar/rvbbit.y` only. bison 3.8.2 with `%expect 0` reports no conflicts (the nested `CHOOSE`/`WHEN` shift is forced with `%nonassoc`). The engine parses with `rvbbit_parse`. Nothing was added to PostgreSQL `gram.y` or `kwlist.h`, so `SELECT 1 AS backend` still works. Splicing productions in front of `stmt`'s `/*EMPTY*/` (reference `gram.y` line 1116) would renumber every later bison rule, and `gram_core` `actions.rs` matches those numbers. That cost is written up in `RVBBIT_SEAMS.md`. This branch does not pay it.

**M2.** `docs/rvbbit/HUTCH_CONTRACT.md` is contract `rvbbit-hutch-v1`. Types are in `crates/rvbbit/rvbbit_contract` (no engine dependencies): batch predict/classify/extract/embed/rerank/decide, positional item errors, cache key v1, `POST /v1/cache/lookup`, plus put/purge/stats shapes for the real hutch. The client key does not include the tenant. `storage_key` mixes the tenant on the hutch side. MEANS posts `{backend}/v1/predict` with `X-Rvbbit-Cache: mode=off` and does not call lookup, so the 313-call count is unchanged. Backend rows supply URL, timeout, retries, concurrency, and the auth env var. Transport is still `ureq` on the fixed pool. In-flight calls are not aborted.

The real hutch is a separate private repo and was not modified. The contract doc lists the twelve changes it needs (contract header, batch body, cache header modes, tenant storage key, lookup/put/purge/stats, writer queue, single-flight, `cost_basis = "cache"`, cancel on disconnect, `Retry-After`). The mock implements the six operations and lookup (always a miss, not counted in `/stats`). It does not implement put, purge, stats, or a cache table.

## Upstream files

19 source files under `crates/backend` and `crates/_support` differ from `main` (`1bc8065fa9`). The spike's 14 are still there. M1/M2 added 5:

| File | Why |
| --- | --- |
| `crates/_support/types/nodes/src/parsenodes.rs` | `OBJECT_RVBBIT` appended after `OBJECT_VIEW`. Existing discriminants stay put. |
| `crates/backend/commands/seclabel/src/lib.rs` | Exhaustive `ObjectType` arm. |
| `crates/backend/parser/parser_driver/src/lib.rs` | rvbbit DDL becomes `DefineStmt`. Scripts with none of it are parsed exactly as before. |
| `crates/backend/tcop/utility/src/dispatch.rs` | Runs that statement through the catalog hook. |
| `crates/backend/tcop/utility/src/commandtag.rs` | Reuses pinned command tags. |

Manifest edits are path dependencies only (`parser_driver`, `utility`, `explain`, `seams_init`, `parse_expr`, `execmain`, plus the workspace `Cargo.toml` / `Cargo.lock`). New code is under `crates/rvbbit/` (`rvbbit_catalog`, `rvbbit_contract`, `rvbbit_parse`, `rvbbit_resolve`, `grammar/rvbbit.y`) and `regress/rvbbit/`.

## Build

`CC=gcc CXX=g++` (the system `c++` is clang and cannot compile re2). Incremental `cargo build -p main_main -p hutch_mock --profile fast-profile` was 1m 47s. The later drop-order fix relinked in 3.7s. `cargo test -p rvbbit_parse`, `rvbbit_contract`, and `rvbbit_dispatch --test hutch` passed (the dispatch test is one test; the pool is process-global).

## Open questions

1. **Command tags.** psql says `CREATE FUNCTION` for every rvbbit DDL verb that is not alter/drop/grant/revoke/comment. A real tag needs a new `cmdtaglist.h` entry, which is pinned to PG 18.3 the same way `gram.y` rule numbers are.
2. **Stock `pg_dump`.** It will not emit backends, models, operators, trees, or workflows. `rvbbit.dump_sql()` is the round trip. Roles are not part of that text; restore assumes they already exist.
3. **Other infix words.** `ABOUT` and any operator other than `means` can be stored. The lexer still special-cases only `means`, and the batch executor still returns bools. Non-bool operators are not executed.
4. **Trees and workflows** are stored and dumped. Nothing runs an ASK/YES/NO tree or a workflow body. That is M3+.
5. **In-flight HTTP.** Cancel stops the backend wait (515 ms here). The four `ureq` calls already on workers run to completion. The contract asks real hutch to cancel provider work when the client disconnects. pgrustic still cannot abort `ureq`.
6. **Limiter lifetime.** Concurrency is keyed by URL plus the concurrency number. An `ALTER` that changes `max_concurrent` uses a new limiter. The old one stays for the life of the process.
7. **Drop implementation.** `performDeletion` rejects the custom classid (`unsupported object class`). `DROP` deletes `pg_depend` first, then `DROP FUNCTION`, then the row. A crash between those steps can leave a function without a semantic row, or the reverse.
8. **OpenRouter.** Worth one smoke test, but not from this tree against `api.openrouter.ai` directly. OpenRouter does not speak `rvbbit-hutch-v1`. The useful test is a real hutch (or a thin adapter in front of it) with a temporary key, one catalog `MEANS`, and one `extract`, to confirm the batch JSON and a non-bool value. The mock's predict is a substring, so it cannot catch a provider-shape mistake. A key is useful once that server exists. This run did not call a provider.
