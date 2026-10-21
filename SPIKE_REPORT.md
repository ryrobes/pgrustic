# Spike report: native `MEANS`

**Recommendation: go.** A scan-level `expr MEANS 'text'` filter can live in this engine. The batch path, cancel, and the upstream regression gate all behaved the way the spike asked. The seams are few enough to rebase. Wider syntax (joins, `HAVING`, target lists, a real catalog row) is follow-on work, and the lexer special case will get awkward if many new words are added the same way.

M1 and M2 continued on `cursor/rvbbit-m1-m2-4990` (PR #2, stacked on this branch). The catalog row, dump round trip, and hutch contract are in `M1_M2_REPORT.md`. The 313-call, cancel, and 217/231 numbers below are the spike baseline; M1/M2 re-measured them and they held. `EXPLAIN` now prints `rvbbit.means(...)` once the catalog row exists.

Branch `cursor/rvbbit-semantic-spike-4990`. Measurements below are from this VM on 2026-10-21, against `target/fast-profile/postgres` and `hutch_mock` on `127.0.0.1:8099`.

## Criteria

| # | Criterion | Result |
| --- | --- | --- |
| a | 10k-row `MEANS` query issues ~313 HTTP calls, no prewarm | **Pass.** 313 calls, batch size 32. |
| b | Cancel works mid-dispatch | **Pass.** `statement_timeout = 500ms` aborted in 515ms. |
| c | Upstream regression stays at the pre-spike baseline | **Pass.** 217/231 on `main` (`1bc8065fa9`) and 217/231 on the spike, same 14 tests. |
| d | Upstream files touched ≤ ~15, listed in `RVBBIT_SEAMS.md` | **Pass.** 14 source files. |
| — | Fixed-latency benchmark | **Done.** 50ms mock latency, 10k rows, 4030.945ms. |

## (a) 10k rows, 313 calls

`scripts/rvbbit-spike.sh` loads 10,000 rows (`body` text contains `alpha`) and runs the filter with no cache warmup. `EXPLAIN ANALYZE` is the call counter that belongs to a single execution. The script then runs the same `SELECT` again, so `GET /stats` after both statements is 626. One execution is 313.

```
EXPLAIN ANALYZE SELECT count(*) FROM rvbbit_spike WHERE body MEANS 'alpha';
```

```
Semantic Filter
  Batch Size: 32
  Concurrency: 4
  Batches: 313
  HTTP Calls: 313
  Memo Hits: 0
  Semantic Rows In: 10000
  Semantic Rows Out: 10000
Execution Time: 29.580 ms
```

`GET /stats` after that single `EXPLAIN ANALYZE` (50ms-latency run, below) was `calls: 313, rows: 10000, max_batch: 32`. `10000 / 32 = 312.5`, so 313 posts is the exact batch count.

Cheap predicate stays on the scan. `EXPLAIN` of `WHERE id > 9000 AND body MEANS 'alpha'` shows `Filter: (id > 9000)` on the Seq Scan and `rvbbit_means` only on the Semantic Filter. The following `SELECT count(*)` returned 1000 and `GET /stats` was `calls: 32, rows: 1000, max_batch: 32`.

Memo, identical pair on every row:

```
EXPLAIN ANALYZE SELECT count(*) FROM rvbbit_spike WHERE 'same' MEANS 'same';
```

```
Batches: 313
HTTP Calls: 4
Memo Hits: 9996
Execution Time: 13.284 ms
```

`GET /stats`: `calls: 4, rows: 4, max_batch: 1, batches: [1,1,1,1]`. Four calls go out because four batches are in flight before the first response fills the memo.

## (b) Cancel

Mock restarted with `HUTCH_LATENCY_MS=3000`. Each post sleeps 3s. Concurrency is 4, so a full 10k scan would take on the order of `313 / 4 * 3s`.

```
psql -h /tmp -p 55432 -U postgres -v ON_ERROR_STOP=1 \
  -c "SET statement_timeout = '500ms'" \
  -c "SELECT count(*) FROM rvbbit_spike WHERE body MEANS 'alpha'"
```

```
ERROR:  canceling statement due to statement timeout
wall_ms=515
```

`psql` exited 1. After the in-flight posts finished, `GET /stats` was `calls: 4, rows: 128, max_batch: 32`. The backend stopped waiting. The four HTTP calls already handed to workers ran to completion; the pool does not abort a `ureq` call that has started. That is the behavior described in `RVBBIT_SEAMS.md`.

## (c) Regression baseline

`scripts/pg-regress-fast.sh` was missing from the tree (the README names it). The script in this branch drives the PostgreSQL 18 `pg_regress` binary with `--use-existing` against a pgrust postmaster. Two harness facts matter:

- The schedule path has to be absolute. `pg_regress` opens it relative to the cwd.
- `LANGUAGE C` tests load `regress.so` by absolute path. pgrust serves that module from the builtin registry in `regress_lib`, and `stat()` has to succeed first. The script creates an empty `$REGRESS_DLPATH/regress.so` (default `/tmp/pgrust-dl/regress.so`) and passes `--dlpath`.

Without that placeholder the same server failed 32/231, led by `could not access file ".../regress.so"`. With it, the suite is the comparison below.

Both runs: `parallel_schedule` (231 tests), `--dlpath=/tmp/pgrust-dl`, server flag `max_parallel_workers_per_gather=2` (PostgreSQL 18's boot value; this fork's boot value is 4). A spike run at the fork default of 4 failed the same 14 tests.

| Tree | Result | Wall clock |
| --- | --- | --- |
| `main` `1bc8065fa9` | **217 ok, 14 failed** | 04:14:18–04:14:34Z (~16s) |
| spike, this branch | **217 ok, 14 failed** | 04:04:08–04:04:24Z (~16s) |

Failing tests, both trees: `create_index`, `select_distinct`, `subselect`, `join`, `aggregates`, `portals`, `join_hash`, `tidscan`, `incremental_sort`, `limit`, `partition_join`, `partition_prune`, `partition_aggregate`, `memoize`.

Thirteen of the fourteen result files are byte-identical between `main` and the spike. `memoize` differs in one `EXPLAIN ANALYZE` hit line (`Hits: 998` on the spike; `Hits: 992` plus `Worker 0: Hits: 4` on `main`). Both still fail the vendor expected file. The other failures are plan-shape diffs against `crates/postgres-18.6-reference` expected output (parallel Gather where the expected file has a non-parallel plan, or a different worker count). `portals` drops a `Materialize` on two scroll-cursor explains; `proofs/TESTING-STRATEGY.md` already calls that out as a ruled divergence. The 230/230 figure in that note used an overlay comparator that is not in this checkout. This run compared raw vendor expected files.

`boolean` passes on the spike (`ok`, 113ms in the full schedule). The word `means` appears in the regress SQL only inside comments, so the lexer special case does not rewrite those statements.

## (d) Files

14 upstream source files, listed with reasons in `RVBBIT_SEAMS.md`. Eight manifests plus `Cargo.lock` gain path dependencies. New behavior is under `crates/rvbbit/` (`rvbbit_oids`, `rvbbit_plan`, `rvbbit_dispatch`, `rvbbit_builtins`, `hutch_mock`). `crates/backend/executor/execmain/src/semantic.rs` is new and sits in an upstream crate because the batch loop calls `exec_proc_node` and slots.

## Latency

`HUTCH_LATENCY_MS=50`, same 10k query, one `EXPLAIN ANALYZE`:

```
Batches: 313
HTTP Calls: 313
Memo Hits: 0
Execution Time: 4030.945 ms
```

`313 / 4 * 50ms = 3912.5ms`. The run landed 118ms above that floor.

## Build times

| Build | Time | Note |
| --- | --- | --- |
| Incremental `fast-profile` link of `main_main` on the already-warm spike tree | 1.98s | `/tmp/pg-build.log`, 03:55:24–03:55:26Z |
| `main` rebuilt after checking out `1bc8065fa9` from the spike tree | **4m 24s** (265s wall) | `cargo build -p main_main --bin postgres --profile fast-profile`, 04:06:56–04:11:21Z |
| Dispatch test on the spike branch | 14.49s compile, test ok | `cargo test -p rvbbit_dispatch --profile fast-profile --test hutch` (`batch_match_and_rate_limit`) |
| Spike `postgres` rebuilt after that test | 26.54s | same `main_main` command, 04:16Z. Shared crates were already warm from the test. Combined wall for test + binary was 42s |
| Cold compile of this checkout | not re-measured | `target/` was already populated when measurement started |

The first `main` rebuild attempt failed in `regexp_alt`: `/usr/bin/c++` is Ubuntu clang 18, and `re2.h` includes `<algorithm>`, which that driver did not find. `CC=gcc CXX=g++` compiles the shim. The link also needs `/usr/lib/x86_64-linux-gnu/libstdc++.so` pointing at `libstdc++.so.6` (the Ubuntu package shipped the versioned library only).

## What was hard

- Adding a gram.y production renumbers the bison actions that `gram_core` indexes. `MEANS` is the existing `Op` token, emitted when the previous token ends an expression and the identifier is `means`. `SELECT means`, `AS means`, and `t.means` stay identifiers. A bare alias (`SELECT expr means`) does not parse; write `AS means`.
- There is no `pg_proc` row. Planner cost and volatility come from a synthetic projection for oid 9020. `EXPLAIN` deparse calls `generate_function_name`, which SearchSysCache's the oid; that path returns `rvbbit_means` directly. Calling `rvbbit_means(...)` by name still does not resolve.
- The batch loop has to live in `execmain`. A sibling crate would cycle on `exec_proc_node`.
- The regress driver and the `regress.so` placeholder were both missing. A run that skips the placeholder reports a pile of `LANGUAGE C` failures that are a harness gap.

## Rebase risk

Low for the parser, moderate for the executor hook.

- No new grammar production and no new keyword token. A scanner rebase that rewrites `lex.rs` can collide with about thirty lines. Action-table renumbering from upstream gram.y changes does not get worse because of this spike.
- `parse_expr`, `projections.rs`, `ruleutils`, `planner`, and `explain` are early returns or a single call (`lift_semantic_filters` just before `set_plan_references`). Small conflict surface. New plan shapes upstream adds (joins, subquery filters) will not be lifted until `rvbbit_plan` learns them; today those shapes keep the per-row builtin.
- `noderesult.rs`, `execmain.rs`, and `procnode.rs` are the likely conflict sites. `semantic.rs` is a new file and rebases cleanly. If upstream reworks Result, the hook to reattach is the `ResultState.rvbbit` field and the three call sites.
- Dispatch, the mock, and the oid constants are new crates. They do not rebase against upstream pgrust at all.

## Follow-ons, in order

1. A real `pg_proc` / `pg_proc` builtin entry so oid 9020 is not a side channel, and so `rvbbit_means` can be called by name.
2. Lift `MEANS` in joins and `HAVING`, or document them as per-row on purpose.
3. Abort or bound the HTTP call itself on cancel. The backend wait already honors `statement_timeout`.
4. If more clause words arrive, replace the single-identifier lexer check with one table. Still avoid new bison productions.
