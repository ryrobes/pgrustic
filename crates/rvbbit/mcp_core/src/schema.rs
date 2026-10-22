//! JSON Schema input to typed SQL arguments, and output to `jsonb` or
//! `RETURNS TABLE` when the schema is an array of objects.

use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub struct ArgSpec {
    pub name: String,
    pub sql_type: String,
    pub default_sql: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolShape {
    pub args: Vec<ArgSpec>,
    pub returns_table: bool,
    pub columns: Vec<ArgSpec>,
    pub return_sql: String,
}

pub const RETURNS_TABLE: &str = "table";

/// Property names in the same order as [`sql_args`]. serde_json maps sort keys.
pub fn json_names(input: &Value) -> Vec<String> {
    input
        .get("properties")
        .and_then(|p| p.as_object())
        .map(|props| props.keys().cloned().collect())
        .unwrap_or_default()
}

/// Positional SQL arguments become the tool's JSON object.
pub fn arguments_object(names: &[String], positional: &Value) -> Value {
    let Some(items) = positional.as_array() else {
        return positional.clone();
    };
    if names.is_empty() {
        return Value::Object(serde_json::Map::new());
    }
    let mut map = serde_json::Map::new();
    for (i, name) in names.iter().enumerate() {
        map.insert(name.clone(), items.get(i).cloned().unwrap_or(Value::Null));
    }
    Value::Object(map)
}

pub fn schema_hash(tool: &str, input: &Value, output: &Value) -> String {
    let body = format!("{tool}\n{input}\n{output}");
    blake3::hash(body.as_bytes()).to_hex().to_string()
}

pub fn sql_ident(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c.to_ascii_lowercase());
        } else {
            out.push('_');
        }
    }
    if out.is_empty() || out.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        out.insert(0, 't');
    }
    out
}

/// Map a tool's input and output schema onto a SQL function shape.
pub fn sql_args(input: &Value, output: &Value) -> ToolShape {
    let args = object_fields(input);
    let (returns_table, columns) = table_columns(output);
    let return_sql = if returns_table {
        let cols = columns
            .iter()
            .map(|c| format!("{} {}", quote_ident(&c.name), c.sql_type))
            .collect::<Vec<_>>()
            .join(", ");
        format!("TABLE ({cols})")
    } else {
        "jsonb".into()
    };
    ToolShape {
        args,
        returns_table,
        columns,
        return_sql,
    }
}

fn object_fields(schema: &Value) -> Vec<ArgSpec> {
    let Some(props) = schema.get("properties").and_then(|p| p.as_object()) else {
        return Vec::new();
    };
    props
        .iter()
        .map(|(name, spec)| ArgSpec {
            name: sql_ident(name),
            sql_type: json_type_to_sql(spec),
            default_sql: spec.get("default").map(json_default_sql),
        })
        .collect()
}

fn table_columns(schema: &Value) -> (bool, Vec<ArgSpec>) {
    if schema.get("type").and_then(|t| t.as_str()) != Some("array") {
        return (false, Vec::new());
    }
    let Some(items) = schema.get("items") else {
        return (false, Vec::new());
    };
    if items.get("type").and_then(|t| t.as_str()) != Some("object") {
        return (false, Vec::new());
    }
    let cols = object_fields(items);
    if cols.is_empty() {
        return (false, Vec::new());
    }
    (true, cols)
}

fn json_type_to_sql(spec: &Value) -> String {
    match spec.get("type").and_then(|t| t.as_str()) {
        Some("string") => "text".into(),
        Some("integer") => "bigint".into(),
        Some("number") => "double precision".into(),
        Some("boolean") => "boolean".into(),
        Some("object") | Some("array") => "jsonb".into(),
        _ => "jsonb".into(),
    }
}

fn json_default_sql(value: &Value) -> String {
    match value {
        Value::Null => "NULL".into(),
        Value::Bool(true) => "true".into(),
        Value::Bool(false) => "false".into(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => quote_lit(s),
        other => format!("{}::jsonb", quote_lit(&other.to_string())),
    }
}

pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub fn quote_lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// SQL body. `OFFSET 0` keeps the planner from inlining the function, so
/// the project batcher sees the tool's own signature.
pub fn tool_fn_body(nsp: &str, fn_name: &str, args: &[ArgSpec], returns_table: bool, columns: &[ArgSpec]) -> String {
    let list = if args.is_empty() {
        String::new()
    } else {
        args.iter()
            .map(|a| quote_ident(&a.name))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let array = if list.is_empty() {
        "jsonb_build_array()".to_string()
    } else {
        format!("jsonb_build_array({list})")
    };
    let call = format!(
        "rvbbit.mcp_invoke_text({}, {}, ({array})::text, false, NULL::text)",
        quote_lit(nsp),
        quote_lit(fn_name),
    );
    if returns_table {
        let cols = columns
            .iter()
            .map(|c| format!("{} {}", quote_ident(&c.name), c.sql_type))
            .collect::<Vec<_>>()
            .join(", ");
        format!("SELECT * FROM jsonb_to_recordset(({call})::jsonb) AS t({cols}) OFFSET 0")
    } else {
        format!("SELECT ({call})::jsonb OFFSET 0")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn scalars_become_typed_args_and_arrays_of_objects_become_tables() {
        let input = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "default": "Ada"},
                "rank": {"type": "integer"},
                "score": {"type": "number"},
                "ok": {"type": "boolean"},
                "meta": {"type": "object"}
            }
        });
        let output = json!({
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "id": {"type": "integer"},
                    "name": {"type": "string"}
                }
            }
        });
        let shape = sql_args(&input, &output);
        let by_name = |n: &str| shape.args.iter().find(|a| a.name == n).unwrap();
        assert_eq!(by_name("name").sql_type, "text");
        assert_eq!(by_name("name").default_sql.as_deref(), Some("'Ada'"));
        assert_eq!(by_name("rank").sql_type, "bigint");
        assert_eq!(by_name("score").sql_type, "double precision");
        assert_eq!(by_name("ok").sql_type, "boolean");
        assert_eq!(by_name("meta").sql_type, "jsonb");
        assert!(shape.returns_table);
        assert_eq!(shape.columns.len(), 2);
        assert!(shape.return_sql.starts_with("TABLE"));
        let body = tool_fn_body("mcp_demo", "people", &shape.args, true, &shape.columns);
        assert!(body.contains("jsonb_to_recordset"), "{body}");
        assert!(body.contains("OFFSET 0"), "{body}");
    }

    #[test]
    fn missing_output_schema_returns_jsonb() {
        let shape = sql_args(&json!({"type": "object", "properties": {"q": {"type": "string"}}}), &Value::Null);
        assert!(!shape.returns_table);
        assert_eq!(shape.return_sql, "jsonb");
        let hash = schema_hash("echo", &json!({"type": "object"}), &Value::Null);
        assert_eq!(hash, schema_hash("echo", &json!({"type": "object"}), &Value::Null));
        assert_ne!(hash, schema_hash("echo2", &json!({"type": "object"}), &Value::Null));
    }

    #[test]
    fn positional_args_become_the_schema_object() {
        let input = json!({"type": "object", "properties": {"text": {"type": "string"}}});
        assert_eq!(json_names(&input), vec!["text".to_string()]);
        let obj = arguments_object(&json_names(&input), &json!(["Ken"]));
        assert_eq!(obj, json!({"text": "Ken"}));
    }

    #[test]
    fn odd_tool_names_become_idents() {
        assert_eq!(sql_ident("get-Time"), "get_time");
        assert_eq!(sql_ident("1st"), "t1st");
    }
}
