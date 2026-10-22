//! Rewrite `tool(args) ON ERROR expr` into `rvbbit.mcp_invoke_text`.
//!
//! Semantic `ON ERROR` is consumed by [`super::rewrite_native_semantic`]
//! first. What remains is a function call gram.y does not know. The
//! replacement is jsonb. A `RETURNS TABLE` tool keeps its error as a SQL
//! error: this pass always yields jsonb, and a set-returning call is left
//! alone when the name is not a scalar tool call we can see here. Named
//! arguments (`=>` / `:=`) keep the right-hand side, in positional order.

const SKIP: &[&str] = &[
    "about",
    "classify",
    "classify_enum",
    "classify_labels",
    "consensus",
    "decide",
    "extract",
    "extract_many",
    "extract_typed",
    "mcp_invoke_text",
    "means",
    "means_score",
    "rerank",
    "rerank_for",
    "summarize",
];

/// `None` when `sql` has no MCP `ON ERROR` call. Callers keep the original
/// string so regression text stays byte-identical.
pub fn rewrite_mcp_on_error(sql: &str) -> Option<String> {
    if !sql.to_ascii_lowercase().contains("error") {
        return None;
    }
    let b = sql.as_bytes();
    let mut spans: Vec<(usize, usize, String)> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if let Some(end) = skip_opaque(sql, i) {
            i = end;
            continue;
        }
        if word_at(sql, i, "on") {
            let after_on = skip_ws(b, i + 2);
            if word_at(sql, after_on, "error") {
                let expr_at = skip_ws(b, after_on + 5);
                if let Some(call) = call_before(sql, i) {
                    if !skip_name(&call.name) {
                        if let Some((expr, expr_end)) = error_expr(sql, expr_at) {
                            let overlaps = spans.iter().any(|(s, e, _)| call.name_start < *e && expr_end > *s);
                            if !overlaps {
                                spans.push((call.name_start, expr_end, replacement(&call, &expr)));
                                i = expr_end;
                                continue;
                            }
                        }
                    }
                }
            }
        }
        i += 1;
    }
    if spans.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(sql.len());
    let mut cursor = 0;
    for (start, end, repl) in spans {
        out.push_str(&sql[cursor..start]);
        out.push_str(&repl);
        cursor = end;
    }
    out.push_str(&sql[cursor..]);
    Some(out)
}

struct Call {
    name_start: usize,
    name: String,
    args: String,
}

fn replacement(call: &Call, expr: &str) -> String {
    let (schema_sql, fn_sql) = match call.name.split_once('.') {
        Some((schema, name)) => (sql_string(schema), sql_string(name)),
        None => ("current_schema()".to_string(), sql_string(&call.name)),
    };
    let args = positional_args(&call.args);
    let list = if args.is_empty() {
        "jsonb_build_array()".to_string()
    } else {
        format!("jsonb_build_array({})", args.join(", "))
    };
    // The builtin JSON-encodes a non-null fallback, so `::jsonb` is a JSON
    // string. NULL stays SQL NULL. Success returns the tool's own JSON text.
    format!(
        "(rvbbit.mcp_invoke_text({schema_sql}, {fn_sql}, ({list})::text, true, ({expr})::text))::jsonb"
    )
}

fn positional_args(raw: &str) -> Vec<String> {
    split_args(raw)
        .into_iter()
        .map(|arg| strip_named(&arg))
        .filter(|arg| !arg.is_empty())
        .collect()
}

fn strip_named(arg: &str) -> String {
    let trimmed = arg.trim();
    if let Some(rest) = split_named(trimmed, "=>").or_else(|| split_named(trimmed, ":=")) {
        return rest;
    }
    trimmed.to_string()
}

fn split_named(arg: &str, op: &str) -> Option<String> {
    let (left, right) = arg.split_once(op)?;
    let left = left.trim();
    if left.is_empty() || !left.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_') {
        return None;
    }
    if left.as_bytes()[0].is_ascii_digit() {
        return None;
    }
    Some(right.trim().to_string())
}

fn split_args(raw: &str) -> Vec<String> {
    let b = raw.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut start = 0;
    let mut depth = 0i32;
    while i < b.len() {
        if let Some(end) = skip_opaque(raw, i) {
            i = end;
            continue;
        }
        if b[i] == b'(' {
            depth += 1;
        } else if b[i] == b')' {
            depth -= 1;
        } else if b[i] == b',' && depth == 0 {
            out.push(raw[start..i].to_string());
            start = i + 1;
        }
        i += 1;
    }
    if start < raw.len() || !raw.is_empty() {
        out.push(raw[start..].to_string());
    }
    out.retain(|s| !s.trim().is_empty());
    out
}

fn skip_name(name: &str) -> bool {
    let base = name.rsplit('.').next().unwrap_or(name);
    SKIP.iter().any(|s| base.eq_ignore_ascii_case(s))
}

fn call_before(sql: &str, on_at: usize) -> Option<Call> {
    let head = sql[..on_at].trim_end();
    if !head.ends_with(')') {
        return None;
    }
    let open = matching_open(head)?;
    let args = head[open + 1..head.len() - 1].to_string();
    let before = head[..open].trim_end();
    let name_start_in_before = trailing_name_start(before)?;
    let name = before[name_start_in_before..].to_string();
    if name.is_empty() {
        return None;
    }
    Some(Call { name_start: name_start_in_before, name, args })
}

fn matching_open(head: &str) -> Option<usize> {
    let b = head.as_bytes();
    let mut depth = 0i32;
    let mut open_at = None;
    let mut i = 0;
    while i < b.len() {
        if let Some(end) = skip_opaque(head, i) {
            i = end;
            continue;
        }
        if b[i] == b'(' {
            if depth == 0 {
                open_at = Some(i);
            }
            depth += 1;
        } else if b[i] == b')' {
            depth -= 1;
            if depth == 0 && i + 1 == b.len() {
                return open_at;
            }
        }
        i += 1;
    }
    None
}

fn trailing_name_start(before: &str) -> Option<usize> {
    let b = before.as_bytes();
    if b.is_empty() {
        return None;
    }
    let mut i = b.len();
    while i > 0 && (is_ident(b[i - 1]) || b[i - 1] == b'.') {
        i -= 1;
    }
    if i == b.len() || !is_ident_start(b[i]) {
        return None;
    }
    Some(i)
}

fn error_expr(sql: &str, mut i: usize) -> Option<(String, usize)> {
    let b = sql.as_bytes();
    i = skip_ws(b, i);
    if i >= b.len() {
        return None;
    }
    if b[i] == b'(' {
        let end = end_paren(b, i)? + 1;
        return Some((sql[i..end].to_string(), end));
    }
    if b[i] == b'\'' {
        let end = end_quote(b, i);
        return Some((sql[i..end].to_string(), end));
    }
    if is_ident_start(b[i]) || b[i].is_ascii_digit() || b[i] == b'-' || b[i] == b'.' {
        let start = i;
        i += 1;
        while i < b.len() && (is_ident(b[i]) || b[i] == b'.') {
            i += 1;
        }
        return Some((sql[start..i].to_string(), i));
    }
    None
}

fn sql_string(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn word_at(sql: &str, i: usize, word: &str) -> bool {
    let b = sql.as_bytes();
    let w = word.as_bytes();
    if i + w.len() > b.len() || !b[i..i + w.len()].eq_ignore_ascii_case(w) {
        return false;
    }
    let after = i + w.len();
    (i == 0 || !is_ident(b[i - 1])) && (after == b.len() || !is_ident(b[after]))
}

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

fn skip_opaque(sql: &str, i: usize) -> Option<usize> {
    let b = sql.as_bytes();
    if i >= b.len() {
        return None;
    }
    if b[i] == b'\'' {
        return Some(end_quote(b, i));
    }
    if b[i] == b'"' {
        return Some(end_dquote(b, i));
    }
    if b[i] == b'$' {
        return dollar_end(sql, i);
    }
    if b[i] == b'-' && i + 1 < b.len() && b[i + 1] == b'-' {
        let end = sql[i..].find('\n').map(|n| i + n).unwrap_or(b.len());
        return Some(end);
    }
    if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
        let end = sql[i + 2..].find("*/").map(|n| i + 2 + n + 2).unwrap_or(b.len());
        return Some(end);
    }
    None
}

fn dollar_end(sql: &str, start: usize) -> Option<usize> {
    let b = sql.as_bytes();
    if start >= b.len() || b[start] != b'$' {
        return None;
    }
    let mut j = start + 1;
    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
        j += 1;
    }
    if j >= b.len() || b[j] != b'$' {
        return None;
    }
    let tag = &sql[start..=j];
    let rest = &sql[j + 1..];
    rest.find(tag).map(|k| j + 1 + k + tag.len())
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
    while i < b.len() && b[i] != b'"' {
        i += 1;
    }
    (i + 1).min(b.len())
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_a_tool_call_and_skips_semantic_names() {
        let out = rewrite_mcp_on_error("SELECT echo(name) ON ERROR NULL FROM t").unwrap();
        assert!(out.contains("mcp_invoke_text"), "{out}");
        assert!(out.contains("current_schema()"), "{out}");
        assert!(out.contains("true"), "{out}");
        assert!(!out.to_ascii_lowercase().contains("on error"), "{out}");

        let qualified = rewrite_mcp_on_error("SELECT mcp_demo.echo(text => name) ON ERROR 'x'").unwrap();
        assert!(qualified.contains("'mcp_demo'"), "{qualified}");
        assert!(qualified.contains("'echo'"), "{qualified}");
        assert!(qualified.contains("jsonb_build_array(name)"), "{qualified}");
        assert!(!qualified.contains("text =>"), "{qualified}");

        assert!(rewrite_mcp_on_error("SELECT classify(x, 'a') ON ERROR NULL").is_none());
        assert!(rewrite_mcp_on_error("SELECT 'on error'").is_none());
        assert!(rewrite_mcp_on_error("SELECT 1").is_none());
    }
}
