//! Built-in `rvbbit_means(text, text) -> bool`.
//!
//! The batch executor does not call this per row. It exists so a MEANS
//! expression that was not lifted (a target list, for example) still
//! returns the hutch's answer, one HTTP call at a time.

use datum::Datum;
use rvbbit_oids::{
    CALL_LOG_NAME, CALL_LOG_OID, CLASSIFY_NAME, CLASSIFY_OID, DECIDE_NAME, DECIDE_OID, DUMP_NAME,
    DUMP_OID, EXTRACT_NAME,
    EXTRACT_OID, GETDEF_NAME, GETDEF_OID, MEANS_NAME, MEANS_OID, MEANS_SCORE_NAME, MEANS_SCORE_OID,
    RESTORE_NAME, RESTORE_OID,
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
];
