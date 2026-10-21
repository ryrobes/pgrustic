//! Built-in `rvbbit_means(text, text) -> bool`.
//!
//! The batch executor does not call this per row. It exists so a MEANS
//! expression that was not lifted (a target list, for example) still
//! returns the hutch's answer, one HTTP call at a time.

use datum::Datum;
use rvbbit_oids::{MEANS_NAME, MEANS_OID};
use types_error::PgResult;
use types_fmgr::{FmgrBuiltin, FmgrInfo, FunctionCallInfoBaseData as Fcinfo, PGFunction};

pub fn rvbbit_means(_flinfo: Option<&mut FmgrInfo>, fcinfo: &mut Fcinfo) -> PgResult<Datum> {
    // SAFETY: the builtin is strict, so both arguments are non-null text.
    let (left, right) = unsafe {
        (
            fcinfo.arg_varlena_packed(0)?.data(),
            fcinfo.arg_varlena_packed(1)?.data(),
        )
    };
    let left = String::from_utf8_lossy(left);
    let right = String::from_utf8_lossy(right);
    let matched = rvbbit_dispatch::means_pair(&left, &right)?;
    Ok(Datum::from_bool(matched))
}

pub static MEANS_BUILTINS: &[FmgrBuiltin] = &[FmgrBuiltin {
    foid: MEANS_OID,
    name: MEANS_NAME,
    nargs: 2,
    strict: true,
    retset: false,
    func: rvbbit_means as PGFunction,
}];
