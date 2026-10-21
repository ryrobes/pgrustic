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
There is no pre-flight `POST /v1/cache/lookup`: that hop would change the
10k-row benchmark. The mock ignores the header and counts every POST, so
that benchmark's call count is unchanged. Hutch-lite honors the header
inside the POST and does not call the provider again on a hit.

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
puts the pair in `inputs`.

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

## Cache key v1

The client key does **not** include the tenant. Hutch mixes the tenant in
when it stores or looks up. A client-supplied tenant is not trusted.

`client_cache_key` is blake3 over length-prefixed fields, in order:

1. magic `rvbbit-cache-v1`
2. function name
3. function version (decimal)
4. template hash
5. model
6. canonical temperature (JSON number; non-finite becomes `null`)
7. `max_tokens` decimal, or empty
8. take index (decimal, MEANS sends `0`)
9. seed (MEANS sends empty)
10. canonical JSON of `inputs`

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
- no call to `/v1/cache/lookup`

A text semantic function posts the same header to `{backend.url}/v1/extract`,
one HTTP call per row, with `inputs.input` set to the concatenated text
arguments. Hutch returns JSON; the SQL function returns that JSON as
`text`.

A bool `value` of JSON `true`/`false` is the predicate result. A non-bool
value fails the batch. Item errors fail the batch.

## Hutch-lite

`crates/rvbbit/rvbbit_hutch_lite` is an in-tree server that speaks this
contract over plain HTTP and forwards `predict`, `extract`, and
`classify` to an OpenAI-compatible chat endpoint (OpenRouter by default).
It is not the private hutch. No JSON field was added.

What it implements:

- `POST /v1/predict` (and the `/v1/means` alias), `/v1/extract`, `/v1/classify`
- batched items, per-item errors, siblings may succeed
- in-memory cache. The storage key is `storage_key(client_key, tenant)` from `rvbbit_contract`. The client key already mixes operation identity (function name and version), template hash, model, temperature, max_tokens, take index, seed, and canonical inputs. Tenant is mixed only on the hutch side: blake3 hex of the bearer token, or `HUTCH_TENANT` (default `local`) when the request has no bearer
- single-flight for concurrent misses of the same storage key
- `POST /v1/cache/lookup`, `/put`, `/purge`, `/stats` in memory. Put is inline, not a background writer queue
- `GET /stats` is lite instrumentation. It adds `provider_calls` and `requests` on top of the cache counters. `GET` or `POST /v1/cache/stats` stays the contract shape and does not include those two fields

What it stubs:

- `TAKES` / `PICK` have no route. The SQL parser still errors with “not implemented until milestone M5”. Take index on the wire is 0
- `embed`, `rerank`, and `decide` return a per-item error `code=unsupported` and do not call the provider
- a wrong or missing `X-Rvbbit-Contract` is accepted, same as the mock
- client disconnect does not cancel an in-flight provider call
- `POST /reset` clears the process cache. That route is for tests and the demo

The upstream API key is `OPENROUTER_API_KEY`, read by hutch-lite and never
logged. `OPENROUTER_BASE_URL` overrides the chat base (default
`https://openrouter.ai/api/v1`); the call is `{base}/chat/completions`.
`HUTCH_MODEL` (default `openai/gpt-4o-mini`) is used when the request
model is empty or `mock`. Any other request model is sent through.
`BACKEND AUTH ENV` is the bearer for hutch itself. Leave it null for
hutch-lite; do not put the OpenRouter key there.

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

Dependency: add `rvbbit_contract` (this repo's `crates/rvbbit/rvbbit_contract`) as a path or git dependency. The crate's public types are the ones named above: `BatchRequest`, `ItemRequest`, `BatchResponse`, `ItemResponse`, `ItemError`, `CacheLookupRequest`, `CacheLookupResponse`, `CacheHit`, `CachePutRequest`, `CachePutEntry`, `CachePutResponse`, `CachePurgeRequest`, `CachePurgeResponse`, `CacheStatsResponse`, `CacheKeyParts`, `client_cache_key`, `client_cache_key_hex`, `storage_key`, `canonical_json`, `cache_header`.
