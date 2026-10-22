//! `mcp.server` and `mcp.tool`. SQL functions are ordinary `LANGUAGE sql`
//! bodies that call `rvbbit.mcp_invoke_text`. Dropping a server does not
//! create the rvbbit schema: [`mcp_try_drop`] only looks.

use std::collections::{BTreeMap, BTreeSet};

use mcp_core::{
    json_names, server_from_options, sql_args, sql_ident, tool_fn_body, ArgSpec, ListedTool,
    ServerSpec,
};
use rvbbit_parse::Command;
use types_error::PgResult;

use super::{cell, cell_opt, exec, pg, qident, qlit, query, scalar};

const SERVER_DDL: &str = "CREATE TABLE IF NOT EXISTS mcp.server (
    name text PRIMARY KEY,
    transport text NOT NULL,
    url text,
    command text,
    args text,
    headers text,
    env text,
    timeout_ms int,
    retries int)";

const TOOL_DDL: &str = "CREATE TABLE IF NOT EXISTS mcp.tool (
    nsp text NOT NULL,
    name text NOT NULL,
    server text NOT NULL REFERENCES mcp.server(name) ON DELETE CASCADE,
    tool text NOT NULL,
    schema_hash text NOT NULL,
    input_schema text,
    output_schema text,
    argkeys text,
    argtypes text,
    returns_table boolean NOT NULL DEFAULT false,
    return_cols text,
    timeout_ms int,
    cache_ttl_ms int,
    max_cost float8,
    read_only boolean NOT NULL DEFAULT false,
    procoid oid,
    PRIMARY KEY (nsp, name))";

pub fn ensure() -> PgResult<()> {
    exec("CREATE SCHEMA IF NOT EXISTS mcp")?;
    exec(SERVER_DDL)?;
    exec(TOOL_DDL)?;
    if super::scalar(
        "SELECT 1 FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
         WHERE n.nspname = 'rvbbit' AND p.proname = 'mcp_invoke_text' AND p.pronargs = 5",
    )?
    .is_none()
    {
        exec(
            "CREATE FUNCTION rvbbit.mcp_invoke_text(nsp text, name text, args text, use_fallback boolean, fallback text) \
             RETURNS text LANGUAGE internal VOLATILE PARALLEL UNSAFE AS 'rvbbit_mcp_invoke_text'",
        )?;
    }
    Ok(())
}

pub fn load() -> PgResult<()> {
    let have = scalar(
        "SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'mcp' AND c.relname = 'tool'",
    )?;
    if have.is_none() {
        rvbbit_resolve::install_mcp_tools(Vec::new());
        return Ok(());
    }
    let rows = query(
        "SELECT t.nsp, t.name, t.server, t.tool, COALESCE(t.procoid::text, '0'), \
                COALESCE(t.argtypes, '[]'), COALESCE(t.argkeys, '[]'), \
                t.returns_table::text, COALESCE(t.read_only::text, 'f'), \
                COALESCE(t.timeout_ms::text, ''), COALESCE(t.cache_ttl_ms::text, '0'), \
                t.max_cost::text, \
                s.transport, s.url, s.command, COALESCE(s.args, '[]'), \
                COALESCE(s.headers, '{}'), COALESCE(s.env, '{}'), \
                COALESCE(s.retries, 2)::text, COALESCE(s.timeout_ms, 30000)::text \
         FROM mcp.tool t JOIN mcp.server s ON s.name = t.server",
    )?;
    let mut tools = Vec::with_capacity(rows.len());
    for row in rows {
        let procoid: u32 = cell(&row, 4).parse().unwrap_or(0);
        if procoid == 0 {
            continue;
        }
        let timeout_ms = cell_opt(&row, 9)
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| cell(&row, 19).parse().unwrap_or(30_000));
        tools.push(rvbbit_resolve::McpTool {
            nsp: cell(&row, 0),
            name: cell(&row, 1),
            server: cell(&row, 2),
            tool: cell(&row, 3),
            procoid,
            arg_types: json_strings(&cell(&row, 5)),
            arg_keys: json_strings(&cell(&row, 6)),
            returns_table: is_true(&cell(&row, 7)),
            read_only: is_true(&cell(&row, 8)),
            timeout_ms,
            cache_ttl_ms: cell(&row, 10).parse().unwrap_or(0),
            max_cost: cell_opt(&row, 11).and_then(|s| s.parse().ok()),
            transport: cell(&row, 12),
            url: cell_opt(&row, 13),
            command: cell_opt(&row, 14),
            args: json_strings(&cell(&row, 15)),
            headers_json: cell(&row, 16),
            env_json: cell(&row, 17),
            retries: cell(&row, 18).parse().unwrap_or(2),
        });
    }
    rvbbit_resolve::install_mcp_tools(tools);
    Ok(())
}

pub fn apply(cmd: Command) -> PgResult<()> {
    match cmd {
        Command::CreateMcpServer { or_replace, if_not_exists, name, options } => {
            create_server(or_replace, if_not_exists, &name, &options)
        }
        Command::ImportMcpTools { limit, except, server, schema } => {
            import(&server, &schema, limit.as_deref(), except.as_deref(), false)
        }
        Command::AlterMcpTool { schema, name, options } => alter(&schema, &name, &options),
        Command::RefreshMcpTools { server } => refresh(&server),
        other => Err(pg(format!("mcp catalog does not run {other:?}"))),
    }
}

fn create_server(or_replace: bool, if_not_exists: bool, name: &str, options: &BTreeMap<String, String>) -> PgResult<()> {
    let spec = server_from_options(name, options).map_err(pg)?;
    let exists = scalar(&format!("SELECT 1 FROM mcp.server WHERE name = {}", qlit(name)))?.is_some();
    if exists && if_not_exists {
        return Ok(());
    }
    if exists && !or_replace {
        return Err(pg(format!("MCP server \"{name}\" already exists")));
    }
    let args = serde_json::to_string(&spec.args).unwrap_or_else(|_| "[]".into());
    let headers = serde_json::to_string(&spec.headers).unwrap_or_else(|_| "{}".into());
    let env = serde_json::to_string(&spec.env).unwrap_or_else(|_| "{}".into());
    if exists {
        exec(&format!(
            "UPDATE mcp.server SET transport = {}, url = {}, command = {}, args = {}, headers = {}, env = {}, timeout_ms = {}, retries = {} WHERE name = {}",
            qlit(&spec.transport),
            opt_lit(spec.url.as_deref()),
            opt_lit(spec.command.as_deref()),
            qlit(&args),
            qlit(&headers),
            qlit(&env),
            spec.timeout_ms,
            spec.retries,
            qlit(name),
        ))?;
    } else {
        exec(&format!(
            "INSERT INTO mcp.server (name, transport, url, command, args, headers, env, timeout_ms, retries) VALUES ({}, {}, {}, {}, {}, {}, {}, {}, {})",
            qlit(name),
            qlit(&spec.transport),
            opt_lit(spec.url.as_deref()),
            opt_lit(spec.command.as_deref()),
            qlit(&args),
            qlit(&headers),
            qlit(&env),
            spec.timeout_ms,
            spec.retries,
        ))?;
    }
    Ok(())
}

fn import(server: &str, schema: &str, limit: Option<&[String]>, except: Option<&[String]>, refresh: bool) -> PgResult<()> {
    let spec = load_spec(server)?;
    let tools = rvbbit_dispatch::mcp_list(&spec).map_err(pg)?;
    let selected = select_tools(&tools, limit, except)?;
    exec(&format!("CREATE SCHEMA IF NOT EXISTS {}", qident(schema)))?;
    let mut used = sql_names(schema)?;
    let mut added = Vec::new();
    let mut changed = Vec::new();
    let mut unchanged = Vec::new();
    for tool in &selected {
        let existing = query(&format!(
            "SELECT name, schema_hash, COALESCE(argtypes, '[]'), COALESCE(procoid::text, '0') \
             FROM mcp.tool WHERE nsp = {} AND server = {} AND tool = {}",
            qlit(schema),
            qlit(server),
            qlit(&tool.name),
        ))?;
        if let Some(row) = existing.into_iter().next() {
            let sql_name = cell(&row, 0);
            used.insert(sql_name.clone());
            if cell(&row, 1) == tool.schema_hash {
                unchanged.push(sql_name);
                continue;
            }
            let old_hash = cell(&row, 1);
            drop_fn(schema, &sql_name, &json_strings(&cell(&row, 2)))?;
            create_fn(schema, &sql_name, server, tool, &spec)?;
            changed.push(format!("{sql_name}: {old_hash} -> {} (recreated)", tool.schema_hash));
        } else {
            let sql_name = unique_name(schema, &tool.name, &mut used)?;
            create_fn(schema, &sql_name, server, tool, &spec)?;
            added.push(sql_name);
        }
    }
    if refresh {
        notice(&format!(
            "mcp refresh {server} into {schema}: unchanged: {}; added: {}; changed: {}",
            join_or_dash(&unchanged),
            join_or_dash(&added),
            join_or_dash(&changed),
        ))?;
    }
    Ok(())
}

fn refresh(server: &str) -> PgResult<()> {
    let _spec = load_spec(server)?;
    let tools = rvbbit_dispatch::mcp_list(&_spec).map_err(pg)?;
    let live: BTreeSet<String> = tools.iter().map(|t| t.name.clone()).collect();
    let schemas = query(&format!(
        "SELECT DISTINCT nsp FROM mcp.tool WHERE server = {}",
        qlit(server)
    ))?;
    if schemas.is_empty() {
        let names: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();
        notice(&format!(
            "mcp refresh {server}: added: {} (no schema yet; IMPORT FOREIGN TOOLS to create functions)",
            join_or_dash(&names)
        ))?;
        return Ok(());
    }
    for row in &schemas {
        let schema = cell(row, 0);
        import(server, &schema, None, None, true)?;
        let have = query(&format!(
            "SELECT name, tool, COALESCE(argtypes, '[]') FROM mcp.tool WHERE nsp = {} AND server = {}",
            qlit(&schema),
            qlit(server),
        ))?;
        let mut removed = Vec::new();
        for tool_row in have {
            let tool_name = cell(&tool_row, 1);
            if live.contains(&tool_name) {
                continue;
            }
            let sql_name = cell(&tool_row, 0);
            drop_fn(&schema, &sql_name, &json_strings(&cell(&tool_row, 2)))?;
            exec(&format!(
                "DELETE FROM mcp.tool WHERE nsp = {} AND name = {}",
                qlit(&schema),
                qlit(&sql_name),
            ))?;
            removed.push(format!("{sql_name} (dropped)"));
        }
        if !removed.is_empty() {
            notice(&format!("mcp refresh {server} into {schema}: removed: {}", removed.join(", ")))?;
        }
    }
    Ok(())
}

fn alter(schema: &str, name: &str, options: &BTreeMap<String, String>) -> PgResult<()> {
    let (schema, name) = resolve_qual(schema, name)?;
    let known = ["timeout", "cache_ttl", "max_cost", "read_only", "side_effects"];
    for key in options.keys() {
        if !known.contains(&key.as_str()) {
            return Err(pg(format!("unknown MCP tool option {key}")));
        }
    }
    if options.contains_key("read_only") && options.contains_key("side_effects") {
        return Err(pg("read_only and side_effects are mutually exclusive"));
    }
    let rows = query(&format!(
        "SELECT COALESCE(timeout_ms::text, ''), COALESCE(cache_ttl_ms::text, '0'), max_cost::text, \
                read_only::text, COALESCE(argtypes, '[]') \
         FROM mcp.tool WHERE nsp = {} AND name = {}",
        qlit(&schema),
        qlit(&name),
    ))?;
    let Some(row) = rows.into_iter().next() else {
        return Err(pg(format!("MCP tool {schema}.{name} does not exist")));
    };
    let mut timeout: Option<i64> = cell_opt(&row, 0).and_then(|s| s.parse().ok());
    let mut cache: i64 = cell(&row, 1).parse().unwrap_or(0);
    let mut max_cost: Option<f64> = cell_opt(&row, 2).and_then(|s| s.parse().ok());
    let mut read_only = is_true(&cell(&row, 3));
    let argtypes = json_strings(&cell(&row, 4));
    if let Some(raw) = options.get("timeout") {
        timeout = Some(mcp_core::parse_timeout_ms(raw).map_err(pg)? as i64);
    }
    if let Some(raw) = options.get("cache_ttl") {
        cache = mcp_core::parse_timeout_ms(raw).map_err(pg)? as i64;
    }
    if let Some(raw) = options.get("max_cost") {
        max_cost = Some(raw.parse::<f64>().map_err(|_| pg(format!("max_cost {raw} is not a number")))?);
    }
    if let Some(raw) = options.get("read_only") {
        read_only = parse_bool(raw)?;
    }
    if let Some(raw) = options.get("side_effects") {
        read_only = !parse_bool(raw)?;
    }
    let vol = if read_only { "STABLE" } else { "VOLATILE" };
    exec(&format!(
        "ALTER FUNCTION {}.{}({}) {vol}",
        qident(&schema),
        qident(&name),
        argtypes.join(", "),
    ))?;
    exec(&format!(
        "UPDATE mcp.tool SET timeout_ms = {}, cache_ttl_ms = {}, max_cost = {}, read_only = {} WHERE nsp = {} AND name = {}",
        match timeout {
            Some(v) => v.to_string(),
            None => "NULL".into(),
        },
        cache,
        match max_cost {
            Some(v) => v.to_string(),
            None => "NULL".into(),
        },
        if read_only { "true" } else { "false" },
        qlit(&schema),
        qlit(&name),
    ))?;
    Ok(())
}

pub fn mcp_try_drop(names: &[String], cascade: bool, missing_ok: bool) -> PgResult<bool> {
    let table = scalar(
        "SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'mcp' AND c.relname = 'server'",
    )?;
    if table.is_none() {
        return Ok(false);
    }
    let mut hits = Vec::new();
    let mut misses = Vec::new();
    for name in names {
        if scalar(&format!("SELECT 1 FROM mcp.server WHERE name = {}", qlit(name)))?.is_some() {
            hits.push(name.clone());
        } else {
            misses.push(name.clone());
        }
    }
    if hits.is_empty() {
        return Ok(false);
    }
    if !misses.is_empty() {
        return Err(pg(format!(
            "DROP SERVER mixes MCP servers ({}) and other names ({})",
            hits.join(", "),
            misses.join(", ")
        )));
    }
    for name in &hits {
        drop_server(name, cascade, missing_ok)?;
    }
    let _ = load();
    Ok(true)
}

fn drop_server(name: &str, cascade: bool, missing_ok: bool) -> PgResult<()> {
    let rows = query(&format!(
        "SELECT nsp, name, COALESCE(argtypes, '[]') FROM mcp.tool WHERE server = {}",
        qlit(name)
    ))?;
    if rows.is_empty() && scalar(&format!("SELECT 1 FROM mcp.server WHERE name = {}", qlit(name)))?.is_none() {
        return if missing_ok { Ok(()) } else { Err(pg(format!("MCP server \"{name}\" does not exist"))) };
    }
    if !rows.is_empty() && !cascade {
        return Err(pg(format!("MCP server \"{name}\" has tools; DROP SERVER ... CASCADE")));
    }
    for row in &rows {
        drop_fn(&cell(row, 0), &cell(row, 1), &json_strings(&cell(row, 2)))?;
    }
    exec(&format!("DELETE FROM mcp.server WHERE name = {}", qlit(name)))?;
    Ok(())
}

fn create_fn(schema: &str, sql_name: &str, server: &str, tool: &ListedTool, spec: &ServerSpec) -> PgResult<()> {
    let shape = sql_args(&tool.input_schema, &tool.output_schema);
    let keys = json_names(&tool.input_schema);
    let body = tool_fn_body(schema, sql_name, &shape.args, shape.returns_table, &shape.columns);
    let args_sql = shape
        .args
        .iter()
        .map(arg_sql)
        .collect::<Vec<_>>()
        .join(", ");
    let vol = if tool.read_only_hint { "STABLE" } else { "VOLATILE" };
    exec(&format!(
        "CREATE OR REPLACE FUNCTION {}.{}({args_sql}) RETURNS {} LANGUAGE sql {vol} PARALLEL UNSAFE COST 1000000 AS $mcp$\n{body}\n$mcp$",
        qident(schema),
        qident(sql_name),
        shape.return_sql,
    ))?;
    let oid = scalar(&format!(
        "SELECT p.oid::text FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
         WHERE n.nspname = {} AND p.proname = {} ORDER BY p.oid DESC LIMIT 1",
        qlit(schema),
        qlit(sql_name),
    ))?
    .ok_or_else(|| pg(format!("MCP function {schema}.{sql_name} was not created")))?;
    let argtypes = serde_json::to_string(&shape.args.iter().map(|a| a.sql_type.clone()).collect::<Vec<_>>()).unwrap_or_else(|_| "[]".into());
    let argkeys = serde_json::to_string(&keys).unwrap_or_else(|_| "[]".into());
    let cols = serde_json::to_string(&shape.columns.iter().map(|c| format!("{} {}", c.name, c.sql_type)).collect::<Vec<_>>()).unwrap_or_else(|_| "[]".into());
    exec(&format!("DELETE FROM mcp.tool WHERE nsp = {} AND name = {}", qlit(schema), qlit(sql_name)))?;
    exec(&format!(
        "INSERT INTO mcp.tool (nsp, name, server, tool, schema_hash, input_schema, output_schema, argkeys, argtypes, returns_table, return_cols, timeout_ms, cache_ttl_ms, max_cost, read_only, procoid) \
         VALUES ({}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, 0, NULL, {}, {})",
        qlit(schema),
        qlit(sql_name),
        qlit(server),
        qlit(&tool.name),
        qlit(&tool.schema_hash),
        qlit(&tool.input_schema.to_string()),
        qlit(&tool.output_schema.to_string()),
        qlit(&argkeys),
        qlit(&argtypes),
        if shape.returns_table { "true" } else { "false" },
        qlit(&cols),
        spec.timeout_ms,
        if tool.read_only_hint { "true" } else { "false" },
        qlit(&oid),
    ))?;
    Ok(())
}

fn arg_sql(arg: &ArgSpec) -> String {
    let mut sql = format!("{} {}", mcp_core::quote_ident(&arg.name), arg.sql_type);
    if let Some(default) = &arg.default_sql {
        sql.push_str(" DEFAULT ");
        sql.push_str(default);
    }
    sql
}

fn drop_fn(schema: &str, name: &str, argtypes: &[String]) -> PgResult<()> {
    let sig = if argtypes.is_empty() { String::new() } else { argtypes.join(", ") };
    exec(&format!("DROP FUNCTION IF EXISTS {}.{}({sig})", qident(schema), qident(name)))?;
    Ok(())
}

fn unique_name(schema: &str, tool: &str, used: &mut BTreeSet<String>) -> PgResult<String> {
    let base = sql_ident(tool);
    let mut candidate = base.clone();
    let mut n = 2u32;
    loop {
        if !used.contains(&candidate) && !fn_exists(schema, &candidate)? {
            used.insert(candidate.clone());
            return Ok(candidate);
        }
        candidate = format!("{base}_{n}");
        n += 1;
        if n > 1000 {
            return Err(pg(format!("MCP tool {tool} has no free SQL name in {schema}")));
        }
    }
}

fn fn_exists(schema: &str, name: &str) -> PgResult<bool> {
    Ok(scalar(&format!(
        "SELECT 1 FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
         WHERE n.nspname = {} AND p.proname = {}",
        qlit(schema),
        qlit(name)
    ))?
    .is_some())
}

fn sql_names(schema: &str) -> PgResult<BTreeSet<String>> {
    let rows = query(&format!("SELECT name FROM mcp.tool WHERE nsp = {}", qlit(schema)))?;
    Ok(rows.into_iter().map(|r| cell(&r, 0)).collect())
}

fn select_tools(tools: &[ListedTool], limit: Option<&[String]>, except: Option<&[String]>) -> PgResult<Vec<ListedTool>> {
    let wanted = |name: &str, list: &[String]| {
        list.iter().any(|item| item.eq_ignore_ascii_case(name) || item.eq_ignore_ascii_case(&sql_ident(name)))
    };
    if let Some(limit) = limit {
        let mut out = Vec::new();
        for item in limit {
            let found = tools.iter().find(|t| wanted(&t.name, &[item.clone()]));
            let Some(found) = found else {
                return Err(pg(format!("MCP server has no tool {item}")));
            };
            out.push(found.clone());
        }
        return Ok(out);
    }
    if let Some(except) = except {
        return Ok(tools.iter().filter(|t| !wanted(&t.name, except)).cloned().collect());
    }
    Ok(tools.to_vec())
}

fn load_spec(name: &str) -> PgResult<ServerSpec> {
    let rows = query(&format!(
        "SELECT transport, url, command, COALESCE(args, '[]'), COALESCE(headers, '{{}}'), COALESCE(env, '{{}}'), \
                COALESCE(timeout_ms, 30000)::text, COALESCE(retries, 2)::text \
         FROM mcp.server WHERE name = {}",
        qlit(name)
    ))?;
    let Some(row) = rows.into_iter().next() else {
        return Err(pg(format!("MCP server \"{name}\" does not exist")));
    };
    let headers: BTreeMap<String, String> = serde_json::from_str(&cell(&row, 4)).unwrap_or_default();
    let env: BTreeMap<String, String> = serde_json::from_str(&cell(&row, 5)).unwrap_or_default();
    let spec = ServerSpec {
        name: name.to_string(),
        transport: cell(&row, 0),
        url: cell_opt(&row, 1),
        command: cell_opt(&row, 2),
        args: json_strings(&cell(&row, 3)),
        headers,
        env,
        timeout_ms: cell(&row, 6).parse().unwrap_or(30_000),
        retries: cell(&row, 7).parse().unwrap_or(2),
    };
    spec.validate_stored().map_err(pg)?;
    Ok(spec)
}

fn resolve_qual(schema: &str, name: &str) -> PgResult<(String, String)> {
    if !schema.is_empty() {
        return Ok((schema.to_string(), name.to_string()));
    }
    let current = scalar("SELECT current_schema()")?.unwrap_or_else(|| "public".into());
    Ok((current, name.to_string()))
}

fn notice(message: &str) -> PgResult<()> {
    exec(&format!("DO $mcp$ BEGIN RAISE NOTICE {}; END $mcp$", qlit(message)))
}

fn join_or_dash(items: &[String]) -> String {
    if items.is_empty() { "-".into() } else { items.join(", ") }
}

fn json_strings(raw: &str) -> Vec<String> {
    serde_json::from_str(raw).unwrap_or_default()
}

fn is_true(raw: &str) -> bool {
    matches!(raw.trim().to_ascii_lowercase().as_str(), "t" | "true" | "1" | "on")
}

fn parse_bool(raw: &str) -> PgResult<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "t" | "on" | "1" | "yes" => Ok(true),
        "false" | "f" | "off" | "0" | "no" => Ok(false),
        other => Err(pg(format!("expected true or false, got {other}"))),
    }
}

fn opt_lit(value: Option<&str>) -> String {
    match value {
        Some(v) => qlit(v),
        None => "NULL".into(),
    }
}
