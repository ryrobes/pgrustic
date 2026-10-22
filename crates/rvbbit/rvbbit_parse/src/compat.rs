//! Turn a stored `CREATE WORKFLOW` body into one statement for `rvbbit.flow`.
//!
//! pg_rvbbit called a stored flow by name. The body is ordinary SQL in the
//! native syntax (`MEANS`, `CLASSIFY`, `EXTRACT`, `|>`). `{{arg}}` is the
//! same placeholder a semantic template uses. This module does not execute
//! SQL. The catalog runs the statement.

/// Function-call names the migration guide treats as the common shims.
/// `means` is created with the operator. The others are created with it,
/// or already were.
pub const COMMON_CALLS: &[&str] = &[
    "means",
    "means_score",
    "extract",
    "classify",
    "decide",
    "summarize",
    "consensus",
    "rerank",
    "semantic_similarity",
    "flow",
];

/// Argument names from a workflow args blob such as `(ticket text)`.
pub fn flow_arg_names(args: Option<&str>) -> Vec<String> {
    let Some(raw) = args.map(str::trim).filter(|s| !s.is_empty()) else {
        return Vec::new();
    };
    let raw = strip_wrapping_parens(raw);
    split_top_commas(raw)
        .into_iter()
        .filter_map(|piece| first_ident(piece.trim()))
        .collect()
}

/// One statement. `input` is bound to `{{input}}` and to the first declared
/// argument (`{{ticket}}` when the workflow is `(ticket text)`). A body that
/// is not a `SELECT`, `WITH`, or `VALUES` is wrapped as `SELECT (body)`.
/// Native `CLASSIFY` / `EXTRACT` / pipes are rewritten first. `MEANS` stays
/// an operator so the lexer still sees it.
pub fn flow_statement(args: Option<&str>, body: &str, input: &str) -> Result<String, String> {
    let body = body.trim().trim_end_matches(';').trim();
    if body.is_empty() {
        return Err("workflow body is empty".into());
    }
    if !one_statement(body) {
        return Err("workflow body must be one statement".into());
    }
    let names = flow_arg_names(args);
    let mut keys = vec!["input".to_string()];
    if let Some(first) = names.first() {
        if !first.eq_ignore_ascii_case("input") {
            keys.push(first.clone());
        }
    }
    let quoted = sql_quote(input);
    let mut sql = body.to_string();
    for key in &keys {
        let needle = format!("{{{{{key}}}}}");
        if sql.to_ascii_lowercase().contains(&needle.to_ascii_lowercase()) {
            sql = replace_placeholder(&sql, key, &quoted);
        }
    }
    if let Some(unbound) = unbound_placeholder(&sql) {
        return Err(format!(
            "workflow argument \"{unbound}\" is not bound by rvbbit.flow; it binds {{{{input}}}} and the first argument"
        ));
    }
    let sql = super::rewrite_native_semantic(&sql).unwrap_or(sql);
    let sql = sql.trim().trim_end_matches(';').trim().to_string();
    if sql.is_empty() {
        return Err("workflow body is empty".into());
    }
    let head = sql.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
    if head == "select" || head == "with" || head == "values" {
        Ok(sql)
    } else {
        Ok(format!("SELECT ({sql})"))
    }
}

fn replace_placeholder(sql: &str, key: &str, quoted: &str) -> String {
    let lower = sql.to_ascii_lowercase();
    let needle = format!("{{{{{key}}}}}");
    let needle = needle.to_ascii_lowercase();
    let mut out = String::with_capacity(sql.len() + quoted.len());
    let mut rest = sql;
    let mut lower_rest = lower.as_str();
    while let Some(at) = lower_rest.find(&needle) {
        out.push_str(&rest[..at]);
        out.push_str(quoted);
        rest = &rest[at + needle.len()..];
        lower_rest = &lower_rest[at + needle.len()..];
    }
    out.push_str(rest);
    out
}

fn unbound_placeholder(sql: &str) -> Option<String> {
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            let start = i + 2;
            if let Some(rel) = sql[start..].find("}}") {
                let name = sql[start..start + rel].trim();
                if !name.is_empty() && name.chars().all(|c| c == '_' || c.is_ascii_alphanumeric()) {
                    return Some(name.to_string());
                }
            }
        }
        i += 1;
    }
    None
}

fn sql_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn strip_wrapping_parens(s: &str) -> &str {
    let s = s.trim();
    if s.starts_with('(') && s.ends_with(')') && paren_depth_covers(s) {
        s[1..s.len() - 1].trim()
    } else {
        s
    }
}

fn paren_depth_covers(s: &str) -> bool {
    let mut depth = 0i32;
    for (i, c) in s.chars().enumerate() {
        if c == '(' {
            depth += 1;
        } else if c == ')' {
            depth -= 1;
            if depth == 0 {
                return i + c.len_utf8() == s.len();
            }
            if depth < 0 {
                return false;
            }
        }
    }
    false
}

fn split_top_commas(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

fn first_ident(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= bytes.len() || !is_ident_start(bytes[i]) {
        return None;
    }
    let start = i;
    i += 1;
    while i < bytes.len() && is_ident_cont(bytes[i]) {
        i += 1;
    }
    Some(s[start..i].to_string())
}

fn is_ident_start(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphabetic()
}

fn is_ident_cont(b: u8) -> bool {
    is_ident_start(b) || b.is_ascii_digit()
}

/// False when a semicolon is followed by more than whitespace, outside
/// quotes. A trailing semicolon is still one statement.
fn one_statement(sql: &str) -> bool {
    let mut chars = sql.chars().peekable();
    let mut quote = false;
    let mut dollar: Option<String> = None;
    while let Some(c) = chars.next() {
        if let Some(tag) = dollar.clone() {
            if c == '$' {
                let mut got = String::new();
                while let Some(n) = chars.peek().copied() {
                    if n == '$' {
                        break;
                    }
                    if !n.is_ascii_alphanumeric() && n != '_' {
                        break;
                    }
                    got.push(n);
                    chars.next();
                }
                if got == tag && chars.peek() == Some(&'$') {
                    chars.next();
                    dollar = None;
                }
            }
            continue;
        }
        if quote {
            if c == '\'' {
                if chars.peek() == Some(&'\'') {
                    chars.next();
                } else {
                    quote = false;
                }
            }
            continue;
        }
        if c == '\'' {
            quote = true;
            continue;
        }
        if c == '$' {
            let mut tag = String::new();
            while let Some(n) = chars.peek().copied() {
                if n == '$' {
                    break;
                }
                if !n.is_ascii_alphanumeric() && n != '_' {
                    break;
                }
                tag.push(n);
                chars.next();
            }
            if chars.peek() == Some(&'$') {
                chars.next();
                dollar = Some(tag);
            }
            continue;
        }
        if c == ';' {
            let rest: String = chars.collect();
            return rest.trim().is_empty();
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arg_names_come_from_the_workflow_blob() {
        assert_eq!(flow_arg_names(Some("(ticket text)")), vec!["ticket".to_string()]);
        assert_eq!(
            flow_arg_names(Some("(ticket text, note text)")),
            vec!["ticket".to_string(), "note".to_string()]
        );
        assert!(flow_arg_names(None).is_empty());
    }

    #[test]
    fn flow_binds_the_first_argument_and_rewrites_classify() {
        let sql = flow_statement(
            Some("(ticket text)"),
            "CLASSIFY({{ticket}} INTO 'biped', 'invoice')",
            "a hairy biped",
        )
        .unwrap();
        assert_eq!(
            sql,
            "SELECT (rvbbit.classify(('a hairy biped'), 'biped\u{1f}invoice', NULL))"
        );
    }

    #[test]
    fn flow_keeps_means_as_an_operator() {
        let sql = flow_statement(Some("(ticket text)"), "{{ticket}} MEANS 'alpha'", "hello alpha").unwrap();
        assert_eq!(sql, "SELECT ('hello alpha' MEANS 'alpha')");
    }

    #[test]
    fn flow_quotes_the_input() {
        let sql = flow_statement(None, "SELECT {{input}}", "O'Brien").unwrap();
        assert_eq!(sql, "SELECT 'O''Brien'");
    }

    #[test]
    fn flow_rejects_a_second_statement_and_an_unbound_argument() {
        assert!(flow_statement(None, "SELECT 1; DROP TABLE t", "x").is_err());
        let err = flow_statement(Some("(ticket text, note text)"), "{{note}}", "x").unwrap_err();
        assert!(err.contains("note"), "{err}");
    }

    #[test]
    fn flow_runs_a_select_body_unchanged() {
        assert_eq!(flow_statement(Some("(ticket text)"), "SELECT 1", "ignored").unwrap(), "SELECT 1");
    }

    #[test]
    fn migration_guide_names_every_common_call() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../docs/rvbbit/MIGRATION.md");
        let doc = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        for name in COMMON_CALLS {
            assert!(
                doc.contains(&format!("rvbbit.{name}")),
                "migration guide is missing rvbbit.{name}"
            );
        }
    }
}
