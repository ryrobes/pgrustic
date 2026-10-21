//! SQL catalog for rvbbit objects.
//!
//! Relations live in schema `rvbbit` (ordinary tables, not bootstrap BKI) so
//! a database that never mentions MEANS or rvbbit DDL stays untouched.
//! Dependencies are real `pg_depend` rows keyed by those tables' relfilenodes
//! (their `pg_class` oids). `pg_proc` rows make semantic functions visible
//! to `\df` and `has_function_privilege`.
//!
//! Secrets: `AUTH ENV` and `AUTH SECRET` both store an environment-variable
//! name. The value is read at dispatch and is never written to the catalog.

use mcx::MemoryContext;
use rvbbit_parse::{Arg, Command, FnClauses, ObjKind, WithOpts};
use rvbbit_resolve::{Endpoint, Resolved};
use types_error::{PgError, PgResult};

pub fn ensure_and_load() -> PgResult<()> {
    ensure_schema()?;
    bootstrap_means()?;
    reload()
}

pub fn execute(sql: &str) -> PgResult<()> {
    ensure_schema()?;
    let cmd = rvbbit_parse::parse_command(sql).map_err(pg)?;
    apply(cmd)?;
    reload()
}

pub fn dump_sql() -> PgResult<String> {
    ensure_schema()?;
    Ok(render_dump()?)
}

pub fn restore_sql(sql: &str) -> PgResult<String> {
    ensure_schema()?;
    let pieces = rvbbit_parse::split_if_rvbbit(sql).unwrap_or_default();
    if pieces.is_empty() && !sql.trim().is_empty() {
        return Err(pg("restore_sql expected rvbbit DDL"));
    }
    for piece in pieces {
        if !piece.rvbbit {
            return Err(pg(format!("restore_sql cannot run `{}`", piece.sql.trim())));
        }
        let cmd = rvbbit_parse::parse_command(piece.sql).map_err(pg)?;
        apply(cmd)?;
    }
    reload()?;
    Ok("ok".into())
}

pub fn semfn_def(name: &str) -> PgResult<Option<String>> {
    ensure_schema()?;
    let rows = query(&format!(
        "SELECT name, argnames::text, argtypes::text, rettype, model_name, system_prompt, \
                user_prompt, max_tokens::text, temperature::text, cache_mode, tests, version::text, \
                labels \
         FROM rvbbit.semfn WHERE nsp = 'rvbbit' AND name = {} ORDER BY name",
        qlit(name)
    ))?;
    Ok(rows.into_iter().next().map(|r| render_function(&r)))
}

fn bootstrap_means() -> PgResult<()> {
    if scalar("SELECT 1 FROM rvbbit.semop WHERE word = 'means'")?.is_some() {
        return Ok(());
    }
    let url = rvbbit_resolve::default_base_url();
    apply(Command::CreateBackend {
        or_replace: false,
        if_not_exists: true,
        name: "hutch".into(),
        kind: "hutch".into(),
        url,
        auth_env: None,
        with: WithOpts {
            batch_size: Some(32),
            max_concurrent: Some(4),
            timeout_ms: Some(30_000),
            retries: Some(5),
        },
        version: Some(1),
    })?;
    apply(Command::CreateModel {
        or_replace: false,
        if_not_exists: true,
        embedding: false,
        name: "default".into(),
        backend: "hutch".into(),
        remote: "mock".into(),
        dimensions: None,
        cost_input: None,
        cost_output: None,
        version: Some(1),
    })?;
    let mut clauses = FnClauses::default();
    clauses.model = Some("default".into());
    clauses.max_tokens = Some(16);
    clauses.temperature = Some(0.0);
    clauses.cache = Some("memoize".into());
    apply(Command::CreateFunction {
        or_replace: true,
        name: "means".into(),
        args: vec![
            Arg { name: "text".into(), typ: "text".into() },
            Arg { name: "criterion".into(), typ: "text".into() },
        ],
        returns: "boolean".into(),
        clauses,
    })?;
    apply(Command::CreateOperator {
        or_replace: true,
        word: "means".into(),
        left: "text".into(),
        right: "text".into(),
        function: "means".into(),
        symbol: None,
        negator: None,
        version: Some(1),
    })?;
    Ok(())
}

fn reload() -> PgResult<()> {
    let rows = query(
        "SELECT COALESCE(o.word, f.name), f.procoid::text, f.name, f.version::text, \
                COALESCE(f.system_prompt, ''), COALESCE(f.user_prompt, ''), f.rettype, \
                COALESCE(f.temperature, 0)::text, f.max_tokens::text, \
                COALESCE(m.remote_name, 'mock'), b.url, b.auth_env, \
                COALESCE(b.batch_size, 32)::text, COALESCE(b.max_concurrent, 4)::text, \
                COALESCE(b.timeout_ms, 30000)::text, COALESCE(b.retries, 5)::text, \
                COALESCE(f.cache_mode, 'memoize'), COALESCE(f.labels, '') \
         FROM rvbbit.semfn f \
         LEFT JOIN rvbbit.semop o ON o.func_name = f.name \
         LEFT JOIN rvbbit.model m ON m.name = f.model_name \
         LEFT JOIN rvbbit.backend b ON b.oid = m.backend_oid \
         WHERE f.nsp = 'rvbbit'",
    )?;
    let mut entries = Vec::with_capacity(rows.len());
    for r in rows {
        let word = cell(&r, 0);
        let func_oid: u32 = cell(&r, 1).parse().unwrap_or(0);
        if func_oid == 0 {
            continue;
        }
        let url = {
            let raw = cell(&r, 10);
            if raw.is_empty() {
                rvbbit_resolve::default_base_url()
            } else {
                raw
            }
        };
        let auth = {
            let v = r.get(11).and_then(|c| c.clone());
            if v.as_deref().unwrap_or("").is_empty() { None } else { v }
        };
        entries.push(Resolved {
            word,
            func_oid,
            endpoint: Endpoint {
                base_url: url,
                auth_env: auth,
                batch_rows: cell(&r, 12).parse().unwrap_or(32),
                max_concurrent: cell(&r, 13).parse().unwrap_or(4),
                timeout_ms: cell(&r, 14).parse().unwrap_or(30_000),
                retries: cell(&r, 15).parse().unwrap_or(5),
                model: cell(&r, 9),
                temperature: cell(&r, 7).parse().unwrap_or(0.0),
                max_tokens: cell_opt(&r, 8).and_then(|s| s.parse().ok()),
                fn_name: cell(&r, 2),
                fn_version: cell(&r, 3).parse().unwrap_or(1),
                system_prompt: cell(&r, 4),
                user_prompt: cell(&r, 5),
                return_type: cell(&r, 6),
                cache_mode: {
                    let mode = cell(&r, 16);
                    if mode.is_empty() { "memoize".into() } else { mode }
                },
                labels: parse_labels(&cell(&r, 17)),
            },
        });
    }
    rvbbit_resolve::install(entries);
    Ok(())
}

fn apply(cmd: Command) -> PgResult<()> {
    match cmd {
        Command::CreateBackend {
            or_replace, if_not_exists, name, kind, url, auth_env, with, version,
        } => create_backend(or_replace, if_not_exists, &name, &kind, &url, auth_env.as_deref(), &with, version),
        Command::CreateModel {
            or_replace, if_not_exists, embedding, name, backend, remote, dimensions, cost_input, cost_output, version,
        } => create_model(or_replace, if_not_exists, embedding, &name, &backend, &remote, dimensions, cost_input, cost_output, version),
        Command::CreateFunction { or_replace, name, args, returns, clauses } => {
            create_function(or_replace, &name, &args, &returns, &clauses)
        }
        Command::CreateOperator {
            or_replace, word, left, right, function, symbol, negator, version,
        } => create_operator(or_replace, &word, &left, &right, &function, symbol.as_deref(), negator.as_deref(), version),
        Command::CreateTree { or_replace, name, args, model, body, nodes, version } => {
            create_tree(or_replace, &name, &args, model.as_deref(), &body, &nodes, version)
        }
        Command::CreateWorkflow { or_replace, name, args, returns, version, body } => {
            create_workflow(or_replace, &name, args.as_deref(), returns.as_deref(), version.unwrap_or(1), &body)
        }
        Command::AlterBackend { name, url, auth_env, with } => alter_backend(&name, url.as_deref(), auth_env.as_deref(), &with),
        Command::AlterModel { name, remote, backend } => alter_model(&name, remote.as_deref(), backend.as_deref()),
        Command::AlterFunction { name, args, model, prompt, system } => {
            alter_function(&name, &args, model.as_deref(), prompt.as_deref(), system.as_deref())
        }
        Command::AlterOperator { word, function } => alter_operator(&word, &function),
        Command::AlterTree { name, model } => alter_tree(&name, &model),
        Command::AlterWorkflow { name, version, body } => alter_workflow(&name, version, &body),
        Command::Drop { kind, if_exists, name, args, version, cascade } => {
            drop_obj(kind, if_exists, &name, &args, version, cascade)
        }
        Command::Grant { is_grant, privilege, kind, name, args, role } => {
            grant(is_grant, &privilege, kind, &name, &args, &role)
        }
        Command::Comment { kind, name, args, text } => comment(kind, &name, &args, text.as_deref()),
    }
}

fn create_backend(
    or_replace: bool,
    if_not_exists: bool,
    name: &str,
    kind: &str,
    url: &str,
    auth_env: Option<&str>,
    with: &WithOpts,
    version: Option<i64>,
) -> PgResult<()> {
    if let Some(oid) = scalar(&format!("SELECT oid::text FROM rvbbit.backend WHERE name = {}", qlit(name)))? {
        if if_not_exists {
            return Ok(());
        }
        if !or_replace {
            return Err(pg(format!("backend \"{name}\" already exists")));
        }
        exec(&format!(
            "UPDATE rvbbit.backend SET kind = {kind}, url = {url}, auth_env = {auth}, \
             batch_size = {batch}, max_concurrent = {conc}, timeout_ms = {timeout}, retries = {retries}, \
             version = {ver} WHERE oid = {oid}",
            kind = qlit(kind),
            url = qlit(url),
            auth = opt_text(&auth_env.map(|s| s.to_string())),
            batch = with.batch_size.unwrap_or(32),
            conc = with.max_concurrent.unwrap_or(4),
            timeout = with.timeout_ms.unwrap_or(30_000),
            retries = with.retries.unwrap_or(3),
            ver = version_expr(version),
        ))?;
        return Ok(());
    }
    let oid = next_oid()?;
    exec(&format!(
        "INSERT INTO rvbbit.backend (oid, name, kind, url, auth_env, batch_size, max_concurrent, timeout_ms, retries, owner_name, version) \
         VALUES ({oid}, {name}, {kind}, {url}, {auth}, {batch}, {conc}, {timeout}, {retries}, current_user, {ver})",
        name = qlit(name),
        kind = qlit(kind),
        url = qlit(url),
        auth = opt_text(&auth_env.map(|s| s.to_string())),
        batch = with.batch_size.unwrap_or(32),
        conc = with.max_concurrent.unwrap_or(4),
        timeout = with.timeout_ms.unwrap_or(30_000),
        retries = with.retries.unwrap_or(3),
        ver = version.unwrap_or(1),
    ))?;
    Ok(())
}

fn create_model(
    or_replace: bool,
    if_not_exists: bool,
    embedding: bool,
    name: &str,
    backend: &str,
    remote: &str,
    dimensions: Option<i64>,
    cost_input: Option<f64>,
    cost_output: Option<f64>,
    version: Option<i64>,
) -> PgResult<()> {
    let backend_oid = backend_oid(backend)?;
    let kind = if embedding { "embedding" } else { "chat" };
    if let Some(oid) = scalar(&format!("SELECT oid::text FROM rvbbit.model WHERE name = {}", qlit(name)))? {
        if if_not_exists {
            return Ok(());
        }
        if !or_replace {
            return Err(pg(format!("model \"{name}\" already exists")));
        }
        clear_deps("model", &oid)?;
        exec(&format!(
            "UPDATE rvbbit.model SET backend_oid = {backend_oid}, remote_name = {remote}, kind = {kind}, \
             dimensions = {dim}, cost_input = {ci}, cost_output = {co}, version = {ver} WHERE oid = {oid}",
            remote = qlit(remote),
            kind = qlit(kind),
            dim = opt_i64(dimensions),
            ci = opt_f64(cost_input),
            co = opt_f64(cost_output),
            ver = version_expr(version),
        ))?;
        depend("model", &oid, "backend", &backend_oid)?;
        return Ok(());
    }
    let oid = next_oid()?;
    exec(&format!(
        "INSERT INTO rvbbit.model (oid, name, backend_oid, remote_name, kind, dimensions, cost_input, cost_output, owner_name, version) \
         VALUES ({oid}, {name}, {backend_oid}, {remote}, {kind}, {dim}, {ci}, {co}, current_user, {ver})",
        name = qlit(name),
        remote = qlit(remote),
        kind = qlit(kind),
        dim = opt_i64(dimensions),
        ci = opt_f64(cost_input),
        co = opt_f64(cost_output),
        ver = version.unwrap_or(1),
    ))?;
    depend("model", &oid, "backend", &backend_oid)?;
    Ok(())
}

fn create_function(or_replace: bool, name: &str, args: &[Arg], returns: &str, clauses: &FnClauses) -> PgResult<()> {
    if let Some(model) = clauses.model.as_deref() {
        let _ = model_oid(model)?;
    }
    let existing = scalar(&format!(
        "SELECT oid::text FROM rvbbit.semfn WHERE nsp = 'rvbbit' AND name = {}",
        qlit(name)
    ))?;
    if existing.is_some() && !or_replace {
        return Err(pg(format!("semantic function \"{name}\" already exists")));
    }
    if !clauses.labels.is_empty() && !is_text_return(returns) {
        return Err(pg("LABELS is only valid on a text semantic function"));
    }
    let procoid = create_proc(name, args, returns, &clauses.labels)?;
    let tests = if clauses.tests.is_empty() {
        "NULL".to_string()
    } else {
        qlit(&clauses.tests.join("\n"))
    };
    let version_sql = match (existing.is_some(), clauses.version) {
        (true, Some(v)) => v.to_string(),
        (true, None) => "version + 1".to_string(),
        (false, v) => v.unwrap_or(1).to_string(),
    };
    let sets = format!(
        "argnames = {an}, argtypes = {at}, rettype = {rt}, model_name = {model}, \
         system_prompt = {sys}, user_prompt = {prompt}, max_tokens = {max}, temperature = {temp}, \
         cache_mode = {cache}, tests = {tests}, procoid = {procoid}, version = {version_sql}, \
         labels = {labels}",
        an = text_array(&args.iter().map(|a| a.name.clone()).collect::<Vec<_>>()),
        at = text_array(&args.iter().map(|a| a.typ.clone()).collect::<Vec<_>>()),
        rt = qlit(returns),
        model = opt_text(&clauses.model),
        sys = opt_text(&clauses.system),
        prompt = opt_text(&clauses.prompt),
        max = opt_i64(clauses.max_tokens),
        temp = opt_f64(clauses.temperature.or(Some(0.0))),
        cache = qlit(clauses.cache.as_deref().unwrap_or("memoize")),
        tests = tests,
        labels = labels_sql(&clauses.labels),
    );
    let oid = if let Some(oid) = existing {
        clear_deps("semfn", &oid)?;
        exec(&format!("UPDATE rvbbit.semfn SET {sets} WHERE oid = {oid}"))?;
        oid
    } else {
        let oid = next_oid()?;
        exec(&format!(
            "INSERT INTO rvbbit.semfn (oid, name, nsp, argnames, argtypes, rettype, model_name, system_prompt, user_prompt, max_tokens, temperature, cache_mode, tests, procoid, owner_name, version, labels) \
             VALUES ({oid}, {name}, 'rvbbit', {an}, {at}, {rt}, {model}, {sys}, {prompt}, {max}, {temp}, {cache}, {tests}, {procoid}, current_user, {ver}, {labels})",
            name = qlit(name),
            an = text_array(&args.iter().map(|a| a.name.clone()).collect::<Vec<_>>()),
            at = text_array(&args.iter().map(|a| a.typ.clone()).collect::<Vec<_>>()),
            rt = qlit(returns),
            model = opt_text(&clauses.model),
            sys = opt_text(&clauses.system),
            prompt = opt_text(&clauses.prompt),
            max = opt_i64(clauses.max_tokens),
            temp = opt_f64(clauses.temperature.or(Some(0.0))),
            cache = qlit(clauses.cache.as_deref().unwrap_or("memoize")),
            tests = if clauses.tests.is_empty() { "NULL".to_string() } else { qlit(&clauses.tests.join("\n")) },
            ver = clauses.version.unwrap_or(1),
            labels = labels_sql(&clauses.labels),
        ))?;
        oid
    };
    if let Some(model) = clauses.model.as_deref() {
        let moid = model_oid(model)?;
        depend("semfn", &oid, "model", &moid)?;
    }
    depend("semfn", &oid, "pg_proc", &procoid)?;
    Ok(())
}

fn create_operator(
    or_replace: bool,
    word: &str,
    left: &str,
    right: &str,
    function: &str,
    symbol: Option<&str>,
    negator: Option<&str>,
    version: Option<i64>,
) -> PgResult<()> {
    let foid = fn_oid(function)?;
    if let Some(oid) = scalar(&format!("SELECT oid::text FROM rvbbit.semop WHERE word = {}", qlit(word)))? {
        if !or_replace {
            return Err(pg(format!("semantic operator \"{word}\" already exists")));
        }
        clear_deps("semop", &oid)?;
        exec(&format!(
            "UPDATE rvbbit.semop SET symbol = {sym}, lefttype = {left}, righttype = {right}, \
             func_name = {func}, negator = {neg}, version = {ver} WHERE oid = {oid}",
            sym = opt_text(&symbol.map(|s| s.to_string())),
            left = qlit(left),
            right = qlit(right),
            func = qlit(function),
            neg = opt_text(&negator.map(|s| s.to_string())),
            ver = version_expr(version),
        ))?;
        depend("semop", &oid, "semfn", &foid)?;
        return Ok(());
    }
    let oid = next_oid()?;
    exec(&format!(
        "INSERT INTO rvbbit.semop (oid, word, symbol, lefttype, righttype, func_name, negator, owner_name, version) \
         VALUES ({oid}, {word}, {sym}, {left}, {right}, {func}, {neg}, current_user, {ver})",
        word = qlit(word),
        sym = opt_text(&symbol.map(|s| s.to_string())),
        left = qlit(left),
        right = qlit(right),
        func = qlit(function),
        neg = opt_text(&negator.map(|s| s.to_string())),
        ver = version.unwrap_or(1),
    ))?;
    depend("semop", &oid, "semfn", &foid)?;
    Ok(())
}

fn create_tree(
    or_replace: bool,
    name: &str,
    args: &[Arg],
    model: Option<&str>,
    body: &str,
    nodes: &serde_json::Value,
    version: Option<i64>,
) -> PgResult<()> {
    if let Some(model) = model {
        let _ = model_oid(model)?;
    }
    let nodes_sql = format!("{}::jsonb", qlit(&nodes.to_string()));
    if let Some(oid) = scalar(&format!("SELECT oid::text FROM rvbbit.decision_tree WHERE name = {}", qlit(name)))? {
        if !or_replace {
            return Err(pg(format!("decision tree \"{name}\" already exists")));
        }
        clear_deps("decision_tree", &oid)?;
        exec(&format!(
            "UPDATE rvbbit.decision_tree SET argnames = {an}, argtypes = {at}, model_name = {model}, \
             body = {body}, nodes = {nodes_sql}, version = {ver} WHERE oid = {oid}",
            an = text_array(&args.iter().map(|a| a.name.clone()).collect::<Vec<_>>()),
            at = text_array(&args.iter().map(|a| a.typ.clone()).collect::<Vec<_>>()),
            model = opt_text(&model.map(|s| s.to_string())),
            body = qlit(body),
            ver = version_expr(version),
        ))?;
        if let Some(model) = model {
            depend("decision_tree", &oid, "model", &model_oid(model)?)?;
        }
        return Ok(());
    }
    let oid = next_oid()?;
    exec(&format!(
        "INSERT INTO rvbbit.decision_tree (oid, name, argnames, argtypes, model_name, body, nodes, owner_name, version) \
         VALUES ({oid}, {name}, {an}, {at}, {model}, {body}, {nodes_sql}, current_user, {ver})",
        name = qlit(name),
        an = text_array(&args.iter().map(|a| a.name.clone()).collect::<Vec<_>>()),
        at = text_array(&args.iter().map(|a| a.typ.clone()).collect::<Vec<_>>()),
        model = opt_text(&model.map(|s| s.to_string())),
        body = qlit(body),
        ver = version.unwrap_or(1),
    ))?;
    if let Some(model) = model {
        depend("decision_tree", &oid, "model", &model_oid(model)?)?;
    }
    Ok(())
}

fn create_workflow(
    or_replace: bool,
    name: &str,
    args: Option<&str>,
    returns: Option<&str>,
    version: i64,
    body: &str,
) -> PgResult<()> {
    if let Some(oid) = scalar(&format!(
        "SELECT oid::text FROM rvbbit.workflow WHERE name = {} AND version = {version}",
        qlit(name)
    ))? {
        if !or_replace {
            return Err(pg(format!("workflow \"{name}\" version {version} already exists")));
        }
        exec(&format!(
            "UPDATE rvbbit.workflow SET args = {args}, returns = {returns}, body = {body} WHERE oid = {oid}",
            args = opt_text(&args.map(|s| s.to_string())),
            returns = opt_text(&returns.map(|s| s.to_string())),
            body = qlit(body),
        ))?;
        return Ok(());
    }
    let oid = next_oid()?;
    exec(&format!(
        "INSERT INTO rvbbit.workflow (oid, name, version, args, returns, body, owner_name) \
         VALUES ({oid}, {name}, {version}, {args}, {returns}, {body}, current_user)",
        name = qlit(name),
        args = opt_text(&args.map(|s| s.to_string())),
        returns = opt_text(&returns.map(|s| s.to_string())),
        body = qlit(body),
    ))?;
    Ok(())
}

fn alter_backend(name: &str, url: Option<&str>, auth_env: Option<&str>, with: &WithOpts) -> PgResult<()> {
    let oid = must(scalar(&format!("SELECT oid::text FROM rvbbit.backend WHERE name = {}", qlit(name)))?, "backend", name)?;
    let mut sets = vec!["version = version + 1".to_string()];
    if let Some(url) = url {
        sets.push(format!("url = {}", qlit(url)));
    }
    if let Some(auth) = auth_env {
        sets.push(format!("auth_env = {}", qlit(auth)));
    }
    if let Some(v) = with.batch_size {
        sets.push(format!("batch_size = {v}"));
    }
    if let Some(v) = with.max_concurrent {
        sets.push(format!("max_concurrent = {v}"));
    }
    if let Some(v) = with.timeout_ms {
        sets.push(format!("timeout_ms = {v}"));
    }
    if let Some(v) = with.retries {
        sets.push(format!("retries = {v}"));
    }
    exec(&format!("UPDATE rvbbit.backend SET {} WHERE oid = {oid}", sets.join(", ")))
}

fn alter_model(name: &str, remote: Option<&str>, backend: Option<&str>) -> PgResult<()> {
    let oid = must(scalar(&format!("SELECT oid::text FROM rvbbit.model WHERE name = {}", qlit(name)))?, "model", name)?;
    if let Some(remote) = remote {
        exec(&format!(
            "UPDATE rvbbit.model SET remote_name = {}, version = version + 1 WHERE oid = {oid}",
            qlit(remote)
        ))?;
    }
    if let Some(backend) = backend {
        let boid = backend_oid(backend)?;
        clear_deps("model", &oid)?;
        exec(&format!(
            "UPDATE rvbbit.model SET backend_oid = {boid}, version = version + 1 WHERE oid = {oid}"
        ))?;
        depend("model", &oid, "backend", &boid)?;
    }
    Ok(())
}

fn alter_function(
    name: &str,
    _args: &[Arg],
    model: Option<&str>,
    prompt: Option<&str>,
    system: Option<&str>,
) -> PgResult<()> {
    let oid = must(
        scalar(&format!("SELECT oid::text FROM rvbbit.semfn WHERE nsp = 'rvbbit' AND name = {}", qlit(name)))?,
        "semantic function",
        name,
    )?;
    let mut sets = vec!["version = version + 1".to_string()];
    if let Some(model) = model {
        let moid = model_oid(model)?;
        sets.push(format!("model_name = {}", qlit(model)));
        clear_deps("semfn", &oid)?;
        let procoid = scalar(&format!("SELECT procoid::text FROM rvbbit.semfn WHERE oid = {oid}"))?
            .unwrap_or_default();
        depend("semfn", &oid, "model", &moid)?;
        if !procoid.is_empty() {
            depend("semfn", &oid, "pg_proc", &procoid)?;
        }
    }
    if let Some(prompt) = prompt {
        sets.push(format!("user_prompt = {}", qlit(prompt)));
    }
    if let Some(system) = system {
        sets.push(format!("system_prompt = {}", qlit(system)));
    }
    exec(&format!("UPDATE rvbbit.semfn SET {} WHERE oid = {oid}", sets.join(", ")))
}

fn alter_operator(word: &str, function: &str) -> PgResult<()> {
    let oid = must(scalar(&format!("SELECT oid::text FROM rvbbit.semop WHERE word = {}", qlit(word)))?, "semantic operator", word)?;
    let foid = fn_oid(function)?;
    clear_deps("semop", &oid)?;
    exec(&format!(
        "UPDATE rvbbit.semop SET func_name = {}, version = version + 1 WHERE oid = {oid}",
        qlit(function)
    ))?;
    depend("semop", &oid, "semfn", &foid)
}

fn alter_tree(name: &str, model: &str) -> PgResult<()> {
    let oid = must(scalar(&format!("SELECT oid::text FROM rvbbit.decision_tree WHERE name = {}", qlit(name)))?, "decision tree", name)?;
    let moid = model_oid(model)?;
    clear_deps("decision_tree", &oid)?;
    exec(&format!(
        "UPDATE rvbbit.decision_tree SET model_name = {}, version = version + 1 WHERE oid = {oid}",
        qlit(model)
    ))?;
    depend("decision_tree", &oid, "model", &moid)
}

fn alter_workflow(name: &str, version: i64, body: &str) -> PgResult<()> {
    let oid = scalar(&format!(
        "SELECT oid::text FROM rvbbit.workflow WHERE name = {} AND version = {version}",
        qlit(name)
    ))?;
    if let Some(oid) = oid {
        exec(&format!("UPDATE rvbbit.workflow SET body = {} WHERE oid = {oid}", qlit(body)))?;
        return Ok(());
    }
    create_workflow(false, name, None, None, version, body)
}

fn drop_obj(kind: ObjKind, if_exists: bool, name: &str, args: &[Arg], version: Option<i64>, cascade: bool) -> PgResult<()> {
    match kind {
        ObjKind::Backend => drop_backend(name, if_exists, cascade),
        ObjKind::Model => drop_model(name, if_exists, cascade),
        ObjKind::Function => drop_function(name, args, if_exists, cascade),
        ObjKind::Operator => drop_operator(name, if_exists),
        ObjKind::Tree => drop_tree(name, if_exists),
        ObjKind::Workflow => drop_workflow(name, version, if_exists),
    }
}

fn drop_backend(name: &str, if_exists: bool, cascade: bool) -> PgResult<()> {
    let Some(oid) = scalar(&format!("SELECT oid::text FROM rvbbit.backend WHERE name = {}", qlit(name)))? else {
        return missing(if_exists, "backend", name);
    };
    let models = query(&format!("SELECT name FROM rvbbit.model WHERE backend_oid = {oid} ORDER BY name"))?;
    if !models.is_empty() && !cascade {
        return Err(pg(format!(
            "cannot drop backend \"{name}\" because model \"{}\" depends on it",
            cell(&models[0], 0)
        )));
    }
    for m in models {
        drop_model(&cell(&m, 0), true, true)?;
    }
    clear_deps("backend", &oid)?;
    clear_comment("backend", &oid)?;
    exec(&format!("DELETE FROM rvbbit.backend WHERE oid = {oid}"))
}

fn drop_model(name: &str, if_exists: bool, cascade: bool) -> PgResult<()> {
    let Some(oid) = scalar(&format!("SELECT oid::text FROM rvbbit.model WHERE name = {}", qlit(name)))? else {
        return missing(if_exists, "model", name);
    };
    let fns = query(&format!("SELECT name FROM rvbbit.semfn WHERE model_name = {} ORDER BY name", qlit(name)))?;
    let trees = query(&format!("SELECT name FROM rvbbit.decision_tree WHERE model_name = {} ORDER BY name", qlit(name)))?;
    if (!fns.is_empty() || !trees.is_empty()) && !cascade {
        let who = if !fns.is_empty() {
            format!("semantic function \"{}\"", cell(&fns[0], 0))
        } else {
            format!("decision tree \"{}\"", cell(&trees[0], 0))
        };
        return Err(pg(format!("cannot drop model \"{name}\" because {who} depends on it")));
    }
    for f in fns {
        drop_function(&cell(&f, 0), &[], true, true)?;
    }
    for t in trees {
        drop_tree(&cell(&t, 0), true)?;
    }
    clear_deps("model", &oid)?;
    clear_comment("model", &oid)?;
    exec(&format!("DELETE FROM rvbbit.model WHERE oid = {oid}"))
}

fn drop_function(name: &str, args: &[Arg], if_exists: bool, cascade: bool) -> PgResult<()> {
    let Some(row) = query(&format!(
        "SELECT oid::text, argtypes::text FROM rvbbit.semfn WHERE nsp = 'rvbbit' AND name = {}",
        qlit(name)
    ))?.into_iter().next() else {
        return missing(if_exists, "semantic function", name);
    };
    let oid = cell(&row, 0);
    let ops = query(&format!("SELECT word FROM rvbbit.semop WHERE func_name = {} ORDER BY word", qlit(name)))?;
    if !ops.is_empty() && !cascade {
        return Err(pg(format!(
            "cannot drop semantic function \"{name}\" because semantic operator \"{}\" depends on it",
            cell(&ops[0], 0)
        )));
    }
    for op in ops {
        drop_operator(&cell(&op, 0), true)?;
    }
    let types = if args.is_empty() {
        parse_pg_array(&cell(&row, 1))
    } else {
        args.iter().map(|a| a.typ.clone()).collect()
    };
    // Drop our pg_depend row before DROP FUNCTION. The row names this
    // table as classid, which performDeletion does not know.
    clear_deps("semfn", &oid)?;
    clear_comment("semfn", &oid)?;
    exec(&format!("DROP FUNCTION IF EXISTS rvbbit.{}({})", qident(name), types.join(", ")))?;
    exec(&format!("DELETE FROM rvbbit.semfn WHERE oid = {oid}"))
}

fn drop_operator(word: &str, if_exists: bool) -> PgResult<()> {
    let Some(oid) = scalar(&format!("SELECT oid::text FROM rvbbit.semop WHERE word = {}", qlit(word)))? else {
        return missing(if_exists, "semantic operator", word);
    };
    clear_deps("semop", &oid)?;
    clear_comment("semop", &oid)?;
    exec(&format!("DELETE FROM rvbbit.semop WHERE oid = {oid}"))
}

fn drop_tree(name: &str, if_exists: bool) -> PgResult<()> {
    let Some(oid) = scalar(&format!("SELECT oid::text FROM rvbbit.decision_tree WHERE name = {}", qlit(name)))? else {
        return missing(if_exists, "decision tree", name);
    };
    clear_deps("decision_tree", &oid)?;
    clear_comment("decision_tree", &oid)?;
    exec(&format!("DELETE FROM rvbbit.decision_tree WHERE oid = {oid}"))
}

fn drop_workflow(name: &str, version: Option<i64>, if_exists: bool) -> PgResult<()> {
    let sql = match version {
        Some(v) => format!("SELECT oid::text FROM rvbbit.workflow WHERE name = {} AND version = {v}", qlit(name)),
        None => format!("SELECT oid::text FROM rvbbit.workflow WHERE name = {}", qlit(name)),
    };
    let rows = query(&sql)?;
    if rows.is_empty() {
        return missing(if_exists, "workflow", name);
    }
    for r in rows {
        let oid = cell(&r, 0);
        clear_deps("workflow", &oid)?;
        clear_comment("workflow", &oid)?;
        exec(&format!("DELETE FROM rvbbit.workflow WHERE oid = {oid}"))?;
    }
    Ok(())
}

fn grant(is_grant: bool, privilege: &str, kind: ObjKind, name: &str, args: &[Arg], role: &str) -> PgResult<()> {
    match (privilege, kind) {
        ("USAGE", ObjKind::Backend) => {
            let _ = must(scalar(&format!("SELECT oid::text FROM rvbbit.backend WHERE name = {}", qlit(name)))?, "backend", name)?;
            if is_grant {
                exec(&format!(
                    "UPDATE rvbbit.backend SET acl = CASE WHEN {role} = ANY(acl) THEN acl ELSE array_append(acl, {role}) END WHERE name = {name}",
                    role = qlit(role),
                    name = qlit(name),
                ))
            } else {
                exec(&format!(
                    "UPDATE rvbbit.backend SET acl = array_remove(acl, {role}) WHERE name = {name}",
                    role = qlit(role),
                    name = qlit(name),
                ))
            }
        }
        ("EXECUTE", ObjKind::Function) => {
            let types = fn_types(name, args)?;
            let verb = if is_grant { "GRANT" } else { "REVOKE" };
            let dir = if is_grant { "TO" } else { "FROM" };
            exec(&format!(
                "{verb} EXECUTE ON FUNCTION rvbbit.{}({}) {dir} {}",
                qident(name),
                types.join(", "),
                qident(role),
            ))
        }
        _ => Err(pg(format!("cannot {privilege} on this rvbbit object"))),
    }
}

fn comment(kind: ObjKind, name: &str, args: &[Arg], text: Option<&str>) -> PgResult<()> {
    let (table, oid) = match kind {
        ObjKind::Backend => ("backend", must(scalar(&format!("SELECT oid::text FROM rvbbit.backend WHERE name = {}", qlit(name)))?, "backend", name)?),
        ObjKind::Model => ("model", must(scalar(&format!("SELECT oid::text FROM rvbbit.model WHERE name = {}", qlit(name)))?, "model", name)?),
        ObjKind::Function => ("semfn", must(scalar(&format!("SELECT oid::text FROM rvbbit.semfn WHERE name = {}", qlit(name)))?, "semantic function", name)?),
        ObjKind::Operator => ("semop", must(scalar(&format!("SELECT oid::text FROM rvbbit.semop WHERE word = {}", qlit(name)))?, "semantic operator", name)?),
        ObjKind::Tree => ("decision_tree", must(scalar(&format!("SELECT oid::text FROM rvbbit.decision_tree WHERE name = {}", qlit(name)))?, "decision tree", name)?),
        ObjKind::Workflow => ("workflow", must(scalar(&format!("SELECT oid::text FROM rvbbit.workflow WHERE name = {} ORDER BY version LIMIT 1", qlit(name)))?, "workflow", name)?),
    };
    exec(&format!("UPDATE rvbbit.{table} SET comment = {} WHERE oid = {oid}", opt_text(&text.map(|s| s.to_string()))))?;
    clear_comment(table, &oid)?;
    if let Some(text) = text {
        let class = rel_oid(table)?;
        exec(&format!(
            "INSERT INTO pg_catalog.pg_description (objoid, classoid, objsubid, description) VALUES ({oid}, {class}, 0, {})",
            qlit(text)
        ))?;
    }
    if kind == ObjKind::Function {
        let types = fn_types(name, args)?;
        match text {
            Some(text) => exec(&format!(
                "COMMENT ON FUNCTION rvbbit.{}({}) IS {}",
                qident(name),
                types.join(", "),
                qlit(text)
            ))?,
            None => exec(&format!(
                "COMMENT ON FUNCTION rvbbit.{}({}) IS NULL",
                qident(name),
                types.join(", ")
            ))?,
        }
    }
    Ok(())
}

fn fn_types(name: &str, args: &[Arg]) -> PgResult<Vec<String>> {
    if !args.is_empty() {
        return Ok(args.iter().map(|a| a.typ.clone()).collect());
    }
    let raw = scalar(&format!(
        "SELECT argtypes::text FROM rvbbit.semfn WHERE nsp = 'rvbbit' AND name = {}",
        qlit(name)
    ))?;
    let Some(raw) = raw else {
        return Err(pg(format!("semantic function \"{name}\" does not exist")));
    };
    Ok(parse_pg_array(&raw))
}

fn is_text_return(returns: &str) -> bool {
    returns.trim().eq_ignore_ascii_case("text")
}

fn create_proc(name: &str, args: &[Arg], returns: &str, labels: &[String]) -> PgResult<String> {
    let sig = args
        .iter()
        .map(|a| format!("{} {}", qident(&a.name), a.typ))
        .collect::<Vec<_>>()
        .join(", ");
    let prosrc = match returns.trim().to_ascii_lowercase().as_str() {
        "boolean" | "bool" => "rvbbit_means",
        "double precision" | "float8" | "float" => "rvbbit_means_score",
        // JSON is parsed by hutch and returned as text. Cast in SQL for jsonb.
        // A declared label set uses the decision model and returns
        // {"label","score","scores"} as text.
        "text" if labels.is_empty() => "rvbbit_extract",
        "text" => {
            if labels.len() < 2 {
                return Err(pg("classify label set needs at least two labels"));
            }
            "rvbbit_classify"
        }
        other => {
            return Err(pg(format!(
                "semantic function return type {other} is not supported yet; use boolean, double precision, or text"
            )))
        }
    };
    exec(&format!(
        "CREATE OR REPLACE FUNCTION rvbbit.{}({sig}) RETURNS {returns} \
         LANGUAGE internal STRICT VOLATILE PARALLEL UNSAFE COST 1000000 AS '{prosrc}'",
        qident(name)
    ))?;
    scalar(&format!(
        "SELECT p.oid::text FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
         WHERE n.nspname = 'rvbbit' AND p.proname = {}",
        qlit(name)
    ))?
    .ok_or_else(|| pg(format!("function rvbbit.{name} was not created")))
}

fn ensure_schema() -> PgResult<()> {
    exec("SELECT pg_catalog.pg_advisory_xact_lock(910001)")?;
    let have = scalar("SELECT 1 FROM pg_namespace WHERE nspname = 'rvbbit'")?;
    if have.is_none() {
        for sql in SCHEMA_DDL {
            exec(sql)?;
        }
    }
    // Clusters created before fixed-label classify have no labels column.
    // Skip the ALTER when the column is already there. ADD COLUMN IF NOT
    // EXISTS still emits a notice, and ensure runs on ordinary queries.
    let have_labels = scalar(
        "SELECT 1 FROM pg_attribute a \
         JOIN pg_class c ON c.oid = a.attrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'rvbbit' AND c.relname = 'semfn' \
           AND a.attname = 'labels' AND NOT a.attisdropped",
    )?;
    if have_labels.is_none() {
        exec("ALTER TABLE rvbbit.semfn ADD COLUMN IF NOT EXISTS labels text")?;
    }
    if scalar(
        "SELECT 1 FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
         WHERE n.nspname = 'rvbbit' AND p.proname = 'dump_sql'",
    )?
    .is_none()
    {
        for sql in FUNCTION_DDL {
            exec(sql)?;
        }
    }
    Ok(())
}

const SCHEMA_DDL: &[&str] = &[
    "CREATE SCHEMA IF NOT EXISTS rvbbit",
    "CREATE SEQUENCE IF NOT EXISTS rvbbit.oid_seq AS bigint START 910000",
    "CREATE TABLE IF NOT EXISTS rvbbit.backend (
        oid oid PRIMARY KEY,
        name text UNIQUE NOT NULL,
        kind text NOT NULL,
        url text NOT NULL,
        auth_env text,
        batch_size int NOT NULL DEFAULT 32,
        max_concurrent int NOT NULL DEFAULT 4,
        timeout_ms int NOT NULL DEFAULT 30000,
        retries int NOT NULL DEFAULT 3,
        owner_name text NOT NULL,
        acl text[] NOT NULL DEFAULT '{}',
        comment text,
        version int NOT NULL DEFAULT 1)",
    "CREATE TABLE IF NOT EXISTS rvbbit.model (
        oid oid PRIMARY KEY,
        name text UNIQUE NOT NULL,
        backend_oid oid NOT NULL,
        remote_name text NOT NULL,
        kind text NOT NULL,
        dimensions int,
        cost_input float8,
        cost_output float8,
        owner_name text NOT NULL,
        comment text,
        version int NOT NULL DEFAULT 1)",
    "CREATE TABLE IF NOT EXISTS rvbbit.semfn (
        oid oid PRIMARY KEY,
        name text NOT NULL,
        nsp text NOT NULL DEFAULT 'rvbbit',
        argnames text[] NOT NULL,
        argtypes text[] NOT NULL,
        rettype text NOT NULL,
        model_name text,
        system_prompt text,
        user_prompt text,
        max_tokens int,
        temperature float8 NOT NULL DEFAULT 0,
        cache_mode text NOT NULL DEFAULT 'memoize',
        tests text,
        procoid oid NOT NULL,
        owner_name text NOT NULL,
        comment text,
        version int NOT NULL DEFAULT 1,
        labels text,
        UNIQUE (nsp, name))",
    "CREATE TABLE IF NOT EXISTS rvbbit.semop (
        oid oid PRIMARY KEY,
        word text UNIQUE NOT NULL,
        symbol text,
        lefttype text NOT NULL,
        righttype text NOT NULL,
        func_name text NOT NULL,
        negator text,
        owner_name text NOT NULL,
        comment text,
        version int NOT NULL DEFAULT 1)",
    "CREATE TABLE IF NOT EXISTS rvbbit.decision_tree (
        oid oid PRIMARY KEY,
        name text UNIQUE NOT NULL,
        argnames text[] NOT NULL,
        argtypes text[] NOT NULL,
        model_name text,
        body text NOT NULL,
        nodes jsonb NOT NULL,
        owner_name text NOT NULL,
        comment text,
        version int NOT NULL DEFAULT 1)",
    "CREATE TABLE IF NOT EXISTS rvbbit.workflow (
        oid oid PRIMARY KEY,
        name text NOT NULL,
        version int NOT NULL,
        args text,
        returns text,
        body text NOT NULL,
        owner_name text NOT NULL,
        comment text,
        UNIQUE (name, version))",
    "CREATE OR REPLACE VIEW rvbbit.backends AS
        SELECT oid, name, kind, url, auth_env, batch_size, max_concurrent, timeout_ms, retries, version, comment
        FROM rvbbit.backend",
    "CREATE OR REPLACE VIEW rvbbit.models AS
        SELECT m.oid, m.name, b.name AS backend, m.remote_name, m.kind, m.dimensions,
               m.cost_input, m.cost_output, m.version, m.comment
        FROM rvbbit.model m JOIN rvbbit.backend b ON b.oid = m.backend_oid",
    "CREATE OR REPLACE VIEW rvbbit.semantic_functions AS
        SELECT oid, name, argnames, argtypes, rettype, model_name, max_tokens, temperature,
               cache_mode, procoid, version, comment
        FROM rvbbit.semfn",
    "CREATE OR REPLACE VIEW rvbbit.semantic_operators AS
        SELECT oid, word, symbol, lefttype, righttype, func_name, negator, version, comment
        FROM rvbbit.semop",
    "CREATE OR REPLACE VIEW rvbbit.decision_trees AS
        SELECT oid, name, argnames, argtypes, model_name, version, comment
        FROM rvbbit.decision_tree",
    "CREATE OR REPLACE VIEW rvbbit.workflows AS
        SELECT oid, name, version, args, returns, comment
        FROM rvbbit.workflow",
];

const FUNCTION_DDL: &[&str] = &[
    "CREATE OR REPLACE FUNCTION rvbbit.dump_sql() RETURNS text
        LANGUAGE internal VOLATILE PARALLEL UNSAFE AS 'rvbbit_dump_sql'",
    "CREATE OR REPLACE FUNCTION rvbbit.restore_sql(sql text) RETURNS text
        LANGUAGE internal STRICT VOLATILE PARALLEL UNSAFE AS 'rvbbit_restore_sql'",
    "CREATE OR REPLACE FUNCTION rvbbit.pg_get_semanticfunctiondef(fname text) RETURNS text
        LANGUAGE internal STRICT VOLATILE PARALLEL UNSAFE AS 'rvbbit_get_semfn_def'",
];

fn render_dump() -> PgResult<String> {
    let mut out = String::new();
    let backends = query(
        "SELECT name, kind, url, auth_env, batch_size::text, max_concurrent::text, timeout_ms::text, retries::text, version::text, comment, acl::text \
         FROM rvbbit.backend ORDER BY name",
    )?;
    for r in &backends {
        out.push_str(&format!(
            "CREATE BACKEND {} TYPE {} URL {} ",
            qident(&cell(r, 0)),
            qident(&cell(r, 1)),
            qlit(&cell(r, 2))
        ));
        if let Some(auth) = cell_opt(r, 3) {
            out.push_str(&format!("AUTH ENV {} ", qlit(&auth)));
        }
        out.push_str(&format!(
            "WITH (batch_size = {}, max_concurrent = {}, timeout = '{}ms', retries = {}) VERSION {};\n",
            cell(r, 4), cell(r, 5), cell(r, 6), cell(r, 7), cell(r, 8)
        ));
    }
    let models = query(
        "SELECT m.name, b.name, m.remote_name, m.kind, m.dimensions::text, m.cost_input::text, m.cost_output::text, m.version::text, m.comment \
         FROM rvbbit.model m JOIN rvbbit.backend b ON b.oid = m.backend_oid ORDER BY m.name",
    )?;
    for r in &models {
        if cell(r, 3) == "embedding" {
            out.push_str(&format!(
                "CREATE EMBEDDING MODEL {} ON BACKEND {} NAME {} DIMENSIONS {} VERSION {};\n",
                qident(&cell(r, 0)),
                qident(&cell(r, 1)),
                qlit(&cell(r, 2)),
                cell(r, 4),
                cell(r, 7)
            ));
        } else {
            out.push_str(&format!(
                "CREATE MODEL {} ON BACKEND {} NAME {}",
                qident(&cell(r, 0)),
                qident(&cell(r, 1)),
                qlit(&cell(r, 2))
            ));
            if cell_opt(r, 5).is_some() || cell_opt(r, 6).is_some() {
                out.push_str(" COST (");
                let mut bits = Vec::new();
                if let Some(v) = cell_opt(r, 5) {
                    bits.push(format!("input = {v}"));
                }
                if let Some(v) = cell_opt(r, 6) {
                    bits.push(format!("output = {v}"));
                }
                out.push_str(&bits.join(", "));
                out.push(')');
            }
            out.push_str(&format!(" VERSION {};\n", cell(r, 7)));
        }
    }
    let fns = query(
        "SELECT name, argnames::text, argtypes::text, rettype, model_name, system_prompt, user_prompt, \
                max_tokens::text, temperature::text, cache_mode, tests, version::text, labels, comment \
         FROM rvbbit.semfn WHERE nsp = 'rvbbit' ORDER BY name",
    )?;
    for r in &fns {
        out.push_str(&render_function(r));
        out.push('\n');
    }
    let ops = query(
        "SELECT word, lefttype, righttype, func_name, symbol, negator, version::text, comment FROM rvbbit.semop ORDER BY word",
    )?;
    for r in &ops {
        out.push_str(&format!(
            "CREATE SEMANTIC OPERATOR {} (LEFTARG = {}, RIGHTARG = {}, FUNCTION = {}",
            qident(&cell(r, 0)),
            cell(r, 1),
            cell(r, 2),
            qident(&cell(r, 3))
        ));
        if let Some(sym) = cell_opt(r, 4) {
            out.push_str(&format!(", SYMBOL = {}", qlit(&sym)));
        }
        if let Some(neg) = cell_opt(r, 5) {
            out.push_str(&format!(", NEGATOR = {neg}"));
        }
        out.push_str(&format!(") VERSION {};\n", cell(r, 6)));
    }
    let trees = query(
        "SELECT name, argnames::text, argtypes::text, model_name, body, version::text, comment FROM rvbbit.decision_tree ORDER BY name",
    )?;
    for r in &trees {
        let args = pair_args(&cell(r, 1), &cell(r, 2));
        out.push_str(&format!("CREATE DECISION TREE {} ({args}) ", qident(&cell(r, 0))));
        if let Some(model) = cell_opt(r, 3) {
            out.push_str(&format!("MODEL {} ", qident(&model)));
        }
        // Raw ASK/YES/NO (or CHOOSE) text. Dollar quotes would be one string
        // token, and the tree parser would not see ASK.
        out.push_str(&format!("AS {} VERSION {};\n", cell(r, 4), cell(r, 5)));
    }
    let flows = query(
        "SELECT name, args, returns, version::text, body, comment FROM rvbbit.workflow ORDER BY name, version",
    )?;
    for r in &flows {
        out.push_str(&format!("CREATE WORKFLOW {}", qident(&cell(r, 0))));
        if let Some(args) = cell_opt(r, 1) {
            out.push(' ');
            out.push_str(&args);
        }
        if let Some(ret) = cell_opt(r, 2) {
            out.push_str(&format!(" RETURNS {ret}"));
        }
        out.push_str(&format!(" VERSION {} AS {};\n", cell(r, 3), dollar(&cell(r, 4))));
    }
    push_comments(&mut out, "BACKEND", "backend", &query("SELECT name, comment FROM rvbbit.backend WHERE comment IS NOT NULL ORDER BY name")?)?;
    push_comments(&mut out, "MODEL", "model", &query("SELECT name, comment FROM rvbbit.model WHERE comment IS NOT NULL ORDER BY name")?)?;
    push_comments(&mut out, "SEMANTIC FUNCTION", "semfn", &query("SELECT name, comment FROM rvbbit.semfn WHERE comment IS NOT NULL ORDER BY name")?)?;
    push_comments(&mut out, "SEMANTIC OPERATOR", "semop", &query("SELECT word, comment FROM rvbbit.semop WHERE comment IS NOT NULL ORDER BY word")?)?;
    push_comments(&mut out, "DECISION TREE", "decision_tree", &query("SELECT name, comment FROM rvbbit.decision_tree WHERE comment IS NOT NULL ORDER BY name")?)?;
    push_comments(&mut out, "WORKFLOW", "workflow", &query("SELECT name, comment FROM rvbbit.workflow WHERE comment IS NOT NULL ORDER BY name, version")?)?;
    for r in &backends {
        for role in parse_pg_array(&cell(r, 10)) {
            out.push_str(&format!(
                "GRANT USAGE ON BACKEND {} TO {};\n",
                qident(&cell(r, 0)),
                qident(&role)
            ));
        }
    }
    for r in &fns {
        let name = cell(r, 0);
        let args = pair_args(&cell(r, 1), &cell(r, 2));
        out.push_str(&format!(
            "REVOKE EXECUTE ON SEMANTIC FUNCTION {name}({args}) FROM public;\n"
        ));
        let grantees = query(&format!(
            "SELECT CASE WHEN a.grantee = 0 THEN 'public' ELSE r.rolname END \
             FROM pg_proc p \
             JOIN pg_namespace n ON n.oid = p.pronamespace \
             CROSS JOIN LATERAL aclexplode(COALESCE(p.proacl, acldefault('f', p.proowner))) AS a \
             LEFT JOIN pg_roles r ON r.oid = a.grantee \
             WHERE n.nspname = 'rvbbit' AND p.proname = {} AND a.privilege_type = 'EXECUTE' \
             ORDER BY 1",
            qlit(&name)
        ))?;
        for g in grantees {
            let role = cell(&g, 0);
            if role.is_empty() {
                continue;
            }
            out.push_str(&format!(
                "GRANT EXECUTE ON SEMANTIC FUNCTION {name}({args}) TO {};\n",
                qident(&role)
            ));
        }
    }
    Ok(out)
}

fn render_function(r: &[Option<String>]) -> String {
    let name = cell(r, 0);
    let args = pair_args(&cell(r, 1), &cell(r, 2));
    let mut s = format!(
        "CREATE SEMANTIC FUNCTION {}({}) RETURNS {}",
        qident(&name),
        args,
        cell(r, 3)
    );
    if let Some(raw) = cell_opt(r, 12) {
        let labels = parse_labels(&raw);
        if !labels.is_empty() {
            let quoted = labels.iter().map(|l| qlit(l)).collect::<Vec<_>>().join(", ");
            s.push_str(&format!(" LABELS ({quoted})"));
        }
    }
    if let Some(model) = cell_opt(r, 4) {
        s.push_str(&format!(" MODEL {}", qident(&model)));
    }
    if let Some(sys) = cell_opt(r, 5) {
        s.push_str(&format!(" SYSTEM {}", dollar(&sys)));
    }
    if let Some(prompt) = cell_opt(r, 6) {
        s.push_str(&format!(" PROMPT {}", dollar(&prompt)));
    }
    if let Some(max) = cell_opt(r, 7) {
        s.push_str(&format!(" MAX TOKENS {max}"));
    }
    if let Some(temp) = cell_opt(r, 8) {
        s.push_str(&format!(" TEMPERATURE {temp}"));
    }
    if let Some(cache) = cell_opt(r, 9) {
        s.push_str(&format!(" CACHE {}", qident(&cache)));
    }
    if let Some(tests) = cell_opt(r, 10) {
        for line in tests.lines() {
            s.push_str(&format!(" TEST {line}"));
        }
    }
    s.push_str(&format!(" VERSION {};", cell(r, 11)));
    s
}

fn push_comments(out: &mut String, kind: &str, _table: &str, rows: &[Vec<Option<String>>]) -> PgResult<()> {
    for r in rows {
        out.push_str(&format!(
            "COMMENT ON {kind} {} IS {};\n",
            qident(&cell(r, 0)),
            qlit(&cell(r, 1))
        ));
    }
    Ok(())
}

fn pair_args(names: &str, types: &str) -> String {
    let ns = parse_pg_array(names);
    let ts = parse_pg_array(types);
    ns.into_iter()
        .zip(ts)
        .map(|(n, t)| format!("{n} {t}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// pg text[] output is `{a,b}` with quotes around elements that need them.
fn parse_pg_array(raw: &str) -> Vec<String> {
    let s = raw.trim();
    if s.is_empty() || s == "{}" || s == "NULL" {
        return Vec::new();
    }
    let inner = s.trim_start_matches('{').trim_end_matches('}');
    if inner.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quote {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    cur.push('"');
                    chars.next();
                } else {
                    in_quote = false;
                }
            } else {
                cur.push(c);
            }
        } else if c == '"' {
            in_quote = true;
        } else if c == ',' {
            out.push(cur.trim().to_string());
            cur.clear();
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() || !out.is_empty() {
        out.push(cur.trim().to_string());
    }
    out.into_iter().filter(|s| !s.is_empty()).collect()
}

fn depend(dep_rel: &str, dep_oid: &str, ref_rel: &str, ref_oid: &str) -> PgResult<()> {
    let dep_class = rel_oid(dep_rel)?;
    let ref_class = if ref_rel == "pg_proc" {
        "1255".to_string()
    } else {
        rel_oid(ref_rel)?
    };
    exec(&format!(
        "INSERT INTO pg_catalog.pg_depend (classid, objid, objsubid, refclassid, refobjid, refobjsubid, deptype) \
         VALUES ({dep_class}, {dep_oid}, 0, {ref_class}, {ref_oid}, 0, 'n')"
    ))
}

fn clear_deps(rel: &str, oid: &str) -> PgResult<()> {
    let class = rel_oid(rel)?;
    exec(&format!(
        "DELETE FROM pg_catalog.pg_depend WHERE (classid = {class} AND objid = {oid}) OR (refclassid = {class} AND refobjid = {oid})"
    ))
}

fn clear_comment(rel: &str, oid: &str) -> PgResult<()> {
    let class = rel_oid(rel)?;
    exec(&format!("DELETE FROM pg_catalog.pg_description WHERE classoid = {class} AND objoid = {oid}"))
}

fn rel_oid(rel: &str) -> PgResult<String> {
    scalar(&format!(
        "SELECT c.oid::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'rvbbit' AND c.relname = {} AND c.relkind = 'r'",
        qlit(rel)
    ))?
    .ok_or_else(|| pg(format!("rvbbit.{rel} is missing")))
}

fn backend_oid(name: &str) -> PgResult<String> {
    must(scalar(&format!("SELECT oid::text FROM rvbbit.backend WHERE name = {}", qlit(name)))?, "backend", name)
}

fn model_oid(name: &str) -> PgResult<String> {
    must(scalar(&format!("SELECT oid::text FROM rvbbit.model WHERE name = {}", qlit(name)))?, "model", name)
}

fn fn_oid(name: &str) -> PgResult<String> {
    must(
        scalar(&format!("SELECT oid::text FROM rvbbit.semfn WHERE nsp = 'rvbbit' AND name = {}", qlit(name)))?,
        "semantic function",
        name,
    )
}

fn next_oid() -> PgResult<String> {
    scalar("SELECT nextval('rvbbit.oid_seq')::oid::text")?.ok_or_else(|| pg("oid sequence returned nothing"))
}

fn must(v: Option<String>, kind: &str, name: &str) -> PgResult<String> {
    v.ok_or_else(|| pg(format!("{kind} \"{name}\" does not exist")))
}

fn missing(if_exists: bool, kind: &str, name: &str) -> PgResult<()> {
    if if_exists {
        Ok(())
    } else {
        Err(pg(format!("{kind} \"{name}\" does not exist")))
    }
}

fn version_expr(version: Option<i64>) -> String {
    match version {
        Some(v) => v.to_string(),
        None => "version + 1".into(),
    }
}

fn labels_sql(labels: &[String]) -> String {
    if labels.is_empty() {
        "NULL".into()
    } else {
        qlit(&serde_json::to_string(labels).unwrap_or_else(|_| "[]".into()))
    }
}

fn parse_labels(raw: &str) -> Vec<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Vec::new();
    }
    serde_json::from_str::<Vec<String>>(raw).unwrap_or_default()
}

fn cell(row: &[Option<String>], i: usize) -> String {
    row.get(i).and_then(|c| c.clone()).unwrap_or_default()
}

fn cell_opt(row: &[Option<String>], i: usize) -> Option<String> {
    row.get(i).and_then(|c| c.clone()).filter(|s| !s.is_empty())
}

fn opt_text(v: &Option<String>) -> String {
    match v {
        Some(s) => qlit(s),
        None => "NULL".into(),
    }
}

fn opt_i64(v: Option<i64>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "NULL".into())
}

fn opt_f64(v: Option<f64>) -> String {
    match v {
        Some(n) => {
            let s = serde_json::Number::from_f64(n).map(|n| n.to_string()).unwrap_or_else(|| "0".into());
            s
        }
        None => "NULL".into(),
    }
}

fn text_array(items: &[String]) -> String {
    if items.is_empty() {
        return "'{}'::text[]".into();
    }
    let inner = items.iter().map(|s| qlit(s)).collect::<Vec<_>>().join(", ");
    format!("ARRAY[{inner}]::text[]")
}

fn qident(name: &str) -> String {
    let simple = !name.is_empty()
        && name.chars().enumerate().all(|(i, c)| {
            c == '_' || c.is_ascii_lowercase() || (i > 0 && c.is_ascii_digit())
        });
    if simple {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

fn qlit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn dollar(s: &str) -> String {
    let mut tag = "rvbbit".to_string();
    while s.contains(&format!("${tag}$")) {
        tag.push('x');
    }
    format!("${tag}${s}${tag}$")
}

fn pg(message: impl Into<String>) -> Box<PgError> {
    Box::new(PgError::error(message.into()))
}

fn with_spi<T>(f: impl FnOnce() -> PgResult<T>) -> PgResult<T> {
    spi::SPI_connect()?;
    let result = f();
    let finished = spi::SPI_finish();
    match (result, finished) {
        (Ok(v), Ok(_)) => Ok(v),
        (Err(e), _) => Err(e),
        (Ok(_), Err(e)) => Err(e),
    }
}

fn exec(sql: &str) -> PgResult<()> {
    with_spi(|| {
        let code = spi::SPI_execute(sql, false, 0)?;
        if code < 0 {
            Err(pg(format!("SPI error {code} executing {sql}")))
        } else {
            Ok(())
        }
    })
}

fn scalar(sql: &str) -> PgResult<Option<String>> {
    let rows = query(sql)?;
    Ok(rows.into_iter().next().and_then(|r| r.into_iter().next().flatten()))
}

fn query(sql: &str) -> PgResult<Vec<Vec<Option<String>>>> {
    with_spi(|| {
        let code = spi::SPI_execute(sql, false, 0)?;
        if code != spi::SPI_OK_SELECT {
            return Err(pg(format!("expected a row set from {sql}")));
        }
        let n = spi::SPI_processed() as usize;
        let Some(h) = spi::SPI_tuptable() else {
            return Ok(Vec::new());
        };
        let cx = MemoryContext::new("rvbbit catalog");
        let mcx = cx.mcx();
        let mut failed: Option<Box<PgError>> = None;
        let rows = spi::tuptable_with(h, |t| {
            let cols = t.tupdesc.natts as i32;
            let mut rows = Vec::with_capacity(n);
            for i in 0..n {
                let mut row = Vec::with_capacity(cols as usize);
                for c in 1..=cols {
                    match spi::SPI_getvalue(mcx, &t.vals[i], &t.tupdesc, c) {
                        Ok(Some(bytes)) => row.push(Some(String::from_utf8_lossy(bytes).into_owned())),
                        Ok(None) => row.push(None),
                        Err(e) => {
                            failed = Some(e);
                            return rows;
                        }
                    }
                }
                rows.push(row);
            }
            rows
        });
        if let Some(e) = failed {
            return Err(e);
        }
        Ok(rows)
    })
}
