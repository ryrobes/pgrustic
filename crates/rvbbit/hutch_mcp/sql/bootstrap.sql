-- Inbound MCP auth tables. Roles, codes, refresh tokens, and API keys
-- live here so more than one hutch-mcp process can share them.
-- The caller creates LOGIN role rvbbit_auth (NOSUPERUSER NOINHERIT)
-- before running this script.

CREATE SCHEMA IF NOT EXISTS rvbbit;

CREATE TABLE IF NOT EXISTS rvbbit.users (
    role text PRIMARY KEY,
    credential_version integer NOT NULL DEFAULT 1,
    max_cost double precision,
    statement_timeout_ms integer NOT NULL DEFAULT 15000,
    read_only boolean NOT NULL DEFAULT true,
    CHECK (role ~ '^[A-Za-z_][A-Za-z0-9_]*$')
);

CREATE TABLE IF NOT EXISTS rvbbit.oauth_client (
    client_id text PRIMARY KEY,
    secret_hash text,
    redirect_uris jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS rvbbit.oauth_code (
    code_hash text PRIMARY KEY,
    client_id text NOT NULL,
    role text NOT NULL,
    subject text NOT NULL,
    redirect_uri text NOT NULL,
    code_challenge text NOT NULL,
    credential_version integer NOT NULL,
    expires_at timestamptz NOT NULL
);

CREATE TABLE IF NOT EXISTS rvbbit.oauth_refresh (
    token_hash text PRIMARY KEY,
    client_id text NOT NULL,
    role text NOT NULL,
    subject text NOT NULL,
    credential_version integer NOT NULL,
    expires_at timestamptz NOT NULL
);

CREATE TABLE IF NOT EXISTS rvbbit.api_key (
    key_hash text PRIMARY KEY,
    name text NOT NULL,
    role text NOT NULL,
    credential_version integer NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS rvbbit.mcp_audit (
    id bigserial PRIMARY KEY,
    ts timestamptz NOT NULL DEFAULT now(),
    subject text NOT NULL,
    role text NOT NULL,
    tool text NOT NULL,
    sql_hash text,
    duration_ms integer NOT NULL,
    rows integer,
    cost double precision
);

GRANT USAGE ON SCHEMA rvbbit TO rvbbit_auth;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA rvbbit TO rvbbit_auth;
GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA rvbbit TO rvbbit_auth;
