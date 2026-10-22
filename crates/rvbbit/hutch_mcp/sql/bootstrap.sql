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
    cost double precision,
    status text,
    error text,
    cost_usd double precision,
    semantic_calls double precision
);

-- ADD COLUMN IF NOT EXISTS notices "column ... already exists, skipping"
-- on every re-run. Add the column only when pg_attribute does not have it.
DO $rvbbit_audit_cols$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_attribute a
        JOIN pg_class c ON c.oid = a.attrelid
        JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname = 'rvbbit' AND c.relname = 'mcp_audit'
          AND a.attname = 'status' AND NOT a.attisdropped
    ) THEN
        ALTER TABLE rvbbit.mcp_audit ADD COLUMN status text;
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM pg_attribute a
        JOIN pg_class c ON c.oid = a.attrelid
        JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname = 'rvbbit' AND c.relname = 'mcp_audit'
          AND a.attname = 'error' AND NOT a.attisdropped
    ) THEN
        ALTER TABLE rvbbit.mcp_audit ADD COLUMN error text;
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM pg_attribute a
        JOIN pg_class c ON c.oid = a.attrelid
        JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname = 'rvbbit' AND c.relname = 'mcp_audit'
          AND a.attname = 'cost_usd' AND NOT a.attisdropped
    ) THEN
        ALTER TABLE rvbbit.mcp_audit ADD COLUMN cost_usd double precision;
    END IF;
    IF NOT EXISTS (
        SELECT 1 FROM pg_attribute a
        JOIN pg_class c ON c.oid = a.attrelid
        JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname = 'rvbbit' AND c.relname = 'mcp_audit'
          AND a.attname = 'semantic_calls' AND NOT a.attisdropped
    ) THEN
        ALTER TABLE rvbbit.mcp_audit ADD COLUMN semantic_calls double precision;
    END IF;
END
$rvbbit_audit_cols$;

-- Future tables created by this role (rvbbit setup after this script)
-- are granted too. ALL TABLES only covers relations that exist now.
ALTER DEFAULT PRIVILEGES IN SCHEMA rvbbit GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO rvbbit_auth;
ALTER DEFAULT PRIVILEGES IN SCHEMA rvbbit GRANT USAGE, SELECT ON SEQUENCES TO rvbbit_auth;
GRANT USAGE ON SCHEMA rvbbit TO rvbbit_auth;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA rvbbit TO rvbbit_auth;
GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA rvbbit TO rvbbit_auth;

-- Mapped warehouse roles. Object privileges wait until the catalog
-- tables exist (rvbbit setup creates them per relation). Membership is
-- granted here, including for rows inserted after this script.
DO $rvbbit_sem_role$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'rvbbit_semantic_user') THEN
        CREATE ROLE rvbbit_semantic_user NOLOGIN;
    END IF;
END
$rvbbit_sem_role$;

DO $rvbbit_sem_grant$
BEGIN
    IF EXISTS (
        SELECT 1 FROM pg_class c
        JOIN pg_namespace n ON n.oid = c.relnamespace
        WHERE n.nspname = 'rvbbit' AND c.relname = 'semfn'
    ) THEN
        GRANT USAGE ON SCHEMA rvbbit TO rvbbit_semantic_user;
        GRANT SELECT ON rvbbit.semop, rvbbit.semfn, rvbbit.backend, rvbbit.model,
            rvbbit.decision_tree, rvbbit.workflow TO rvbbit_semantic_user;
    END IF;
END
$rvbbit_sem_grant$;

CREATE OR REPLACE FUNCTION rvbbit.grant_semantic_user() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog
AS $fn$
BEGIN
    BEGIN
        EXECUTE format('GRANT rvbbit_semantic_user TO %I', NEW.role);
    EXCEPTION
        WHEN undefined_object OR insufficient_privilege THEN
            NULL;
    END;
    RETURN NEW;
END
$fn$;

DROP TRIGGER IF EXISTS grant_semantic_user ON rvbbit.users;
CREATE TRIGGER grant_semantic_user
    AFTER INSERT ON rvbbit.users
    FOR EACH ROW
    EXECUTE FUNCTION rvbbit.grant_semantic_user();

DO $rvbbit_sem_members$
DECLARE
    r text;
BEGIN
    FOR r IN SELECT role FROM rvbbit.users LOOP
        BEGIN
            EXECUTE format('GRANT rvbbit_semantic_user TO %I', r);
        EXCEPTION
            WHEN undefined_object OR insufficient_privilege THEN
                NULL;
        END;
    END LOOP;
END
$rvbbit_sem_members$;
