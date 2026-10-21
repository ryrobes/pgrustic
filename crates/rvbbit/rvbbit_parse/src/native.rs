//! Rewrite native semantic calls into ordinary function calls.
//!
//! `rvbbit.y` never sees a SELECT expression, and gram.y already accepts
//! `EXTRACT(ident FROM a_expr)` and `func(args)`. This pass only rewrites
//! spans gram.y cannot keep as datetime EXTRACT or as a plain function:
//! `CLASSIFY(expr INTO labels)` / `CLASSIFY(expr AS type)` and a typed
//! `EXTRACT`. A query with neither form is returned unchanged, so
//! regression text stays byte-identical.
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
    let out = rewrite_all(sql);
    if out == sql {
        None
    } else {
        Some(out)
    }
}

fn mentions(sql: &str) -> bool {
    let lower = sql.to_ascii_lowercase();
    lower.contains("classify") || lower.contains("extract")
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
            if next < b.len() && b[next] == b'(' && (folded == "classify" || folded == "extract") {
                if let Some((end, rewritten)) = rewrite_call(&folded, sql, next) {
                    out.push_str(&rewritten);
                    i = end;
                    continue;
                }
            }
            out.push_str(word);
            continue;
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
    let (fallback, end) = on_error(sql, after);
    let opted_in = fallback.is_some();
    let fallback_sql = fallback.unwrap_or_else(|| "NULL".into());
    if kind == "classify" {
        let (expr, labels, enum_name) = split_classify(&body)?;
        let call = if let Some(enum_name) = enum_name {
            format!(
                "rvbbit.classify_enum(({expr}), {name}, {fallback_sql})",
                name = sql_string(&enum_name),
            )
        } else {
            format!(
                "rvbbit.classify(({expr}), {labels}, {fallback_sql})",
                labels = sql_string(&labels.join("\u{1f}")),
            )
        };
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
    let call = format!(
        "rvbbit.extract_typed({field}, ({expr}), {fallback_sql})",
        field = sql_string(&field),
    );
    Some((end, call))
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
    fn a_string_is_not_rewritten() {
        let sql = "SELECT 'CLASSIFY(notes INTO ''a'', ''b'')', EXTRACT(day FROM d)";
        assert!(rewrite_native_semantic(sql).is_none());
    }
}
