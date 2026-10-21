# RVBBIT seams

Spike for an infix `expr MEANS 'text'` predicate that plans as a batched
HTTP call to a hutch service. New behavior lives in `crates/rvbbit/`. The
engine edits are the hooks that crate cannot reach on its own: the scanner
has no extension point for a contextual operator, `pg_proc` projections are
set-once seams, and the Result executor is not a plugin.

## Upstream files touched

19 existing or new files under the upstream tree (14 from the spike, 5 more
for the catalog). Plus dependency lines in the manifests below and
`Cargo.lock`. Manifest edits are path dependencies only.

| File | Why |
| --- | --- |
| `crates/backend/parser/scan_fgram/src/lex.rs` | After an expression-ending token, identifier `means` is emitted as the existing `Op` token. No new gram.y production, so bison rule numbers stay put. |
| `crates/backend/parser/scan_fgram/src/lib.rs` | `Scanner` remembers the previous token for that decision. |
| `crates/backend/parser/scan_fgram/src/tests.rs` | Lexer test: infix `means` is `Op`; `SELECT means`, `AS means`, and `t.means` stay identifiers. |
| `crates/backend/parser/parse_expr/src/lib.rs` | `transformAExprOp` turns operator name `means` into a `FuncExpr`. The oid is the catalog `pg_proc` row when the catalog hook is installed, otherwise 9020. Both sides are coerced to `text`. |
| `crates/backend/utils/cache/cache_syscache/src/projections.rs` | Synthetic `pg_proc` projection for oid 9020, used only when no catalog row has been installed. Shape is volatile and parallel-unsafe. |
| `crates/backend/optimizer/plan/planner/src/lib.rs` | `lift_semantic_filters` runs immediately before `set_plan_references` on the main plan and each subplan. |
| `crates/backend/executor/execmain/src/noderesult.rs` | `ResultState.rvbbit` holds the batch executor when the qual contains MEANS. |
| `crates/backend/executor/execmain/src/procnode.rs` | `release_owned` drops that state before the node is forgotten. |
| `crates/backend/executor/execmain/src/execmain.rs` | Clears per-query EXPLAIN counters at executor start. |
| `crates/backend/executor/execmain/src/lib.rs` | Declares the `semantic` module. |
| `crates/backend/executor/execmain/src/semantic.rs` | **New file in an upstream crate.** The batch loop has to call `exec_proc_node`, projection, and slots. Putting it in `execmain` avoids an `execmain` ↔ `rvbbit_exec` cycle. |
| `crates/backend/commands/explain/src/node.rs` | A MEANS Result prints as `Semantic Filter`. Batch size and concurrency come from the catalog endpoint (32 and 4 after bootstrap). ANALYZE counters are unchanged. |
| `crates/backend/utils/adt/ruleutils/src/lib.rs` | Deparse of oid 9020 still prints `rvbbit_means(...)` without a `pg_proc` lookup. A catalog oid deparses through the normal `pg_proc` path as `rvbbit.means(...)`. |
| `crates/_support/seams_init/src/lib.rs` | Registers builtins 9020–9027, installs the catalog ensure/execute hooks, and starts the dispatch pool at server boot. |
| `crates/_support/types/nodes/src/parsenodes.rs` | `ObjectType::OBJECT_RVBBIT` is appended after `OBJECT_VIEW`. Existing discriminants, including `OBJECT_VIEW`, stay put. It is not a C object type (`objtype_from_i32` still stops at `OBJECT_VIEW`, and the nodes `check_enum` list is unchanged). |
| `crates/backend/commands/seclabel/src/lib.rs` | Exhaustive `ObjectType` match: `OBJECT_RVBBIT` does not support security labels. |
| `crates/backend/parser/parser_driver/src/lib.rs` | If a script contains rvbbit DDL, those statements become `DefineStmt` `OBJECT_RVBBIT`. Scripts with no rvbbit DDL are handed to gram.y unchanged. |
| `crates/backend/tcop/utility/src/dispatch.rs` | `OBJECT_RVBBIT` runs the statement through the catalog hook. |
| `crates/backend/tcop/utility/src/commandtag.rs` | `OBJECT_RVBBIT` reuses pinned PG 18.3 command tags (`CREATE FUNCTION`, `ALTER FUNCTION`, `DROP FUNCTION`, `GRANT`, `REVOKE`, `COMMENT`). `cmdtaglist.h` is not edited. |

Manifests (dependency lines only):

- `Cargo.toml` (workspace members)
- `Cargo.lock`
- `crates/backend/parser/parse_expr/Cargo.toml`
- `crates/backend/utils/cache/cache_syscache/Cargo.toml`
- `crates/backend/optimizer/plan/planner/Cargo.toml`
- `crates/backend/executor/execmain/Cargo.toml`
- `crates/backend/commands/explain/Cargo.toml`
- `crates/backend/utils/adt/ruleutils/Cargo.toml`
- `crates/_support/seams_init/Cargo.toml`
- `crates/backend/parser/parser_driver/Cargo.toml`
- `crates/backend/tcop/utility/Cargo.toml`

## New crates

| Crate | Role |
| --- | --- |
| `crates/rvbbit/rvbbit_oids` | Builtin oids 9020–9027, default batch size 32, concurrency 4, pool size 8. |
| `crates/rvbbit/rvbbit_plan` | Walks the plan and lifts MEANS quals off base scans onto a Result. Cheap quals stay on the scan. |
| `crates/rvbbit/rvbbit_dispatch` | Fixed OS thread pool, `ureq` (no tokio), bounded queue, cancel-aware wait. Each job carries the catalog backend URL, auth, timeout, and concurrency. |
| `crates/rvbbit/rvbbit_builtins` | `rvbbit_means`, `rvbbit_means_score`, `rvbbit_extract`, `rvbbit_classify`, `rvbbit_call_log`, `rvbbit_dump_sql`, `rvbbit_restore_sql`, `rvbbit_get_semfn_def`. |
| `crates/rvbbit/rvbbit_contract` | `rvbbit-hutch-v1` request and response types and cache key v1. No engine dependencies. |
| `crates/rvbbit/rvbbit_parse` | Recursive descent for rvbbit DDL. Keywords exist only here. |
| `crates/rvbbit/rvbbit_resolve` | Process-local catalog cache and the function pointers `seams_init` installs. Breaks the parser → SPI cycle. |
| `crates/rvbbit/rvbbit_catalog` | Schema `rvbbit`, DDL, `pg_depend`, grants, comments, `dump_sql` / `restore_sql`. |
| `crates/rvbbit/hutch_mock` | HTTP stand-in for `rvbbit-hutch-v1`. `POST /v1/means` is an alias of `/v1/predict` and accepts the batch body, not the spike's JSON array. |
| `crates/rvbbit/rvbbit_hutch_lite` | Early real hutch. MEANS and fixed-label classify call the OpenRouter Decisions API (Jev). Extract and open-ended classify stay on chat. In-memory cache, single-flight. `TAKES` / `PICK` are still a parser error. Embed and rerank are stubbed. |
| `crates/rvbbit/grammar/rvbbit.y` | Standalone bison grammar. Not linked into the engine. |

## Syntax

`expr MEANS 'text'` is a boolean predicate. The word is not in `kwlist.h`
(that table maps keywords to gram token codes, and there is no new token).
Unreserved behavior comes from the lexer:

- `SELECT means`, `AS means`, `"MEANS"`, and `t.means` are identifiers.
- A bare alias, `SELECT expr means FROM t`, does not parse. Write `AS means`.

The first `MEANS` in a database bootstraps backend `hutch`, model `default`,
semantic function `rvbbit.means(text, text)`, and operator `MEANS`. After
that, `\df rvbbit.means` shows a real `pg_proc` row. Oid 9020 remains the
fallback when the catalog hook is not installed (unit tests).

## What the batch node does

- Batch size 32, up to 4 batches in flight, one HTTP POST per batch of
  distinct `(input, criterion)` pairs.
- A per-query memo. Repeated pairs are not sent again. EXPLAIN ANALYZE
  reports batches, HTTP calls (including 429 retries), memo hits, and rows
  in/out.
- NULL arguments fail the row and do not hit the network (the builtin is
  strict).
- Transport is plain HTTP. `RVBBIT_HUTCH_URL` (default
  `http://127.0.0.1:8099`) is the base URL used when the catalog has no
  backend yet. A trailing `/v1/means` or `/v1/predict` is stripped. The
  pool does not latch the URL: each job posts `{base}/v1/predict`.
  `RVBBIT_HUTCH_TIMEOUT_MS` (default 30000) applies to that fallback.
  A `BACKEND` row's `timeout_ms`, `retries`, and `auth_env` override it.
- Waits poll `InterruptPending` every 20ms and then call
  `check_for_interrupts`, so `statement_timeout` and `pg_cancel_backend`
  abort the backend wait. An HTTP call already in flight on a worker runs
  to completion.

## Deliberately not lifted

Joins, HAVING, target-list `MEANS`, and foreign scans still use the per-row
builtin. Index-only scans whose MEANS arguments are not in the index can
fail at execution; the spike measurement is a sequential scan.

## Grammar, and why gram.y was not spliced

`crates/rvbbit/grammar/rvbbit.y` is a standalone grammar compiled with
bison 3.8.2 (`%expect 0`). One WHEN shift/reduce (a nested `CHOOSE`) is
resolved by `%nonassoc LOW_WHEN` / `%nonassoc WHEN` and
`CHOOSE ... when_list %prec LOW_WHEN`, so WHEN shifts onto the inner
CHOOSE. The engine does not link that parser. `rvbbit_parse` implements
the same language. Keywords are tokens of that grammar only, so
`SELECT 1 AS backend` still parses: nothing was added to `kwlist.h` or
to PostgreSQL `gram.y`.

Splicing the productions into `gram.y` is possible and was not done.
The reference `stmt` rule ends with `/*EMPTY*/` at line 1116
(`crates/postgres-18.6-reference/src/backend/parser/gram.y`, 19728 lines).
Inserting N alternatives before that EMPTY alternative renumbers every
bison rule after `stmt`. `crates/backend/parser/gram_core/src/actions.rs`
matches those rule numbers literally. The vendored `gram.c` was generated
by bison 2.3 and is not regenerated here. Appending unreserved keywords at
`unreserved_keyword` (line 17685) would shift only the rules that follow
that production. Token codes stay stable if new `%token` entries are
appended rather than inserted. That is the rebase cost of a later splice:
one mechanical renumber of `actions.rs` (and a bison 2.3 regeneration, or
a decision to stop vendoring `gram.c`), proportional to how many `stmt`
alternatives are added. M1 adds zero alternatives to `gram.y`, so this
branch does not pay that cost.

`cmdtaglist.h` is pinned the same way. psql prints `CREATE FUNCTION` for
`CREATE BACKEND`. The catalog row is still a backend.

## Catalog resolution

`MEANS` resolves operator `means` → semantic function → model → backend.
The backend row supplies the URL, the env-var name for the bearer token,
batch size, concurrency, timeout, and retries. The secret value is read
from the environment at dispatch and is never stored. `AUTH ENV` and
`AUTH SECRET` are the same column.

The schema is created on first MEANS or rvbbit DDL, inside that
transaction, not at server start. A database that never mentions either
(the upstream `regression` database) does not get schema `rvbbit`.

Dependencies are `pg_depend` rows whose `classid` is the `rvbbit.*` table's
`pg_class` oid. `DROP` is implemented in Rust. `RESTRICT` errors when a
model, function, operator, or tree still references the object. `CASCADE`
walks backend → model → function → operator, and model → tree.
`performDeletion` is not used, because a custom classid does not have
object-class properties.

`rvbbit.dump_sql()` / `rvbbit.restore_sql(text)` are the supported dump
round trip. Stock `pg_dump` does not emit this metadata.

MEANS posts the catalog `cache_mode` on the operation POST (`memoize`
→ `mode=read-write`) and does not call `/v1/cache/lookup`. Adding that
hop would change the 313-call benchmark. The mock ignores the header, so
the benchmark count is unchanged. The pre-catalog fallback still sends
`mode=off`. The wire shapes are in `docs/rvbbit/HUTCH_CONTRACT.md`.

Hutch-lite (`crates/rvbbit/rvbbit_hutch_lite`) adds no `gram.y` splice and
no new upstream file. The only engine edit for it is the `seams_init`
comment that names builtin oid 9024 (`rvbbit_extract`). `TAKES` / `PICK`
stay unimplemented in the parser until milestone M5.
