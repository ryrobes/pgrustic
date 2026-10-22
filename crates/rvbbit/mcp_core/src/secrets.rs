use std::collections::BTreeMap;

/// `Authorization=secret:NAME, X-Region=secret:REGION`.
pub fn parse_assignments(raw: &str) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    if raw.trim().is_empty() {
        return Ok(out);
    }
    for part in raw.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let Some((key, value)) = part.split_once('=') else {
            return Err(format!("expected name=secret:NAME in {part}"));
        };
        let key = key.trim();
        if key.is_empty() {
            return Err(format!("empty name in {part}"));
        }
        out.insert(key.to_string(), value.trim().to_string());
    }
    Ok(out)
}

pub fn reject_plaintext(map: &BTreeMap<String, String>) -> Result<(), String> {
    for (key, value) in map {
        if !value.starts_with("secret:") {
            return Err(format!(
                "plaintext secret in {key}; store secret:{key_hint} and export RVBBIT_SECRET_{key_hint}",
                key_hint = value_hint(value)
            ));
        }
        let name = value.trim_start_matches("secret:").trim();
        if !secret_name_ok(name) {
            return Err(format!("secret name {name} is not an identifier"));
        }
    }
    Ok(())
}

fn value_hint(value: &str) -> &str {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        "NAME"
    } else if secret_name_ok(trimmed) {
        trimmed
    } else {
        "NAME"
    }
}

fn secret_name_ok(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Read `RVBBIT_SECRET_<NAME>`. The catalog never stores the value.
pub fn resolve_ref(value: &str) -> Result<String, String> {
    let Some(name) = value.strip_prefix("secret:") else {
        return Err("refusing a plaintext secret".into());
    };
    let name = name.trim();
    if !secret_name_ok(name) {
        return Err(format!("secret name {name} is not an identifier"));
    }
    let key = format!("RVBBIT_SECRET_{name}");
    std::env::var(&key).map_err(|_| format!("environment variable {key} is not set"))
}

pub fn resolve_map(map: &BTreeMap<String, String>) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    for (key, value) in map {
        out.insert(key.clone(), resolve_ref(value)?);
    }
    Ok(out)
}
