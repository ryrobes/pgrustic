//! Built-in `rvbbit_means(text, text) -> bool`.
//!
//! The batch executor does not call this per row. It exists so a MEANS
//! expression that was not lifted (a target list, for example) still
//! returns the hutch's answer, one HTTP call at a time.

use datum::Datum;
use rvbbit_oids::{
    DUMP_NAME, DUMP_OID, GETDEF_NAME, GETDEF_OID, MEANS_NAME, MEANS_OID, RESTORE_NAME, RESTORE_OID,
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
];
