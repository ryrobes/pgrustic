# RVBBIT seams

Spike for an infix `expr MEANS 'text'` predicate that plans as a batched
HTTP call to a hutch service. New behavior lives in `crates/rvbbit/`. The
engine edits are the hooks that crate cannot reach on its own: the scanner
has no extension point for a contextual operator, `pg_proc` projections are
set-once seams, and the Result executor is not a plugin.

## Upstream files touched

13 existing or new files under the upstream tree, plus dependency lines in
7 manifests and `Cargo.lock`. The source count is inside the ~15 target.
Manifest edits are path dependencies only.

| File | Why |
| --- | --- |
| `crates/backend/parser/scan_fgram/src/lex.rs` | After an expression-ending token, identifier `means` is emitted as the existing `Op` token. No new gram.y production, so bison rule numbers stay put. |
| `crates/backend/parser/scan_fgram/src/lib.rs` | `Scanner` remembers the previous token for that decision. |
| `crates/backend/parser/scan_fgram/src/tests.rs` | Lexer test: infix `means` is `Op`; `SELECT means`, `AS means`, and `t.means` stay identifiers. |
| `crates/backend/parser/parse_expr/src/lib.rs` | `transformAExprOp` turns operator name `means` into `FuncExpr` oid 9020, coercing both sides to `text`. |
| `crates/backend/utils/cache/cache_syscache/src/projections.rs` | Synthetic `pg_proc` name, shape, and cost for oid 9020. There is no catalog row. Shape is volatile and parallel-unsafe. |
| `crates/backend/optimizer/plan/planner/src/lib.rs` | `lift_semantic_filters` runs immediately before `set_plan_references` on the main plan and each subplan. |
| `crates/backend/executor/execmain/src/noderesult.rs` | `ResultState.rvbbit` holds the batch executor when the qual contains MEANS. |
| `crates/backend/executor/execmain/src/procnode.rs` | `release_owned` drops that state before the node is forgotten. |
| `crates/backend/executor/execmain/src/execmain.rs` | Clears per-query EXPLAIN counters at executor start. |
| `crates/backend/executor/execmain/src/lib.rs` | Declares the `semantic` module. |
| `crates/backend/executor/execmain/src/semantic.rs` | **New file in an upstream crate.** The batch loop has to call `exec_proc_node`, projection, and slots. Putting it in `execmain` avoids an `execmain` ↔ `rvbbit_exec` cycle. |
| `crates/backend/commands/explain/src/node.rs` | A MEANS Result prints as `Semantic Filter` with batch size, concurrency, and ANALYZE counters. |
| `crates/_support/seams_init/src/lib.rs` | Registers the oid-9020 builtin and starts the dispatch pool at server boot. |

Manifests (dependency lines only):

- `Cargo.toml` (workspace members)
- `Cargo.lock`
- `crates/backend/parser/parse_expr/Cargo.toml`
- `crates/backend/utils/cache/cache_syscache/Cargo.toml`
- `crates/backend/optimizer/plan/planner/Cargo.toml`
- `crates/backend/executor/execmain/Cargo.toml`
- `crates/backend/commands/explain/Cargo.toml`
- `crates/_support/seams_init/Cargo.toml`

## New crates

| Crate | Role |
| --- | --- |
| `crates/rvbbit/rvbbit_oids` | Oid 9020, batch size 32, concurrency 4, pool size 8. |
| `crates/rvbbit/rvbbit_plan` | Walks the plan and lifts MEANS quals off base scans onto a Result. Cheap quals stay on the scan. |
| `crates/rvbbit/rvbbit_dispatch` | Fixed OS thread pool, `ureq` (no tokio), bounded queue, cancel-aware wait, 4 in-flight calls. |
| `crates/rvbbit/rvbbit_builtins` | Per-row `rvbbit_means(text, text) -> bool` for expressions that were not lifted. |
| `crates/rvbbit/hutch_mock` | HTTP stand-in. `POST /v1/means` takes `[{key, input, criterion}]` and returns bools. |

## Syntax

`expr MEANS 'text'` is a boolean predicate. The word is not in `kwlist.h`
(that table maps keywords to gram token codes, and there is no new token).
Unreserved behavior comes from the lexer:

- `SELECT means`, `AS means`, `"MEANS"`, and `t.means` are identifiers.
- A bare alias, `SELECT expr means FROM t`, does not parse. Write `AS means`.

Calling `rvbbit_means(...)` by name does not resolve: there is no `pg_proc`
row. The infix form builds the `FuncExpr` directly.

## What the batch node does

- Batch size 32, up to 4 batches in flight, one HTTP POST per batch of
  distinct `(input, criterion)` pairs.
- A per-query memo. Repeated pairs are not sent again. EXPLAIN ANALYZE
  reports batches, HTTP calls (including 429 retries), memo hits, and rows
  in/out.
- NULL arguments fail the row and do not hit the network (the builtin is
  strict).
- Transport is plain HTTP. `RVBBIT_HUTCH_URL` (default
  `http://127.0.0.1:8099/v1/means`) and `RVBBIT_HUTCH_TIMEOUT_MS` (default
  30000) are read when the pool starts.
- Waits poll `InterruptPending` every 20ms and then call
  `check_for_interrupts`, so `statement_timeout` and `pg_cancel_backend`
  abort the backend wait. An HTTP call already in flight on a worker runs
  to completion.

## Deliberately not lifted

Joins, HAVING, target-list `MEANS`, and foreign scans still use the per-row
builtin. Index-only scans whose MEANS arguments are not in the index can
fail at execution; the spike measurement is a sequential scan.
