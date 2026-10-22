//! Built-in `rvbbit_means(text, text) -> bool`.
//!
//! The batch executor does not call this per row. It exists so a MEANS
//! expression that was not lifted (a target list, for example) still
//! returns the hutch's answer, one HTTP call at a time.

use datum::Datum;
use rvbbit_oids::{
    APPLY_CLAUSES_NAME, APPLY_CLAUSES_OID, CALL_LOG_NAME, CALL_LOG_OID, CLASSIFY_ENUM_NAME,
    CLASSIFY_ENUM_OID, CLASSIFY_LABELS_NAME, CLASSIFY_LABELS_OID, CLASSIFY_NAME, CLASSIFY_OID,
    CLAUSE_BOOL_NAME, CLAUSE_BOOL_OID, CLAUSE_SCORE_NAME, CLAUSE_SCORE_OID, CLAUSE_TEXT_NAME,
    CLAUSE_TEXT_OID, CONSENSUS_FINAL_NAME, CONSENSUS_FINAL_OID, CONSENSUS_TRANS_NAME,
    CONSENSUS_TRANS_OID, DECIDE_NAME, DECIDE_OID, DUMP_NAME, DUMP_OID, EXTRACT_MANY_NAME,
    EXTRACT_MANY_OID, EXTRACT_NAME, EXTRACT_OID, EXTRACT_TYPED_NAME, EXTRACT_TYPED_OID, FLOW_NAME,
    FLOW_OID, GETDEF_NAME,
    GETDEF_OID, MEANS_NAME, MEANS_OID, MEANS_SCORE_NAME, MEANS_SCORE_OID, RERANK_FOR_NAME,
    RERANK_FOR_OID, RERANK_NAME, RERANK_OID, RESTORE_NAME, RESTORE_OID, SUMMARIZE_FINAL_NAME,
    SUMMARIZE_FINAL_OID, SUMMARIZE_TEXT_NAME, SUMMARIZE_TEXT_OID, SUMMARIZE_TRANS_NAME,
    SUMMARIZE_TRANS_OID, USER_NAME, USER_OID, SIMILARITY_NAME, SIMILARITY_OID,
};
use types_error::PgResult;
use types_fmgr::{
    varlena_result, FmgrBuiltin, FmgrInfo, FunctionCallInfoBaseData as Fcinfo, PGFunction,
};

pub fn rvbbit_means(flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    // SAFETY: the builtin is strict, so both arguments are non-null text.
    let (left, right) = unsafe {
        (
            fcinfo.arg_varlena_packed(0)?.data(),
            fcinfo.arg_varlena_packed(1)?.data(),
        )
    };
    let left = String::from_utf8_lossy(left);
    let right = String::from_utf8_lossy(right);
    let oid = flinfo.as_ref().map(|f| f.fn_oid).unwrap_or(MEANS_OID);
    let matched = rvbbit_dispatch::means_oid(oid, &left, &right)?;
    Ok(Datum::from_bool(matched))
}

/// Text scalar for a semantic function whose SQL return type is `text`.
/// Hutch parses the model output as JSON. One argument, exposed to the
/// prompt as `{{input}}`. Callers cast to `jsonb` when they want a document.
pub fn rvbbit_extract(flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    rvbbit_resolve::ensure()?;
    if fcinfo.nargs() == 0 {
        return Err(Box::new(types_error::PgError::error(
            "rvbbit_extract requires the source text".to_string(),
        )));
    }
    // SAFETY: strict, so argument 0 is non-null text. Extra arguments are
    // appended so a multi-arg function still reaches the model.
    let mut input = String::new();
    for i in 0..fcinfo.nargs() {
        let packed = unsafe { fcinfo.arg_varlena_packed(i)? };
        if i > 0 {
            input.push('\n');
        }
        input.push_str(&String::from_utf8_lossy(packed.data()));
    }
    let oid = flinfo.as_ref().map(|f| f.fn_oid).unwrap_or(EXTRACT_OID);
    let text = rvbbit_dispatch::extract_text(oid, &input)?;
    Ok(varlena_result(varlena::cstring_to_text(fcinfo.result_mcx(), text.as_bytes())?))
}

/// `double precision` semantic function. Same inputs as MEANS. Returns
/// the decision yes-probability instead of the boolean.
pub fn rvbbit_means_score(flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    // SAFETY: the builtin is strict, so both arguments are non-null text.
    let (left, right) = unsafe {
        (
            fcinfo.arg_varlena_packed(0)?.data(),
            fcinfo.arg_varlena_packed(1)?.data(),
        )
    };
    let left = String::from_utf8_lossy(left);
    let right = String::from_utf8_lossy(right);
    let oid = flinfo.as_ref().map(|f| f.fn_oid).unwrap_or(MEANS_SCORE_OID);
    let score = rvbbit_dispatch::means_score(oid, &left, &right)?;
    Ok(Datum::from_f64(score))
}

/// Text scalar for a semantic function with a fixed `LABELS` set.
/// The returned text is JSON `{label, score, scores}`.
pub fn rvbbit_classify(flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    rvbbit_resolve::ensure()?;
    if fcinfo.nargs() == 0 {
        return Err(Box::new(types_error::PgError::error(
            "rvbbit_classify requires the source text".to_string(),
        )));
    }
    let mut input = String::new();
    for i in 0..fcinfo.nargs() {
        let packed = unsafe { fcinfo.arg_varlena_packed(i)? };
        if i > 0 {
            input.push('\n');
        }
        input.push_str(&String::from_utf8_lossy(packed.data()));
    }
    let oid = flinfo.as_ref().map(|f| f.fn_oid).unwrap_or(CLASSIFY_OID);
    let text = rvbbit_dispatch::classify_text(oid, &input)?;
    Ok(varlena_result(varlena::cstring_to_text(fcinfo.result_mcx(), text.as_bytes())?))
}

/// JSON array of recent hutch calls, one object per provider call or
/// cache hit. `rvbbit.calls` is a view over `json_to_recordset` of this
/// text. Column `cost` is USD. A hit has cost 0 and `cost_saved` set.
/// `rvbbit.decide(tree_name, input) -> text`. One row. A batch executor
/// may have already walked this input; that answer is reused.
pub fn rvbbit_decide(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    rvbbit_resolve::ensure()?;
    let (tree_name, input) = unsafe {
        (
            String::from_utf8_lossy(fcinfo.arg_varlena_packed(0)?.data()).into_owned(),
            String::from_utf8_lossy(fcinfo.arg_varlena_packed(1)?.data()).into_owned(),
        )
    };
    if let Some(hit) = rvbbit_dispatch::recall_decide(&tree_name, &input) {
        return Ok(varlena_result(varlena::cstring_to_text(
            fcinfo.result_mcx(),
            hit.as_bytes(),
        )?));
    }
    let tree = rvbbit_resolve::tree(&tree_name).ok_or_else(|| {
        Box::new(types_error::PgError::error(format!(
            "decision tree \"{tree_name}\" does not exist"
        ))) as Box<types_error::PgError>
    })?;
    let reply = rvbbit_dispatch::decide_texts(&tree.endpoint, &tree.nodes, &[input])?;
    let value = reply.values.first().cloned().unwrap_or_default();
    Ok(varlena_result(varlena::cstring_to_text(
        fcinfo.result_mcx(),
        value.as_bytes(),
    )?))
}

/// Native `CLASSIFY(x INTO ...)` and `rvbbit.classify(input, labels)`.
/// Three arguments: `labels` is U+001F-separated and `fallback` may be
/// null. Two arguments: `labels` may be comma-separated, and a model
/// error is raised. The SQL result is the label.
pub fn rvbbit_classify_labels(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    rvbbit_resolve::ensure()?;
    let Some(input) = arg_text(fcinfo, 0)? else {
        return Ok(fcinfo.return_null());
    };
    let Some(raw) = arg_text(fcinfo, 1)? else {
        return Err(Box::new(types_error::PgError::error(
            "CLASSIFY requires a label list".to_string(),
        )));
    };
    let labels = rvbbit_dispatch::split_classify_labels(&raw);
    if labels.len() < 2 {
        return Err(Box::new(types_error::PgError::error(
            "CLASSIFY needs at least two labels".to_string(),
        )));
    }
    let fallback = if fcinfo.nargs() >= 3 { arg_text(fcinfo, 2)? } else { None };
    let key = rvbbit_dispatch::scalar_key_classify(&labels);
    if let Some(hit) = rvbbit_dispatch::recall_scalar(&key, &input) {
        return apply_scalar(fcinfo, fallback, hit);
    }
    match rvbbit_dispatch::classify_labels(&input, &labels) {
        Ok(label) => Ok(text_datum(fcinfo, &label)?),
        Err(err) => fallback_or(fcinfo, fallback, err),
    }
}

/// Native `CLASSIFY(x AS enum)`. Labels come from `pg_enum`.
pub fn rvbbit_classify_enum(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    rvbbit_resolve::ensure()?;
    let Some(input) = arg_text(fcinfo, 0)? else {
        return Ok(fcinfo.return_null());
    };
    let Some(enum_type) = arg_text(fcinfo, 1)? else {
        return Err(Box::new(types_error::PgError::error(
            "CLASSIFY AS requires an enum type name".to_string(),
        )));
    };
    let fallback = arg_text(fcinfo, 2)?;
    let labels = rvbbit_catalog::enum_labels(&enum_type)?;
    if labels.len() < 2 {
        let err = Box::new(types_error::PgError::error(format!(
            "enum \"{enum_type}\" does not have two labels"
        )));
        return fallback_or(fcinfo, fallback, err);
    }
    let key = rvbbit_dispatch::scalar_key_classify(&labels);
    if let Some(hit) = rvbbit_dispatch::recall_scalar(&key, &input) {
        return apply_scalar(fcinfo, fallback, hit);
    }
    match rvbbit_dispatch::classify_labels(&input, &labels) {
        Ok(label) => Ok(text_datum(fcinfo, &label)?),
        Err(err) => fallback_or(fcinfo, fallback, err),
    }
}

/// Native `EXTRACT(type FROM x)` for a type that is not a datetime field.
/// `int`, `float`, and `bool` must parse. Anything else is returned trimmed.
/// `ON ERROR` supplies the text used when the model or the parse fails.
pub fn rvbbit_extract_typed(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    rvbbit_resolve::ensure()?;
    // The native call is `(kind, input, fallback)`. `rvbbit.extract(input, kind)`
    // is the two-argument function-call form and has no fallback.
    let (kind, input, fallback) = if fcinfo.nargs() == 2 {
        let Some(input) = arg_text(fcinfo, 0)? else {
            return Ok(fcinfo.return_null());
        };
        let Some(kind) = arg_text(fcinfo, 1)? else {
            return Ok(fcinfo.return_null());
        };
        (kind, input, None)
    } else {
        let Some(kind) = arg_text(fcinfo, 0)? else {
            return Ok(fcinfo.return_null());
        };
        let Some(input) = arg_text(fcinfo, 1)? else {
            return Ok(fcinfo.return_null());
        };
        (kind, input, arg_text(fcinfo, 2)?)
    };
    let key = rvbbit_dispatch::scalar_key_extract(&kind);
    if let Some(hit) = rvbbit_dispatch::recall_scalar(&key, &input) {
        return apply_scalar(fcinfo, fallback, hit);
    }
    let raw = match rvbbit_dispatch::extract_typed(&kind, &input) {
        Ok(raw) => raw,
        Err(err) => return fallback_or(fcinfo, fallback, err),
    };
    if raw == rvbbit_dispatch::EXTRACT_ABSENT {
        return Ok(fcinfo.return_null());
    }
    match typed_text(&kind, &raw) {
        Ok(value) => Ok(text_datum(fcinfo, &value)?),
        Err(message) => {
            let err = Box::new(types_error::PgError::error(message));
            fallback_or(fcinfo, fallback, err)
        }
    }
}

pub fn rvbbit_summarize_trans(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let state = arg_text(fcinfo, 0)?.unwrap_or_default();
    let value = arg_text(fcinfo, 1)?.unwrap_or_default();
    let clauses = arg_text(fcinfo, 2)?.unwrap_or_default();
    let next = rvbbit_dispatch::push_agg(&state, &value, "", &clauses);
    text_datum(fcinfo, &next)
}

pub fn rvbbit_summarize_final(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    rvbbit_resolve::ensure()?;
    let state = arg_text(fcinfo, 0)?.unwrap_or_default();
    match rvbbit_dispatch::summarize_state(&state)? {
        Some(text) => text_datum(fcinfo, &text),
        None => Ok(fcinfo.return_null()),
    }
}

pub fn rvbbit_summarize_text(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    rvbbit_resolve::ensure()?;
    let input = arg_text(fcinfo, 0)?.unwrap_or_default();
    let clauses = arg_text(fcinfo, 1)?.unwrap_or_default();
    let text = rvbbit_dispatch::summarize_text(&input, &clauses)?;
    text_datum(fcinfo, &text)
}

pub fn rvbbit_consensus_trans(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let state = arg_text(fcinfo, 0)?.unwrap_or_default();
    let value = arg_text(fcinfo, 1)?.unwrap_or_default();
    let criterion = arg_text(fcinfo, 2)?.unwrap_or_default();
    let clauses = arg_text(fcinfo, 3)?.unwrap_or_default();
    let next = rvbbit_dispatch::push_agg(&state, &value, &criterion, &clauses);
    text_datum(fcinfo, &next)
}

pub fn rvbbit_consensus_final(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    rvbbit_resolve::ensure()?;
    let state = arg_text(fcinfo, 0)?.unwrap_or_default();
    match rvbbit_dispatch::consensus_state(&state)? {
        Some(matched) => Ok(Datum::from_bool(matched)),
        None => Ok(fcinfo.return_null()),
    }
}

pub fn rvbbit_rerank(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    rerank_datum(fcinfo, "")
}

pub fn rvbbit_rerank_for(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let model = arg_text(fcinfo, 2)?.unwrap_or_default();
    rerank_datum(fcinfo, &model)
}

fn rerank_datum(fcinfo: &mut Fcinfo, model: &str) -> PgResult<Datum> {
    rvbbit_resolve::ensure()?;
    let document = arg_text(fcinfo, 0)?.unwrap_or_default();
    let query = arg_text(fcinfo, 1)?.unwrap_or_default();
    let key = rvbbit_dispatch::scalar_key_rerank(model, &query);
    if let Some(hit) = rvbbit_dispatch::recall_scalar(&key, &document) {
        return score_hit(hit);
    }
    let score = if model.is_empty() {
        rvbbit_dispatch::rerank_score(&document, &query)?
    } else {
        let reply = rvbbit_dispatch::rerank_many(model, &query, &[document])?;
        let text = reply
            .values
            .into_iter()
            .next()
            .ok_or_else(|| Box::new(types_error::PgError::error("rerank returned no score")) as Box<types_error::PgError>)?
            .map_err(|message| Box::new(types_error::PgError::error(message)) as Box<types_error::PgError>)?;
        text.parse::<f64>().map_err(|_| {
            Box::new(types_error::PgError::error("rerank returned no score")) as Box<types_error::PgError>
        })?
    };
    Ok(Datum::from_f64(score))
}

/// Cosine of the two texts' embeddings. The project batcher fills the
/// memo first; a miss embeds on its own.
pub fn rvbbit_semantic_similarity(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    rvbbit_resolve::ensure()?;
    let left = arg_text(fcinfo, 0)?.unwrap_or_default();
    let right = arg_text(fcinfo, 1)?.unwrap_or_default();
    let key = rvbbit_dispatch::scalar_key_similarity();
    let input = format!("{left}\u{1e}{right}");
    if let Some(hit) = rvbbit_dispatch::recall_scalar(&key, &input) {
        return score_hit(hit);
    }
    let reply = rvbbit_dispatch::similarity_many(&[(left, right)])?;
    let text = reply
        .values
        .into_iter()
        .next()
        .ok_or_else(|| Box::new(types_error::PgError::error("semantic_similarity returned no score")) as Box<types_error::PgError>)?
        .map_err(|message| Box::new(types_error::PgError::error(message)) as Box<types_error::PgError>)?;
    text.parse::<f64>()
        .map(Datum::from_f64)
        .map_err(|_| Box::new(types_error::PgError::error("semantic_similarity returned no score")) as Box<types_error::PgError>)
}

fn score_hit(hit: rvbbit_dispatch::ScalarHit) -> PgResult<Datum> {
    match hit {
        rvbbit_dispatch::ScalarHit::Value(value) => value.parse::<f64>().map(Datum::from_f64).map_err(|_| {
            Box::new(types_error::PgError::error(format!("rerank memo {value}"))) as Box<types_error::PgError>
        }),
        rvbbit_dispatch::ScalarHit::Missing => {
            Err(Box::new(types_error::PgError::error("rerank memo was missing")) as Box<types_error::PgError>)
        }
        rvbbit_dispatch::ScalarHit::Failed(message) => {
            Err(Box::new(types_error::PgError::error(message)) as Box<types_error::PgError>)
        }
    }
}

pub fn rvbbit_extract_many(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    rvbbit_resolve::ensure()?;
    let kind = arg_text(fcinfo, 0)?.unwrap_or_default();
    let input = arg_text(fcinfo, 1)?.unwrap_or_default();
    let clauses = arg_text(fcinfo, 2)?.unwrap_or_default();
    if clauses.trim().is_empty() {
        let key = rvbbit_dispatch::scalar_key_extract_many(&kind);
        if let Some(hit) = rvbbit_dispatch::recall_scalar(&key, &input) {
            return apply_scalar(fcinfo, None, hit);
        }
    }
    let text = rvbbit_dispatch::extract_many_json(&kind, &input, &clauses)?;
    text_datum(fcinfo, &text)
}

/// Bool from a clause tail. The JSON report is `rvbbit.apply_clauses`.
pub fn rvbbit_clause_bool(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let report = clause_report(fcinfo)?;
    Ok(Datum::from_bool(clause_winner_bool(&report)?))
}

/// Score from a clause tail. RERANK's natural type.
pub fn rvbbit_clause_score(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let report = clause_report(fcinfo)?;
    Ok(Datum::from_f64(clause_report_score(&report)?))
}

/// Text winner from a clause tail. CLASSIFY, EXTRACT, and EXTRACT MANY.
pub fn rvbbit_clause_text(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let report = clause_report(fcinfo)?;
    text_datum(fcinfo, &clause_winner_text(&report)?)
}

fn clause_report(fcinfo: &mut Fcinfo) -> PgResult<String> {
    rvbbit_resolve::ensure()?;
    let kind = arg_text(fcinfo, 0)?.unwrap_or_default();
    let input = arg_text(fcinfo, 1)?.unwrap_or_default();
    let extra = arg_text(fcinfo, 2)?.unwrap_or_default();
    let tail = arg_text(fcinfo, 3)?.unwrap_or_default();
    rvbbit_dispatch::apply_clauses_text(&kind, &input, &extra, &tail)
}

fn clause_winner_text(report: &str) -> PgResult<String> {
    let v: serde_json::Value = serde_json::from_str(report).map_err(|e| {
        Box::new(types_error::PgError::error(format!("clause report: {e}"))) as Box<types_error::PgError>
    })?;
    Ok(match v.get("winner") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    })
}

fn clause_winner_bool(report: &str) -> PgResult<bool> {
    let text = clause_winner_text(report)?;
    Ok(matches!(
        text.trim().to_ascii_lowercase().as_str(),
        "true" | "yes" | "t" | "1"
    ))
}

fn clause_report_score(report: &str) -> PgResult<f64> {
    let v: serde_json::Value = serde_json::from_str(report).map_err(|e| {
        Box::new(types_error::PgError::error(format!("clause report: {e}"))) as Box<types_error::PgError>
    })?;
    v.get("score")
        .and_then(|s| s.as_f64())
        .ok_or_else(|| Box::new(types_error::PgError::error("clause score missing")) as Box<types_error::PgError>)
}

/// Clause tail. `clauses` is the postfix text (`WITH TAKES 3 PICK BY VOTE`).
/// The result is JSON `{winner, score, retries, identical, fallback}`.
pub fn rvbbit_apply_clauses(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    rvbbit_resolve::ensure()?;
    let kind = arg_text(fcinfo, 0)?.unwrap_or_default();
    let input = arg_text(fcinfo, 1)?.unwrap_or_default();
    let extra = arg_text(fcinfo, 2)?.unwrap_or_default();
    let tail = arg_text(fcinfo, 3)?.unwrap_or_default();
    let text = rvbbit_dispatch::apply_clauses_text(&kind, &input, &extra, &tail)?;
    text_datum(fcinfo, &text)
}

fn arg_text(fcinfo: &Fcinfo, index: usize) -> PgResult<Option<String>> {
    if fcinfo.argisnull(index) {
        return Ok(None);
    }
    let packed = unsafe { fcinfo.arg_varlena_packed(index)? };
    Ok(Some(String::from_utf8_lossy(packed.data()).into_owned()))
}

fn text_datum(fcinfo: &Fcinfo, text: &str) -> PgResult<Datum> {
    Ok(varlena_result(varlena::cstring_to_text(fcinfo.result_mcx(), text.as_bytes())?))
}

fn fallback_or(
    fcinfo: &mut Fcinfo,
    fallback: Option<String>,
    err: Box<types_error::PgError>,
) -> PgResult<Datum> {
    match fallback {
        Some(text) => text_datum(fcinfo, &text),
        None => Err(err),
    }
}

fn apply_scalar(
    fcinfo: &mut Fcinfo,
    fallback: Option<String>,
    hit: rvbbit_dispatch::ScalarHit,
) -> PgResult<Datum> {
    match hit {
        rvbbit_dispatch::ScalarHit::Value(value) => text_datum(fcinfo, &value),
        rvbbit_dispatch::ScalarHit::Missing => Ok(fcinfo.return_null()),
        rvbbit_dispatch::ScalarHit::Failed(message) => {
            fallback_or(fcinfo, fallback, Box::new(types_error::PgError::error(message)))
        }
    }
}

fn typed_text(kind: &str, value: &str) -> Result<String, String> {
    rvbbit_dispatch::coerce_extracted(kind, value)
}

/// User semantic function. One builtin serves every arity: `pg_proc.pronargs`
/// is what the call sees. A project batch may already have the answer.
pub fn rvbbit_user(flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    rvbbit_resolve::ensure()?;
    let oid = flinfo.as_ref().map(|f| f.fn_oid).unwrap_or(USER_OID);
    let mut args = Vec::new();
    for i in 0..fcinfo.nargs() {
        let packed = unsafe { fcinfo.arg_varlena_packed(i)? };
        args.push(String::from_utf8_lossy(packed.data()).into_owned());
    }
    let key = rvbbit_dispatch::user_memo_key(oid);
    let joined = args.join("\u{1e}");
    if let Some(hit) = rvbbit_dispatch::recall_scalar(&key, &joined) {
        return user_datum(fcinfo, oid, hit);
    }
    let reply = rvbbit_dispatch::user_eval_many(oid, &[args])?;
    let value = reply.values.into_iter().next().unwrap_or_default();
    user_value(fcinfo, oid, &value)
}

fn user_datum(fcinfo: &mut Fcinfo, oid: u32, hit: rvbbit_dispatch::ScalarHit) -> PgResult<Datum> {
    match hit {
        rvbbit_dispatch::ScalarHit::Value(value) => user_value(fcinfo, oid, &value),
        rvbbit_dispatch::ScalarHit::Missing => Ok(fcinfo.return_null()),
        rvbbit_dispatch::ScalarHit::Failed(message) => Err(Box::new(types_error::PgError::error(message))),
    }
}

fn user_value(fcinfo: &mut Fcinfo, oid: u32, value: &str) -> PgResult<Datum> {
    let ep = rvbbit_resolve::endpoint_for(oid);
    let ret = ep.return_type.to_ascii_lowercase();
    if ret == "boolean" || ret == "bool" {
        return Ok(Datum::from_bool(value == "true"));
    }
    if ret.contains("double") || ret == "float8" || ret == "float" {
        let n: f64 = value.parse().map_err(|_| {
            Box::new(types_error::PgError::error(format!("semantic score {value} is not a number")))
                as Box<types_error::PgError>
        })?;
        return Ok(Datum::from_f64(n));
    }
    Ok(varlena_result(varlena::cstring_to_text(fcinfo.result_mcx(), value.as_bytes())?))
}

pub fn rvbbit_call_log(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    rvbbit_resolve::ensure()?;
    let urls = rvbbit_catalog::hutch_urls()?;
    let mut rows = Vec::new();
    for url in urls {
        let text = rvbbit_dispatch::recent_calls_json(&url).map_err(|message| {
            Box::new(types_error::PgError::error(message)) as Box<types_error::PgError>
        })?;
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&text).map_err(|e| {
            Box::new(types_error::PgError::error(format!("rvbbit.call_log: {e}")))
                as Box<types_error::PgError>
        })?;
        rows.extend(parsed);
    }
    let body = serde_json::Value::Array(rows).to_string();
    Ok(varlena_result(varlena::cstring_to_text(fcinfo.result_mcx(), body.as_bytes())?))
}

pub fn rvbbit_dump_sql(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    let sql = rvbbit_catalog::dump_sql()?;
    Ok(varlena_result(varlena::cstring_to_text(fcinfo.result_mcx(), sql.as_bytes())?))
}

pub fn rvbbit_restore_sql(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    // SAFETY: strict, so the argument is non-null text.
    let arg = unsafe { fcinfo.arg_varlena_packed(0)?.data() };
    let sql = String::from_utf8_lossy(arg);
    let msg = rvbbit_catalog::restore_sql(&sql)?;
    Ok(varlena_result(varlena::cstring_to_text(fcinfo.result_mcx(), msg.as_bytes())?))
}

/// `rvbbit.flow(name, input)` runs the latest stored workflow. A third
/// argument is the version. The body is native SQL; `{{input}}` and the
/// first workflow argument are the row.
pub fn rvbbit_flow(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    rvbbit_resolve::ensure()?;
    let name = arg_text(fcinfo, 0)?.unwrap_or_default();
    let input = arg_text(fcinfo, 1)?.unwrap_or_default();
    let version = if fcinfo.nargs() >= 3 && !fcinfo.argisnull(2) {
        Some(i64::from(fcinfo.arg_i32(2)))
    } else {
        None
    };
    let text = rvbbit_catalog::run_flow(&name, &input, version)?;
    text_datum(fcinfo, &text)
}

pub fn rvbbit_get_semfn_def(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    // SAFETY: strict, so the argument is non-null text.
    let arg = unsafe { fcinfo.arg_varlena_packed(0)?.data() };
    let name = String::from_utf8_lossy(arg);
    match rvbbit_catalog::semfn_def(name.trim())? {
        Some(sql) => Ok(varlena_result(varlena::cstring_to_text(
            fcinfo.result_mcx(),
            sql.as_bytes(),
        )?)),
        None => Ok(Datum::null()),
    }
}

pub static MEANS_BUILTINS: &[FmgrBuiltin] = &[
    FmgrBuiltin {
        foid: MEANS_OID,
        name: MEANS_NAME,
        nargs: 2,
        strict: true,
        retset: false,
        func: rvbbit_means as PGFunction,
    },
    FmgrBuiltin {
        foid: DUMP_OID,
        name: DUMP_NAME,
        nargs: 0,
        strict: false,
        retset: false,
        func: rvbbit_dump_sql as PGFunction,
    },
    FmgrBuiltin {
        foid: RESTORE_OID,
        name: RESTORE_NAME,
        nargs: 1,
        strict: true,
        retset: false,
        func: rvbbit_restore_sql as PGFunction,
    },
    FmgrBuiltin {
        foid: GETDEF_OID,
        name: GETDEF_NAME,
        nargs: 1,
        strict: true,
        retset: false,
        func: rvbbit_get_semfn_def as PGFunction,
    },
    FmgrBuiltin {
        foid: EXTRACT_OID,
        name: EXTRACT_NAME,
        nargs: 1,
        strict: true,
        retset: false,
        func: rvbbit_extract as PGFunction,
    },
    FmgrBuiltin {
        foid: MEANS_SCORE_OID,
        name: MEANS_SCORE_NAME,
        nargs: 2,
        strict: true,
        retset: false,
        func: rvbbit_means_score as PGFunction,
    },
    FmgrBuiltin {
        foid: CLASSIFY_OID,
        name: CLASSIFY_NAME,
        nargs: 1,
        strict: true,
        retset: false,
        func: rvbbit_classify as PGFunction,
    },
    FmgrBuiltin {
        foid: CALL_LOG_OID,
        name: CALL_LOG_NAME,
        nargs: 0,
        strict: false,
        retset: false,
        func: rvbbit_call_log as PGFunction,
    },
    FmgrBuiltin {
        foid: DECIDE_OID,
        name: DECIDE_NAME,
        nargs: 2,
        strict: true,
        retset: false,
        func: rvbbit_decide as PGFunction,
    },
    FmgrBuiltin {
        foid: CLASSIFY_LABELS_OID,
        name: CLASSIFY_LABELS_NAME,
        nargs: 3,
        strict: false,
        retset: false,
        func: rvbbit_classify_labels as PGFunction,
    },
    FmgrBuiltin {
        foid: CLASSIFY_ENUM_OID,
        name: CLASSIFY_ENUM_NAME,
        nargs: 3,
        strict: false,
        retset: false,
        func: rvbbit_classify_enum as PGFunction,
    },
    FmgrBuiltin {
        foid: EXTRACT_TYPED_OID,
        name: EXTRACT_TYPED_NAME,
        nargs: 3,
        strict: false,
        retset: false,
        func: rvbbit_extract_typed as PGFunction,
    },
    FmgrBuiltin {
        foid: APPLY_CLAUSES_OID,
        name: APPLY_CLAUSES_NAME,
        nargs: 4,
        strict: false,
        retset: false,
        func: rvbbit_apply_clauses as PGFunction,
    },
    FmgrBuiltin {
        foid: SUMMARIZE_TRANS_OID,
        name: SUMMARIZE_TRANS_NAME,
        nargs: 3,
        strict: true,
        retset: false,
        func: rvbbit_summarize_trans as PGFunction,
    },
    FmgrBuiltin {
        foid: SUMMARIZE_FINAL_OID,
        name: SUMMARIZE_FINAL_NAME,
        nargs: 1,
        strict: false,
        retset: false,
        func: rvbbit_summarize_final as PGFunction,
    },
    FmgrBuiltin {
        foid: SUMMARIZE_TEXT_OID,
        name: SUMMARIZE_TEXT_NAME,
        nargs: 2,
        strict: false,
        retset: false,
        func: rvbbit_summarize_text as PGFunction,
    },
    FmgrBuiltin {
        foid: CONSENSUS_TRANS_OID,
        name: CONSENSUS_TRANS_NAME,
        nargs: 4,
        strict: true,
        retset: false,
        func: rvbbit_consensus_trans as PGFunction,
    },
    FmgrBuiltin {
        foid: CONSENSUS_FINAL_OID,
        name: CONSENSUS_FINAL_NAME,
        nargs: 1,
        strict: false,
        retset: false,
        func: rvbbit_consensus_final as PGFunction,
    },
    FmgrBuiltin {
        foid: RERANK_OID,
        name: RERANK_NAME,
        nargs: 2,
        strict: true,
        retset: false,
        func: rvbbit_rerank as PGFunction,
    },
    FmgrBuiltin {
        foid: USER_OID,
        name: USER_NAME,
        nargs: 1,
        strict: true,
        retset: false,
        func: rvbbit_user as PGFunction,
    },
    FmgrBuiltin {
        foid: RERANK_FOR_OID,
        name: RERANK_FOR_NAME,
        nargs: 3,
        strict: false,
        retset: false,
        func: rvbbit_rerank_for as PGFunction,
    },
    FmgrBuiltin {
        foid: EXTRACT_MANY_OID,
        name: EXTRACT_MANY_NAME,
        nargs: 3,
        strict: false,
        retset: false,
        func: rvbbit_extract_many as PGFunction,
    },
    FmgrBuiltin {
        foid: CLAUSE_BOOL_OID,
        name: CLAUSE_BOOL_NAME,
        nargs: 4,
        strict: false,
        retset: false,
        func: rvbbit_clause_bool as PGFunction,
    },
    FmgrBuiltin {
        foid: CLAUSE_SCORE_OID,
        name: CLAUSE_SCORE_NAME,
        nargs: 4,
        strict: false,
        retset: false,
        func: rvbbit_clause_score as PGFunction,
    },
    FmgrBuiltin {
        foid: CLAUSE_TEXT_OID,
        name: CLAUSE_TEXT_NAME,
        nargs: 4,
        strict: false,
        retset: false,
        func: rvbbit_clause_text as PGFunction,
    },
    FmgrBuiltin {
        foid: FLOW_OID,
        name: FLOW_NAME,
        nargs: 2,
        strict: true,
        retset: false,
        func: rvbbit_flow as PGFunction,
    },
    FmgrBuiltin {
        foid: SIMILARITY_OID,
        name: SIMILARITY_NAME,
        nargs: 2,
        strict: true,
        retset: false,
        func: rvbbit_semantic_similarity as PGFunction,
    },
];
