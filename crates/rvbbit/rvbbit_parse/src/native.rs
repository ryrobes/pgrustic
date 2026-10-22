//! Rewrite native semantic calls into ordinary function calls.
//!
//! `rvbbit.y` never sees a SELECT expression, and gram.y already accepts
//! `EXTRACT(ident FROM a_expr)` and `func(args)`. This pass only rewrites
//! spans gram.y cannot keep as datetime EXTRACT or as a plain function:
//! `CLASSIFY(expr INTO labels)` / `CLASSIFY(expr AS type)` and a typed
//! `EXTRACT`. Also `SUMMARIZE`, `CONSENSUS`, `RERANK`, `EXTRACT MANY`,
//! and `expr |> op`. A query with none of those forms is returned
//! unchanged, so regression text stays byte-identical. `|>>` is a
//! geometric operator and is not a pipe.
//!
//! `EXTRACT` is rewritten only for a scalar kind (`int`, `float`, `bool`,
//! `text`, and the aliases below) or when `ON ERROR` is present. Any other
//! field, including unknown units such as `FORTNIGHT`, stays the datetime
//! production. gram.y already accepts those idents and regress expects the
//! datetime error, not a call to `rvbbit.extract_typed`.

const DATETIME_FIELDS: &[&str] = &[
    "century",
    "day",
    "decade",
    "dow",
    "doy",
    "epoch",
    "hour",
    "isodow",
    "isoyear",
    "julian",
    "microseconds",
    "millennium",
    "milliseconds",
    "minute",
    "month",
    "quarter",
    "second",
    "timezone",
    "timezone_hour",
    "timezone_minute",
    "week",
    "year",
];

/// Kinds the typed extractor accepts without `ON ERROR`. Anything else
/// stays datetime EXTRACT unless the query opts in with `ON ERROR`.
const SEMANTIC_KINDS: &[&str] = &[
    "bigint",
    "bool",
    "boolean",
    "double",
    "float",
    "float4",
    "float8",
    "int",
    "int2",
    "int4",
    "int8",
    "integer",
    "numeric",
    "real",
    "smallint",
    "string",
    "text",
];

/// `None` when `sql` has no native semantic call. Callers must keep the
/// original string in that case.
pub fn rewrite_native_semantic(sql: &str) -> Option<String> {
    if !mentions(sql) {
        return None;
    }
    let out = lateralize_extract_many(&rewrite_all(sql));
    if out == sql {
        None
    } else {
        Some(out)
    }
}

fn mentions(sql: &str) -> bool {
    let lower = sql.to_ascii_lowercase();
    lower.contains("classify")
        || lower.contains("extract")
        || lower.contains("means")
        || lower.contains("summarize")
        || lower.contains("consensus")
        || lower.contains("rerank")
        || lower.contains("|>")
}

fn rewrite_all(sql: &str) -> String {
    let b = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'-' && i + 1 < b.len() && b[i + 1] == b'-' {
            let end = sql[i..].find('\n').map(|n| i + n).unwrap_or(b.len());
            out.push_str(&sql[i..end]);
            i = end;
            continue;
        }
        if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            let end = sql[i + 2..].find("*/").map(|n| i + 2 + n + 2).unwrap_or(b.len());
            out.push_str(&sql[i..end]);
            i = end;
            continue;
        }
        if b[i] == b'\'' {
            let end = end_quote(b, i);
            out.push_str(&sql[i..end]);
            i = end;
            continue;
        }
        if b[i] == b'"' {
            let end = end_dquote(b, i);
            out.push_str(&sql[i..end]);
            i = end;
            continue;
        }
        if b[i] == b'$' {
            if let Some(end) = end_dollar(b, i) {
                out.push_str(&sql[i..end]);
                i = end;
                continue;
            }
        }
        if is_ident_start(b[i]) && !ident_continues(b, i) {
            let start = i;
            i += 1;
            while i < b.len() && is_ident_cont(b[i]) {
                i += 1;
            }
            let word = &sql[start..i];
            let folded = word.to_ascii_lowercase();
            let next = skip_ws(b, i);
            if folded == "extract" {
                let many_at = skip_ws(b, i);
                if word_at(sql, many_at, "many") {
                    if let Some((end, rewritten)) = rewrite_extract_many(sql, many_at + 4, None) {
                        out.push_str(&rewritten);
                        i = end;
                        continue;
                    }
                }
            }
            if next < b.len()
                && b[next] == b'('
                && matches!(
                    folded.as_str(),
                    "classify" | "extract" | "summarize" | "consensus" | "rerank"
                )
            {
                if let Some((end, rewritten)) = rewrite_call(&folded, sql, next) {
                    out.push_str(&rewritten);
                    i = end;
                    continue;
                }
            }
            if folded == "means" {
                if let Some(end) = rewrite_means(sql, i, &mut out) {
                    i = end;
                    continue;
                }
            }
            out.push_str(word);
            continue;
        }
        if b[i] == b'|' && i + 1 < b.len() && b[i + 1] == b'>' && (i + 2 >= b.len() || b[i + 2] != b'>') {
            if let Some(end) = rewrite_pipe(sql, i, &mut out) {
                i = end;
                continue;
            }
        }
        let ch = sql[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn ident_continues(b: &[u8], i: usize) -> bool {
    i > 0 && (is_ident_cont(b[i - 1]) || b[i - 1] == b'.')
}

fn rewrite_call(kind: &str, sql: &str, paren: usize) -> Option<(usize, String)> {
    let b = sql.as_bytes();
    let body_start = paren + 1;
    let body_end = end_paren(b, paren)?;
    let body = rewrite_all(&sql[body_start..body_end]);
    let after = body_end + 1;
    let (fallback, mut end) = on_error(sql, after);
    let opted_in = fallback.is_some();
    let fallback_sql = fallback.unwrap_or_else(|| "NULL".into());
    let tail = clause_tail(sql, end);
    if let Some((tail_end, _)) = &tail {
        end = *tail_end;
    }
    if matches!(kind, "summarize" | "consensus" | "rerank") {
        if let Some(call) = rewrite_named_call(kind, &body, tail.as_ref().map(|(_, t)| t.as_str()).unwrap_or("")) {
            let call = if kind == "rerank" && tail.is_some() {
                alias_op(sql, end, &call, "rerank")
            } else {
                call
            };
            return Some((end, call));
        }
        return None;
    }
    if kind == "classify" {
        let (expr, labels, enum_name) = split_classify(&body)?;
        let call = if let Some(enum_name) = enum_name.as_deref() {
            format!(
                "rvbbit.classify_enum(({expr}), {name}, {fallback_sql})",
                name = sql_string(enum_name),
            )
        } else {
            format!(
                "rvbbit.classify(({expr}), {labels}, {fallback_sql})",
                labels = sql_string(&labels.join("\u{1f}")),
            )
        };
        if let Some((_, tail_sql)) = tail {
            let extra = if let Some(enum_name) = enum_name.as_deref() {
                sql_string(enum_name)
            } else {
                sql_string(&labels.join("\u{1f}"))
            };
            let call = typed_clause("classify", &format!("({expr})"), &extra, &tail_sql);
            return Some((end, alias_op(sql, end, &call, "classify")));
        }
        return Some((end, call));
    }
    let (field, expr) = split_extract(&body)?;
    // `ON ERROR` is not valid after datetime EXTRACT, so it opts into the
    // typed call even for a custom name like `creature`. A bare unknown
    // unit does not: regress uses those to assert the datetime error.
    if !opted_in && !is_semantic_kind(&field) {
        return None;
    }
    if is_datetime(&field) && !opted_in {
        return None;
    }
    if let Some((_, tail_sql)) = tail {
        let call = typed_clause("extract", &format!("({expr})"), &sql_string(&field), &tail_sql);
        return Some((end, alias_op(sql, end, &call, "extract")));
    }
    let call = format!(
        "rvbbit.extract_typed({field}, ({expr}), {fallback_sql})",
        field = sql_string(&field),
    );
    Some((end, call))
}

/// `MEANS` with a clause tail becomes `rvbbit.apply_clauses`. A plain
/// `MEANS` returns `None` and the caller keeps the operator.
fn rewrite_means(sql: &str, means_end: usize, out: &mut String) -> Option<usize> {
    let b = sql.as_bytes();
    let right_at = skip_ws(b, means_end);
    let right_end = simple_expr_end(sql, right_at)?;
    let (tail_end, tail) = clause_tail(sql, right_end)?;
    let left = pop_simple_expr(out)?;
    let right = sql[right_at..right_end].trim();
    let call = typed_clause("means", &format!("({left})"), &format!("({right})"), &tail);
    // A SELECT-list tail is a column and needs an alias. A WHERE / HAVING
    // / ON predicate cannot take one.
    let rendered = if predicate_context(sql, means_end) {
        call
    } else {
        alias_op(sql, tail_end, &call, "means")
    };
    out.push_str(&rendered);
    Some(tail_end)
}

fn last_kw_pos(sql: &str, at: usize, word: &str) -> Option<usize> {
    let head = sql[..at].to_ascii_lowercase();
    let b = head.as_bytes();
    let w = word.as_bytes();
    let mut from = 0usize;
    let mut found = None;
    while let Some(pos) = head[from..].find(word) {
        let i = from + pos;
        let before_ok = i == 0 || !is_ident_cont(b[i - 1]);
        let after = i + w.len();
        let after_ok = after >= b.len() || !is_ident_cont(b[after]);
        if before_ok && after_ok {
            found = Some(i);
        }
        from = i + word.len();
    }
    found
}

fn predicate_context(sql: &str, at: usize) -> bool {
    let pred = ["where", "having", "on"]
        .into_iter()
        .filter_map(|word| last_kw_pos(sql, at, word))
        .max()
        .unwrap_or(0);
    let select_at = last_kw_pos(sql, at, "select").unwrap_or(0);
    pred > select_at
}

fn rewrite_named_call(kind: &str, body: &str, clauses: &str) -> Option<String> {
    let args = split_top_commas(body);
    match kind {
        "summarize" if args.len() == 1 => Some(format!(
            "rvbbit.summarize(({}), {})",
            args[0].trim(),
            sql_string(clauses)
        )),
        "consensus" if args.len() == 2 => Some(format!(
            "rvbbit.consensus(({}), ({}), {})",
            args[0].trim(),
            args[1].trim(),
            sql_string(clauses)
        )),
        "rerank" if args.len() == 2 && clauses.is_empty() => {
            Some(format!("rvbbit.rerank(({}), ({}))", args[0].trim(), args[1].trim()))
        }
        "rerank" if args.len() == 2 => Some(match model_only(clauses) {
            Some(model) => format!(
                "rvbbit.rerank_for(({}), ({}), {})",
                args[0].trim(),
                args[1].trim(),
                sql_string(&model)
            ),
            None => typed_clause(
                "rerank",
                &format!("({})", args[0].trim()),
                &format!("({})", args[1].trim()),
                clauses,
            ),
        }),
        _ => None,
    }
}

/// `EXTRACT MANY kind FROM expr`, or the pipe form whose input is already known.
fn rewrite_extract_many(sql: &str, after_many: usize, piped: Option<&str>) -> Option<(usize, String)> {
    let b = sql.as_bytes();
    let kind_at = skip_ws(b, after_many);
    let (kind_end, kind) = take_kind(sql, kind_at)?;
    let (expr, mut end) = if let Some(piped) = piped {
        (piped.to_string(), kind_end)
    } else {
        let from_at = skip_ws(b, kind_end);
        if !word_at(sql, from_at, "from") {
            return None;
        }
        let expr_at = skip_ws(b, from_at + 4);
        let expr_end = simple_expr_end(sql, expr_at)?;
        (sql[expr_at..expr_end].trim().to_string(), expr_end)
    };
    let tail = clause_tail(sql, end);
    let clauses = tail.as_ref().map(|(_, t)| t.as_str()).unwrap_or("");
    if let Some((tail_end, _)) = &tail {
        end = *tail_end;
    }
    // The call stays a scalar JSON array so a Semantic Project can batch it.
    // `lateralize_extract_many` explodes that array per source row afterwards.
    // `REPORT` keeps the JSON clause report and does not explode.
    let call = if clauses.is_empty() {
        format!("rvbbit.extract_many({}, ({}), '')", sql_string(&kind), expr)
    } else if tail_reports(clauses) {
        let call = typed_clause("extract_many", &format!("({expr})"), &sql_string(&kind), clauses);
        alias_op(sql, end, &call, "extract_many")
    } else {
        typed_clause("extract_many", &format!("({expr})"), &sql_string(&kind), clauses)
    };
    Some((end, call))
}

fn rewrite_pipe(sql: &str, pipe: usize, out: &mut String) -> Option<usize> {
    let b = sql.as_bytes();
    if pipe + 2 < b.len() && b[pipe + 2] == b'>' {
        return None;
    }
    let verb_at = skip_ws(b, pipe + 2);
    let verb_end = ident_end(b, verb_at)?;
    let verb = sql[verb_at..verb_end].to_ascii_lowercase();
    let known = matches!(
        verb.as_str(),
        "means" | "means_score" | "extract" | "classify" | "rerank" | "summarize" | "consensus"
    );
    let left = pop_simple_expr(out)?;
    if !known {
        let Some((end, sql_out)) = pipe_user(sql, verb_end, &left, &verb) else {
            out.push_str(&left);
            return None;
        };
        out.push_str(&sql_out);
        return Some(end);
    }
    let rewritten = match verb.as_str() {
        "means" | "means_score" | "consensus" => pipe_means(sql, verb_end, &left, &verb),
        "rerank" => pipe_binary(sql, verb_end, &left, "rerank"),
        "summarize" => pipe_summarize(sql, verb_end, &left),
        "extract" => pipe_extract(sql, verb_end, &left),
        "classify" => pipe_classify(sql, verb_end, &left),
        _ => None,
    };
    let Some((end, sql_out)) = rewritten else {
        out.push_str(&left);
        return None;
    };
    out.push_str(&sql_out);
    Some(end)
}

fn pipe_user(sql: &str, at: usize, left: &str, name: &str) -> Option<(usize, String)> {
    let b = sql.as_bytes();
    let mut end = at;
    let mut extras = Vec::new();
    loop {
        let arg_at = skip_ws(b, end);
        if arg_at >= b.len() {
            break;
        }
        if b[arg_at] == b',' || b[arg_at] == b')' || b[arg_at] == b';' || b[arg_at] == b'|' {
            break;
        }
        if is_ident_start(b[arg_at]) {
            let word_end = ident_end(b, arg_at)?;
            let word = sql[arg_at..word_end].to_ascii_lowercase();
            if matches!(
                word.as_str(),
                "from" | "where" | "group" | "order" | "limit" | "union" | "having"
                    | "window" | "offset" | "fetch" | "as" | "with" | "join" | "on"
            ) {
                break;
            }
        }
        let expr_end = simple_expr_end(sql, arg_at)?;
        if expr_end == arg_at {
            break;
        }
        extras.push(sql[arg_at..expr_end].trim().to_string());
        end = expr_end;
        let comma = skip_ws(b, end);
        if comma < b.len() && b[comma] == b',' {
            end = comma + 1;
            continue;
        }
        break;
    }
    let mut call = format!("rvbbit.{name}(({left})");
    for extra in extras {
        call.push_str(", (");
        call.push_str(&extra);
        call.push(')');
    }
    call.push(')');
    Some((end, call))
}

fn pipe_means(sql: &str, at: usize, left: &str, op: &str) -> Option<(usize, String)> {
    let b = sql.as_bytes();
    let arg_at = skip_ws(b, at);
    let arg_end = simple_expr_end(sql, arg_at)?;
    let right = sql[arg_at..arg_end].trim();
    let tail = clause_tail(sql, arg_end);
    if let Some((end, clauses)) = tail {
        let call = typed_clause(op, &format!("({left})"), &format!("({right})"), &clauses);
        return Some((end, alias_op(sql, end, &call, op)));
    }
    if op == "means_score" {
        return Some((arg_end, format!("rvbbit.means_score(({left}), ({right}))")));
    }
    // A bare `|> CONSENSUS` is still a yes/no MEANS. Consensus-the-aggregate
    // needs the group, which a pipe does not have.
    Some((arg_end, format!("({left}) MEANS ({right})")))
}

fn pipe_binary(sql: &str, at: usize, left: &str, kind: &str) -> Option<(usize, String)> {
    let b = sql.as_bytes();
    let arg_at = skip_ws(b, at);
    let arg_end = simple_expr_end(sql, arg_at)?;
    let right = sql[arg_at..arg_end].trim();
    let tail = clause_tail(sql, arg_end);
    if let Some((end, clauses)) = tail {
        let call = if kind == "rerank" {
            match model_only(&clauses) {
                Some(model) => format!(
                    "rvbbit.rerank_for(({left}), ({right}), {})",
                    sql_string(&model)
                ),
                None => typed_clause(kind, &format!("({left})"), &format!("({right})"), &clauses),
            }
        } else {
            typed_clause(kind, &format!("({left})"), &format!("({right})"), &clauses)
        };
        return Some((end, alias_op(sql, end, &call, kind)));
    }
    let call = if kind == "rerank" {
        format!("rvbbit.rerank(({left}), ({right}))")
    } else {
        format!("rvbbit.{kind}(({left}), ({right}))")
    };
    Some((arg_end, call))
}

fn pipe_summarize(sql: &str, at: usize, left: &str) -> Option<(usize, String)> {
    let tail = clause_tail(sql, at);
    let (end, clauses) = match tail {
        Some((end, clauses)) => (end, clauses),
        None => (at, String::new()),
    };
    let call = format!("rvbbit.summarize_text(({left}), {})", sql_string(&clauses));
    Some((end, alias_op(sql, end, &call, "summarize")))
}

fn pipe_extract(sql: &str, at: usize, left: &str) -> Option<(usize, String)> {
    let b = sql.as_bytes();
    let next = skip_ws(b, at);
    if word_at(sql, next, "many") {
        return rewrite_extract_many(sql, next + 4, Some(left));
    }
    let (kind_end, kind) = take_kind(sql, next)?;
    let (fallback, mut end) = on_error(sql, kind_end);
    let fallback_sql = fallback.unwrap_or_else(|| "NULL".into());
    let tail = clause_tail(sql, end);
    if let Some((tail_end, clauses)) = tail {
        end = tail_end;
        let call = typed_clause("extract", &format!("({left})"), &sql_string(&kind), &clauses);
        return Some((end, alias_op(sql, end, &call, "extract")));
    }
    Some((
        end,
        format!(
            "rvbbit.extract_typed({}, ({left}), {fallback_sql})",
            sql_string(&kind)
        ),
    ))
}

fn pipe_classify(sql: &str, at: usize, left: &str) -> Option<(usize, String)> {
    let b = sql.as_bytes();
    let mut i = skip_ws(b, at);
    let mut labels = Vec::new();
    loop {
        let end = simple_expr_end(sql, i)?;
        labels.push(literal_text(sql[i..end].trim()));
        i = skip_ws(b, end);
        if i < b.len() && b[i] == b',' {
            i = skip_ws(b, i + 1);
            continue;
        }
        break;
    }
    if labels.len() < 2 {
        return None;
    }
    let joined = labels.join("\u{1f}");
    let tail = clause_tail(sql, i);
    if let Some((end, clauses)) = tail {
        let call = typed_clause("classify", &format!("({left})"), &sql_string(&joined), &clauses);
        return Some((end, alias_op(sql, end, &call, "classify")));
    }
    Some((
        i,
        format!("rvbbit.classify(({left}), {}, NULL)", sql_string(&joined)),
    ))
}

fn take_kind(sql: &str, at: usize) -> Option<(usize, String)> {
    let end = simple_expr_end(sql, at)?;
    let kind = literal_text(sql[at..end].trim());
    if kind.is_empty() {
        return None;
    }
    Some((end, kind))
}

fn literal_text(token: &str) -> String {
    let t = token.trim();
    if t.len() >= 2 && t.starts_with('\'') && t.ends_with('\'') {
        t[1..t.len() - 1].replace("''", "'")
    } else {
        t.to_string()
    }
}

fn ident_end(b: &[u8], i: usize) -> Option<usize> {
    if i >= b.len() || !is_ident_start(b[i]) {
        return None;
    }
    let mut j = i + 1;
    while j < b.len() && is_ident_cont(b[j]) {
        j += 1;
    }
    Some(j)
}

fn split_top_commas(body: &str) -> Vec<String> {
    let b = body.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    let mut depth = 0i32;
    while i < b.len() {
        if b[i] == b'\'' {
            i = end_quote(b, i);
            continue;
        }
        if b[i] == b'(' {
            depth += 1;
        } else if b[i] == b')' {
            depth -= 1;
        } else if b[i] == b',' && depth == 0 {
            out.push(body[start..i].trim().to_string());
            start = i + 1;
        }
        i += 1;
    }
    let last = body[start..].trim();
    if !last.is_empty() {
        out.push(last.to_string());
    }
    out
}

fn clause_tail(sql: &str, at: usize) -> Option<(usize, String)> {
    let b = sql.as_bytes();
    let start = skip_ws(b, at);
    if start >= b.len() {
        return None;
    }
    let head = sql[start..].trim_start().to_ascii_lowercase();
    let starts = [
        "using model",
        "with takes",
        "with budget",
        "with temperature",
        "with phrasings",
        "with max",
        "with across",
        "pick by",
        "retry ",
        "check input",
        "check output",
        "no cache",
        "refresh",
        "timeout ",
        "budget ",
        "cache",
        "threshold ",
    ];
    if !starts.iter().any(|k| head.starts_with(k)) {
        return None;
    }
    let end = clause_end(sql, start);
    let tail = sql[start..end].trim().trim_end_matches(';').trim().to_string();
    if tail.is_empty() {
        return None;
    }
    Some((end, tail))
}

fn clause_end(sql: &str, start: usize) -> usize {
    let b = sql.as_bytes();
    let mut i = start;
    let mut depth = 0i32;
    while i < b.len() {
        if b[i] == b'\'' {
            i = end_quote(b, i);
            continue;
        }
        if b[i] == b'(' {
            depth += 1;
            i += 1;
            continue;
        }
        if b[i] == b')' {
            if depth == 0 {
                return i;
            }
            depth -= 1;
            i += 1;
            continue;
        }
        if depth == 0 && b[i] == b',' {
            return i;
        }
        if depth == 0 && b[i] == b';' {
            return i;
        }
        if depth == 0 && is_ident_start(b[i]) && (i == 0 || !is_ident_cont(b[i - 1])) {
            let word_at = i;
            i += 1;
            while i < b.len() && is_ident_cont(b[i]) {
                i += 1;
            }
            let word = sql[word_at..i].to_ascii_lowercase();
            if word_at > start && is_clause_stop(&word) {
                // Keep the space before AS / FROM. The tail is trimmed, so
                // the clause text stays the same and the next token does
                // not glue onto the rewritten call.
                let mut ws = word_at;
                while ws > start && b[ws - 1].is_ascii_whitespace() {
                    ws -= 1;
                }
                return ws;
            }
            continue;
        }
        i += 1;
    }
    b.len()
}

fn is_clause_stop(word: &str) -> bool {
    matches!(
        word,
        "as" | "from" | "where" | "group" | "order" | "limit" | "union" | "offset"
            | "window" | "having" | "and" | "or" | "join" | "intersect" | "except"
    )
}

/// `REPORT` keeps the JSON clause report. Every other tail returns the
/// op's natural type: bool, score, or text. The column is named after the
/// op unless the caller already wrote `AS`.
fn typed_clause(op: &str, input_sql: &str, extra_sql: &str, tail: &str) -> String {
    let func = if tail_reports(tail) {
        "apply_clauses"
    } else {
        match op {
            "rerank" | "means_score" => "clause_score",
            "means" | "consensus" => "clause_bool",
            _ => "clause_text",
        }
    };
    format!(
        "rvbbit.{func}('{op}', {input_sql}, {extra_sql}, {})",
        sql_string(tail)
    )
}

fn alias_op(sql: &str, end: usize, call: &str, op: &str) -> String {
    let b = sql.as_bytes();
    let next = skip_ws(b, end);
    // `AS` is the caller's alias. A `)` means the call is still inside
    // parentheses, where a column alias would not parse.
    if word_at(sql, next, "as") || (next < b.len() && b[next] == b')') {
        call.to_string()
    } else {
        format!("{call} AS {op}")
    }
}

/// `USING MODEL 'slug'` with nothing else becomes `rerank_for`, so the
/// project batcher can send every document in one call. Any other tail
/// stays on the clause functions.
fn model_only(tail: &str) -> Option<String> {
    let b = tail.as_bytes();
    let mut i = skip_ws(b, 0);
    if !word_at(tail, i, "using") {
        return None;
    }
    i = skip_ws(b, i + 5);
    if !word_at(tail, i, "model") {
        return None;
    }
    i = skip_ws(b, i + 5);
    let (model, next) = if i < b.len() && b[i] == b'\'' {
        let end = end_quote(b, i);
        (literal_text(&tail[i..end]), end)
    } else {
        let start = i;
        while i < b.len() && is_model_char(b[i]) {
            i += 1;
        }
        if start == i {
            return None;
        }
        (tail[start..i].to_string(), i)
    };
    if !tail[next..].trim().is_empty() || model.is_empty() {
        return None;
    }
    Some(model)
}

fn is_model_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'/' | b'.' | b':' | b'-' | b'_' | b'@')
}

fn tail_reports(tail: &str) -> bool {
    let b = tail.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\'' {
            i = end_quote(b, i);
            continue;
        }
        if is_ident_start(b[i]) && (i == 0 || !is_ident_cont(b[i - 1])) {
            let start = i;
            i += 1;
            while i < b.len() && is_ident_cont(b[i]) {
                i += 1;
            }
            if tail[start..i].eq_ignore_ascii_case("report") {
                return true;
            }
            continue;
        }
        i += 1;
    }
    false
}

/// `EXTRACT MANY` is a scalar JSON array in the subquery, then a
/// `LEFT JOIN LATERAL` so each value stays on its source row. An empty
/// array keeps the row and yields NULL. `AS eN_list` marks a call this
/// pass already wrapped, so a second rewrite does not nest another join.
fn lateralize_extract_many(sql: &str) -> String {
    if !sql.contains("rvbbit.extract_many") && !sql.contains("rvbbit.clause_text") {
        return sql.to_string();
    }
    let mut sql = sql.to_string();
    for _ in 0..64 {
        let regions = select_regions(&sql);
        let Some(region) = regions.into_iter().rev().find(|r| select_needs_lateral(&sql, r)) else {
            break;
        };
        sql = rewrite_select_lateral(&sql, &region);
    }
    sql
}

struct SelectRegion {
    kw_start: usize,
    kw_end: usize,
    list_end: usize,
    stmt_end: usize,
    union: bool,
}

fn select_regions(sql: &str) -> Vec<SelectRegion> {
    let b = sql.as_bytes();
    let mut i = 0usize;
    let mut depth = 0i32;
    let mut out = Vec::new();
    while i < b.len() {
        if b[i] == b'-' && i + 1 < b.len() && b[i + 1] == b'-' {
            i = sql[i..].find('\n').map(|n| i + n).unwrap_or(b.len());
            continue;
        }
        if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            i = sql[i + 2..].find("*/").map(|n| i + 2 + n + 2).unwrap_or(b.len());
            continue;
        }
        if b[i] == b'\'' {
            i = end_quote(b, i);
            continue;
        }
        if b[i] == b'"' {
            i = end_dquote(b, i);
            continue;
        }
        if b[i] == b'$' {
            if let Some(end) = end_dollar(b, i) {
                i = end;
                continue;
            }
        }
        if b[i] == b'(' {
            depth += 1;
            i += 1;
            continue;
        }
        if b[i] == b')' {
            depth -= 1;
            i += 1;
            continue;
        }
        if is_ident_start(b[i]) && (i == 0 || !is_ident_cont(b[i - 1])) && word_at(sql, i, "select") {
            let kw_start = i;
            let kw_end = i + 6;
            let (list_end, stmt_end, is_union) = scan_select_end(sql, kw_end, depth);
            out.push(SelectRegion {
                kw_start,
                kw_end,
                list_end,
                stmt_end,
                union: is_union,
            });
            i = kw_end;
            continue;
        }
        i += 1;
    }
    out
}

fn scan_select_end(sql: &str, from: usize, depth0: i32) -> (usize, usize, bool) {
    let b = sql.as_bytes();
    let mut i = from;
    let mut depth = depth0;
    let mut list_end = None;
    let mut is_union = false;
    while i < b.len() {
        if b[i] == b'-' && i + 1 < b.len() && b[i + 1] == b'-' {
            i = sql[i..].find('\n').map(|n| i + n).unwrap_or(b.len());
            continue;
        }
        if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            i = sql[i + 2..].find("*/").map(|n| i + 2 + n + 2).unwrap_or(b.len());
            continue;
        }
        if b[i] == b'\'' {
            i = end_quote(b, i);
            continue;
        }
        if b[i] == b'"' {
            i = end_dquote(b, i);
            continue;
        }
        if b[i] == b'$' {
            if let Some(end) = end_dollar(b, i) {
                i = end;
                continue;
            }
        }
        if b[i] == b'(' {
            depth += 1;
            i += 1;
            continue;
        }
        if b[i] == b')' {
            if depth == depth0 {
                // This paren closes the query that contains the SELECT.
                // A FROM clause runs up to the paren; a bare select list ends here.
                return (list_end.unwrap_or(i), i, is_union);
            }
            depth -= 1;
            i += 1;
            continue;
        }
        if depth == depth0 && b[i] == b';' {
            return (list_end.unwrap_or(i), i, is_union);
        }
        if depth == depth0 && is_ident_start(b[i]) && (i == 0 || !is_ident_cont(b[i - 1])) {
            let start = i;
            i += 1;
            while i < b.len() && is_ident_cont(b[i]) {
                i += 1;
            }
            let word = sql[start..i].to_ascii_lowercase();
            if list_end.is_none()
                && matches!(
                    word.as_str(),
                    "from" | "where" | "group" | "order" | "limit" | "offset" | "window" | "having"
                        | "union" | "intersect" | "except"
                )
            {
                list_end = Some(start);
                is_union = matches!(word.as_str(), "union" | "intersect" | "except");
            }
            if list_end.is_some() && matches!(word.as_str(), "union" | "intersect" | "except") {
                return (list_end.unwrap(), start, true);
            }
            continue;
        }
        i += 1;
    }
    let end = list_end.unwrap_or(b.len());
    let stmt = if is_union { list_end.unwrap_or(b.len()) } else { b.len() };
    (end, stmt, is_union)
}

fn select_needs_lateral(sql: &str, region: &SelectRegion) -> bool {
    if region.union {
        return false;
    }
    select_items(sql, region).iter().any(|item| extract_many_item(item).is_some())
}

fn select_items(sql: &str, region: &SelectRegion) -> Vec<String> {
    let list = sql[region.kw_end..region.list_end].trim();
    let list = strip_quantifier(list);
    split_top_commas(list)
}

fn strip_quantifier(list: &str) -> &str {
    let trimmed = list.trim_start();
    if word_at(trimmed, 0, "distinct") {
        return trimmed[8..].trim_start();
    }
    if word_at(trimmed, 0, "all") {
        return trimmed[3..].trim_start();
    }
    trimmed
}

fn rewrite_select_lateral(sql: &str, region: &SelectRegion) -> String {
    let raw_list = sql[region.kw_end..region.list_end].trim();
    let distinct = word_at(raw_list.trim_start(), 0, "distinct");
    let items = select_items(sql, region);
    let mut extracts = Vec::new();
    let mut others = Vec::new();
    for item in &items {
        if let Some(ex) = extract_many_item(item) {
            extracts.push(ex);
        } else if !item.trim().is_empty() {
            others.push(item.trim().to_string());
        }
    }
    let has_from = word_at(sql, skip_ws(sql.as_bytes(), region.list_end), "from");
    let mut value_cols = Vec::new();
    let mut list_cols = Vec::new();
    let mut joins = String::new();
    for (i, ex) in extracts.iter().enumerate() {
        let list_name = format!("e{i}_list");
        // The list column stays inside the subquery. The outer select
        // projects source columns and the exploded value, not eN_list.
        list_cols.push(format!("{} AS {list_name}", ex.call));
        let value_name = ex.alias.clone().unwrap_or_else(|| sql_ident(&ex.kind));
        value_cols.push(format!("e{i}.value AS {value_name}"));
        joins.push_str(&format!(
            " LEFT JOIN LATERAL jsonb_array_elements_text(q.{list_name}::jsonb) AS e{i}(value) ON true"
        ));
    }
    let quant = if distinct { "DISTINCT " } else { "" };
    let rest = sql[region.list_end..region.stmt_end].trim();
    let star = has_from && (others.is_empty() || others.iter().any(|item| is_star_item(item)));
    let mut inner_items = Vec::new();
    let mut outer_items = Vec::new();
    if star {
        // `s` is the source row. `(q.src).*` expands those columns and
        // leaves the helper list column behind.
        inner_items.push("s AS src".to_string());
        outer_items.push("(q.src).*".to_string());
    }
    for (i, item) in others.iter().filter(|item| !is_star_item(item)).enumerate() {
        let (sql_item, name) = named_projection(item, i);
        inner_items.push(sql_item);
        outer_items.push(format!("q.{name}"));
    }
    inner_items.extend(list_cols);
    outer_items.extend(value_cols);
    let inner = if star {
        format!("SELECT {} FROM (SELECT * {rest}) s", inner_items.join(", "))
    } else if rest.is_empty() {
        format!("SELECT {}", inner_items.join(", "))
    } else {
        format!("SELECT {} {rest}", inner_items.join(", "))
    };
    let rewritten = format!(
        "SELECT {quant}{} FROM ({inner}) q{joins}",
        outer_items.join(", ")
    );
    let mut out = String::with_capacity(sql.len() + rewritten.len());
    out.push_str(&sql[..region.kw_start]);
    out.push_str(&rewritten);
    out.push_str(&sql[region.stmt_end..]);
    out
}

struct ExtractProj {
    call: String,
    kind: String,
    alias: Option<String>,
}

fn extract_many_item(item: &str) -> Option<ExtractProj> {
    let (call, alias) = split_trailing_alias(item);
    if alias.as_deref().is_some_and(is_list_alias) {
        return None;
    }
    if let Some(body) = prefixed_call(&call, "rvbbit.extract_many") {
        let args = split_top_commas(body);
        let kind = args.first().map(|s| literal_text(s.trim())).unwrap_or_default();
        if kind.is_empty() {
            return None;
        }
        return Some(ExtractProj { call, kind, alias });
    }
    if let Some(body) = prefixed_call(&call, "rvbbit.clause_text") {
        let args = split_top_commas(body);
        if args.len() >= 3 && literal_text(args[0].trim()) == "extract_many" {
            let kind = literal_text(args[2].trim());
            if kind.is_empty() {
                return None;
            }
            return Some(ExtractProj { call, kind, alias });
        }
    }
    None
}

fn is_list_alias(alias: &str) -> bool {
    let name = alias.trim_matches('"');
    let Some(rest) = name.strip_prefix('e') else {
        return false;
    };
    let Some(n) = rest.strip_suffix("_list") else {
        return false;
    };
    !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())
}

fn prefixed_call<'a>(expr: &'a str, name: &str) -> Option<&'a str> {
    let expr = expr.trim();
    if expr.len() < name.len() + 2 || !expr[..name.len()].eq_ignore_ascii_case(name) {
        return None;
    }
    let rest = expr[name.len()..].trim_start();
    if !rest.starts_with('(') || !rest.ends_with(')') {
        return None;
    }
    let b = rest.as_bytes();
    let end = end_paren(b, 0)?;
    if end + 1 != b.len() {
        return None;
    }
    Some(&rest[1..end])
}

fn split_trailing_alias(item: &str) -> (String, Option<String>) {
    let b = item.as_bytes();
    let mut i = 0usize;
    let mut depth = 0i32;
    let mut as_at = None;
    while i < b.len() {
        if b[i] == b'\'' {
            i = end_quote(b, i);
            continue;
        }
        if b[i] == b'(' {
            depth += 1;
            i += 1;
            continue;
        }
        if b[i] == b')' {
            depth -= 1;
            i += 1;
            continue;
        }
        if depth == 0 && word_at(item, i, "as") {
            as_at = Some(i);
        }
        i += 1;
    }
    let Some(at) = as_at else {
        return (item.trim().to_string(), None);
    };
    let name_at = skip_ws(b, at + 2);
    if name_at >= b.len() {
        return (item.trim().to_string(), None);
    }
    let (alias_sql, after) = if b[name_at] == b'"' {
        let end = end_dquote(b, name_at);
        (item[name_at..end].to_string(), end)
    } else if let Some(end) = ident_end(b, name_at) {
        (item[name_at..end].to_string(), end)
    } else {
        return (item.trim().to_string(), None);
    };
    if item[after..].trim().is_empty() {
        (item[..at].trim().to_string(), Some(alias_sql))
    } else {
        (item.trim().to_string(), None)
    }
}

fn is_star_item(item: &str) -> bool {
    let item = item.trim();
    if item == "*" {
        return true;
    }
    let Some(prefix) = item.strip_suffix(".*") else {
        return false;
    };
    !prefix.is_empty() && prefix.bytes().all(|c| is_ident_cont(c) || c == b'.')
}

/// `(sql, output name)` so the outer select can name the column and skip
/// the extract-many helper.
fn named_projection(item: &str, index: usize) -> (String, String) {
    let (expr, alias) = split_trailing_alias(item);
    if let Some(alias) = alias {
        let name = alias.trim().trim_matches('"').to_string();
        return (format!("{} AS {alias}", expr.trim()), name);
    }
    let expr = expr.trim();
    if is_plain_ident(expr) {
        return (expr.to_string(), expr.to_string());
    }
    let name = format!("c{index}");
    (format!("{expr} AS {name}"), name)
}

fn is_plain_ident(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty() && is_ident_start(b[0]) && b.iter().copied().all(is_ident_cont)
}

fn sql_ident(name: &str) -> String {
    let plain = !name.is_empty()
        && name.bytes().next().is_some_and(is_ident_start)
        && name.bytes().all(is_ident_cont);
    if plain {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

fn simple_expr_end(sql: &str, at: usize) -> Option<usize> {
    let b = sql.as_bytes();
    if at >= b.len() {
        return None;
    }
    if b[at] == b'\'' {
        return Some(end_quote(b, at));
    }
    if b[at] == b'(' {
        return end_paren(b, at).map(|i| i + 1);
    }
    if is_ident_start(b[at]) {
        let mut i = at + 1;
        while i < b.len() && (is_ident_cont(b[i]) || b[i] == b'.') {
            i += 1;
        }
        let next = skip_ws(b, i);
        if next < b.len() && b[next] == b'(' {
            return end_paren(b, next).map(|n| n + 1);
        }
        return Some(i);
    }
    None
}

fn pop_simple_expr(out: &mut String) -> Option<String> {
    let trimmed = out.trim_end();
    let drop_ws = out.len() - trimmed.len();
    if drop_ws > 0 {
        out.truncate(out.len() - drop_ws);
    }
    let b = out.as_bytes();
    if b.is_empty() {
        return None;
    }
    if b[b.len() - 1] == b')' {
        let mut depth = 0i32;
        let mut i = b.len();
        while i > 0 {
            i -= 1;
            if b[i] == b')' {
                depth += 1;
            } else if b[i] == b'(' {
                depth -= 1;
                if depth == 0 {
                    while i > 0 && (is_ident_cont(b[i - 1]) || b[i - 1] == b'.') {
                        i -= 1;
                    }
                    let expr = out[i..].trim().to_string();
                    out.truncate(i);
                    return Some(expr);
                }
            }
        }
        return None;
    }
    if b[b.len() - 1] == b'\'' {
        let mut i = b.len() - 1;
        while i > 0 {
            i -= 1;
            if b[i] == b'\'' {
                let expr = out[i..].to_string();
                out.truncate(i);
                return Some(expr);
            }
        }
        return None;
    }
    let mut i = b.len();
    while i > 0 && (is_ident_cont(b[i - 1]) || b[i - 1] == b'.') {
        i -= 1;
    }
    if i == b.len() {
        return None;
    }
    let expr = out[i..].to_string();
    out.truncate(i);
    Some(expr)
}

fn split_classify(body: &str) -> Option<(String, Vec<String>, Option<String>)> {
    let (expr, rest, mode) = split_kw(body, &["into", "as"])?;
    let expr = expr.trim().to_string();
    if expr.is_empty() {
        return None;
    }
    if mode == "as" {
        let name = unquote_one(rest.trim())?;
        if name.is_empty() {
            return None;
        }
        return Some((expr, Vec::new(), Some(name)));
    }
    let labels = split_labels(rest.trim());
    if labels.len() < 2 {
        return None;
    }
    Some((expr, labels, None))
}

fn split_extract(body: &str) -> Option<(String, String)> {
    let (field, expr, mode) = split_kw(body, &["from"])?;
    if mode != "from" {
        return None;
    }
    let field = unquote_one(field.trim())?;
    let expr = expr.trim().to_string();
    if field.is_empty() || expr.is_empty() {
        return None;
    }
    Some((field, expr))
}

/// Split `body` on the first depth-0 keyword in `words`.
fn split_kw(body: &str, words: &[&str]) -> Option<(String, String, String)> {
    let b = body.as_bytes();
    let mut i = 0;
    let mut depth = 0i32;
    while i < b.len() {
        if b[i] == b'\'' {
            i = end_quote(b, i);
            continue;
        }
        if b[i] == b'(' {
            depth += 1;
            i += 1;
            continue;
        }
        if b[i] == b')' {
            depth -= 1;
            i += 1;
            continue;
        }
        if depth == 0 && is_ident_start(b[i]) && (i == 0 || !is_ident_cont(b[i - 1])) {
            let start = i;
            i += 1;
            while i < b.len() && is_ident_cont(b[i]) {
                i += 1;
            }
            let word = body[start..i].to_ascii_lowercase();
            if words.contains(&word.as_str()) {
                return Some((body[..start].to_string(), body[i..].to_string(), word));
            }
            continue;
        }
        i += 1;
    }
    None
}

fn split_labels(rest: &str) -> Vec<String> {
    let rest = rest.trim().trim_start_matches('(').trim_end_matches(')').trim();
    let b = rest.as_bytes();
    let mut labels = Vec::new();
    let mut i = 0;
    let mut start = 0;
    let mut depth = 0i32;
    while i < b.len() {
        if b[i] == b'\'' {
            i = end_quote(b, i);
            continue;
        }
        if b[i] == b'(' {
            depth += 1;
        } else if b[i] == b')' {
            depth -= 1;
        } else if b[i] == b',' && depth == 0 {
            if let Some(label) = unquote_one(rest[start..i].trim()) {
                labels.push(label);
            }
            start = i + 1;
        }
        i += 1;
    }
    if let Some(label) = unquote_one(rest[start..].trim()) {
        labels.push(label);
    }
    labels
}

fn unquote_one(raw: &str) -> Option<String> {
    let raw = raw.trim().trim_end_matches(';').trim();
    if raw.is_empty() {
        return None;
    }
    if raw.starts_with('\'') && raw.ends_with('\'') && raw.len() >= 2 {
        return Some(raw[1..raw.len() - 1].replace("''", "'"));
    }
    if is_ident_start(raw.as_bytes()[0]) && raw.bytes().all(is_ident_cont) {
        return Some(raw.to_string());
    }
    None
}

fn on_error(sql: &str, mut i: usize) -> (Option<String>, usize) {
    let b = sql.as_bytes();
    let start = i;
    i = skip_ws(b, i);
    if !word_at(sql, i, "on") {
        return (None, start);
    }
    i = skip_ws(b, i + 2);
    if !word_at(sql, i, "error") {
        return (None, start);
    }
    i = skip_ws(b, i + 5);
    if i >= b.len() {
        return (None, start);
    }
    if b[i] == b'(' {
        let end = match end_paren(b, i) {
            Some(end) => end + 1,
            None => return (None, start),
        };
        return (Some(sql[i..end].to_string()), end);
    }
    if b[i] == b'\'' {
        let end = end_quote(b, i);
        return (Some(sql[i..end].to_string()), end);
    }
    if is_ident_start(b[i]) || b[i].is_ascii_digit() || b[i] == b'-' || b[i] == b'.' {
        let s = i;
        i += 1;
        while i < b.len() && (is_ident_cont(b[i]) || b[i] == b'.') {
            i += 1;
        }
        return (Some(sql[s..i].to_string()), i);
    }
    (None, start)
}

fn is_datetime(field: &str) -> bool {
    DATETIME_FIELDS.contains(&field.to_ascii_lowercase().as_str())
}

fn is_semantic_kind(field: &str) -> bool {
    SEMANTIC_KINDS.contains(&field.to_ascii_lowercase().as_str())
}

fn sql_string(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn word_at(sql: &str, i: usize, word: &str) -> bool {
    let b = sql.as_bytes();
    let w = word.as_bytes();
    if i + w.len() > b.len() {
        return false;
    }
    if !b[i..i + w.len()].eq_ignore_ascii_case(w) {
        return false;
    }
    let after = i + w.len();
    (i == 0 || !is_ident_cont(b[i - 1])) && (after == b.len() || !is_ident_cont(b[after]))
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

fn end_paren(b: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = open;
    while i < b.len() {
        if b[i] == b'\'' {
            i = end_quote(b, i);
            continue;
        }
        if b[i] == b'(' {
            depth += 1;
        } else if b[i] == b')' {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

fn end_quote(b: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while i < b.len() {
        if b[i] == b'\'' {
            if i + 1 < b.len() && b[i + 1] == b'\'' {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    b.len()
}

fn end_dquote(b: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while i < b.len() {
        if b[i] == b'"' {
            if i + 1 < b.len() && b[i + 1] == b'"' {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    b.len()
}

fn end_dollar(b: &[u8], start: usize) -> Option<usize> {
    if start >= b.len() || b[start] != b'$' {
        return None;
    }
    let mut i = start + 1;
    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
        i += 1;
    }
    if i >= b.len() || b[i] != b'$' {
        return None;
    }
    let tag = &b[start..=i];
    let mut j = i + 1;
    while j + tag.len() <= b.len() {
        if &b[j..j + tag.len()] == tag {
            return Some(j + tag.len());
        }
        j += 1;
    }
    None
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn is_ident_cont(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datetime_extract_is_unchanged() {
        let sql = "SELECT EXTRACT(YEAR FROM ts), extract(hour FROM ts) FROM t";
        assert!(rewrite_native_semantic(sql).is_none());
    }

    #[test]
    fn typed_extract_and_classify_into() {
        let sql = "SELECT EXTRACT(int FROM notes), EXTRACT(creature FROM notes) ON ERROR 'unknown', CLASSIFY(notes INTO 'bigfoot', 'known animal', 'unclear') FROM sightings";
        let out = rewrite_native_semantic(sql).unwrap();
        assert!(out.contains("rvbbit.extract_typed('int', (notes), NULL)"));
        assert!(out.contains("rvbbit.extract_typed('creature', (notes), 'unknown')"));
        assert!(out.contains("rvbbit.classify((notes), 'bigfoot\u{1f}known animal\u{1f}unclear', NULL)"));
        assert!(!out.to_ascii_lowercase().contains("extract(creature"));
    }

    #[test]
    fn unknown_extract_unit_stays_datetime() {
        let sql = "SELECT EXTRACT(FORTNIGHT FROM t), EXTRACT(TIMEZONE_M FROM d)";
        assert!(rewrite_native_semantic(sql).is_none());
    }

    #[test]
    fn regress_sql_is_not_rewritten() {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../regress");
        let mut files = 0usize;
        for dir in ["overlay/sql", "rvbbit/sql"] {
            let path = root.join(dir);
            let rd = std::fs::read_dir(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
            for entry in rd {
                let entry = entry.expect("dirent");
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) != Some("sql") {
                    continue;
                }
                let sql = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
                if let Some(out) = rewrite_native_semantic(&sql) {
                    panic!("{} was rewritten:\n{}", path.display(), out);
                }
                files += 1;
            }
        }
        assert!(files >= 150, "expected the regress corpus, saw {files} files");
    }

    #[test]
    fn classify_as_and_on_error() {
        let sql = "SELECT CLASSIFY(notes AS sighting_class) ON ERROR 'unclear'";
        let out = rewrite_native_semantic(sql).unwrap();
        assert_eq!(
            out,
            "SELECT rvbbit.classify_enum((notes), 'sighting_class', 'unclear')"
        );
    }

    #[test]
    fn means_clause_tail_is_apply_clauses() {
        let sql = "SELECT notes MEANS 'a light in the sky' WITH TAKES 3 PICK BY VOTE FROM ufo";
        let out = rewrite_native_semantic(sql).unwrap();
        assert!(
            out.contains(
                "rvbbit.clause_bool('means', (notes), ('a light in the sky'), 'WITH TAKES 3 PICK BY VOTE') AS means"
            ),
            "{out}"
        );
        assert!(out.contains("FROM ufo"), "{out}");
        assert!(!out.contains("apply_clauses"), "{out}");
    }

    #[test]
    fn m6_aggregate_rerank_extract_many_and_pipe() {
        let sum = rewrite_native_semantic("SELECT SUMMARIZE(notes) FROM t GROUP BY kind").unwrap();
        assert_eq!(sum, "SELECT rvbbit.summarize((notes), '') FROM t GROUP BY kind");
        let summed = rewrite_native_semantic(
            "SELECT SUMMARIZE(notes) WITH TAKES 2 FROM t GROUP BY kind",
        )
        .unwrap();
        assert!(summed.contains("rvbbit.summarize((notes), 'WITH TAKES 2')"), "{summed}");
        let vote = rewrite_native_semantic(
            "SELECT CONSENSUS(notes, 'a large biped was seen') FROM t GROUP BY kind",
        )
        .unwrap();
        assert!(vote.contains("rvbbit.consensus((notes), ('a large biped was seen'), '')"), "{vote}");
        let rank = rewrite_native_semantic("SELECT RERANK(notes, 'logging road') FROM t").unwrap();
        assert_eq!(rank, "SELECT rvbbit.rerank((notes), ('logging road')) FROM t");
        let ranked = rewrite_native_semantic(
            "SELECT notes |> RERANK 'logging road' WITH TAKES 2 FROM t",
        )
        .unwrap();
        assert!(
            ranked.contains(
                "rvbbit.clause_score('rerank', (notes), ('logging road'), 'WITH TAKES 2') AS rerank"
            ),
            "{ranked}"
        );
        let many = rewrite_native_semantic("SELECT EXTRACT MANY creature FROM notes FROM t").unwrap();
        assert!(
            many.contains("rvbbit.extract_many('creature', (notes), '') AS e0_list"),
            "{many}"
        );
        assert!(
            many.contains("LEFT JOIN LATERAL jsonb_array_elements_text(q.e0_list::jsonb) AS e0(value) ON true"),
            "{many}"
        );
        assert!(many.contains("e0.value AS creature"), "{many}");
        assert!(many.contains("(q.src).*"), "{many}");
        assert!(!many.contains("q.*"), "{many}");
        let many_head = many.split(" FROM (").next().unwrap_or("");
        assert!(!many_head.contains("e0_list"), "{many}");
        let mixed = rewrite_native_semantic(
            "SELECT kind, EXTRACT MANY color FROM body FROM notes",
        )
        .unwrap();
        assert!(
            mixed.contains("SELECT kind, rvbbit.extract_many('color', (body), '') AS e0_list FROM notes"),
            "{mixed}"
        );
        assert!(mixed.contains("q.kind"), "{mixed}");
        assert!(mixed.contains("e0.value AS color"), "{mixed}");
        assert!(!mixed.split(" FROM (").next().unwrap_or("").contains("e0_list"), "{mixed}");
        let aliased = rewrite_native_semantic(
            "SELECT EXTRACT MANY creature FROM notes AS r FROM t",
        )
        .unwrap();
        assert!(aliased.contains("e0.value AS r"), "{aliased}");
        assert!(aliased.contains("AS e0_list"), "{aliased}");
        let piped = rewrite_native_semantic("SELECT notes |> EXTRACT creature FROM t").unwrap();
        assert_eq!(piped, "SELECT rvbbit.extract_typed('creature', (notes), NULL) FROM t");
        let means = rewrite_native_semantic(
            "SELECT notes |> MEANS 'a large biped was seen' WITH TAKES 2 FROM t",
        )
        .unwrap();
        assert!(
            means.contains(
                "rvbbit.clause_bool('means', (notes), ('a large biped was seen'), 'WITH TAKES 2') AS means"
            ),
            "{means}"
        );
        let geom = "SELECT p |>> q FROM box_temp";
        assert!(rewrite_native_semantic(geom).is_none(), "geometric |>> is not a pipe");
        let voyage = rewrite_native_semantic(
            "SELECT notes |> RERANK 'logging road' USING MODEL 'voyageai/rerank-3' FROM t",
        )
        .unwrap();
        assert!(
            voyage.contains(
                "rvbbit.rerank_for((notes), ('logging road'), 'voyageai/rerank-3') AS rerank"
            ),
            "{voyage}"
        );
        let nemotron = rewrite_native_semantic(
            "SELECT RERANK(notes, 'logging road') USING MODEL 'nvidia/nemotron-3.5-rerank-8b:free' FROM t",
        )
        .unwrap();
        assert!(
            nemotron.contains("rvbbit.rerank_for((notes), ('logging road'), 'nvidia/nemotron-3.5-rerank-8b:free')"),
            "{nemotron}"
        );
    }

    #[test]
    fn pipe_user_function_becomes_a_call() {
        let sql = "SELECT notes |> is_biped FROM t";
        let out = rewrite_native_semantic(sql).unwrap();
        assert!(out.contains("rvbbit.is_biped((notes))"), "{out}");
        assert!(out.contains("FROM t"), "{out}");
        let extra = rewrite_native_semantic("SELECT notes |> mentions 'biped'").unwrap();
        assert!(extra.contains("rvbbit.mentions((notes), ('biped'))"), "{extra}");
        let geo = "SELECT p |>> q";
        assert!(rewrite_native_semantic(geo).is_none());
    }

    #[test]
    fn pipe_means_score_takes_a_clause_tail() {
        let sql = "SELECT body |> MEANS_SCORE 'a large biped was seen' WITH TAKES 3 FROM t";
        let out = rewrite_native_semantic(sql).unwrap();
        assert!(
            out.contains(
                "rvbbit.clause_score('means_score', (body), ('a large biped was seen'), 'WITH TAKES 3') AS means_score"
            ),
            "{out}"
        );
        assert!(out.contains("FROM t"), "{out}");
        assert!(!out.contains("|>"), "{out}");
        let bare = rewrite_native_semantic("SELECT body |> MEANS_SCORE 'a large biped' FROM t").unwrap();
        assert_eq!(bare, "SELECT rvbbit.means_score((body), ('a large biped')) FROM t");
    }

    #[test]
    fn pipe_means_stops_the_clause_tail_at_as() {
        let sql = "SELECT body |> MEANS 'a large biped was seen' WITH TAKES 3 AS r FROM t";
        let out = rewrite_native_semantic(sql).unwrap();
        assert!(
            out.contains(
                "rvbbit.clause_bool('means', (body), ('a large biped was seen'), 'WITH TAKES 3') AS r"
            ),
            "{out}"
        );
        assert!(out.contains("FROM t"), "{out}");
        assert!(!out.contains("apply_clauses"), "{out}");
        assert!(!out.to_ascii_lowercase().contains("'with takes 3 as r'"), "{out}");
    }

    #[test]
    fn report_keeps_the_json_clause_report() {
        let sql = "SELECT notes MEANS 'a light in the sky' WITH TAKES 3 REPORT FROM ufo";
        let out = rewrite_native_semantic(sql).unwrap();
        assert!(
            out.contains(
                "rvbbit.apply_clauses('means', (notes), ('a light in the sky'), 'WITH TAKES 3 REPORT') AS means"
            ),
            "{out}"
        );
    }

    #[test]
    fn means_budget_in_where_has_no_alias() {
        let sql = "SELECT count(*) FROM notes WHERE body MEANS 'alpha' WITH BUDGET 0.0000001";
        let out = rewrite_native_semantic(sql).unwrap();
        assert!(
            out.contains(
                "rvbbit.clause_bool('means', (body), ('alpha'), 'WITH BUDGET 0.0000001')"
            ),
            "{out}"
        );
        assert!(!out.to_ascii_lowercase().contains("as means"), "{out}");
        assert!(out.contains("WHERE"), "{out}");
    }

    #[test]
    fn plain_means_stays() {
        let sql = "SELECT notes MEANS 'a light in the sky' FROM ufo";
        assert!(rewrite_native_semantic(sql).is_none());
    }

    #[test]
    fn a_string_is_not_rewritten() {
        let sql = "SELECT 'CLASSIFY(notes INTO ''a'', ''b'')', EXTRACT(day FROM d)";
        assert!(rewrite_native_semantic(sql).is_none());
    }
}
