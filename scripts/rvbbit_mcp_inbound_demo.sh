#!/usr/bin/env bash
# Inbound MCP warehouse: OAuth 2.1 (PKCE + dynamic client registration),
# role mapping, RLS, read-only, a zero budget, the audit table, and
# OIDC login against a local mock issuer (not Google).
# Postgres 55463, hutch-mcp --inbound 8114, mock issuer 8124.
# The spike on 55432 / 8099 is not used, and neither is the outbound
# demo (55461 / 8112). No API key, client secret, or token is printed.
set -euo pipefail

unset PGPORT HUTCH_PORT RVBBIT_HUTCH_URL
ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

PG_BIN=${PG_BIN:-/usr/lib/postgresql/18/bin}
PGPORT=${RVBBIT_MCP_INBOUND_PGPORT:-55463}
HUTCH_MCP_INBOUND_PORT=${HUTCH_MCP_INBOUND_PORT:-8114}
OIDC_PORT=${RVBBIT_OIDC_PORT:-8124}
OIDC_CLIENT_ID=rvbbit-demo
OIDC_SECRET=demo-oidc-secret
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

if [[ "$PGPORT" == "55432" || "$HUTCH_MCP_INBOUND_PORT" == "8099" || "$PGPORT" == "55461" || "$HUTCH_MCP_INBOUND_PORT" == "8112" || "$OIDC_PORT" == "55432" || "$OIDC_PORT" == "8099" || "$OIDC_PORT" == "55461" || "$OIDC_PORT" == "8112" || "$OIDC_PORT" == "55465" || "$OIDC_PORT" == "8116" || "$OIDC_PORT" == "$PGPORT" || "$OIDC_PORT" == "$HUTCH_MCP_INBOUND_PORT" ]]; then
  echo "Refusing spike ports 55432 / 8099, the outbound demo ports 55461 / 8112, and a colliding OIDC port." >&2
  exit 1
fi

for cmd in gcc g++ curl initdb psql cargo python3 openssl; do
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
if port_open "$OIDC_PORT"; then
  echo "127.0.0.1:$OIDC_PORT is already in use." >&2
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
  if [[ -n "${OIDC_PID:-}" ]]; then
    kill "$OIDC_PID" >/dev/null 2>&1 || true
    wait "$OIDC_PID" 2>/dev/null || true
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
    if ! port_open "$HUTCH_MCP_INBOUND_PORT" && ! port_open "$OIDC_PORT" && [[ ! -S "$PGHOST/.s.PGSQL.$PGPORT" ]]; then
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
CREATE ROLE writer LOGIN PASSWORD 'writer-secret' NOSUPERUSER;
GRANT analyst, intern, broke, writer TO rvbbit_auth;
SQL
"${PSQL[@]}" -f "$ROOT/crates/rvbbit/hutch_mcp/sql/bootstrap.sql"
"${PSQL[@]}" <<'SQL'
INSERT INTO rvbbit.users (role, max_cost) VALUES ('analyst', NULL), ('intern', NULL), ('broke', 0), ('writer', NULL);
INSERT INTO rvbbit.oidc_role (claim, value, role, read_only) VALUES
  ('email', 'ada@example.com', 'analyst', true),
  ('email', 'writer@example.com', 'writer', false),
  ('hd', 'example.com', 'intern', true);
CREATE TABLE notes (owner text, body text);
ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
ALTER TABLE notes FORCE ROW LEVEL SECURITY;
CREATE POLICY own ON notes USING (owner = current_user);
GRANT SELECT ON notes TO analyst, intern, broke, writer;
GRANT INSERT ON notes TO writer;
INSERT INTO notes (owner, body) VALUES ('analyst', 'a-secret'), ('intern', 'i-secret');
SQL

echo "=== mock OIDC issuer on 127.0.0.1:$OIDC_PORT ==="
OIDC_LOG=$RUNDIR/oidc.log
cat >"$RUNDIR/oidc_issuer.py" <<'PY'
import base64, hashlib, json, os, subprocess, sys, threading, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

port = int(sys.argv[1])
key_path = sys.argv[2]
issuer = f"http://127.0.0.1:{port}"
client_id = os.environ["RVBBIT_OIDC_CLIENT_ID"]
client_secret = os.environ["RVBBIT_OIDC_CLIENT_SECRET"]
subprocess.check_call(
    ["openssl", "genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:2048", "-out", key_path],
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
)

def b64url(raw):
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode()

def s256(verifier):
    return b64url(hashlib.sha256(verifier.encode()).digest())

mod = subprocess.check_output(["openssl", "rsa", "-in", key_path, "-noout", "-modulus"], text=True, stderr=subprocess.DEVNULL)
n_bytes = bytes.fromhex(mod.strip().split("=", 1)[1]).lstrip(b"\x00") or b"\x00"
text = subprocess.check_output(["openssl", "rsa", "-in", key_path, "-noout", "-text"], text=True, stderr=subprocess.DEVNULL)
exp = 65537
for line in text.splitlines():
    if "Exponent" in line and "0x" in line:
        exp = int(line.split("0x", 1)[1].split(")")[0], 16)
        break
e_bytes = exp.to_bytes((exp.bit_length() + 7) // 8, "big")
N = b64url(n_bytes)
E = b64url(e_bytes)
lock = threading.Lock()
next_profile = {}
pending = {}

def sign(payload):
    header = b64url(json.dumps({"alg": "RS256", "kid": "test", "typ": "JWT"}, separators=(",", ":")).encode())
    body = b64url(json.dumps(payload, separators=(",", ":")).encode())
    sig = subprocess.check_output(["openssl", "dgst", "-sha256", "-sign", key_path], input=f"{header}.{body}".encode())
    return f"{header}.{body}.{b64url(sig)}"

class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, fmt, *args):
        return
    def _send(self, status, body, content_type="application/json", location=None):
        data = body.encode() if isinstance(body, str) else body
        self.send_response(status)
        self.send_header("content-type", content_type)
        self.send_header("content-length", str(len(data)))
        self.send_header("connection", "close")
        if location:
            self.send_header("location", location)
        self.end_headers()
        self.wfile.write(data)
    def do_GET(self):
        parsed = urlparse(self.path)
        if parsed.path == "/health":
            self._send(200, "ok", "text/plain")
            return
        if parsed.path == "/.well-known/openid-configuration":
            self._send(200, json.dumps({
                "issuer": issuer,
                "authorization_endpoint": f"{issuer}/authorize",
                "token_endpoint": f"{issuer}/token",
                "jwks_uri": f"{issuer}/jwks",
            }))
            return
        if parsed.path == "/jwks":
            self._send(200, json.dumps({"keys": [{"kty": "RSA", "kid": "test", "alg": "RS256", "n": N, "e": E}]}))
            return
        if parsed.path == "/authorize":
            q = parse_qs(parsed.query)
            with lock:
                profile = dict(next_profile)
            if not profile.get("email"):
                self._send(400, "no profile", "text/plain")
                return
            code = b64url(os.urandom(18))
            with lock:
                pending[code] = {
                    "nonce": (q.get("nonce") or [""])[0],
                    "challenge": (q.get("code_challenge") or [""])[0],
                    "redirect": (q.get("redirect_uri") or [""])[0],
                    "profile": profile,
                }
            redirect = (q.get("redirect_uri") or [""])[0]
            state = (q.get("state") or [""])[0]
            sep = "&" if "?" in redirect else "?"
            self._send(302, "", "text/plain", location=f"{redirect}{sep}code={code}&state={state}")
            return
        self._send(404, "not found", "text/plain")
    def do_POST(self):
        length = int(self.headers.get("content-length") or "0")
        raw = self.rfile.read(length)
        parsed = urlparse(self.path)
        if parsed.path == "/profile":
            try:
                profile = json.loads(raw.decode() or "{}")
            except json.JSONDecodeError:
                self._send(400, "bad profile", "text/plain")
                return
            kept = {
                "email": str(profile.get("email") or ""),
                "hd": profile.get("hd") or "",
                "verified": bool(profile.get("verified", True)),
                "bad_nonce": bool(profile.get("bad_nonce", False)),
            }
            with lock:
                next_profile.clear()
                next_profile.update(kept)
            self._send(200, json.dumps({"ok": True}))
            return
        if parsed.path == "/token":
            form = parse_qs(raw.decode())
            def one(name):
                return (form.get(name) or [""])[0]
            if one("client_id") != client_id or one("client_secret") != client_secret:
                self._send(400, json.dumps({"error": "invalid_client"}))
                return
            code = one("code")
            with lock:
                row = pending.pop(code, None)
            if row is None or s256(one("code_verifier")) != row["challenge"] or one("redirect_uri") != row["redirect"]:
                self._send(400, json.dumps({"error": "invalid_grant"}))
                return
            profile = row["profile"]
            now = int(time.time())
            payload = {
                "iss": issuer,
                "aud": client_id,
                "exp": now + 600,
                "iat": now,
                "nonce": "bad-nonce" if profile.get("bad_nonce") else row["nonce"],
                "email": profile["email"],
                "email_verified": profile["verified"],
            }
            if profile.get("hd"):
                payload["hd"] = profile["hd"]
            self._send(200, json.dumps({"id_token": sign(payload), "token_type": "Bearer", "expires_in": 3600}))
            return
        self._send(404, "not found", "text/plain")

print(f"oidc mock listening on {issuer}", flush=True)
ThreadingHTTPServer(("127.0.0.1", port), Handler).serve_forever()
PY
env -u PGPORT -u HUTCH_PORT \
  RVBBIT_OIDC_CLIENT_ID="$OIDC_CLIENT_ID" \
  RVBBIT_OIDC_CLIENT_SECRET="$OIDC_SECRET" \
  python3 "$RUNDIR/oidc_issuer.py" "$OIDC_PORT" "$RUNDIR/oidc.pem" \
  >"$OIDC_LOG" 2>&1 &
OIDC_PID=$!
ready=0
for _ in $(seq 1 50); do
  if curl -sf "http://127.0.0.1:${OIDC_PORT}/health" >/dev/null; then
    ready=1
    break
  fi
  sleep 0.1
done
if [[ "$ready" != 1 ]]; then
  echo "OIDC issuer did not listen. Log: $OIDC_LOG" >&2
  exit 1
fi

echo "=== hutch-mcp --inbound on $BASE ==="
: >"$MCP_LOG"
env -u PGPORT -u HUTCH_PORT \
  RVBBIT_PG_CONNINFO="host=${PGHOST} port=${PGPORT} dbname=postgres user=rvbbit_auth" \
  RVBBIT_PG_PASSWORD_HOST=127.0.0.1 \
  RVBBIT_PG_PASSWORD_PORT="$PGPORT" \
  RVBBIT_PG_PASSWORD_DB=postgres \
  RVBBIT_MCP_JWT_SECRET=inbound-demo-secret \
  RVBBIT_MCP_PUBLIC_URL="$BASE" \
  RVBBIT_OIDC_ISSUER="http://127.0.0.1:${OIDC_PORT}" \
  RVBBIT_OIDC_CLIENT_ID="$OIDC_CLIENT_ID" \
  RVBBIT_OIDC_CLIENT_SECRET="$OIDC_SECRET" \
  RVBBIT_OIDC_DOMAINS=example.com \
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

python3 - "$BASE" "$HUTCH_BIN" "$PGHOST" "$PGPORT" "http://127.0.0.1:${OIDC_PORT}" <<'PY'
import json, os, subprocess, sys, urllib.error, urllib.parse, urllib.request

base, binary, pghost, pgport, mock = sys.argv[1:]
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

def safe(text):
    text = text if isinstance(text, str) else text.decode(errors="replace")
    if "eyJ" in text or "client_secret" in text or "demo-oidc-secret" in text:
        return "<redacted>"
    return text[:300]

def raw_req(method, url, body=None, headers=None, form=False):
    data = None
    hdrs = dict(headers or {})
    if body is not None:
        if form:
            data = urllib.parse.urlencode(body).encode()
            hdrs["content-type"] = "application/x-www-form-urlencoded"
        else:
            data = json.dumps(body).encode()
            hdrs.setdefault("content-type", "application/json")
    request = urllib.request.Request(url, data=data, headers=hdrs, method=method)
    try:
        with OPENER.open(request, timeout=15) as resp:
            return resp.status, resp.headers, resp.read()
    except urllib.error.HTTPError as err:
        return err.code, err.headers, err.read()

def req(method, path, body=None, headers=None, form=False):
    return raw_req(method, base + path, body, headers, form)

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

def sql_text(token, session, sql, id_):
    value, _ = mcp(token, id_, "tools/call", {"name": "run_sql", "arguments": {"sql": sql}}, session)
    return value

def oidc_round(profile, client_state):
    status, _, raw = raw_req("POST", mock + "/profile", profile)
    if status != 200:
        raise SystemExit(f"profile {status} {safe(raw)}")
    page_q = urllib.parse.urlencode({
        "response_type": "code",
        "client_id": client_id,
        "redirect_uri": "http://127.0.0.1:9/cb",
        "code_challenge": CHALLENGE,
        "code_challenge_method": "S256",
        "state": client_state,
    })
    status, _, page = req("GET", "/authorize?" + page_q)
    html = page.decode()
    if status != 200 or "Sign in with OIDC" not in html or 'name="password"' not in html or 'name="username"' not in html:
        raise SystemExit(f"login page {status} {safe(html)}")
    href = html.split('href="', 1)[1].split('"', 1)[0]
    status, hdrs, body = req("GET", href)
    if status != 302:
        raise SystemExit(f"oidc start {status} {safe(body)}")
    loc = header(hdrs, "Location")
    if "code_challenge=" not in loc or "nonce=" not in loc or OIDC_SECRET_MARK in loc:
        raise SystemExit(f"issuer redirect {safe(loc)}")
    status, hdrs, body = raw_req("GET", loc)
    if status != 302:
        raise SystemExit(f"issuer authorize {status} {safe(body)}")
    status, hdrs, body = raw_req("GET", header(hdrs, "Location"))
    text = body.decode()
    if "eyJ" in text or OIDC_SECRET_MARK in text:
        raise SystemExit("callback leaked a token or secret")
    return status, text, header(hdrs, "Location")

OIDC_SECRET_MARK = "demo-oidc-secret"
writer_pw, _ = bearer(client_id, "writer", "writer-secret")
_, writer_pw_session = mcp(writer_pw, 1, "initialize", {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "writer-pw", "version": "0"}})
writer_ro = sql_text(writer_pw, writer_pw_session, "SELECT current_setting('transaction_read_only')", 2)
if '"on"' not in tool_text(writer_ro):
    raise SystemExit(safe(tool_text(writer_ro)))
writer_denied = sql_text(writer_pw, writer_pw_session, "INSERT INTO notes (owner, body) VALUES ('writer', 'pw')", 3)
if "read-only" not in json.dumps(writer_denied).lower():
    raise SystemExit(safe(json.dumps(writer_denied)))

def oidc_token(profile, client_state):
    status, text, location = oidc_round(profile, client_state)
    if status != 302:
        raise SystemExit(f"oidc login {status} {safe(text)}")
    query = urllib.parse.parse_qs(urllib.parse.urlparse(location).query)
    code = query.get("code", [""])[0]
    if not code or query.get("state", [""])[0] != client_state:
        raise SystemExit(f"oidc redirect {safe(location)}")
    status, _, raw = req("POST", "/token", {
        "grant_type": "authorization_code",
        "client_id": client_id,
        "redirect_uri": "http://127.0.0.1:9/cb",
        "code": code,
        "code_verifier": VERIFIER,
    }, form=True)
    tokens = json.loads(raw)
    if status != 200 or tokens.get("access_token", "").count(".") != 2:
        raise SystemExit(f"oidc token {status}")
    return tokens["access_token"]

ada = oidc_token({"email": "ada@example.com", "hd": "example.com", "verified": True}, "ada-state")
_, ada_session = mcp(ada, 1, "initialize", {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "ada", "version": "0"}})
ada_ro = sql_text(ada, ada_session, "SELECT current_setting('transaction_read_only')", 2)
if '"on"' not in tool_text(ada_ro):
    raise SystemExit(safe(tool_text(ada_ro)))
ada_notes = tool_text(sql_text(ada, ada_session, "SELECT body FROM notes", 3))
if "a-secret" not in ada_notes or "i-secret" in ada_notes:
    raise SystemExit(safe(ada_notes))
ada_denied = sql_text(ada, ada_session, "INSERT INTO notes (owner, body) VALUES ('analyst', 'oidc')", 4)
if "read-only" not in json.dumps(ada_denied).lower():
    raise SystemExit(safe(json.dumps(ada_denied)))

peer = oidc_token({"email": "peer@example.com", "hd": "example.com", "verified": True}, "hd-state")
_, peer_session = mcp(peer, 1, "initialize", {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "peer", "version": "0"}})
peer_notes = tool_text(sql_text(peer, peer_session, "SELECT body FROM notes", 2))
if "i-secret" not in peer_notes or "a-secret" in peer_notes:
    raise SystemExit(safe(peer_notes))

status, text, _ = oidc_round({"email": "stranger@example.com", "verified": True}, "stranger-state")
if status != 400 or "no role mapping" not in text or "stranger@" in text:
    raise SystemExit(f"stranger {status} {safe(text)}")
status, text, _ = oidc_round({"email": "foreign@other.test", "verified": True}, "foreign-state")
if status != 400 or "not allowed" not in text or "foreign@" in text:
    raise SystemExit(f"foreign {status} {safe(text)}")
status, text, _ = oidc_round({"email": "ada@example.com", "hd": "example.com", "verified": False}, "unverified-state")
if status != 400 or "not verified" not in text:
    raise SystemExit(f"unverified {status} {safe(text)}")
status, text, _ = oidc_round({"email": "ada@example.com", "hd": "example.com", "verified": True, "bad_nonce": True}, "nonce-state")
if status != 400 or "nonce" not in text:
    raise SystemExit(f"nonce {status} {safe(text)}")

writer = oidc_token({"email": "writer@example.com", "hd": "example.com", "verified": True}, "writer-state")
_, writer_session = mcp(writer, 1, "initialize", {"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "writer", "version": "0"}})
writer_flag = tool_text(sql_text(writer, writer_session, "SELECT current_setting('transaction_read_only')", 2))
if '"off"' not in writer_flag:
    raise SystemExit(safe(writer_flag))
inserted = sql_text(writer, writer_session, "INSERT INTO notes (owner, body) VALUES ('writer', 'w-secret')", 3)
if inserted.get("result", {}).get("isError") is True:
    raise SystemExit(safe(json.dumps(inserted)))
wrote = tool_text(sql_text(writer, writer_session, "SELECT body FROM notes", 4))
if "w-secret" not in wrote or "a-secret" in wrote:
    raise SystemExit(safe(wrote))
blocked = sql_text(writer, writer_session, "SET ROLE postgres", 5)
if "SET ROLE" not in tool_text(blocked):
    raise SystemExit(safe(tool_text(blocked)))
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
case "$SUBJECTS" in
  *ada@example.com*) ;;
  *) echo "audit missing oidc email" >&2; exit 1 ;;
esac
if grep -F -q "$OIDC_SECRET" "$MCP_LOG" || grep -F -q "$OIDC_SECRET" "$OIDC_LOG"; then
  echo "OIDC client secret appeared in a server log" >&2
  exit 1
fi
if grep -q "id_token" "$MCP_LOG"; then
  echo "id_token appeared in the inbound server log" >&2
  exit 1
fi
CODES=$("${PSQL[@]}" -tA -c "SELECT count(*) FROM rvbbit.oauth_code")
echo "spent codes=$CODES"
test "$CODES" = "0"
REFRESH=$("${PSQL[@]}" -tA -c "SELECT count(*) FROM rvbbit.oauth_refresh")
echo "refresh rows=$REFRESH"
test "$REFRESH" -ge 1

echo "mcp inbound demo ok"
