//! Warehouse tools. Every call is a transaction on the authenticator
//! pool: `SET LOCAL ROLE`, read-only, statement timeout, and the role's
//! `semantic.max_cost`. GRANTs and RLS do the enforcement.

use std::time::Instant;

use mcp_core::{quote_ident, PgRole, SessionPreamble};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, RoleServer, ServerHandler};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use super::oauth::{sha256_hex, Identity, Oauth};
use super::pg::{quote_lit, Pool};

const ROW_CAP: i64 = 200;
const HELP: &str = "\
Native semantic SQL, without a gram.y splice.

MEANS and ABOUT are infix operators. `x MEANS 'is it a bird?'` is a predicate. \
`x ABOUT 'migratory birds'` is cosine similarity of the two embeddings; the default cutoff is 0.30. \
`rvbbit.semantic_similarity(a, b)` returns the cosine.

CLASSIFY(expr INTO 'a', 'b') or CLASSIFY(expr AS ('a', 'b')) returns the label. \
CLASSIFY(expr AS enumname) uses that enum. ON ERROR literal is the fallback; omitted means NULL.

EXTRACT(kind FROM expr) is a typed extract for int, float, numeric, bool, and text. \
A custom kind needs ON ERROR, which datetime EXTRACT does not accept. \
A scalar extract that finds nothing is SQL NULL.

User functions: CREATE SEMANTIC FUNCTION name(arg) RETURNS bool|score|text|label \
AS 'template {{arg}}' USING MODEL name DEFAULT CLAUSES (...). \
`|> name` after an expression calls `rvbbit.name`. A bare identifier after the call is a column alias.

EXPLAIN prints Semantic Provider Calls, Semantic HTTP Batches, Semantic USD, \
Semantic Latency, and Semantic Model, and the same lines for an MCP tool \
(`MCP Provider Calls`, `MCP USD`, `MCP Server`, `MCP Tool`). \
`semantic.max_cost` and a clause BUDGET reject an estimate above the cap before the provider is called.

Inbound `run_sql` is read-only. A role whose max_cost is 0 cannot run it. \
A positive max_cost is SET LOCAL semantic.max_cost and also rejects a plan whose total USD is above it. \
Only SELECT, WITH, VALUES, TABLE, and EXPLAIN of those are accepted.\
";

type ToolOut = Result<rmcp::model::CallToolResult, rmcp::ErrorData>;

pub struct Warehouse {
    oauth: std::sync::Arc<Oauth>,
    stdio_bearer: Option<String>,
}

impl Warehouse {
    pub fn new(oauth: std::sync::Arc<Oauth>, stdio_bearer: Option<String>) -> Self {
        Self { oauth, stdio_bearer }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// Schema name. Empty lists every non-system schema.
    #[serde(default)]
    pub schema: Option<String>,
    /// Case-insensitive substring of the schema or table name.
    #[serde(default)]
    pub q: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DescribeArgs {
    pub schema: String,
    pub table: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SqlArgs {
    pub sql: String,
    /// Row cap for a SELECT. Clamped to 1..=1000. Default 200.
    #[serde(default)]
    pub limit: Option<i64>,
}

#[tool_router(server_handler)]
impl Warehouse {
    #[tool(description = "List non-system schemas the current role can see")]
    async fn list_schemas(
        &self,
        ctx: rmcp::service::RequestContext<RoleServer>,
    ) -> ToolOut {
        self.finish(&ctx, "list_schemas", None, |_id, conn| {
            let rows = conn.query(
                "SELECT nspname FROM pg_namespace \
                 WHERE nspname NOT LIKE 'pg\\_%' AND nspname <> 'information_schema' \
                 ORDER BY 1",
            )?;
            Ok((json_rows(&rows).to_string(), rows.rows.len() as i64, 0.0, 0.0))
        })
        .await
    }

    #[tool(description = "List tables with planner stats and a three-row sample")]
    async fn list_tables(
        &self,
        ctx: rmcp::service::RequestContext<RoleServer>,
        Parameters(args): Parameters<SearchArgs>,
    ) -> ToolOut {
        self.search(&ctx, "list_tables", args).await
    }

    #[tool(description = "Search tables by name and return stats plus a sample")]
    async fn search_data(
        &self,
        ctx: rmcp::service::RequestContext<RoleServer>,
        Parameters(args): Parameters<SearchArgs>,
    ) -> ToolOut {
        self.search(&ctx, "search_data", args).await
    }

    #[tool(description = "Describe one table: columns, stats, and a sample")]
    async fn describe_table(
        &self,
        ctx: rmcp::service::RequestContext<RoleServer>,
        Parameters(args): Parameters<DescribeArgs>,
    ) -> ToolOut {
        self.finish(&ctx, "describe_table", None, move |id, conn| {
            let schema = checked_ident(&args.schema)?;
            let table = checked_ident(&args.table)?;
            let rel = format!("{}.{}", quote_ident(&schema), quote_ident(&table));
            let cols = conn.query(&format!(
                "SELECT a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod), \
                        CASE WHEN a.attnotnull THEN 'NO' ELSE 'YES' END \
                 FROM pg_attribute a \
                 WHERE a.attrelid = {}::regclass AND a.attnum > 0 AND NOT a.attisdropped \
                 ORDER BY a.attnum",
                quote_lit(&format!("{schema}.{table}"))
            ))?;
            let stats = conn.query(&format!(
                "SELECT c.reltuples::bigint::text, pg_size_pretty(pg_total_relation_size(c.oid)) \
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = {} AND c.relname = {}",
                quote_lit(&schema),
                quote_lit(&table)
            ))?;
            let sample = conn.query(&format!("SELECT * FROM {rel} LIMIT 3"));
            let (sample_json, sample_error) = match &sample {
                Ok(rows) => (json_rows(rows), Value::Null),
                Err(message) => (Value::Null, Value::String(message.clone())),
            };
            let body = json!({
                "schema": schema,
                "table": table,
                "columns": json_rows(&cols),
                "stats": json_rows(&stats),
                "sample": sample_json,
                "sample_error": sample_error,
            });
            let _ = id;
            Ok((body.to_string(), cols.rows.len() as i64, 0.0, 0.0))
        })
        .await
    }

    #[tool(description = "Plan SQL and return ok plus the planner text, including semantic and MCP estimates")]
    async fn validate_sql(
        &self,
        ctx: rmcp::service::RequestContext<RoleServer>,
        Parameters(args): Parameters<SqlArgs>,
    ) -> ToolOut {
        self.explain_tool(&ctx, "validate_sql", &args.sql, true).await
    }

    #[tool(description = "EXPLAIN SQL, including semantic and MCP call and cost estimates")]
    async fn explain(
        &self,
        ctx: rmcp::service::RequestContext<RoleServer>,
        Parameters(args): Parameters<SqlArgs>,
    ) -> ToolOut {
        self.explain_tool(&ctx, "explain", &args.sql, false).await
    }

    #[tool(description = "Run one read-only statement. Row cap, timeout, and semantic.max_cost come from the token")]
    async fn run_sql(
        &self,
        ctx: rmcp::service::RequestContext<RoleServer>,
        Parameters(args): Parameters<SqlArgs>,
    ) -> ToolOut {
        let sql = args.sql;
        let limit = args.limit.unwrap_or(ROW_CAP).clamp(1, 1000);
        self.finish(&ctx, "run_sql", Some(sql.clone()), move |id, conn| {
            if id.max_cost == Some(0.0) {
                return Err("semantic estimate exceeds budget 0".into());
            }
            one_statement(&sql)?;
            read_only_sql(&sql)?;
            if is_explain(&sql) {
                let rows = conn.query(sql.trim().trim_end_matches(';').trim())?;
                let plan = explain_lines(&rows);
                return Ok((plan.clone(), rows.rows.len() as i64, total_usd(&plan), semantic_calls(&plan)));
            }
            let statement = capped(&sql, limit);
            let plan = explain_text(conn, &statement)?;
            let usd = total_usd(&plan);
            let calls = semantic_calls(&plan);
            if let Some(cap) = id.max_cost {
                if usd > cap + 1e-12 {
                    return Err(format!("semantic estimate {usd:.8} USD exceeds budget {cap:.8}"));
                }
            }
            let rows = conn.query(&statement)?;
            Ok((json_rows(&rows).to_string(), rows.rows.len() as i64, usd, calls))
        })
        .await
    }

    #[tool(description = "Short grammar for MEANS, ABOUT, CLASSIFY, EXTRACT, and semantic functions")]
    async fn semantic_ops_help(
        &self,
        ctx: rmcp::service::RequestContext<RoleServer>,
    ) -> ToolOut {
        self.finish(&ctx, "semantic_ops_help", None, |_id, _conn| {
            Ok((json!({"help": HELP}).to_string(), 1, 0.0, 0.0))
        })
        .await
    }

    #[tool(description = "List functions in schema rvbbit")]
    async fn list_semantic_functions(
        &self,
        ctx: rmcp::service::RequestContext<RoleServer>,
    ) -> ToolOut {
        self.finish(&ctx, "list_semantic_functions", None, |_id, conn| {
            let rows = conn.query(
                "SELECT p.proname, pg_get_function_identity_arguments(p.oid) \
                 FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
                 WHERE n.nspname = 'rvbbit' ORDER BY 1",
            )?;
            Ok((json_rows(&rows).to_string(), rows.rows.len() as i64, 0.0, 0.0))
        })
        .await
    }
}

impl Warehouse {
    async fn search(
        &self,
        ctx: &rmcp::service::RequestContext<RoleServer>,
        tool: &'static str,
        args: SearchArgs,
    ) -> ToolOut {
        self.finish(ctx, tool, None, move |_id, conn| {
            let mut filter = String::from(
                "c.relkind = 'r' AND n.nspname NOT LIKE 'pg\\_%' AND n.nspname <> 'information_schema'",
            );
            if let Some(schema) = args.schema.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                let schema = checked_ident(schema)?;
                filter.push_str(&format!(" AND n.nspname = {}", quote_lit(&schema)));
            }
            if let Some(q) = args.q.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                filter.push_str(&format!(
                    " AND (n.nspname ILIKE {} OR c.relname ILIKE {})",
                    quote_lit(&format!("%{q}%")),
                    quote_lit(&format!("%{q}%"))
                ));
            }
            let tables = conn.query(&format!(
                "SELECT n.nspname, c.relname, c.reltuples::bigint::text, \
                        pg_size_pretty(pg_total_relation_size(c.oid)) \
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE {filter} ORDER BY 1, 2 LIMIT 50"
            ))?;
            let mut out = Vec::new();
            for row in &tables.rows {
                let schema = row.first().and_then(|c| c.clone()).unwrap_or_default();
                let table = row.get(1).and_then(|c| c.clone()).unwrap_or_default();
                let rel = format!("{}.{}", quote_ident(&schema), quote_ident(&table));
                let sample = conn.query(&format!("SELECT * FROM {rel} LIMIT 3"));
                out.push(json!({
                    "schema": schema,
                    "table": table,
                    "reltuples": row.get(2).and_then(|c| c.clone()),
                    "size": row.get(3).and_then(|c| c.clone()),
                    "sample": sample.as_ref().map(json_rows).unwrap_or(Value::Null),
                }));
            }
            Ok((json!(out).to_string(), out.len() as i64, 0.0, 0.0))
        })
        .await
    }

    async fn explain_tool(
        &self,
        ctx: &rmcp::service::RequestContext<RoleServer>,
        tool: &'static str,
        sql: &str,
        as_validate: bool,
    ) -> ToolOut {
        let sql = sql.to_string();
        self.finish(ctx, tool, Some(sql.clone()), move |_id, conn| {
            match one_statement(&sql).and_then(|_| read_only_sql(&sql)).and_then(|_| explain_text(conn, &sql)) {
                Ok(plan) => {
                    let usd = total_usd(&plan);
                    let calls = semantic_calls(&plan);
                    let body = if as_validate {
                        json!({"ok": true, "plan": plan, "usd": usd}).to_string()
                    } else {
                        plan
                    };
                    Ok((body, 1, usd, calls))
                }
                Err(message) if as_validate => {
                    Ok((json!({"ok": false, "error": message}).to_string(), 0, 0.0, 0.0))
                }
                Err(message) => Err(message),
            }
        })
        .await
    }

    async fn finish(
        &self,
        ctx: &rmcp::service::RequestContext<RoleServer>,
        tool: &'static str,
        sql: Option<String>,
        work: impl FnOnce(&Identity, &mut super::pg::Conn) -> Result<(String, i64, f64, f64), String> + Send + 'static,
    ) -> ToolOut {
        let oauth = self.oauth.clone();
        let token = match bearer_of(ctx, self.stdio_bearer.as_deref()) {
            Ok(token) => token,
            Err(message) => return Ok(tool_failure(message)),
        };
        let result = tokio::task::spawn_blocking(move || {
            let id = oauth.identity_for_bearer(&token)?;
            let started = Instant::now();
            let outcome = oauth.pool.with(|conn| in_role(conn, &id, |conn| work(&id, conn)));
            let elapsed = started.elapsed().as_millis() as i64;
            let (text, rows, cost, calls, err) = match outcome {
                Ok((text, rows, cost, calls)) => (Some(text), Some(rows), cost, calls, None),
                Err(message) => (None, None, 0.0, 0.0, Some(message)),
            };
            let hash = sql.as_deref().map(|s| sha256_hex(s.as_bytes()));
            let status = if err.is_some() { "error" } else { "ok" };
            let _ = audit(
                &oauth.pool,
                &id,
                tool,
                hash.as_deref(),
                elapsed,
                rows,
                cost,
                status,
                err.as_deref(),
                calls,
            );
            match err {
                Some(message) => Err(message),
                None => Ok(text.unwrap_or_default()),
            }
        })
        .await
        .map_err(|e| tool_err(e.to_string()))?;
        match result {
            Ok(text) => Ok(rmcp::model::CallToolResult::success(vec![rmcp::model::ContentBlock::text(text)])),
            Err(message) => Ok(tool_failure(message)),
        }
    }
}

fn in_role<T>(
    conn: &mut super::pg::Conn,
    id: &Identity,
    f: impl FnOnce(&mut super::pg::Conn) -> Result<T, String>,
) -> Result<T, String> {
    let role = PgRole {
        role: id.role.clone(),
        read_only: true,
        max_cost: id.max_cost.filter(|n| *n > 0.0),
        statement_timeout_ms: id.statement_timeout_ms,
    };
    conn.exec("BEGIN")?;
    if let Err(message) = conn.exec(&SessionPreamble::from_role(&role).sql()) {
        let _ = conn.exec("ROLLBACK");
        return Err(message);
    }
    let result = f(conn);
    if result.is_ok() {
        let _ = conn.exec("COMMIT");
    } else {
        let _ = conn.exec("ROLLBACK");
    }
    result
}

fn audit(
    pool: &Pool,
    id: &Identity,
    tool: &str,
    sql_hash: Option<&str>,
    duration_ms: i64,
    rows: Option<i64>,
    cost: f64,
    status: &str,
    error: Option<&str>,
    semantic_calls: f64,
) -> Result<(), String> {
    let hash_sql = match sql_hash {
        Some(hash) => quote_lit(hash),
        None => "NULL".into(),
    };
    let rows_sql = match rows {
        Some(n) => n.to_string(),
        None => "NULL".into(),
    };
    let error_sql = match error {
        Some(message) => quote_lit(message),
        None => "NULL".into(),
    };
    pool.with(|conn| {
        conn.exec(&format!(
            "INSERT INTO rvbbit.mcp_audit \
             (subject, role, tool, sql_hash, duration_ms, rows, cost, status, error, cost_usd, semantic_calls) \
             VALUES ({}, {}, {}, {hash_sql}, {duration_ms}, {rows_sql}, {cost}, {}, {error_sql}, {cost}, {semantic_calls})",
            quote_lit(&id.subject),
            quote_lit(&id.role),
            quote_lit(tool),
            quote_lit(status),
        ))
    })
}

fn explain_text(conn: &mut super::pg::Conn, sql: &str) -> Result<String, String> {
    let rows = conn.query(&format!("EXPLAIN {sql}"))?;
    Ok(explain_lines(&rows))
}

fn explain_lines(rows: &super::pg::Rows) -> String {
    let mut lines = Vec::new();
    for row in &rows.rows {
        if let Some(Some(line)) = row.first() {
            lines.push(line.clone());
        }
    }
    lines.join("\n")
}

fn total_usd(plan: &str) -> f64 {
    plan_metric(plan, "total usd")
}

fn semantic_calls(plan: &str) -> f64 {
    let semantic = plan_metric(plan, "semantic total provider calls");
    if semantic > 0.0 {
        semantic
    } else {
        plan_metric(plan, "mcp total provider calls")
    }
}

fn plan_metric(plan: &str, label: &str) -> f64 {
    let mut best: f64 = 0.0;
    for line in plan.lines() {
        let lower = line.to_ascii_lowercase();
        let Some(rest) = lower.split(label).nth(1) else {
            continue;
        };
        if let Some(num) = first_number(rest) {
            best = best.max(num);
        }
    }
    best
}

fn first_number(text: &str) -> Option<f64> {
    let start = text.find(|c: char| c.is_ascii_digit())?;
    let rest = &text[start..];
    let end = rest.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn json_rows(rows: &super::pg::Rows) -> Value {
    let out: Vec<Value> = rows
        .rows
        .iter()
        .map(|row| {
            let mut obj = serde_json::Map::new();
            for (i, col) in rows.columns.iter().enumerate() {
                let value = match row.get(i).and_then(|c| c.clone()) {
                    None => Value::Null,
                    Some(text) => Value::String(text),
                };
                obj.insert(col.clone(), value);
            }
            Value::Object(obj)
        })
        .collect();
    Value::Array(out)
}

fn checked_ident(name: &str) -> Result<String, String> {
    if super::oauth::ident_ok(name) {
        Ok(name.to_string())
    } else {
        Err(format!("{name} is not an identifier"))
    }
}

const READ_ONLY_SQL: &str = "only read-only SELECT/WITH/VALUES/EXPLAIN allowed";

/// Leading `--` and `/* */` comments, then the first keyword.
fn statement_head(sql: &str) -> (String, String) {
    let mut rest = sql.trim_start();
    loop {
        if let Some(tail) = rest.strip_prefix("--") {
            rest = tail.split_once('\n').map(|(_, right)| right).unwrap_or("").trim_start();
            continue;
        }
        if let Some(tail) = rest.strip_prefix("/*") {
            let Some(end) = tail.find("*/") else {
                return (String::new(), String::new());
            };
            rest = tail[end + 2..].trim_start();
            continue;
        }
        break;
    }
    let head = rest
        .split(|c: char| c.is_whitespace() || c == '(' || c == ';')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    (head, rest.to_string())
}

fn is_explain(sql: &str) -> bool {
    statement_head(sql).0 == "explain"
}

fn command_after_explain(rest: &str) -> String {
    let mut body = rest.trim_start();
    let head = body
        .split(|c: char| c.is_whitespace() || c == '(' || c == ';')
        .next()
        .unwrap_or("");
    if head.eq_ignore_ascii_case("explain") {
        body = body[head.len()..].trim_start();
    }
    if body.starts_with('(') {
        let Some(end) = body.find(')') else {
            return String::new();
        };
        body = body[end + 1..].trim_start();
    }
    loop {
        let word = body
            .split(|c: char| c.is_whitespace() || c == '(' || c == ';')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(word.as_str(), "analyze" | "analyse" | "verbose") {
            body = body[word.len()..].trim_start();
            continue;
        }
        return word;
    }
}

fn read_only_sql(sql: &str) -> Result<(), String> {
    let (head, rest) = statement_head(sql);
    let command = if head == "explain" { command_after_explain(&rest) } else { head };
    if matches!(command.as_str(), "select" | "with" | "values" | "table" | "explain") {
        Ok(())
    } else {
        Err(READ_ONLY_SQL.into())
    }
}

fn one_statement(sql: &str) -> Result<(), String> {
    if sql.contains('\0') {
        return Err("sql contains a nul byte".into());
    }
    let mut chars = sql.chars().peekable();
    let mut in_single = false;
    let mut dollar: Option<String> = None;
    while let Some(c) = chars.next() {
        if let Some(tag) = dollar.clone() {
            if c == '$' && chars.clone().take(tag.len()).collect::<String>() == tag {
                for _ in 0..tag.len() {
                    chars.next();
                }
                dollar = None;
            }
            continue;
        }
        if in_single {
            if c == '\'' {
                if chars.peek() == Some(&'\'') {
                    chars.next();
                } else {
                    in_single = false;
                }
            }
            continue;
        }
        match c {
            '\'' => in_single = true,
            '$' => {
                let mut tag = String::new();
                let mut look = chars.clone();
                loop {
                    match look.next() {
                        Some(ch) if ch.is_ascii_alphanumeric() || ch == '_' => tag.push(ch),
                        Some('$') => {
                            dollar = Some(tag);
                            for _ in 0..dollar.as_ref().map(|t| t.len()).unwrap_or(0) + 1 {
                                chars.next();
                            }
                            break;
                        }
                        _ => break,
                    }
                }
            }
            ';' => {
                let rest = chars.clone().collect::<String>();
                if rest.trim().is_empty() {
                    return Ok(());
                }
                return Err("only one statement is allowed".into());
            }
            _ => {}
        }
    }
    Ok(())
}

fn capped(sql: &str, limit: i64) -> String {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let head = trimmed.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
    if matches!(head.as_str(), "select" | "with" | "values" | "table") {
        format!("SELECT * FROM ({trimmed}) AS rvbbit_mcp_q LIMIT {limit}")
    } else {
        trimmed.to_string()
    }
}

fn bearer_of(ctx: &rmcp::service::RequestContext<RoleServer>, stdio: Option<&str>) -> Result<String, String> {
    if let Some(parts) = ctx.extensions.get::<http::request::Parts>() {
        if let Some(value) = parts.headers.get(http::header::AUTHORIZATION) {
            return value.to_str().map(|s| s.to_string()).map_err(|_| "authorization header is not utf-8".into());
        }
        return Err("missing bearer token".into());
    }
    stdio.map(|s| s.to_string()).ok_or_else(|| "missing bearer token".into())
}

fn tool_failure(message: String) -> rmcp::model::CallToolResult {
    rmcp::model::CallToolResult::error(vec![rmcp::model::ContentBlock::text(message)])
}

fn tool_err(message: String) -> rmcp::ErrorData {
    rmcp::ErrorData::internal_error(message, None)
}

pub fn bootstrap_sql() -> &'static str {
    include_str!("../../sql/bootstrap.sql")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_sql_rejects_ddl_and_set_role() {
        assert!(read_only_sql("SELECT 1").is_ok());
        assert!(read_only_sql("  -- note\nWITH t AS (SELECT 1) SELECT * FROM t").is_ok());
        assert!(read_only_sql("VALUES (1)").is_ok());
        assert!(read_only_sql("TABLE notes").is_ok());
        assert!(read_only_sql("EXPLAIN SELECT 1").is_ok());
        assert!(read_only_sql("EXPLAIN (ANALYZE, VERBOSE) SELECT 1").is_ok());
        assert!(read_only_sql("EXPLAIN ANALYZE SELECT 1").is_ok());
        let drop = read_only_sql("DROP TABLE notes").unwrap_err();
        assert_eq!(drop, READ_ONLY_SQL);
        assert_eq!(read_only_sql("SET ROLE postgres").unwrap_err(), READ_ONLY_SQL);
        assert_eq!(read_only_sql("/* hide */ INSERT INTO notes VALUES (1)").unwrap_err(), READ_ONLY_SQL);
        assert_eq!(read_only_sql("EXPLAIN DROP TABLE notes").unwrap_err(), READ_ONLY_SQL);
        assert_eq!(read_only_sql("EXPLAIN ANALYZE DELETE FROM notes").unwrap_err(), READ_ONLY_SQL);
    }
}

// ServerHandler is emitted by tool_router. This keeps the trait in scope for readers.
#[allow(dead_code)]
fn _handler_bound<T: ServerHandler>() {}
