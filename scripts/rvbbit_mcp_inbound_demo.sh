#!/usr/bin/env bash
# Inbound MCP warehouse: OAuth 2.1 (PKCE + dynamic client registration),
# role mapping, RLS, read-only, a zero budget, and the audit table.
# Postgres 55463, hutch-mcp --inbound 8114. The spike on 55432 / 8099
# is not used, and neither is the outbound demo (55461 / 8112).
# No API key is printed. Mock mode is the gate.
set -euo pipefail

unset PGPORT HUTCH_PORT RVBBIT_HUTCH_URL
ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

PG_BIN=${PG_BIN:-/usr/lib/postgresql/18/bin}
PGPORT=${RVBBIT_MCP_INBOUND_PGPORT:-55463}
HUTCH_MCP_INBOUND_PORT=${HUTCH_MCP_INBOUND_PORT:-8114}
PGHOST=${PGHOST:-/tmp}
PGUSER=${PGUSER:-postgres}
PGDATA=${PGDATA:-/tmp/rvbbit-mcp-inbound-pg}
RUNDIR=${RUNDIR:-/tmp/rvbbit-mcp-inbound}
BASE="http://127.0.0.1:${HUTCH_MCP_INBOUND_PORT}"
export CC=${CC:-gcc}
export CXX=${CXX:-g++}
export PATH="$PG_BIN:$PATH"
export PGRUST_PGSHAREDIR=${PGRUST_PGSHAREDIR:-/usr/share/postgresql/18}
export PGRUST_TZDIR=${PGRUST_TZDIR:-/usr/share/zoneinfo}

if [[ "$PGPORT" == "55432" || "$HUTCH_MCP_INBOUND_PORT" == "8099" || "$PGPORT" == "55461" || "$HUTCH_MCP_INBOUND_PORT" == "8112" ]]; then
  echo "Refusing spike ports 55432 / 8099 and the outbound demo ports 55461 / 8112." >&2
  exit 1
fi

for cmd in gcc g++ curl initdb psql cargo python3; do
  if ! command -v "$cmd" >/dev/null 2>&1; then
    echo "missing command: $cmd" >&2
    exit 1
  fi
done

port_open() {
  (echo >/dev/tcp/127.0.0.1/"$1") >/dev/null 2>&1
}

if port_open "$HUTCH_MCP_INBOUND_PORT"; then
  echo "127.0.0.1:$HUTCH_MCP_INBOUND_PORT is already in use." >&2
  exit 1
fi
if [[ -S "$PGHOST/.s.PGSQL.$PGPORT" ]]; then
  echo "Postgres socket $PGHOST/.s.PGSQL.$PGPORT is already in use." >&2
  exit 1
fi

echo "=== build (CC=$CC CXX=$CXX) ==="
env -u PGPORT -u HUTCH_PORT cargo build -p main_main -p hutch_mcp --profile fast-profile

mkdir -p "$RUNDIR"
MCP_LOG=$RUNDIR/hutch-mcp.log
PG_LOG=$RUNDIR/postgres.log
HUTCH_BIN="$ROOT/target/fast-profile/hutch-mcp"

cleanup() {
  if [[ -n "${MCP_PID:-}" ]]; then
    kill "$MCP_PID" >/dev/null 2>&1 || true
    wait "$MCP_PID" 2>/dev/null || true
  fi
  if [[ -n "${PG_PID:-}" ]]; then
    if [[ -x "$PG_BIN/pg_ctl" && -f "$PGDATA/postmaster.pid" ]]; then
      "$PG_BIN/pg_ctl" -D "$PGDATA" -m fast -w stop >/dev/null 2>&1 || true
    fi
    if [[ -f "$PGDATA/postmaster.pid" ]]; then
      kill "$(head -n 1 "$PGDATA/postmaster.pid")" >/dev/null 2>&1 || true
    fi
    wait "$PG_PID" 2>/dev/null || true
  fi
  for _ in $(seq 1 50); do
    if ! port_open "$HUTCH_MCP_INBOUND_PORT" && [[ ! -S "$PGHOST/.s.PGSQL.$PGPORT" ]]; then
      return 0
    fi
    sleep 0.1
  done
}
trap cleanup EXIT

rm -rf "$PGDATA"

echo "=== initdb $PGDATA ==="
initdb -D "$PGDATA" --no-locale --encoding=UTF8 --auth-local=trust --auth-host=trust -U "$PGUSER" >"$RUNDIR/initdb.log"
cat >"$PGDATA/pg_hba.conf" <<'HBA'
local all postgres trust
local all rvbbit_auth trust
host all postgres 127.0.0.1/32 trust
host all rvbbit_auth 127.0.0.1/32 trust
host all all 127.0.0.1/32 scram-sha-256
HBA

echo "=== postgres on socket $PGHOST port $PGPORT ==="
ulimit -s 65520
env -u PGPORT -u HUTCH_PORT \
  RUST_MIN_STACK=33554432 \
  "$ROOT/target/fast-profile/postgres" \
  -D "$PGDATA" -k "$PGHOST" -p "$PGPORT" \
  -c listen_addresses=127.0.0.1 -c io_method=sync -c max_stack_depth=60000 \
  >"$PG_LOG" 2>&1 &
PG_PID=$!

ready=0
for _ in $(seq 1 80); do
  if psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -X -q -c 'SELECT 1' >/dev/null 2>&1; then
    ready=1
    break
  fi
  sleep 0.25
done
if [[ "$ready" != 1 ]]; then
  echo "postgres did not become ready. Log: $PG_LOG" >&2
  exit 1
fi

PSQL=(psql -h "$PGHOST" -p "$PGPORT" -U "$PGUSER" -v ON_ERROR_STOP=1 -X)
"${PSQL[@]}" <<'SQL'
CREATE ROLE rvbbit_auth LOGIN NOSUPERUSER NOINHERIT;
CREATE ROLE analyst LOGIN PASSWORD 'analyst-secret' NOSUPERUSER;
CREATE ROLE intern LOGIN PASSWORD 'intern-secret' NOSUPERUSER;
CREATE ROLE broke LOGIN PASSWORD 'broke-secret' NOSUPERUSER;
GRANT analyst, intern, broke TO rvbbit_auth;
SQL
"${PSQL[@]}" -f "$ROOT/crates/rvbbit/hutch_mcp/sql/bootstrap.sql"
"${PSQL[@]}" <<'SQL'
INSERT INTO rvbbit.users (role, max_cost) VALUES ('analyst', NULL), ('intern', NULL), ('broke', 0);
CREATE TABLE notes (owner text, body text);
ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
ALTER TABLE notes FORCE ROW LEVEL SECURITY;
CREATE POLICY own ON notes USING (owner = current_user);
GRANT SELECT ON notes TO analyst, intern, broke;
INSERT INTO notes (owner, body) VALUES ('analyst', 'a-secret'), ('intern', 'i-secret');
SQL

echo "=== hutch-mcp --inbound on $BASE ==="
: >"$MCP_LOG"
env -u PGPORT -u HUTCH_PORT \
  RVBBIT_PG_CONNINFO="host=${PGHOST} port=${PGPORT} dbname=postgres user=rvbbit_auth" \
  RVBBIT_PG_PASSWORD_HOST=127.0.0.1 \
  RVBBIT_PG_PASSWORD_PORT="$PGPORT" \
  RVBBIT_PG_PASSWORD_DB=postgres \
  RVBBIT_MCP_JWT_SECRET=inbound-demo-secret \
  RVBBIT_MCP_PUBLIC_URL="$BASE" \
  "$HUTCH_BIN" --inbound "127.0.0.1:${HUTCH_MCP_INBOUND_PORT}" \
  >"$MCP_LOG" 2>&1 &
MCP_PID=$!

ready=0
for _ in $(seq 1 50); do
  if curl -sf "$BASE/health" >/dev/null; then
    ready=1
    break
  fi
  sleep 0.1
done
if [[ "$ready" != 1 ]]; then
  echo "inbound server did not listen. Log: $MCP_LOG" >&2
  exit 1
fi

python3 - "$BASE" "$HUTCH_BIN" "$PGHOST" "$PGPORT" <<'PY'
import json, os, subprocess, sys, urllib.error, urllib.parse, urllib.request

base, binary, pghost, pgport = sys.argv[1:]
VERIFIER = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"
CHALLENGE = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"

class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise urllib.error.HTTPError(req.full_url, code, msg, headers, fp)

OPENER = urllib.request.build_opener(NoRedirect)

def header(hdrs, name):
    if hdrs is None:
        return ""
    return hdrs.get(name) or ""

def req(method, path, body=None, headers=None, form=False):
    data = None
    hdrs = dict(headers or {})
    if body is not None:
        if form:
            data = urllib.parse.urlencode(body).encode()
            hdrs["content-type"] = "application/x-www-form-urlencoded"
        else:
            data = json.dumps(body).encode()
            hdrs.setdefault("content-type", "application/json")
    request = urllib.request.Request(base + path, data=data, headers=hdrs, method=method)
    try:
        with OPENER.open(request, timeout=10) as resp:
            raw = resp.read()
            return resp.status, resp.headers, raw
    except urllib.error.HTTPError as err:
        return err.code, err.headers, err.read()

def mcp(token, id_, method, params, session=None):
    headers = {
        "accept": "application/json, text/event-stream",
        "authorization": f"Bearer {token}",
        "mcp-protocol-version": "2025-03-26",
    }
    if session:
        headers["mcp-session-id"] = session
    status, hdrs, raw = req("POST", "/mcp", {"jsonrpc": "2.0", "id": id_, "method": method, "params": params}, headers)
    text = raw.decode()
    try:
        value = json.loads(text)
    except json.JSONDecodeError:
        value = None
        for line in text.splitlines():
            line = line.strip()
            if line.startswith("data:"):
                payload = line[5:].strip()
                if payload:
                    value = json.loads(payload)
        if value is None:
            raise SystemExit(f"mcp {method} status {status} body {text[:500]}")
    if status != 200:
        raise SystemExit(f"mcp {method} status {status} {value}")
    return value, header(hdrs, "mcp-session-id")

def tool_text(value):
    content = value.get("result", {}).get("content") or []
    if not content:
        return json.dumps(value)
    return content[0].get("text") or json.dumps(value)

def bearer(client_id, user, password):
    status, hdrs, raw = req("POST", "/login", {
        "client_id": client_id,
        "redirect_uri": "http://127.0.0.1:9/cb",
        "code_challenge": CHALLENGE,
        "code_challenge_method": "S256",
        "state": "demo",
        "username": user,
        "password": password,
    }, form=True)
    if status != 302:
        raise SystemExit(f"login {user} status {status} {raw.decode()[:300]}")
    location = header(hdrs, "Location")
    query = urllib.parse.parse_qs(urllib.parse.urlparse(location).query)
    code = query.get("code", [""])[0]
    if not code or query.get("state", [""])[0] != "demo":
        raise SystemExit(f"login {user} location {location}")
    status, _, raw = req("POST", "/token", {
        "grant_type": "authorization_code",
        "client_id": client_id,
        "redirect_uri": "http://127.0.0.1:9/cb",
        "code": code,
        "code_verifier": VERIFIER,
    }, form=True)
    tokens = json.loads(raw)
    if status != 200 or tokens.get("expires_in") != 3600:
        raise SystemExit(f"token {user} status {status} {tokens}")
    access = tokens["access_token"]
    if access.count(".") != 2:
        raise SystemExit("access token is not a JWT")
    return access, tokens["refresh_token"]

status, hdrs, _ = req("GET", "/mcp")
www = header(hdrs, "Www-Authenticate")
if status != 401 or "resource_metadata" not in www:
    raise SystemExit(f"unauthenticated /mcp status {status} www {www}")

status, _, raw = req("GET", "/.well-known/oauth-authorization-server")
meta = json.loads(raw)
if status != 200 or not meta["registration_endpoint"].endswith("/register") or "S256" not in meta["code_challenge_methods_supported"]:
    raise SystemExit(f"metadata {status} {meta}")
status, _, raw = req("GET", "/.well-known/oauth-protected-resource")
protected = json.loads(raw)
if status != 200 or not str(protected.get("resource", "")).endswith("/mcp"):
    raise SystemExit(f"protected resource {protected}")

status, _, raw = req("POST", "/register", {"redirect_uris": ["http://127.0.0.1:9/cb"], "client_name": "demo"})
reg = json.loads(raw)
if status != 201 or reg.get("token_endpoint_auth_method") != "none":
    raise SystemExit(f"register {status} {reg}")
client_id = reg["client_id"]

access, refresh = bearer(client_id, "analyst", "analyst-secret")
init, session = mcp(access, 1, "initialize", {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "demo", "version": "0"}})
if "protocolVersion" not in init.get("result", {}) and "serverInfo" not in init.get("result", {}):
    raise SystemExit(f"initialize {init}")
listed, _ = mcp(access, 2, "tools/list", {}, session)
names = [t.get("name") for t in listed.get("result", {}).get("tools", [])]
for name in ["list_schemas", "list_tables", "search_data", "describe_table", "validate_sql", "explain", "run_sql", "semantic_ops_help", "list_semantic_functions"]:
    if name not in names:
        raise SystemExit(f"missing tool {name} in {names}")

schemas, _ = mcp(access, 3, "tools/call", {"name": "list_schemas", "arguments": {}}, session)
schema_text = tool_text(schemas)
if "public" not in schema_text or "rvbbit" not in schema_text:
    raise SystemExit(schema_text)
notes, _ = mcp(access, 4, "tools/call", {"name": "run_sql", "arguments": {"sql": "SELECT body FROM notes ORDER BY 1"}}, session)
note_text = tool_text(notes)
if "a-secret" not in note_text or "i-secret" in note_text:
    raise SystemExit(note_text)
denied, _ = mcp(access, 5, "tools/call", {"name": "run_sql", "arguments": {"sql": "INSERT INTO notes (owner, body) VALUES ('analyst', 'nope')"}}, session)
if "read-only" not in json.dumps(denied).lower() and "read only" not in json.dumps(denied).lower():
    raise SystemExit(json.dumps(denied))
plan, _ = mcp(access, 6, "tools/call", {"name": "explain", "arguments": {"sql": "SELECT body FROM notes"}}, session)
plan_text = tool_text(plan).lower()
if "seq scan" not in plan_text and "result" not in plan_text:
    raise SystemExit(plan_text)
help_, _ = mcp(access, 7, "tools/call", {"name": "semantic_ops_help", "arguments": {}}, session)
if "MEANS" not in tool_text(help_):
    raise SystemExit(tool_text(help_))
checked, _ = mcp(access, 8, "tools/call", {"name": "validate_sql", "arguments": {"sql": "SELECT body FROM notes"}}, session)
if '"ok":true' not in tool_text(checked).replace(" ", "") and '"ok": true' not in tool_text(checked):
    raise SystemExit(tool_text(checked))

intern, _ = bearer(client_id, "intern", "intern-secret")
_, intern_session = mcp(intern, 1, "initialize", {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "intern", "version": "0"}})
intern_notes, _ = mcp(intern, 2, "tools/call", {"name": "run_sql", "arguments": {"sql": "SELECT body FROM notes"}}, intern_session)
intern_text = tool_text(intern_notes)
if "i-secret" not in intern_text or "a-secret" in intern_text:
    raise SystemExit(intern_text)

broke, _ = bearer(client_id, "broke", "broke-secret")
_, broke_session = mcp(broke, 1, "initialize", {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "broke", "version": "0"}})
budget, _ = mcp(broke, 2, "tools/call", {"name": "run_sql", "arguments": {"sql": "SELECT 1"}}, broke_session)
if "budget" not in json.dumps(budget).lower():
    raise SystemExit(json.dumps(budget))

status, _, raw = req("POST", "/token", {"grant_type": "refresh_token", "client_id": client_id, "refresh_token": refresh}, form=True)
refreshed = json.loads(raw)
if status != 200 or refreshed.get("access_token", "").count(".") != 2:
    raise SystemExit(f"refresh {status} {refreshed}")

env = os.environ.copy()
env["RVBBIT_PG_CONNINFO"] = f"host={pghost} port={pgport} dbname=postgres user=rvbbit_auth"
env["RVBBIT_MCP_JWT_SECRET"] = "inbound-demo-secret"
issued = subprocess.run(
    [binary, "--issue-key", "analyst", "claude"],
    env=env,
    check=False,
    capture_output=True,
    text=True,
)
if issued.returncode != 0 or not issued.stdout.strip().startswith("rvb_"):
    raise SystemExit(issued.stderr.strip() or "issue-key did not print a key")
api_key = issued.stdout.strip()
_, key_session = mcp(api_key, 1, "initialize", {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "key", "version": "0"}})
keyed, _ = mcp(api_key, 2, "tools/call", {"name": "list_schemas", "arguments": {}}, key_session)
if "public" not in tool_text(keyed):
    raise SystemExit(tool_text(keyed))
print("oauth and tools ok")
PY

AUDIT=$("${PSQL[@]}" -tA -c "SELECT count(*) FROM rvbbit.mcp_audit")
echo "audit rows=$AUDIT"
test "$AUDIT" -ge 5
SUBJECTS=$("${PSQL[@]}" -tA -c "SELECT string_agg(DISTINCT subject, ',') FROM rvbbit.mcp_audit")
echo "subjects=$SUBJECTS"
case "$SUBJECTS" in
  *analyst*) ;;
  *) echo "audit missing analyst" >&2; exit 1 ;;
esac
case "$SUBJECTS" in
  *intern*) ;;
  *) echo "audit missing intern" >&2; exit 1 ;;
esac
CODES=$("${PSQL[@]}" -tA -c "SELECT count(*) FROM rvbbit.oauth_code")
echo "spent codes=$CODES"
test "$CODES" = "0"
REFRESH=$("${PSQL[@]}" -tA -c "SELECT count(*) FROM rvbbit.oauth_refresh")
echo "refresh rows=$REFRESH"
test "$REFRESH" -ge 1

echo "mcp inbound demo ok"
