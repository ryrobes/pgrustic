#![allow(non_snake_case)]

extern crate alloc;

use mcx::{Mcx, PgVec};
use parser_seams::RawParseMode;
use types_error::{PgError, PgResult};
use types_nodes::parsenodes::{DefineStmt, ObjectType};
use types_nodes::rawnodes::RawStmt;
use types_nodes::Node;

pub use parser_small1::udeescape::{
    check_uescapechar, str_udeescape, UdeescapeError, UdeescapeFailure,
};

// raw_parser (parser.c:42) over the RawStmt-typed seam: gram.y's parse_toplevel
// yields RawStmt for RAW_PARSE_DEFAULT and the PL/pgSQL modes; RAW_PARSE_TYPE_NAME
// yields a bare TypeName list, which this seam's shape cannot carry — that mode's
// consumer (parse_type.c typeStringToTypeName) reads gram_core::raw_parser
// directly. Any other node shape here is the C `elog(ERROR, "unexpected node
// type")` idiom: a catchable XX000, never a process panic.
pub fn raw_parser<'mcx>(
    mcx: Mcx<'mcx>,
    query_string: &str,
    mode: RawParseMode,
) -> PgResult<PgVec<'mcx, RawStmt<'mcx>>> {
    // Only rvbbit DDL is peeled off. A native CLASSIFY or typed EXTRACT
    // is rewritten into an ordinary function call first; datetime EXTRACT
    // and every other statement are not copied, so regression locations
    // stay byte-identical.
    if matches!(mode, RawParseMode::RAW_PARSE_DEFAULT) {
        // Native CLASSIFY / typed EXTRACT become ordinary function calls.
        // Datetime EXTRACT and every other statement are not copied.
        if let Some(rewritten) = rvbbit_parse::rewrite_native_semantic(query_string) {
            return raw_parser(mcx, &rewritten, mode);
        }
        if let Some(pieces) = rvbbit_parse::split_if_rvbbit(query_string) {
            return parse_pieces(mcx, &pieces);
        }
    }
    copy_gram(mcx, query_string, mode)
}

fn copy_gram<'mcx>(
    mcx: Mcx<'mcx>,
    query_string: &str,
    mode: RawParseMode,
) -> PgResult<PgVec<'mcx, RawStmt<'mcx>>> {
    let list = gram_core::raw_parser(mcx, query_string, mode)?;
    let mut v = PgVec::new_in(mcx);
    v.try_reserve_exact(list.len()).map_err(|_| mcx.oom(list.len()))?;
    for n in list.iter() {
        let Some(rs) = n.as_raw_stmt() else {
            return Err(Box::new(PgError::error(format!(
                "unexpected node type: {:?}",
                n.node_tag()
            ))));
        };
        v.push(RawStmt { stmt: rs.stmt, stmt_location: rs.stmt_location, stmt_len: rs.stmt_len });
    }
    Ok(v)
}

fn parse_pieces<'mcx>(
    mcx: Mcx<'mcx>,
    pieces: &[rvbbit_parse::Piece<'_>],
) -> PgResult<PgVec<'mcx, RawStmt<'mcx>>> {
    let mut v = PgVec::new_in(mcx);
    for piece in pieces {
        if piece.rvbbit {
            v.push(rvbbit_stmt(mcx, piece.sql, piece.offset)?);
            continue;
        }
        let list = gram_core::raw_parser(mcx, piece.sql, RawParseMode::RAW_PARSE_DEFAULT)?;
        for n in list.iter() {
            let Some(rs) = n.as_raw_stmt() else {
                return Err(Box::new(PgError::error(format!(
                    "unexpected node type: {:?}",
                    n.node_tag()
                ))));
            };
            let stmt_location = if rs.stmt_location >= 0 {
                rs.stmt_location.saturating_add(piece.offset as i32)
            } else {
                rs.stmt_location
            };
            v.push(RawStmt { stmt: rs.stmt, stmt_location, stmt_len: rs.stmt_len });
        }
    }
    Ok(v)
}

fn rvbbit_stmt<'mcx>(mcx: Mcx<'mcx>, sql: &str, offset: usize) -> PgResult<RawStmt<'mcx>> {
    let verb = rvbbit_parse::verb(sql);
    let sql_bytes = mcx::slice_borrow_in(mcx, sql.as_bytes())?;
    let sql_s = std::str::from_utf8(sql_bytes).map_err(|_| {
        Box::new(PgError::error("rvbbit statement is not utf-8".to_string()))
    })?;
    let defnames = types_nodes::NodeList::make2(
        mcx,
        Node::mk_string(mcx, verb)?,
        Node::mk_string(mcx, sql_s)?,
    )?;
    let stmt = Node::mk(
        mcx,
        DefineStmt {
            kind: ObjectType::OBJECT_RVBBIT,
            defnames,
            ..DefineStmt::default()
        },
    )?;
    Ok(RawStmt {
        stmt: Some(stmt),
        stmt_location: offset as i32,
        stmt_len: sql.len() as i32,
    })
}

pub fn init_seams() {
    parser_seams::raw_parser::set(raw_parser);
}

#[cfg(test)]
mod tests;
