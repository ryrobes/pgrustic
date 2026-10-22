# Native syntax without a gram.y splice

`MEANS`, `ABOUT`, `CLASSIFY`, and typed `EXTRACT` are parsed without
editing `gram.y`, `crates/backend/parser/gram_core/gram.c`, or
`crates/backend/parser/gram_core/src/actions.rs`.

## What a splice would cost

`EXTRACT '(' extract_list ')'` is rule **2157** in `actions.rs`.
`extract_list: extract_arg FROM a_expr` is rule **2306**. The keyword
forms `year`, `month`, `day`, `hour`, `minute`, and `second` are the
range arm `2308..=2313`, indexed by `rule - 2308`. Ident and string
forms are not their own arms (the comment on 2306: "IDENT/Sconst ride
DISPATCH"). `gram.y` already accepts `EXTRACT(ident FROM a_expr)`,
which is why `EXTRACT(FORTNIGHT FROM t)` and `EXTRACT(TIMEZONE_M FROM d)`
are datetime extracts that fail later, not syntax errors.

`actions.rs` matches rule numbers literally: **1231** arms, ids 0
through 2467. **148** of those arms have an id greater than 2157.
Inserting one production at `EXTRACT` renumbers every later rule, so
each of those arms moves, and every offset range (`rule - 2308`,
`rule - 2315`) moves with it. `tests_rule_gate` fails if a table rule
has no arm.

The parser tables are vendored bison **2.3** output in `gram.c`. This
port does not regenerate that file. A splice that only edits `gram.y`
does not update the tables the server runs.

`crates/rvbbit/grammar/rvbbit.y` is a separate bison 3.8.2 grammar with
`%expect 0`. It parses rvbbit DDL (`CREATE BACKEND`, `CREATE SEMANTIC
FUNCTION`, `CREATE DECISION TREE`) and is not linked into the engine.
It never sees a `SELECT` expression, so it cannot by itself parse
`CLASSIFY(...)` or `EXTRACT(...)` inside a query, and it cannot own
`MEANS` as an infix operator of `a_expr`. The one shift/reduce it does
have (a nested `CHOOSE`) is resolved with `%nonassoc` and is unrelated
to these expressions.

## What the separate path expresses

1. **`MEANS` and `ABOUT`.** After an expression-ending token, the lexer
   retokenizes ident `means` or `about` as the existing `Op` token
   (`scan_fgram/src/lex.rs`). `transformAExprOp` builds the `means`
   FuncExpr. There is no new keyword and no new gram.y production.
   `SELECT expr means FROM t` is still not an alias; write `AS means`.
   `NOT (x ABOUT 'y')` works because `ABOUT` is the same FuncExpr.
   There is no catalog negator named `ABOUT`. The predicate is cosine
   similarity of the two embeddings, not a MEANS decision. The default
   cutoff is 0.30. `rvbbit.semantic_similarity(a, b)` returns the cosine.

2. **`CLASSIFY` and typed `EXTRACT`.** `parser_driver` calls
   `rewrite_native_semantic` only in `RAW_PARSE_DEFAULT`, and only when
   the statement text contains `classify` or `extract`. Unmatched text
   is returned as `None` and is not copied. The rewrite produces
   ordinary function calls:
   - `CLASSIFY(expr INTO 'a', 'b', ...)` →
     `rvbbit.classify((expr), '<labels joined by U+001F>', fallback)`
   - `CLASSIFY(expr AS ('a', 'b', ...))` is the same label list.
   - `CLASSIFY(expr AS enumname)` →
     `rvbbit.classify_enum((expr), 'enumname', fallback)`
   - `EXTRACT(kind FROM expr)` →
     `rvbbit.extract_typed('<kind>', (expr), fallback)`
   - `ON ERROR literal` is that third argument. Omitted means `NULL`.
     The three functions are not `STRICT`, so a null fallback can still
     run and a model error can return the default. A scalar EXTRACT that
     finds nothing returns SQL NULL and does not use `ON ERROR`.

   `EXTRACT` is rewritten for a scalar kind (`int`, `integer`, `bigint`,
   `int2`, `int4`, `int8`, `smallint`, `float`, `float4`, `float8`,
   `double`, `real`, `numeric`, `bool`, `boolean`, `text`, `string`) or
   when `ON ERROR` is present. A custom name such as `creature` needs
   `ON ERROR`, which datetime `EXTRACT` does not accept. Any other
   field, including `FORTNIGHT` and prefix forms such as `TIMEZONE_M`,
   stays the datetime production.

   A unit test reads every file in `regress/overlay/sql` and
   `regress/rvbbit/sql` (160 files) and requires the rewrite to return
   `None`. gram.y therefore still sees the original bytes, and a
   database that only runs that corpus does not get schema `rvbbit`.

3. **User `SEMANTIC FUNCTION`s** batch. The old `MODEL` / `PROMPT` form
   is unchanged: a 2-argument bool is still `rvbbit_means`, a fixed-label
   text function still returns JSON `{"label","score","scores"}` from
   `rvbbit_classify`, and text without labels is still `rvbbit_extract`.
   Shorthand is `RETURNS bool|score|text|label`, `AS 'template {{arg}}'`,
   `USING MODEL name`, and `DEFAULT CLAUSES (...)`. `RETURNS label`
   requires `LABELS`. A template, a 1-argument bool or score, `RETURNS
   label`, or `AS DECISION TREE name` is `rvbbit_user`: one predict,
   classify, extract, or tree batch per window, remembered so the builtin
   does not call again. `AS PIPE $$ ... $$` is a `LANGUAGE sql VOLATILE`
   function whose body is the native rewrite (`{{arg}}` becomes the
   argument). Volatility matches the inner call so the planner can inline
   it, and Semantic Project batches that call. `|> name` after an expression is `rvbbit.name(left, ...)`.
   A bare identifier after that call is a column alias (`body |> fn p`
   is `rvbbit.fn((body)) AS p`). `AS` still works. A string or a
   parenthesized expression stays an argument. `|>>` stays the geometric operator. Native `CLASSIFY` returns the bare
   label and does not need a predeclared function. It uses the `means`
   backend and the decision model (default `typesafe/jev-1.14`). Native
   `EXTRACT` uses that backend's URL but sends model `mock`, which
   hutch maps to the chat model (`HUTCH_MODEL`). A jev slug is not sent
   to chat completions. `CREATE BACKEND` (or any earlier rvbbit DDL)
   creates `rvbbit.classify`, `rvbbit.classify_enum`, and
   `rvbbit.extract_typed`. The check is `proname` plus `prosrc`, so a
   user function with a different signature can share the name.

The operator, the `rvbbit.*` function, and `rvbbit.flow` are the three
pg_rvbbit call styles. [MIGRATION.md](MIGRATION.md) maps each one onto
the expressions above.

## SemanticBatch

The Result node is the batch node. Both variants dedupe, keep child
order, and fall back to the builtin.

| Variant | What is lifted | Group key | HTTP |
| --- | --- | --- | --- |
| Filter | `WHERE ... MEANS` / `ABOUT` on a base scan. Cheap quals stay on the scan. | `(input, criterion)` | one POST per 32 rows, up to 4 in flight |
| Project | `rvbbit.classify`, `rvbbit.classify_enum`, `rvbbit.extract_typed`, or `rvbbit.extract_many` (3 args, empty clause tail), or `rvbbit.rerank` / `rvbbit.rerank_for` (2 or 3 args), or a `rvbbit_user` function (template, label, score, or decision tree), including a call nested under `round` or a cast. A pipe workflow is not batched as itself; its inlined call is | means backend URL, model, and the label set, extract kind, rerank query, or user function oid | one POST per group per window of 32 rows. A dedicated rerank model collapses one query's documents into one OpenRouter `/rerank`. `DEFAULT CLAUSES` (`TAKES`, `PHRASINGS`, `ACROSS`) ride on that POST |

`EXPLAIN` names the filter `Semantic Filter` and the project
`Semantic Project`, and prints `Batch Size`. Under `ANALYZE` it prints
batches, HTTP calls, memo hits, and rows in/out.

Each semantic node also carries a provider estimate. The seed is jev at
$0.042 per million input tokens and about 300 billed tokens a call,
gpt-4o-mini at its list rates, and voyage rerank at $0.05 per million
tokens. `SUMMARIZE` and `EXTRACT` are priced as gpt-4o-mini when the
wire model is empty, `mock`, or `default`, because that is the chat
model hutch actually calls. `USING MODEL 'voyageai/rerank-3'` is priced
as voyage. MEANS stays on jev. A response `calls` array, `rvbbit.calls`,
or hutch `/stats` replaces that seed for the same operation and model.
`TAKES`, `PHRASINGS`, `ACROSS`, and `FALLBACK` multiply the items. A
project or a pipe counts one provider call per input row before those
factors, including when the table is still unanalyzed. A single-model,
single-take CLASSIFY also multiplies by 1.10 for the expected retake.
A cache hit rate, including `CACHE off`, reduces the charged provider
calls. HTTP batches are `ceil(provider calls / batch)` for a fan-out
op, or one batch for a dedicated rerank. Latency is wall time: fan-out
ops take `ceil(provider calls / concurrency)` waves, and a voyage
rerank takes one wave per HTTP batch. A filter's selectivity is 0.5,
or `1 - THRESHOLD` clamped to 0.05..0.95. ABOUT with no catalog
threshold uses 0.30, so its selectivity is 0.70.

`EXPLAIN` prints `Semantic Provider Calls`, `Semantic HTTP Batches`,
`Semantic USD`, `Semantic Latency`, and `Semantic Model` on the node,
and the same `Semantic Total` lines for the statement.
`EXPLAIN (COSTS OFF)` leaves those lines out. `EXPLAIN ANALYZE` always
adds `Actual Semantic Calls` (provider misses), `Actual Choice Retakes`
(the second CLASSIFY take under 0.60), `Actual Cache Hits`,
`Actual HTTP Batches`, `Actual Semantic USD`, `Actual Semantic Latency`
(wall time, not the sum of every call), and `Actual Semantic Model`,
including when every row was a cache hit and the provider-call count
is 0. The statement totals use the same names. A client-memo or
`/v1/cache/lookup` hit has no `calls` array and still counts as a cache
hit. Cheap predicates stay on the scan. A semantic filter moves
above an inner join when the join emits fewer rows than its input, and
a project or decision tree moves above `LIMIT` so the limit runs first.

`BUDGET` on the clause, `semantic.max_cost`, and `semantic.budget` are
pre-flight caps. If the estimate is above the tightest one, the
statement errors before any provider call. `SET semantic.cost_check =
off` skips that check.

The row-at-a-time builtin runs when:

- a MEANS qual or a top-level `rvbbit.decide` already owns the Result, so a
  project call on that node stays per-row (`Project = per-row` in `EXPLAIN`)
- the planner's syscache seams are not installed (unit tests), so the
  scan is not wrapped
- the project memo has no entry for that input (a bad enum, or a null
  argument, which returns null and does not call hutch)

`ON ERROR` applies to an item error or a typed-decode failure. The
batcher remembers the failure and the builtin returns that row's
default. A transport failure still fails the query. `int` / `float` /
`bool` must parse; any other kind is trimmed text. Non-finite floats
are a decode error.

A 5000-row `WHERE body MEANS '...'` with distinct inputs and no prewarm
is `ceil(5000/32) = 157` HTTP calls. `scripts/rvbbit_means_5k.sh` checks
that against `hutch_mock` on Postgres `55434` and mock `8100`. It does
not use the spike ports `55432` / `8099`.

`scripts/rvbbit_native_demo.sh` is the live pass (8 fixture rows,
`MEANS`, `ABOUT`, `CLASSIFY ... INTO`, `EXTRACT(creature FROM ...) ON
ERROR`). It expects at most 60 provider calls. The key is
`OPENROUTER_API_KEY` in the environment and is not printed.

## Upstream file growth

This phase adds **one** file under the engine tree:
`crates/backend/executor/execmain/src/projbatch.rs`. The other engine
edits are in files that already existed (`parser_driver`, `lex.rs`,
`parse_expr`, `noderesult.rs`, `procnode.rs`, `execmain/src/lib.rs`,
`semantic.rs`, `explain/src/node.rs`, `seams_init`). `gram.y`, `gram.c`,
and `actions.rs` have no diff.

## MCP tools

`CREATE SERVER ... FOREIGN DATA WRAPPER mcp`, `IMPORT FOREIGN TOOLS`,
`ALTER FOREIGN TOOL`, and `REFRESH FOREIGN TOOLS` are the same pre-parse.
A foreign-data wrapper other than `mcp`, and `DROP SERVER` of a name
that is not in `mcp.server`, stay in gram.y. See `docs/rvbbit/MCP.md`.
