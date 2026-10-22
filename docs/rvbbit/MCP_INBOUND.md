# Inbound MCP warehouse

`hutch-mcp --inbound` is an MCP server in front of this Postgres. A
client calls SQL and catalog tools. Every call runs as a mapped
Postgres role. Password logins and API keys are read-only. An OIDC
mapping can open a read-write session. OAuth 2.1 state lives in
Postgres, so more than one process can share it. There is no session
pinned to one replica: each `/mcp` request carries its own bearer.

The outbound gateway (no flag, or an address with no flag) is unchanged.
It still does not accept MCP clients. This page is only the inbound mode.

```bash
bash scripts/rvbbit_mcp_inbound_demo.sh
```

Postgres listens on `55463`. The server listens on `127.0.0.1:8114`.
A mock OIDC issuer listens on `127.0.0.1:8124`. The script refuses
`55432` / `8099` and the outbound demo ports `55461` / `8112`.
`cleanup` stops the issuer, Postgres, and the server and waits until
the listen port and the socket are gone, so a second run can start.
The demo does not call Google.

## Authenticator

Create a low-privilege role, then grant it the roles it may become.
`NOINHERIT` means it does not have their table rights until `SET ROLE`.

```sql
CREATE ROLE rvbbit_auth LOGIN NOSUPERUSER NOINHERIT;
GRANT analyst TO rvbbit_auth;
```

`crates/rvbbit/hutch_mcp/sql/bootstrap.sql` creates schema `rvbbit` and
grants the authenticator DML on those tables only. User tables stay
where they are. GRANTs and RLS are the enforcement. The server does not
filter rows itself.

Running that script before rvbbit setup does not block the catalog.
Setup creates each missing table, view, and function on its own, even
when schema `rvbbit` already exists. A read-only transaction, including
`BEGIN READ ONLY` and a hot standby, does not migrate or log into the
catalog. Call history, cost history, and the tool memo stay in memory
(or on hutch). Install or upgrade the catalog in a read-write session.
If it is missing, semantic SQL returns `rvbbit catalog is not installed;
set it up in a read-write transaction`.

`rvbbit_semantic_user` is a `NOLOGIN` role. Bootstrap creates it and
grants it to every role inserted into `rvbbit.users` (a trigger covers
rows added later). Once the catalog tables exist, that role has `USAGE`
on schema `rvbbit` and `SELECT` on `semop`, `semfn`, `backend`, `model`,
`decision_tree`, and `workflow`. `rvbbit.my_calls` is a security-barrier
view of `rvbbit.calls` filtered to `tenant = current_user`. The call log
stores that role in both `tenant` and `role`. Mapped roles read their
own rows there. The full `rvbbit.calls` log stays with the owner.
`rvbbit_auth` receives `ALTER DEFAULT PRIVILEGES` plus explicit grants
when the catalog is set up, so either bootstrap order can read the
tables. OAuth statements do not load the semantic catalog.

`RVBBIT_PG_CONNINFO` is the authenticator connection, usually a local
socket as `rvbbit_auth`. Login is a separate TCP connection as the
person's role, with the password they typed. `pg_hba.conf` needs a
password method for those roles and a method the authenticator can use
without that password. A down server is an error. A refused password is
a failed login.

```text
RVBBIT_PG_CONNINFO=host=/tmp port=5432 dbname=postgres user=rvbbit_auth
RVBBIT_PG_PASSWORD_HOST=127.0.0.1
RVBBIT_PG_PASSWORD_PORT=5432
RVBBIT_PG_PASSWORD_DB=postgres
RVBBIT_MCP_JWT_SECRET=replace-me
RVBBIT_MCP_PUBLIC_URL=http://127.0.0.1:8114
```

`RVBBIT_MCP_JWT_SECRET` is required and must be the same on every
replica. `RVBBIT_MCP_PUBLIC_URL` is the issuer written into the
metadata documents. It defaults to `http://` plus the bind address.

## Allow-list

`rvbbit.users` is the allow-list. A role that can log in to Postgres
still cannot get a token until it has a row here.

| column | meaning |
| --- | --- |
| `role` | Postgres role name. Identifier characters only. |
| `credential_version` | Integer carried in JWTs and API keys. Bump it to revoke both. |
| `max_cost` | USD cap. NULL is uncapped. `0` rejects `run_sql`. A positive value is `SET LOCAL semantic.max_cost` and also rejects a plan whose total USD is above it. |
| `statement_timeout_ms` | `SET LOCAL statement_timeout`. Default 15000. `0` leaves the session default. |
| `read_only` | Recorded on the row. Password logins and API keys ignore it and stay read-only. It does not turn writes on. |

## OIDC

OIDC sits beside the password form. Leave the client id or the client
secret empty and the form is the only login. A configured issuer adds
a link on `GET /authorize`.

```text
RVBBIT_OIDC_ISSUER=https://accounts.google.com
RVBBIT_OIDC_CLIENT_ID=replace-me
RVBBIT_OIDC_CLIENT_SECRET=replace-me
RVBBIT_OIDC_DOMAINS=example.com
```

`RVBBIT_OIDC_PROVIDER=google` with an empty issuer uses
`https://accounts.google.com` and the link says "Sign in with Google".
An explicit issuer is used as written (one trailing slash is stripped).
The link says Google when the provider is `google` or the issuer is
Google's. Any other issuer says "Sign in with OIDC".
`RVBBIT_OIDC_DOMAINS` is an optional comma or whitespace list. An empty
list does not check the domain.

Discovery is `GET {issuer}/.well-known/openid-configuration`. The
document's `issuer` must match. `authorization_endpoint`,
`token_endpoint`, and `jwks_uri` are required. The document and the
JWKS are cached until a signature check fails, which drops the key
cache and fetches it once more.

`GET /oidc/start` requires `response_type=code` and S256, and the MCP
client must already be registered. It stores a hash of a fresh `state`,
a nonce, and the upstream PKCE verifier for ten minutes, then redirects
to the provider with `scope=openid email`, that state and nonce, an
S256 challenge, and `redirect_uri={public}/oidc/callback`. Google also
gets `prompt=select_account`. The client secret is not on this redirect.

`GET /oidc/callback` deletes that state row and posts the code, the
verifier, and the same redirect URI to the token endpoint. The
`id_token` must be an RS256 JWT. `none` and `HS256` are rejected. The
`kid` selects a JWKS RSA key (or every RSA key, when `kid` is empty).
The check covers the signature, `iss`, `aud` (string or array), `exp`,
`nbf` (30 seconds of skew), and `nonce`. `email` is required.
`email_verified: false` is denied. A missing `email_verified` is
allowed. `hd` is optional. A failed callback cannot be retried: the
state row is already gone. Error text does not include the token, the
code, or the secret.

`rvbbit.oidc_role` maps a claim to a Postgres role:

| column | meaning |
| --- | --- |
| `claim` | `email` or `hd`. |
| `value` | The email address, or the hosted-domain value. Compared case-insensitively. |
| `role` | Postgres role. It must also have a row in `rvbbit.users`. |
| `read_only` | Default true. `false` is the only way to open a write session. |

An exact email row wins over an `hd` row. No row is `no role mapping`.
When `RVBBIT_OIDC_DOMAINS` is set, the email's domain or `hd` must be
in the list before the mapping is considered. The access token's `sub`
is the email. A password login's `sub` stays the role name.

```sql
INSERT INTO rvbbit.oidc_role (claim, value, role, read_only) VALUES
    ('email', 'ada@example.com', 'analyst', true),
    ('hd', 'example.com', 'intern', true);
```

`rvbbit.oidc_state` holds the upstream verifier and the nonce. Do not
log that table, the client secret, or tokens. A successful callback
redirects to the MCP client's `redirect_uri` with this server's code.
The client finishes on the existing `/token` PKCE path. Refresh keeps
the `read_only` flag from the login. It does not re-read the mapping.

## OAuth 2.1

| method | path | purpose |
| --- | --- | --- |
| GET | `/.well-known/oauth-authorization-server` | Issuer, authorize, token, registration, S256. |
| GET | `/.well-known/oauth-protected-resource` | Resource is `{public}/mcp`. |
| POST | `/register` | Dynamic client registration. Public clients (`token_endpoint_auth_method=none`). A client secret is issued and not required. |
| GET | `/authorize` | HTML login. Password form, plus an OIDC link when an issuer is configured. `response_type=code` and S256. |
| GET | `/oidc/start` | Redirect to the OIDC provider. Same client, PKCE, and state as `/authorize`. |
| GET | `/oidc/callback` | Provider redirect. Checks the id_token, maps a role, then redirects to the MCP client with this server's code. |
| POST | `/login` | Password check. Redirects to the client's `redirect_uri` with `code` and `state`. Always read-only. |
| POST | `/token` | `authorization_code` with PKCE S256, or `refresh_token`. Form or JSON. |

Access tokens are HS256 JWTs, valid for one hour. Claims are `sub`,
`role`, `ver` (credential version), `ro` (read-only; missing means
true), and `exp`. Refresh tokens are
stored hashed, rotated on use, and last 30 days. Authorization codes
are hashed, one-shot, and deleted on redeem. Nothing is written to a
file.

A static API key is `rvb_` plus random data. Only the SHA-256 hex is
stored. It is not a JWT (a bearer with two dots is a JWT; anything else
is a key). The key's credential version must match `rvbbit.users`.

```bash
hutch-mcp --issue-key analyst claude
```

The plaintext key is printed once.

`GET` or `POST /mcp` without a bearer returns HTTP 401 and

```text
WWW-Authenticate: Bearer resource_metadata="http://127.0.0.1:8114/.well-known/oauth-protected-resource"
```

That is what Claude and Cursor use to start OAuth.

## What a tool call does

The pool is a handful of authenticator connections. One call:

1. `BEGIN`
2. `SET LOCAL ROLE "<mapped role>"`
3. `SET LOCAL transaction_read_only = on` when the session is read-only
4. `SET LOCAL statement_timeout = '<ms>ms'` when the timeout is set
5. `SET LOCAL semantic.max_cost = '<usd>'` when the cap is positive
6. The tool's SQL
7. `COMMIT`, or `ROLLBACK` if the tool failed
8. `INSERT` into `rvbbit.mcp_audit` as the authenticator, outside the user transaction

The mapped role does not need `INSERT` on the audit table.

`run_sql` accepts one statement. The command is classified before any
`EXPLAIN` pre-flight. On a read-only session, `SELECT`, `WITH`,
`VALUES`, `TABLE`, and `EXPLAIN` of those commands are allowed.
Anything else returns `only read-only SELECT/WITH/VALUES/EXPLAIN allowed`.
A write session (an `oidc_role` row with `read_only` false) may run
other statements. `SET ROLE`, `RESET ROLE`, and `SET SESSION` stay
rejected, including `SET SESSION AUTHORIZATION`. `SELECT`, `WITH`,
`VALUES`, and `TABLE` are wrapped as
`SELECT * FROM (...) AS rvbbit_mcp_q LIMIT n`. `EXPLAIN` is not wrapped.
`n` is clamped to 1..=1000 and defaults to 200. A tool failure is an
`isError` result whose text is the message. It is not a JSON-RPC
protocol error. A `max_cost` of 0 returns
`semantic estimate exceeds budget 0` before the statement runs.
`run_sql` reads `Semantic Total USD` and `Semantic Total Provider Calls`
from one `EXPLAIN` of a read-only statement and stores them on the
audit row. A write statement is not preflighted for USD.

## Tools

| tool | returns |
| --- | --- |
| `list_schemas` | Non-system schemas visible to the role. |
| `list_tables` / `search_data` | Tables with planner stats and a three-row sample. Optional `schema` and `q`. |
| `describe_table` | Columns, stats, and a sample. `schema` and `table`. |
| `validate_sql` | `{ok, plan, usd}` or `{ok:false, error}`. The plan text includes semantic and MCP estimates when the statement has them. |
| `explain` | The same plan as text. |
| `run_sql` | Rows, with the cap and timeout above. Writes only when the OIDC mapping says so. |
| `semantic_ops_help` | Short grammar for `MEANS`, `ABOUT`, `CLASSIFY`, `EXTRACT`, and semantic functions. |
| `list_semantic_functions` | Functions in schema `rvbbit`. |

There are no Calliope tools and no Google tools.

## Audit

`rvbbit.mcp_audit` gets one row per tool call: `subject`, `role`,
`tool`, `sql_hash` (SHA-256 hex of the SQL, or NULL), `duration_ms`,
`rows`, `cost`, `status` (`ok` or `error`), `error` (the message, or
NULL), `cost_usd`, and `semantic_calls`. A failed call is still logged.
`subject` is the JWT `sub` (the role name for a password login, the
email for OIDC) or the API key's name. Access tokens carry `iat` and
`jti`, so a refresh is a
different token even in the same second.

## stdio

```bash
RVBBIT_MCP_BEARER='rvb_...' hutch-mcp --inbound-stdio
```

stdio has no HTTP `Authorization` header, so the bearer is
`RVBBIT_MCP_BEARER` (an API key or a JWT). The same conninfo and JWT
secret are required. The password host is only used by the HTTP login
form.

## Claude Desktop

stdio, in `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "rvbbit": {
      "command": "/absolute/path/hutch-mcp",
      "args": ["--inbound-stdio"],
      "env": {
        "RVBBIT_PG_CONNINFO": "host=/tmp port=5432 dbname=postgres user=rvbbit_auth",
        "RVBBIT_MCP_JWT_SECRET": "replace-me",
        "RVBBIT_MCP_BEARER": "rvb_replace_me"
      }
    }
  }
}
```

A remote connector uses the streamable HTTP URL
`http://127.0.0.1:8114/mcp`. The 401 `resource_metadata` points the
client at the authorization server. The login form asks for a Postgres
role that has a row in `rvbbit.users`, and that role's password. When
OIDC is configured, the same page links to the provider. Unmapped
emails are denied. The password form stays as a fallback.

## Cursor

`.cursor/mcp.json`:

```json
{
  "mcpServers": {
    "rvbbit": {
      "url": "http://127.0.0.1:8114/mcp"
    }
  }
}
```

Cursor follows the same 401 metadata and runs the OAuth flow (PKCE and
dynamic client registration). A static key skips the browser:

```json
{
  "mcpServers": {
    "rvbbit": {
      "url": "http://127.0.0.1:8114/mcp",
      "headers": {
        "Authorization": "Bearer rvb_replace_me"
      }
    }
  }
}
```

Start the server with `hutch-mcp --inbound 127.0.0.1:8114` before the
client connects. The default bind, when the address is omitted, is
`127.0.0.1` and `HUTCH_MCP_INBOUND_PORT` or `8113`.

## Known gaps

- `rvbbit.users.read_only` does not enable writes. Password logins and
  API keys stay read-only. Only `rvbbit.oidc_role.read_only = false`
  opens a write session.
- OIDC checks RS256 only. There is no live Google call in the tests or
  the demo. Both use a mock issuer. A refresh keeps the `read_only`
  flag from the login instead of reading the mapping again. A failed
  callback consumes the state row.
- `run_sql` is one statement. A positive `max_cost` compares the
  planner's `total usd` lines. A statement with no USD estimate is
  treated as zero and is allowed under a positive cap.
- Samples in `list_tables` and `describe_table` run as the role. A
  missing GRANT shows up as a sample error on that table.
- Resources, prompts, and sampling are not served.
