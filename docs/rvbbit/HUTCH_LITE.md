# Hutch-lite live demo

`crates/rvbbit/rvbbit_hutch_lite` is an early hutch that speaks
`rvbbit-hutch-v1`. MEANS and fixed-label classify call OpenRouter's
Decisions API (`typesafe/jev-1.14` by default). Extract, and classify
with no label set, call an OpenAI-compatible chat endpoint. The binary
is `hutch_lite`. It does not call a provider until `OPENROUTER_API_KEY`
is set, and it never prints that key.

This VM does not have a key. Run the demo on a machine that does.

## What the demo does

`scripts/rvbbit_live_demo.sh` builds pgrustic and hutch-lite, starts both
on ports that do not collide with the spike (Postgres `55433`, hutch
`8098`), creates a fresh database, and loads the committed fixtures
`crates/rvbbit/fixtures/bigfoot_sightings.csv` and
`crates/rvbbit/fixtures/ufo_sightings.csv` (200 rows each, not the full
source files). Semantic queries run on an 8-row copy of each so a
`WHERE ... MEANS` cannot scan the whole fixture.

It then runs, in order, on those 8 rows:

- `notes MEANS 'a large hairy biped was seen'` on model `jev` (one batched `/v1/predict`; hutch fans that out to one Decisions `noul` call per row)
- `rvbbit.means_score` of that same criterion (same prompt and the same `noul` body, so the decision memo should add no provider calls; the column is the yes-probability; `/stats` `hits` increases)
- `rvbbit.means_score` of a looser criterion, `a sound, a dark shape, or any unusual event was noticed` (a different body, so another `noul` per row). On the old six-row set, wood knocks plus a dark biped scored the tight MEANS at 0.22 and CLASSIFY bigfoot at 0.93. The two score columns are that comparison on the fixture rows
- `rvbbit.kind(notes)` with `LABELS ('bigfoot' AS '...', 'known animal' AS '...', 'unclear' AS '...')` (one Decisions `choice` call per row; the column is JSON `{"label","score","scores"}`)
- `rvbbit.creature(notes)` on the chat model (one `/v1/extract` per row)
- `ufo_sample.notes MEANS 'an ordinary aircraft, satellite, or star'` and `rvbbit.ufo_kind(notes)` (one `noul` and one `choice` per UFO row)

and runs them again. The second pass must not increase `provider_calls`
or `total_cost_usd`. `hits` and `cost_saved_usd` must rise. Forty-eight
provider calls is the expected first pass (24 bigfoot `noul`, 8 bigfoot
`choice`, 8 chat, 8 UFO `noul`, 8 UFO `choice`). The tight score rows
are memo hits: cost 0, and `cost_saved` equal to the original `noul`
charge. The script aborts above 60 provider calls. It prints `GET /stats`
(including `latency_by_op_model`, where `calls` is misses and `hits` is
cache reuse) and:

```sql
SELECT cache, op, model, sum(cost), avg(provider_latency_ms)
FROM rvbbit.calls
GROUP BY 1, 2, 3;
```

`avg(provider_latency_ms)` ignores hits. On a hit that column is null.
`avg(latency_ms)` still includes hutch time for hits, so the grouped
query keeps `cache` in the key.

`rvbbit.calls` is a view. `rvbbit.call_log()` reads `GET /v1/calls` from
each hutch backend. It is the last 1024 calls in that process, not a
stored table, and `POST /reset` clears it.

It aborts above sixty provider calls, or when pass 1 recorded no USD cost.

A rough token count (each word, plus each punctuation mark) on a
demo-shaped `noul` body for one short sighting: the non-text scaffold
went from 109 tokens to 63 (−42%). The full JSON went from 125 tokens
/ 362 bytes to 79 tokens / 240 bytes. A `choice` body with three labels
went from 124 scaffold tokens to 85 (−31%). Live runs had been about
340 input tokens per `noul` versus about 66 for chat; the provider
adds its own prompt on top of this JSON, and the duplicated criterion
was the part we were sending twice.

Fixed-label classify is one Decisions request per row, not one per label.
Jev's `choice` question returns every label's probability. Output tokens
are free; the charge is input tokens of the state and the criteria.
A binary-only model would need three `noul` calls per row and then
argmax. That fallback is not implemented.

`TAKES`, `PHRASINGS`, and `PICK` are clause tails. A call-site clause
wins over `DEFAULT CLAUSES` on the function, which wins over the
`semantic.*` GUCs. The reducer runs in dispatch (Reduce, RetryWave,
CHECK, FALLBACK, BUDGET). Hutch fans takes, models, and phrasings out
inside one batch. Same-model takes that return the same score set
`identical` on the call row. `semantic.budget` aborts the session when
`rvbbit.calls` cost would pass the cap. Embed and rerank are per-item
`unsupported`. Decide is the same `noul` path as predict.

## Fresh Ubuntu

```bash
sudo apt-get update
sudo apt-get install -y build-essential g++ pkg-config ca-certificates curl \
  postgresql-18 postgresql-client-18
```

`/usr/bin/c++` on this image is clang, and re2's `<algorithm>` include
fails under that driver. The script exports `CC=gcc` and `CXX=g++`.
Install `g++` (pulled in by `build-essential`) so those names exist.

The link also needs the unversioned libstdc++ name. If this file is
missing, create it as root and rerun the script. The script prints the
same command and does not run `sudo` itself:

```bash
sudo ln -s libstdc++.so.6 /usr/lib/x86_64-linux-gnu/libstdc++.so
```

Rust is `1.96.0`, pinned by `rust-toolchain.toml`. Install rustup, then
let the first `cargo` invocation download that toolchain:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Postgres share files must be the 18 package layout:

- `PGRUST_PGSHAREDIR=/usr/share/postgresql/18`
- `PGRUST_TZDIR=/usr/share/zoneinfo`

`initdb` and `psql` come from `/usr/lib/postgresql/18/bin`. The script
puts that directory on `PATH`.

## Run

```bash
export OPENROUTER_API_KEY=...    # never commit this; the script does not echo it
# optional:
# export HUTCH_MODEL=openai/gpt-4o-mini
# export HUTCH_DECISION_MODEL=typesafe/jev-1.14
# export OPENROUTER_BASE_URL=https://openrouter.ai/api/v1
# export HUTCH_TIMEOUT_MS=60000
bash scripts/rvbbit_live_demo.sh
```

Leave `BACKEND AUTH ENV` unset. That column is the bearer hutch-lite
expects from pgrustic. The OpenRouter key stays in hutch-lite's
environment. Putting the key in `AUTH ENV` would send it to hutch as a
bearer and would not authorize the upstream call.

`HUTCH_MODEL` defaults to `openai/gpt-4o-mini` and is the chat model
(`creature`). `HUTCH_DECISION_MODEL` defaults to `typesafe/jev-1.14`
and is the Decisions model (`means`, `means_score`, `kind`). The script
writes each slug into its own `CREATE MODEL ... NAME`. A request model
of `mock` or empty uses the chat default for extract and the decision
default for predict and fixed-label classify. `~typesafe/jev-latest` is
sent through unchanged when a catalog model uses that name.
`OPENROUTER_BASE_URL` points chat at `{base}/chat/completions`. The
Decisions URL is derived from that base (`.../api/v1` becomes
`.../api/alpha/decisions`) unless `OPENROUTER_DECISIONS_URL` is set.
The crate tests point the base at a local fake.

Opt-in crate test, one predict, skipped unless you pass `--ignored` and
the key is set:

```bash
cargo test -p rvbbit_hutch_lite -- --ignored live_openrouter_means
```

Fake-endpoint tests (no key, no network provider):

```bash
cargo test -p rvbbit_hutch_lite
```

## Decision trees

`CREATE DECISION TREE` stores an ask/choose tree. `rvbbit.decide(name, input)`
walks it. Each ask is one Decisions `noul` (yes/no). Each choose is one
`choice` over that node's labels. The default model is the tree's
`MODEL`, which the demo sets to `typesafe/jev-1.14`. An empty or `mock`
remote name still becomes `HUTCH_DECISION_MODEL`.

```sql
CREATE DECISION TREE bigfoot_triage (notes text) MODEL jev AS
  ASK 'Does the report describe an unknown upright hairy figure, tracks, or knocks?'
    YES: CHOOSE 'Which report class fits?'
      WHEN 'class a' MEANING 'a witness saw a large upright figure' THEN
        ASK 'Does this class A report need a follow-up interview?'
          YES: TRUE
          NO: FALSE
      WHEN 'class b' MEANING 'tracks, knocks, or other signs without a clear sighting' THEN 'class b'
      WHEN 'class c' MEANING 'secondhand or very weak' THEN 'class c'
    NO: 'not a sighting';
```

A boolean leaf comes back as the text `true` or `false`. A string leaf
comes back as that label. The bigfoot tree returns `not a sighting`,
`class b`, `class c`, or the follow-up boolean. The UFO tree returns
`aircraft`, `satellite`, `astronomical`, `unexplained`, or
`not a sky object`.

`USING (slug, slug, ...) COMBINE mean|vote` is the stack hook on a node.
The slugs are sent as `decision.models` (at least two). `COMBINE vote`
is majority; `mean` is the default and compares the mean score with
`THRESHOLD` (default 0.5). The walker does not call the models itself.
The live script does not stack, because each extra model is another
provider call per row at that node.

Rows on the same node are one HTTP batch. Duplicate inputs at that node
are a memo inside the walk and are not sent. The next level starts after
this level has branched. `rvbbit.calls.node` is `{id}:{question}` (120
characters). The contract cache key is per node, so two nodes that ask
the same question do not share an entry. A MEANS filter on the same
Result keeps the MEANS batcher, and `decide` in that query runs one row
at a time. `EXPLAIN` names the batched node `Decision Tree` and, under
`ANALYZE`, prints batches, HTTP calls, node calls, and memo hits.

`scripts/rvbbit_tree_demo.sh` is the one-command live run. It uses the
same ports as `scripts/rvbbit_live_demo.sh` (Postgres `55433`, hutch
`8098`) and will not start if those are taken. It loads the 200-row
fixtures, copies 8 rows of each into `sightings` and `ufo_sample`, and
runs both trees on those 8. Worst case is 40 provider calls (3 bigfoot
levels plus 2 UFO levels, 8 rows). The script aborts above 60. The rerun
must not change `provider_calls` or `total_cost_usd`.

```bash
export OPENROUTER_API_KEY=...    # never commit this; the script does not echo it
bash scripts/rvbbit_tree_demo.sh
```

Each ask records its noul on `rvbbit.calls.score` and on the `hutch_call` line, next to `node`. Each choose records the winning label's probability. `EXPLAIN ANALYZE` prints `Node Scores` as `id=0.12` per visit. The bigfoot root asks whether the report describes an unknown upright figure, tracks, or knocks. A contrastive "credible ... rather than" question scored NO for every row even when CLASSIFY was 0.97–0.99.

Any rvbbit statement in the session creates `rvbbit.decide`. A later
connection still batches: planning recognizes the function by name, and
execution loads the tree cache before the first batch. The key stays in
hutch-lite's environment. Do not put it in `BACKEND AUTH ENV`.

## Native MEANS, CLASSIFY, and EXTRACT

`docs/rvbbit/GRAMMAR.md` is the gram.y decision: do not splice. `ABOUT`
is the same infix operator as `MEANS`. `CLASSIFY(expr INTO 'a', 'b')`
returns the bare label (the catalog `LABELS` function still returns
JSON). `CLASSIFY(expr AS enumname)` reads `pg_enum`.
`EXTRACT(int FROM expr)` and `EXTRACT(creature FROM expr) ON ERROR 'unknown'`
are the typed form; datetime `EXTRACT`, including `FORTNIGHT`, is unchanged.
The typed call sends `extract_mode = scalar` and asks for a noun or
literal of at most three words, so a creature answer is a name rather
than a description. `ON ERROR` is the default when the model or the
typed parse fails.

Those project calls batch on a Result named `Semantic Project`: one HTTP
call per window per `(backend, model, labels-or-kind)`, duplicates
removed, child order kept. A MEANS filter on the same query keeps the
filter batcher and the project calls run one row at a time.

```bash
export OPENROUTER_API_KEY=...    # never commit this; the script does not echo it
bash scripts/rvbbit_native_demo.sh
```

The mock count for a 5000-row filter, with no key and no prewarm:

```bash
bash scripts/rvbbit_means_5k.sh
```

That uses Postgres `55434` and hutch_mock `8100`. It expects 157 HTTP
calls (`ceil(5000/32)`). The spike on `55432` / `8099` is left alone.

## Cutoff and extract mode

`HUTCH_DECISION_THRESHOLD` (default 0.5) is the noul cutoff when a function
does not set one. Broad criteria such as "a sound, a dark shape, or any
unusual event" score about 0.92–0.98, so 0.5 matches them. Set a higher
cut on the function or the operator:

```sql
CREATE SEMANTIC FUNCTION means(text text, criterion text) RETURNS boolean
  MODEL jev PROMPT $$Does the text match the criterion?$$ THRESHOLD 0.97;
CREATE SEMANTIC OPERATOR means (
  LEFTARG = text, RIGHTARG = text, FUNCTION = means, THRESHOLD = 0.97
);
```

The operator value wins when both are set. An absent operator uses the
function threshold. Both absent means the server default. The first
`ensure` also creates `about` as a copy of `means`
with `THRESHOLD 0.9`. 0.9 still matches a criterion that scores 0.92, so
raise it when a loose predicate should reject those rows. Replacing
`means` does not refresh `about`.

Catalog `EXTRACT` still requires JSON. `extract_mode = scalar` on an
item accepts a JSON scalar or plain text and caches that string. A JSON
object stays an error in scalar mode.

## Cache

`semantic.cache` is `on` (honor the catalog mode), `off`, or `refresh`.
`semantic.memo_size` (default `1024`) and `semantic.memo_ttl` (default
`3600` seconds, `0` disables expiry) bound the process LRU. A per-query
map in front of that LRU is cleared at executor start and is not a
Postgres write, so a read-only transaction and a hot standby can run
these calls. Receipts stay on the hutch call log (`rvbbit.calls` reads
it). There is no audit table. `semantic.audit_payloads` `on` adds a
short input snippet to that log.

Hutch `HUTCH_CACHE` is `memory` (default), `off`, or `persistent`.
Persistent entries are files in `HUTCH_CACHE_DIR` (default `hutch-cache`),
keyed by the tenant-mixed storage key. A restarted hutch serves them
with no provider call. `POST /reset` deletes those files.

`rvbbit.calls` reads each distinct hutch URL once. Two backends that
share a URL do not duplicate the log. The live demo and the tree demo
export `RVBBIT_HUTCH_URL` so that read is the demo hutch, not port 8099.

Decision calls record `score` (the noul, or the winning choice
probability) on the call log and in `hutch_call` lines.

The trimmed noul JSON for the fixed six-row fixture, criterion
`a large hairy biped was seen`, prompt `Does the text match the
criterion?`, model `typesafe/jev-1.14`, is 72–79 rough tokens and
202–240 bytes (`rough_tokens` counts words plus punctuation). Row 1 is
79 tokens and 240 bytes, down from the old 125-token body. Live
provider `input_tokens` of 291–379 include Jev's own prompt on top of
this JSON. That wrapper is not in the request we post.
