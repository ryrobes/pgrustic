# Outbound MCP tools

`hutch-mcp` is a separate binary in this workspace. Postgres talks to it
over HTTP, the same way it talks to hutch. The binary is the only crate
that links `rmcp`. `mcp_core` holds the catalog types, JSON Schema to SQL
mapping, secret references, the read-only tool cache, and the session
pieces the inbound server reuses: a token mapped to a Postgres role,
`SET LOCAL ROLE`, a read-only transaction, and `semantic.max_cost`.
That server is `hutch-mcp --inbound`. See [MCP_INBOUND.md](MCP_INBOUND.md).
`UnconfiguredAuth` is only the stand-in for a process with no token store.

## DDL

The productions are a pre-parse rewrite, like the semantic syntax.
`gram.y` is unchanged. A real `CREATE SERVER ... FOREIGN DATA WRAPPER`
for any wrapper other than `mcp` stays a Postgres foreign server.
`DROP SERVER` stays in gram.y unless every name is a row in `mcp.server`.

```sql
CREATE SERVER demo FOREIGN DATA WRAPPER mcp
  OPTIONS (
    transport 'stdio',
    command '/path/hutch-mcp',
    args '--mock-stdio'
  );

CREATE SERVER remote FOREIGN DATA WRAPPER mcp
  OPTIONS (
    transport 'http',
    url 'http://127.0.0.1:8120/mcp',
    headers 'Authorization=secret:FETCH_TOKEN'
  );

IMPORT FOREIGN TOOLS FROM SERVER demo INTO mcp_demo;
IMPORT FOREIGN TOOLS LIMIT TO (echo, people) FROM SERVER demo INTO mcp_demo;
IMPORT FOREIGN TOOLS EXCEPT (fail) FROM SERVER demo INTO mcp_demo;

ALTER FOREIGN TOOL mcp_demo.echo
  OPTIONS (read_only 'true', cache_ttl '60s', timeout '5s', max_cost '0');

REFRESH FOREIGN TOOLS FROM SERVER demo;
DROP SERVER demo CASCADE;
```

Header and env values must be `secret:NAME`. The catalog stores that
reference. `hutch-mcp` reads `RVBBIT_SECRET_NAME` when it connects.
A plaintext header or env value is rejected. The URL and the command
are not secrets.

`IMPORT` creates one SQL function per tool. JSON Schema scalars become
`text`, `bigint`, `double precision`, or `boolean`. Objects and arrays
become `jsonb`. A union that includes `string` (`anyOf`, `oneOf`, or a
`type` array such as `string|array`) becomes `text`. Defaults in the
schema become SQL defaults. Required arguments come first, then
arguments that have a default. Each group keeps the schema's property
order. When that order was lost (the property keys are sorted and
`required` is a different permutation), the `required` array supplies
it. A required argument is never placed after a defaulted one. A tool Postgres rejects is skipped with
a `WARNING`, stored in `mcp.tool` with a null function oid and
`import_error`, and does not abort the rest of the import. An output schema that is an
array of objects with properties becomes `RETURNS TABLE`; anything else
returns `jsonb`. The function body calls `rvbbit.mcp_invoke_text` and
ends in `OFFSET 0`, so the planner does not inline it. Each row stores
the tool's schema hash.

`readOnlyHint` initializes `read_only`. A tool without the hint is
`VOLATILE`. A read-only tool is `STABLE`. Results are cached only when
`read_only` is set and `cache_ttl` is greater than 0. `ALTER` can set
`timeout`, `cache_ttl`, `max_cost`, and `read_only` or `side_effects`.
Those last two cannot be set together. `side_effects 'true'` clears
`read_only` and switches the function to `VOLATILE`.

`REFRESH` lists the server again and raises a notice with
`unchanged`, `added`, `changed`, and `removed`. A changed hash drops
and recreates the function and resets its options. A removed tool is
dropped. New tools are created in every schema that already imported
that server. `IMPORT` does not drop tools that disappeared.

`DROP SERVER name CASCADE` drops the functions, then the server. Without
`CASCADE`, a server that still has tools is an error. A `DROP` that
mixes an MCP server with a real foreign server is an error. The lookup
does not create `rvbbit` or `mcp` if those schemas are absent, so an
ordinary `DROP SERVER` in regression still reaches gram.y.

## Calls

The gateway listens on `semantic.mcp_url` (default
`http://127.0.0.1:8111`). `RVBBIT_MCP_URL`, or `HUTCH_MCP_PORT` when the
GUC is still the default, overrides it. Tool calls use the gateway's
timeout and retry. A tool error (`is_error`, or an MCP error result) is
a SQL error and is not retried. A transport error is retried.

```sql
SELECT mcp_demo.echo('Ken Lay');
SELECT mcp_demo.fail() ON ERROR NULL;
```

`ON ERROR` rewrites a scalar call to `rvbbit.mcp_invoke_text(..., true, fallback)`.
`NULL` stays SQL NULL. Any other fallback is returned as a JSON string.
The rewrite does not apply to `classify`, `extract`, `summarize`,
`consensus`, `rerank`, or `means`. It always yields `jsonb`, so `ON ERROR`
on a `RETURNS TABLE` tool is left as a SQL error.

Scalar tools are lifted onto a `Result` the same way native project calls
are, including under a subquery scan or an aggregate. A qual-free `Seq Scan`,
`Values Scan`, or `Subquery Scan` that still projects the tool is batched
too. The batch width is the semantic batch size (32). Calls overlap up to 4
on one session for both HTTP and stdio (MCP allows concurrent request ids).
`jsonb` arguments, set-returning tools, a scan that still has a qual, and a
`Result` that the semantic project batcher already owns stay one call per
row. A call outside a batch starts a process for that call. A successful
value the builtin memoized from an earlier statement is a `cache = hit` row
in `rvbbit.calls` even when the gateway cache ttl is 0. A failed call
(tool `isError` or a transport error) is not memoized and is not a hit.
A value the batcher just prefetched is not logged twice.

`EXPLAIN` prints `MCP Provider Calls`, `MCP Latency`, `MCP Server`, and
`MCP Tool` when the node's model is only `mcp:<server>/<tool>`.
A qual-free scan prices those calls from the planner's live tuple count
when it has one, instead of the 10-page floor. That count replaces the
scan's `rows=` estimate, and the `COST 1000000` total is scaled by the
same ratio so an un-analyzed table is not costed as 1360 rows. MCP latency uses the per-tool p50 from
`rvbbit.calls` when that tool has been called; otherwise the seed is 50 ms
per wave. `EXPLAIN ANALYZE` adds
`Actual MCP Calls`, `Actual Cache Hits`, `Actual MCP Latency`, and
`MCP Batches` when a batch ran. A plan that also has a semantic call keeps the
semantic labels. A tool projected by a scan, including `VALUES`, is
costed on that scan. The call log is `rvbbit.calls` with `op = 'mcp'`,
`server`, `tool`, `latency_ms`, and `status`. `rvbbit.call_log()` reads
each hutch backend and the MCP gateway once. A backend that does not
answer is skipped. MCP calls are unpriced
(USD 0). `max_cost '0'` rejects the call. A positive `max_cost` is
stored and does not bill tokens.

## Demo

Mock mode needs no network and no API key. Postgres is `55461`.
The gateway is `8112`. The spike ports `55432` and `8099` are refused.

```bash
bash scripts/rvbbit_mcp_demo.sh
```

Live mode, against a public no-auth MCP server, is optional:

```bash
RVBBIT_MCP_LIVE=1 bash scripts/rvbbit_mcp_demo.sh
```

It tries `npx -y @modelcontextprotocol/server-everything` and then `uvx`.
If neither runner is on `PATH`, the script exits before starting
Postgres. Live mode is not part of the mock gate.

## Known gaps

- Inbound MCP is a separate mode of this binary. See
  [MCP_INBOUND.md](MCP_INBOUND.md). The outbound gateway still does not
  accept MCP clients.
- MCP calls are unpriced. `max_cost 0` disables a tool. A positive
  `max_cost` is not a token budget.
- `RETURNS TABLE` only when the output schema is an array of objects
  with `properties`. A `$ref` or a bare array stays `jsonb`.
- `ON ERROR` on a set-returning tool is not rewritten. Named arguments
  keep the right-hand side and are still positional. The fallback is a
  JSON string, not an arbitrary jsonb value.
- `DROP SERVER` is a utility hook. A name that exists as both a Postgres
  foreign server and an MCP server is treated as MCP.
- Batching covers scalar tools whose arguments are bool, int, float, or
  text, on a `Result` or on a qual-free scan projection. `jsonb` and
  `numeric` arguments, set-returning tools, scans that still have a qual,
  and a Result already owned by the semantic project batcher stay per-row.
  A call outside a batch spawns a process per call. There is no
  cross-statement session pool.
- One HTTP batch shares one timeout, cache ttl, and `read_only` flag.
  Tools that differ on those options are separate batches. Each call
  records its own elapsed time.
- Streamable HTTP only. No legacy SSE. Tools only: no resources, prompts,
  or sampling.
- SQL names are sanitized. A collision gets `_2`, `_3`, and so on.
  A name longer than 63 bytes is skipped with a warning and a catalog
  row. Argument order is required properties, then defaulted properties,
  schema order inside each group.
- Streamable HTTP uses rustls. A plaintext `http://` URL still works.
  The gateway listens with `SO_REUSEADDR`.
- Planning loads installed MCP tools before it lifts a project call,
  including the first statement of a fresh backend. A subquery or
  aggregate over a table batches the same way a `VALUES` scan does.
- The mock `now` tool is not a real RFC3339 timestamp.
