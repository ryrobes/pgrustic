# Hutch contract `rvbbit-hutch-v1`

Version: **rvbbit-hutch-v1**. Cache key version: **rvbbit-cache-v1**.

The types live in `crates/rvbbit/rvbbit_contract` and have no engine
dependencies. pgrustic and the real hutch (a separate private repo, not
modified from this tree) should both depend on that crate. This document
is the behavior those types imply.

The mock in `crates/rvbbit/hutch_mock` implements the operation routes and
`POST /v1/cache/lookup`. It does not implement the cache table, writer
queue, single-flight, put, purge, or stats. Those are listed under
[What the real hutch has to change](#what-the-real-hutch-has-to-change).

Transport is plain HTTP. No HTTP/2 requirement, no tokio.

## Headers

| Header | Value |
| --- | --- |
| `Content-Type` | `application/json` on requests with a body |
| `X-Rvbbit-Contract` | `rvbbit-hutch-v1` |
| `Authorization` | `Bearer <token>` when the backend row names an env var. Absent when `auth_env` is null. |
| `X-Rvbbit-Cache` | `key=<hex>; ttl=<seconds>; mode=read-write\|refresh\|off`. `key` and `ttl` are omitted when absent. |

`X-Rvbbit-Cache` mode applies to every item in the batch. Per-item keys
are `items[].cache_key` in the body, not the header. pgrustic sends the
catalog `cache_mode` on that same operation POST and does not set `key`
or `ttl`. `memoize` (the column default) is `mode=read-write`, `refresh`
is `mode=refresh`, and `off` is `mode=off`. The pre-catalog fallback
endpoint, used before any backend row exists, still sends `mode=off`.
A read-write batch also sends one `POST /v1/cache/lookup` for the keys
the process memo does not already hold, and posts only the misses. The
mock does not count that lookup in `GET /stats`, so a first-run miss
benchmark stays one operation POST per batch. Hutch-lite honors the
header inside the POST and does not call the provider again on a hit.

A backend whose `auth_env` is set, and whose environment does not contain
that variable (or contains an empty value), fails the statement before any
HTTP call. The catalog stores the variable name (`AUTH ENV` and
`AUTH SECRET` are the same column). It never stores the secret.

The mock requires `Authorization` only when `HUTCH_EXPECT_AUTH` is set.
Otherwise it ignores the header.

## Operations

`POST /v1/predict`, `/v1/classify`, `/v1/extract`, `/v1/embed`,
`/v1/rerank`, `/v1/decide`.

`/v1/means` is a mock alias of `/v1/predict` so older spike URLs still hit
the server. New clients post `/v1/predict`. The body is a `BatchRequest`,
not the spike's JSON array.

```json
{
  "contract": "rvbbit-hutch-v1",
  "operation": "predict",
  "model": "mock",
  "temperature": 0.0,
  "max_tokens": 16,
  "items": [
    {
      "key": "0",
      "cache_key": "<64 hex chars>",
      "inputs": {"criterion": "alpha", "input": "row-1 alpha"},
      "criterion": "alpha"
    }
  ]
}
```

`operation` is one of `predict`, `classify`, `extract`, `embed`, `rerank`,
`decide`. `max_tokens` is omitted when the function has none. `system` and
`prompt` on an item are optional overrides; MEANS leaves them null and
puts the pair in `inputs`. `response_schema` is an optional JSON Schema.
Hutch-lite sends it as strict `response_format.json_schema` on the chat
call. EXTRACT MANY uses an object schema with a `mentions` array of
strings and stores that array, so a bare JSON array still decodes. A
provider error that rejects the schema is retried once without
`response_format`. Temperature on that call is 0 unless a clause tail
sets one. pgrustic then asks Jev `is <candidate> a <kind>?` once per
mention and drops the ones that come back false. That check fails open.

Response. `outputs[i]` answers `items[i]`. The `key` is echoed. A failed
item sets `error` and leaves `value` null. Siblings may still succeed.
pgrustic treats any item error as a failure of the whole MEANS batch.

```json
{
  "contract": "rvbbit-hutch-v1",
  "outputs": [
    {"key": "0", "value": true},
    {"key": "1", "value": null, "error": {"code": "provider", "message": "upstream 503"}}
  ]
}
```

Mock values, deterministic, no network:

| Operation | `value` |
| --- | --- |
| predict | bool, case-insensitive substring of `inputs.input` against `inputs.criterion` or `item.criterion` |
| classify | the criterion string |
| extract | `{}` |
| embed | `[0.0]` |
| rerank | `0.0` |
| decide | `{"label": "<criterion or empty>", "status": "mock"}` |

An empty criterion does not match.

## Cache key v2

The client key does **not** include the tenant. Hutch mixes the tenant in
when it stores or looks up. A client-supplied tenant is not trusted.

`client_cache_key` is blake3 over length-prefixed fields, in order.
`rvbbit-cache-v1` keys miss.

1. magic `rvbbit-cache-v2`
2. function name
3. function version (decimal)
4. template hash
5. model
6. canonical temperature (JSON number; non-finite becomes `null`)
7. `max_tokens` decimal, or empty
8. canonical JSON of sorted opts (labels and label descriptions; `{}` when empty)
9. take index (decimal, MEANS sends `0`)
10. seed (MEANS sends empty)
11. threshold as a canonical JSON number, or empty when the function sets none
12. extract mode (`scalar`, `json`, or empty)
13. canonical JSON of `inputs`

Each field is `u32` big-endian length plus that many bytes. The hex form
is 64 lowercase hex characters (`client_cache_key_hex`).

The template hash is blake3 over the same length-prefix framing of
`system_prompt`, `user_prompt`, and the return schema string
(MEANS uses the SQL return type, `boolean`).

Canonical JSON (`canonical_json`):

- object keys sorted by UTF-8 byte order, after NFC normalization of the key
- strings NFC-normalized, then JSON-escaped
- numbers use their JSON spelling
- array order is preserved
- no insignificant whitespace

So `{"b":1,"a":"é"}` and the same object with key order swapped, or with
`e` + combining acute, produce the same key. Changing function version,
model, temperature, max tokens, take index, or seed changes the key.

Storage key, computed only on hutch:

```text
storage_key = blake3(len_prefix(client_key_raw_32) || len_prefix(tenant_utf8))
```

`client_key_raw_32` is the 32-byte digest, not the hex text.

## Cache routes

### `POST /v1/cache/lookup`

Implemented by the mock. Always a miss. Not counted in the mock's
`GET /stats` (that counter is operation posts, so the MEANS benchmark
stays at 313).

```json
{ "keys": ["<hex>", "<hex>"] }
```

```json
{ "hits": [{"key": "<hex>", "output": true}], "misses": ["<hex>"] }
```

Keys are client hex keys. Hutch hashes them with the tenant before
touching storage. Lookup is exact. A miss is not an error.

### `POST /v1/cache/put`

Not implemented by the mock (404). Real hutch's writer queue accepts:

```json
{
  "entries": [
    {"key": "<hex>", "output": true, "ttl_secs": 3600}
  ]
}
```

```json
{ "stored": 1 }
```

`ttl_secs` omitted means the tenant default TTL.

### `POST /v1/cache/purge`

Not implemented by the mock. `{"keys":["<hex>"]}` drops those client keys
for the calling tenant. `{"all": true}` drops the tenant. Response
`{"purged": N}`.

### `GET /v1/cache/stats` or `POST /v1/cache/stats`

Not implemented by the mock. Response:

```json
{"entries": 0, "hits": 0, "misses": 0, "single_flight_waits": 0}
```

The mock's `GET /stats` and `POST /reset` are test instrumentation, not
part of this contract.

## Auth, timeouts, retry, cancellation

Auth is the bearer token above. A wrong token is HTTP 401
`{"error":"unauthorized"}` when the server is configured to check. The
mock checks only if `HUTCH_EXPECT_AUTH` is set. Real hutch checks the
tenant credential on every route, including cache routes, and derives the
tenant id from that credential. The tenant never arrives as a JSON field.

Timeouts come from the `BACKEND` row (`timeout_ms`, default 30000). The
client sets that as the HTTP client timeout for the attempt. A timeout is
a transport failure. pgrustic does not retry it. The mock's
`HUTCH_TIMEOUT_EVERY` sleeps 60s and then returns 504, which is also not
retried.

HTTP 429 is retried. The attempt budget is the backend `retries` value
(bootstrap uses 5; the column default is 3). That number is the total
attempts, including the first. Sleep is `Retry-After` when it is an
integer number of seconds, otherwise 50ms, and never more than 2000ms.
The mock sends `Retry-After: 0` so tests do not wait. The last 429, once
the budget is spent, fails the batch. Other 4xx and 5xx fail immediately.
The error text includes the status and the body.

Cancellation: `statement_timeout` and `pg_cancel_backend` stop the backend
from waiting. The wait polls every 20ms and then calls
`check_for_interrupts`. An HTTP call already running on a pool thread is
not aborted; `ureq` has no cancel handle, and this milestone does not add
one. Real hutch should stop provider work when the client connection
closes. The mock sleeps for the configured latency even if the client has
gone, then fails the write.

In-flight concurrency is per `url` and `max_concurrent`, taken from the
backend row (bootstrap 4). The pool is 8 threads and a queue of 64,
started once at server boot. A later `ALTER` that changes
`max_concurrent` for a URL already seen creates a new limiter only when
the concurrency number changes; the old limiter for that exact key is
process-lifetime.

## What MEANS sends

Resolution is operator → semantic function → model → backend.

- URL: `{backend.url}/v1/predict` (trailing slash stripped)
- model, temperature, max_tokens, function name, function version: the catalog row
- cache header: the catalog `cache_mode` (`memoize` → `mode=read-write`, `refresh` → `mode=refresh`, `off` → `mode=off`). The pre-catalog fallback sends `mode=off`
- one item per distinct `(input, criterion)` pair in the batch
- `inputs` object keys `input` and `criterion`
- take index 0, empty seed
- one `POST /v1/cache/lookup` for keys missing from the process memo, then one predict POST of the misses. A batch that is entirely known is not posted

A text semantic function posts the same header to `{backend.url}/v1/extract`,
one HTTP call per row, with `inputs.input` set to the concatenated text
arguments. Hutch returns JSON; the SQL function returns that JSON as
`text`.

A bool `value` of JSON `true`/`false` is the predicate result. A non-bool
value fails the batch. Item errors fail the batch.

`score` on a predict item is the yes-probability. SQL
`RETURNS double precision` is `rvbbit_means_score` (oid 9025). It posts
the same `/v1/predict` body as MEANS and returns `outputs[0].score`.
Give it the same prompt as MEANS when the two should share a provider
call: hutch-lite memos the Decisions HTTP body, which does not include
the function name.

A text function with `LABELS ('a', 'b', ...)` (at least two labels) is
`rvbbit_classify` (oid 9026). It posts `/v1/classify` with `item.labels`
set. A label may carry a description: `LABELS ('bigfoot' AS 'an unidentified large primate or sasquatch', 'known animal')`.
The SQL result is JSON text `{"label","score","scores"}`. Text
without `LABELS` stays `rvbbit_extract` on `/v1/extract`.

The client cache key is unchanged for functions with no labels. Dispatch
folds a non-empty label list into the return-schema string before
hashing: `{return_type} labels=` plus the labels joined by U+001F.
`ItemRequest.labels` is not a separate hashed field.

## Hutch-lite

`crates/rvbbit/rvbbit_hutch_lite` is an in-tree server that speaks this
contract over plain HTTP. It is not the private hutch. These JSON fields
were added and are optional: `BatchRequest.decision`, `ItemRequest.labels`,
`ItemResponse.score`, `ItemResponse.scores`, `ItemResponse.calls`.
Older clients omit them. Lookup and put still store the value only;
they do not store `score`, `scores`, or `calls`.

Predict, decide, and fixed-label classify call OpenRouter's Decisions API
(Jev by default). Extract, and classify with no label set, call an
OpenAI-compatible chat endpoint.

What it implements:

- `POST /v1/predict` (and the `/v1/means` alias), `/v1/decide`, `/v1/extract`, `/v1/classify`
- batched items, per-item errors, siblings may succeed
- in-memory cache. The storage key starts from `storage_key(client_key, tenant)`. Decision calls then mix the resolved model list, take count, combine mode, threshold, and any non-empty label list. Tenant is blake3 hex of the bearer token, or `HUTCH_TENANT` (default `local`) when the request has no bearer
- a separate memo of Decisions HTTP bodies, keyed by tenant, take index, and the request JSON. It is used only for `mode=read-write`, so MEANS and `means_score` share one provider call when both are memoize and the prompt and inputs match. That reuse increments `hits` the same way a contract-cache hit does, and it is a `cache=hit` row with the saved cost. `latency_by_op_model[].hits` counts those rows per op and model. `latency_by_op_model[].calls` is the miss count. `off` and `refresh` call the provider again. A single-flight waiter stays `single_flight_waits` / `cache=single-flight`, not a hit
- single-flight for concurrent misses of the same storage key and of the same decision body
- `POST /v1/cache/lookup`, `/put`, `/purge`, `/stats` in memory. Put is inline, not a background writer queue
- `GET /stats` is lite instrumentation. It adds `provider_calls`, `requests`, `total_cost_usd`, `cost_saved_usd`, and `latency_by_op_model` (`p50_ms`, `p95_ms`, `calls` = misses, `hits`) on top of the cache counters. `p50_ms` and `p95_ms` are provider latency of misses. `GET` or `POST /v1/cache/stats` stays the contract shape and does not include those fields
- `GET /v1/calls?limit=N` returns the recent call log (`limit` default 50, max 1024), oldest of that window first. `POST /reset` clears it. The log keeps the last 1024 calls
- Each item may include `calls`, one object per provider call or cache hit that produced it. SQL does not insert those rows. `rvbbit.calls` is a view: `rvbbit.call_log()` GETs `{backend}/v1/calls?limit=1024` from every `TYPE hutch` backend and `json_to_recordset` expands it. The view is not part of `rvbbit.dump_sql`

Decision calls:

- URL: `OPENROUTER_DECISIONS_URL`, or derived from `OPENROUTER_BASE_URL`. A base ending in `/api/v1` (the OpenRouter default `https://openrouter.ai/api/v1`) becomes `{root}/api/alpha/decisions`. A base ending in `/v1` (the crate tests) becomes `{origin}/alpha/decisions`. Anything else is `{base}/alpha/decisions`
- Model: empty or `mock` becomes `HUTCH_DECISION_MODEL` (default `typesafe/jev-1.14`). Any other slug, including `~typesafe/jev-latest`, is sent unchanged. Chat still uses `HUTCH_MODEL` (default `openai/gpt-4o-mini`) for extract and for classify with no labels
- MEANS / decide: one `noul` question, id `matches`. `state` is the input string. `instructions` is the catalog prompt, with the criterion appended once when the prompt does not already contain it (or the criterion alone, or `yes or no` when both are empty). `noul.criteria` is omitted. The Decisions API marks it optional, and a present object has to be exactly `true`/`false`, which repeated the question in the provider prompt. `score` is the mean `noul` (0 to 1). The boolean is that mean compared with `HUTCH_DECISION_THRESHOLD` (default 0.5). Published noul answers have no separate confidence field; `noul` is the score
- Fixed-label classify: one `choice` question, id `label`. `state` is the input string. With no catalog prompt the instruction is `Which label fits?` and the label list is not repeated there. `criteria` is an object of label to description. An array is HTTP 400. A label with no description uses the label text itself. SQL may set one with `LABELS ('bigfoot' AS 'an unidentified large primate or sasquatch', ...)`. `scores` is the probability map restricted to those labels. `score` is the winning label's probability. The published `choice` loses to argmax, and `confidence` is not that probability. `confidence` is copied onto the chosen label only when the map omits it
- Stack: optional `decision.models`, `decision.takes` (default `HUTCH_DECISION_TAKES`, clamp 1..=8), `decision.combine` (`mean` or `vote`), `decision.phrasings` (extra wordings of the same question). `CREATE DECISION TREE` emits `models` and `combine` from `USING (slug, slug, ...) COMBINE mean|vote`, and `takes` / `phrasings` from `TAKES n PHRASINGS ('...', ...)`. A SQL call can also write `WITH TAKES n ACROSS MODELS ('slug', ...) PHRASINGS ('...', ...) PICK BY VOTE`. The pre-parse rewriter turns that tail into `rvbbit.clause_bool`, `rvbbit.clause_score`, or `rvbbit.clause_text` (the op's natural type; the column is named after the op). `REPORT` still emits `rvbbit.apply_clauses`. gram.y is unchanged. The take index is in the memo key and is not sent. Temperature and `max_tokens` are not sent and are not part of the decision memo key. A tree call's client cache seed is the node id, so nodes do not share a contract entry. `ItemRequest.node` is `{id}:{question}` truncated to 120 characters and is copied onto `CallMeter.node`. It is not part of the MEANS cache key. `CallMeter.identical` is true when repeated takes of one model agreed and false when they differed. The call log stores that false; a single take leaves the field absent so the SQL column is null. A live Jev run of same-model takes varied by about 0.03, so `identical` was false. A second phrasing moved UFO row 2 from about 0.67 to about 0.83, more than the take-to-take spread
- Vote: more yes-votes than no-votes, where a vote is `noul >= threshold`. A tie uses the mean. The reported score is still the mean
- Cost, classify: one Decisions request per item per take per stacked model. Jev charges input tokens only (about $0.042 per million); output tokens are free. N labels do not multiply the request count, because `choice` returns every label's probability in that one call. A binary-only model would need N `noul` calls (one per label, one-vs-rest) and then argmax. That fallback is not implemented. Jev's `choice` question is multi-class
- Cost, MEANS: the same shape, one `noul` request per item per take per stacked model, not one request per row of a SQL batch on the wire from pgrustic. Hutch-lite fans the batch out

Cost fields, read from the provider response. Inline cost is preferred.
A missing `usage.cost` with a `gen-...` id falls back to
`GET {OPENROUTER_BASE_URL}/generation?id=`. That GET is not a
`provider_calls` increment. Its time is included in `latency_ms` and
not in `provider_latency_ms`.

Chat completions (always present; `usage.include` is deprecated):

- `id` — generation id
- `usage.cost` — amount charged to the account
- `usage.prompt_tokens` — `tokens_in`
- `usage.completion_tokens` — `tokens_out`

Decisions:

- `id` — generation id (`gen-dec-...` in the published examples)
- `model` — dated snapshot that served the request, stored when present
- `usage.cost` — USD
- `usage.input_tokens` — `tokens_in`
- `usage.output_tokens` — `tokens_out`

Generation fallback (`data` object):

- `total_cost`, or numeric `usage` when `total_cost` is absent
- `tokens_prompt` / `tokens_completion`
- `id`, `model` only when the inference body omitted them

`CallMeter` and `GET /v1/calls` use the names in the left column. The
`rvbbit.calls` view renames `cost_usd` to `cost`, `cost_saved_usd` to
`cost_saved`, and `timestamp` to `ts`:

| field | meaning |
| --- | --- |
| `op` | `predict`, `decide`, `classify`, or `extract` |
| `model` | response `model` when sent, otherwise the slug we sent |
| `tokens_in`, `tokens_out` | billed tokens on a miss; 0 on a hit |
| `cost_usd` (SQL column `cost`) | USD charged; 0 on a hit; null when the provider omitted it |
| `cost_saved_usd` (SQL column `cost_saved`) | USD avoided; 0 on a miss; null when the original charge is unknown |
| `provider_latency_ms` | inference POST only; null on a hit and on single-flight, so `avg(provider_latency_ms)` ignores them. `latency_ms` on a hit is hutch time and is not the provider average |
| `latency_ms` | hutch time for this call, including a generation GET |
| `cache` | `hit`, `miss`, or `single-flight` |
| `items` | 1. Hutch-lite fans a batch out to one provider call per item |
| `tenant` | blake3 hex of the bearer, or `HUTCH_TENANT` |
| `timestamp` (SQL column `ts`) | UTC RFC3339 |
| `generation_id` | provider `id`, including on a hit so the avoided call can be found |
| `node` | tree node `{id}:{question}` when the call came from `rvbbit.decide`; null for MEANS, classify, and extract. SQL column `node` |

A contract-cache hit is one row: `cost_usd` 0, `cost_saved_usd` equal to
the USD the misses charged while producing the stored value. A decision-memo
hit (MEANS and `means_score` sharing a body) is its own row with the same
shape. A value that was produced entirely from the memo has no produced
cost, so a later contract-cache hit of that value does not count the
savings a second time. Single-flight waiters record `cache=single-flight`,
cost 0, and the leader's produced cost as saved. Failed provider calls are
in the hutch log and `/stats`. They are not copied onto an item error.
`POST /reset` clears the in-memory log, which is what the view reads.
The executor does not insert into a table: that would cycle through SPI.
The view reads hutch, so a batched MEANS and a per-row function show up
the same way.

```sql
SELECT op, model, sum(cost), avg(latency_ms)
FROM rvbbit.calls
GROUP BY 1, 2;
```

One stderr line per call, `hutch_call` plus the fields above. The API key
is redacted if it appears. The line does not include the request body.

Sources:

- <https://openrouter.ai/docs/cookbook/administration/usage-accounting>
- <https://openrouter.ai/docs/api/api-reference/chat/create-a-chat-completion> (`usage.cost` example `0.0012`)
- <https://openrouter.ai/docs/api/api-reference/generations/get-request-&-usage-metadata-for-a-generation> (`data.total_cost`, `data.tokens_prompt`, `data.tokens_completion`; `data.usage` is a number in that example)
- <https://openrouter.ai/docs/guides/community/jev-tutorial> (Decisions `usage.cost` in USD)
- <https://openrouter.ai/docs/api/api-reference/alphadecisions/submit-a-decisions-request>
- <https://github.com/OpenRouterTeam/skills/blob/main/skills/openrouter-decisions/references/decisions-api.md>
- <https://openrouter.ai/docs/api/api-reference/rerank/submit-a-rerank-request> (`POST /api/v1/rerank`: `documents`, `model`, `query`, `top_n`; `results[].index`, `results[].relevance_score`, `usage`)

Uncertain:

- The usage-accounting cookbook says cost is in credits and shows `0.95` for a short completion. The chat API reference and the generation example use fractional amounts (`0.0012`, `0.0015`), and the Decisions docs say `usage.cost` is USD. This server stores the number as USD and does not rescale.
- On the generation endpoint, `usage` is a number equal to `total_cost` in the published example, not the chat `usage` object. Numeric `usage` is read only when `total_cost` is absent.
- `cost_details.upstream_inference_cost` is the provider's charge to OpenRouter (and is 0 or null for non-BYOK on the generation endpoint). The account charge is `usage.cost` / `total_cost`.
- Published Decisions responses always include `usage`. The generation GET is only for a body that omits `usage.cost`. The docs do not say `GET /generation` accepts a `gen-dec-...` id, though the id matches `^gen-[0-9A-Za-z-]+$`.
- OpenRouter's own `latency` field is not copied. `provider_latency_ms` is the wall clock of the inference POST.

What it stubs:

- A clause tail is rewritten before gram.y. `WITH TAKES`, `PICK BY`, `RETRY`, `CHECK`, `FALLBACK`, `BUDGET`, `CACHE`, and `TIMEOUT` become `rvbbit.clause_bool`, `rvbbit.clause_score`, or `rvbbit.clause_text`. `REPORT` keeps `rvbbit.apply_clauses`. Plain `MEANS` and datetime `EXTRACT` stay. Take index on a single MEANS cache key is 0; a stacked tree puts `takes` and `phrasings` in the cache opts. The `decision` object is the stack hook
- `embed` returns a per-item error `code=unsupported` and does not call the provider. Default `rerank` is a Jev `noul`: the document is `state`, the query is the criterion, and `score` is the relevance. `USING MODEL`, `semantic.rerank_model`, or `HUTCH_RERANK_MODEL` posts one OpenRouter `{base}/rerank` for the query's documents (`documents`, `model`, `query`, `top_n`) and reads `results[].index` plus `relevance_score`. Explicit `jev` stays on noul. For pure ranking, `voyageai/rerank-3` is the recommended model. `nvidia/nemotron-3.5-rerank-8b:free` is limited to 20 requests per minute and returns compressed scores. `SUMMARIZE` and `EXTRACT MANY` are chat (`extract_mode` `summary` or `many`)
- a wrong or missing `X-Rvbbit-Contract` is accepted, same as the mock
- client disconnect does not cancel an in-flight provider call
- `POST /reset` clears the process cache and the decision memo. That route is for tests and the demo

The upstream API key is `OPENROUTER_API_KEY`, read by hutch-lite and never
logged. `OPENROUTER_BASE_URL` overrides the chat base (default
`https://openrouter.ai/api/v1`); chat is `{base}/chat/completions` and a dedicated rerank model is `{base}/rerank`.
`BACKEND AUTH ENV` is the bearer for hutch itself. Leave it null for
hutch-lite; do not put the OpenRouter key there.

Sources for the Decisions API, read when this path was added:

- <https://openrouter.ai/typesafe/jev-1.14>
- <https://openrouter.ai/docs/guides/community/jev>
- <https://openrouter.ai/docs/guides/community/jev-tutorial>
- <https://openrouter.ai/docs/api/api-reference/alphadecisions/submit-a-decisions-request>
- <https://github.com/OpenRouterTeam/skills/blob/main/skills/openrouter-decisions/references/decisions-api.md>
- <https://docs.typesafe.ai/models> (the direct TypeSafe id is `jev-1.14.0`; OpenRouter's id is `typesafe/jev-1.14`)

Uncertain, and called out so a later response shape can be checked:

- Published `noul` examples omit `confidence`. The score is `noul`. A later `confidence` field would not replace it.
- TypeSafe says the caller picks the threshold. Their tool-gating cookbook uses 0.8. This server defaults to 0.5.
- `confidence` on a `choice` answer is not the probability of the winning label.
- Jev is not documented as sampling, but a live run of same-model takes was not identical: scores varied by about 0.03, so `identical` was false. A second phrasing moved UFO row 2 from about 0.67 to about 0.83, more signal than another take. The take index still forces a separate call.

## What the real hutch has to change

The private hutch repo is not in this workspace. To speak
`rvbbit-hutch-v1` it needs all of the following. The mock already does the
items marked mock.

1. Accept `X-Rvbbit-Contract: rvbbit-hutch-v1` on `/v1/predict`, `/v1/classify`, `/v1/extract`, `/v1/embed`, `/v1/rerank`, `/v1/decide`. Reject a missing or different contract with 400. (Mock accepts the body and does not reject a wrong contract string.)
2. Read `BatchRequest` / write `BatchResponse` from `rvbbit_contract`. `outputs[i]` answers `items[i]`. Per-item failures are `ItemError` `{code, message}` with `value: null`, not an HTTP error for the whole batch, unless the request itself is malformed (400) or unauthorized (401).
3. Honor `X-Rvbbit-Cache`. `mode=off` skips both read and write. `mode=read-write` returns a stored hit and enqueues a store on miss. `mode=refresh` ignores a hit, calls the provider, and stores. Header `ttl` is the store TTL when the item does not carry one.
4. Cache table keyed by `storage_key(client_key, tenant)`. Tenant comes from the auth credential, never from the body. Do not serve one tenant another tenant's hit even if the client key matches.
5. `POST /v1/cache/lookup` as specified. Bulk, one round trip, hits and misses partitioned. Misses are not errors.
6. `POST /v1/cache/put` behind a writer queue so predict handlers do not wait on the insert. Put is idempotent for the same storage key.
7. Single-flight: concurrent misses for the same storage key share one provider call. `CacheStatsResponse.single_flight_waits` counts the waiters.
8. `POST /v1/cache/purge` and `POST /v1/cache/stats` as specified, tenant-scoped.
9. Meter a cache hit with `cost_basis = "cache"` and zero provider tokens. A miss that calls the provider keeps the provider cost basis. Do not bill the queued put as a second prediction.
10. On client disconnect, cancel the provider call for items that were not yet sent, and do not write those results. pgrustic will not abort an in-flight `ureq` call on its side; closing the connection is the signal.
11. 429 responses should include `Retry-After` as an integer number of seconds. pgrustic waits that long (capped at 2s) and retries within the backend attempt budget.
12. Keep accepting the previous predict body until callers have moved, or gate it on the contract header. pgrustic no longer sends the spike's JSON array.

The list above is otherwise unchanged. `score`, `scores`, `labels`, `label_descriptions`, `decision`, and `calls` are additive optional fields. A server that ignores them still answers MEANS with a boolean `value`. Item 9's cache meter is the `calls` array: a hit has cost 0 and `cache` `hit`.

Dependency: add `rvbbit_contract` (this repo's `crates/rvbbit/rvbbit_contract`) as a path or git dependency. The crate's public types are the ones named above, plus `DecisionStack`: `BatchRequest`, `ItemRequest`, `BatchResponse`, `ItemResponse`, `ItemError`, `CacheLookupRequest`, `CacheLookupResponse`, `CacheHit`, `CachePutRequest`, `CachePutEntry`, `CachePutResponse`, `CachePurgeRequest`, `CachePurgeResponse`, `CacheStatsResponse`, `CacheKeyParts`, `client_cache_key`, `client_cache_key_hex`, `storage_key`, `canonical_json`, `cache_header`.
