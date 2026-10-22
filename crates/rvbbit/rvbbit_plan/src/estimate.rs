//! Turn a planned semantic node into provider calls, USD, and latency.

use rvbbit_cost::{knobs_from_tail, Estimate, Grouping, Knobs, Op, Work};
use rvbbit_oids::{
    CLASSIFY_ENUM_OID, CLASSIFY_LABELS_OID, CLASSIFY_OID, CLAUSE_BOOL_OID, CLAUSE_SCORE_OID,
    CLAUSE_TEXT_OID, CONSENSUS_FINAL_OID, DECIDE_OID, EXTRACT_MANY_OID, EXTRACT_OID,
    EXTRACT_TYPED_OID, MEANS_OID, MEANS_SCORE_OID, RERANK_FOR_OID, RERANK_OID, SUMMARIZE_FINAL_OID,
    SUMMARIZE_TEXT_OID,
};
use types_nodes::primnodes::FuncExpr;
use types_nodes::{Node, NodeTag};

use crate::qual_contains_means;

#[derive(Clone, Debug)]
pub struct Shown {
    /// Charged provider calls (after the cache hit rate).
    pub provider_calls: f64,
    /// HTTP batches. One batch carries up to `batch_rows` fan-out calls.
    pub http_batches: f64,
    pub usd: f64,
    pub latency_ms: f64,
    pub model: String,
    pub selectivity: f64,
    pub filters: bool,
    pub clause_budget: Option<f64>,
}

pub fn node_estimate(node: Node<'_>) -> Option<Shown> {
    let plan = node.as_plan()?;
    let rows = input_rows(node);
    let mut works = Vec::new();
    if node.node_tag() == NodeTag::T_Result || node.node_tag() == NodeTag::T_Agg {
        collect(node, &plan.qual, &mut works);
        collect_tlist(&plan.targetlist, &mut works);
    } else {
        return None;
    }
    if works.is_empty() {
        return None;
    }
    let mut shown = Shown {
        provider_calls: 0.0,
        http_batches: 0.0,
        usd: 0.0,
        latency_ms: 0.0,
        model: String::new(),
        selectivity: 1.0,
        filters: false,
        clause_budget: None,
    };
    let mut est = Estimate::zero();
    for work in &works {
        let one = rvbbit_cost::estimate(&Work {
            op: work.op,
            model: work.model.clone(),
            rows,
            per_row: work.per_row,
            knobs: work.knobs.clone(),
            grouping: work.grouping,
            batch_rows: rvbbit_resolve::batch_rows().max(1) as u32,
            concurrency: rvbbit_resolve::max_concurrent().max(1) as u32,
            threshold: work.knobs.threshold,
            filters: work.filters,
        });
        est.add(&one);
        if work.filters {
            shown.filters = true;
            shown.selectivity = one.selectivity;
        }
        if let Some(b) = work.knobs.budget {
            shown.clause_budget = Some(shown.clause_budget.map_or(b, |c: f64| c.min(b)));
        }
    }
    shown.provider_calls = est.items;
    shown.http_batches = est.calls;
    shown.usd = est.usd;
    shown.latency_ms = est.latency_ms;
    shown.model = est.model;
    if !shown.filters {
        shown.selectivity = 1.0;
    }
    Some(shown)
}

pub fn tree_estimate(node: Node<'_>) -> Option<Shown> {
    let mut total = Shown {
        provider_calls: 0.0,
        http_batches: 0.0,
        usd: 0.0,
        latency_ms: 0.0,
        model: String::new(),
        selectivity: 1.0,
        filters: false,
        clause_budget: None,
    };
    let mut any = false;
    walk_tree(node, &mut total, &mut any);
    any.then_some(total)
}

fn walk_tree(node: Node<'_>, total: &mut Shown, any: &mut bool) {
    if let Some(one) = node_estimate(node) {
        *any = true;
        total.provider_calls += one.provider_calls;
        total.http_batches += one.http_batches;
        total.usd += one.usd;
        total.latency_ms += one.latency_ms;
        if total.model.is_empty() {
            total.model = one.model;
        } else if !one.model.is_empty() && !total.model.split(',').any(|m| m.trim() == one.model) {
            total.model.push_str(", ");
            total.model.push_str(&one.model);
        }
        if let Some(b) = one.clause_budget {
            total.clause_budget = Some(total.clause_budget.map_or(b, |c: f64| c.min(b)));
        }
    }
    let Some(plan) = node.as_plan() else {
        return;
    };
    if let Some(left) = plan.lefttree {
        walk_tree(left, total, any);
    }
    if let Some(right) = plan.righttree {
        walk_tree(right, total, any);
    }
    if let Some(append) = node.as_append() {
        for child in append.appendplans.iter() {
            walk_tree(child, total, any);
        }
    }
    if let Some(merge) = node.as_merge_append() {
        for child in merge.mergeplans.iter() {
            walk_tree(child, total, any);
        }
    }
    if let Some(sub) = node.as_subquery_scan().and_then(|s| s.subplan) {
        walk_tree(sub, total, any);
    }
}

struct Found {
    op: Op,
    model: String,
    per_row: f64,
    knobs: Knobs,
    grouping: Grouping,
    filters: bool,
}

fn collect(node: Node<'_>, qual: &types_nodes::list::NodeList<'_>, out: &mut Vec<Found>) {
    for clause in qual.iter() {
        collect_expr(clause, true, node.node_tag() == NodeTag::T_Agg, out);
    }
}

fn collect_tlist(tlist: &types_nodes::list::NodeList<'_>, out: &mut Vec<Found>) {
    for tle in tlist.iter() {
        let Some(te) = tle.as_target_entry() else {
            continue;
        };
        collect_expr(te.expr, false, false, out);
    }
}

fn collect_expr(node: Node<'_>, in_qual: bool, in_agg: bool, out: &mut Vec<Found>) {
    match node.node_tag() {
        NodeTag::T_FuncExpr => {
            let func = node.as_func_expr().expect("FuncExpr");
            if let Some(found) = found_func(func, in_qual, in_agg) {
                out.push(found);
            }
            for arg in func.args.iter() {
                collect_expr(arg, in_qual, in_agg, out);
            }
        }
        NodeTag::T_Aggref => {
            let agg = node.as_aggref().expect("Aggref");
            if let Some(found) = found_agg(agg.aggfnoid) {
                out.push(found);
            }
            for arg in agg.args.iter() {
                collect_expr(arg, in_qual, true, out);
            }
        }
        NodeTag::T_TargetEntry => {
            if let Some(te) = node.as_target_entry() {
                collect_expr(te.expr, in_qual, in_agg, out);
            }
        }
        NodeTag::T_RelabelType => {
            collect_expr(node.as_relabel_type().expect("Relabel").arg, in_qual, in_agg, out);
        }
        NodeTag::T_CoerceViaIO => {
            collect_expr(node.as_coerce_via_io().expect("CoerceViaIO").arg, in_qual, in_agg, out);
        }
        NodeTag::T_OpExpr => {
            for arg in node.as_op_expr().expect("OpExpr").args.iter() {
                collect_expr(arg, in_qual, in_agg, out);
            }
        }
        NodeTag::T_BoolExpr => {
            for arg in node.as_bool_expr().expect("BoolExpr").args.iter() {
                collect_expr(arg, in_qual, in_agg, out);
            }
        }
        NodeTag::T_List => {
            if let Some(list) = node.as_list() {
                for arg in list.iter() {
                    collect_expr(arg, in_qual, in_agg, out);
                }
            }
        }
        _ => {}
    }
}

fn found_agg(oid: u32) -> Option<Found> {
    let name = proc_name(oid);
    let op = if oid == SUMMARIZE_FINAL_OID || oid == SUMMARIZE_TEXT_OID {
        Op::Summarize
    } else if oid == CONSENSUS_FINAL_OID {
        Op::Predict
    } else if name_is(&name, &["summarize", "rvbbit_summarize", "rvbbit_summarize_final"]) {
        Op::Summarize
    } else if name_is(&name, &["consensus", "rvbbit_consensus", "rvbbit_consensus_final"]) {
        Op::Predict
    } else {
        return None;
    };
    Some(Found {
        op,
        model: model_for(oid, &name),
        per_row: 1.0,
        knobs: Knobs::default(),
        grouping: Grouping::PerGroup,
        filters: op == Op::Predict,
    })
}

fn found_func(func: &FuncExpr<'_>, in_qual: bool, in_agg: bool) -> Option<Found> {
    let oid = func.funcid;
    let name = proc_name(oid);
    let tail = clause_tail(func);
    let mut knobs = tail.as_deref().map(knobs_from_tail).unwrap_or_default();
    if rvbbit_resolve::is_semantic_func(oid) {
        let ep = rvbbit_resolve::endpoint_for(oid);
        if tail.is_none() {
            if let Some(defaults) = ep.default_clauses.clone() {
                if !defaults.trim().is_empty() {
                    knobs = knobs_from_tail(&defaults);
                }
            }
        }
        if knobs.threshold.is_none() {
            knobs.threshold = ep.threshold;
        }
    }
    let (op, filters, per_row, grouping) = classify(oid, &name, in_qual, in_agg)?;
    let mut model = knobs.model.clone().unwrap_or_else(|| model_for(oid, &name));
    if model.is_empty() {
        model = "jev".into();
    }
    // `USING MODEL` on rerank is the third argument of `rerank_for`, not a
    // clause tail. The catalog oid is not always `RERANK_FOR_OID`, so a
    // slug-shaped argument is enough. A clause tail stays on `knobs`.
    if op == Op::Rerank && knobs.model.is_none() {
        if let Some(slug) = func.args.iter().nth(2).and_then(const_text) {
            if rerank_slug(&slug) {
                model = slug;
            }
        }
    }
    Some(Found { op, model, per_row, knobs, grouping, filters })
}

fn rerank_slug(text: &str) -> bool {
    let low = text.trim().to_ascii_lowercase();
    if low.is_empty() {
        return false;
    }
    if low.contains("takes") || low.contains("budget") || low.contains("phras") || low.contains("fallback") {
        return false;
    }
    low.contains('/') || low.contains("voyage") || low.contains("rerank") || low == "jev" || low == "mock"
}

fn classify(oid: u32, name: &str, in_qual: bool, in_agg: bool) -> Option<(Op, bool, f64, Grouping)> {
    let grouping = if in_agg { Grouping::PerGroup } else { Grouping::Batched };
    if oid == MEANS_OID || oid == MEANS_SCORE_OID || oid == CLAUSE_BOOL_OID || name_is(name, &["means", "rvbbit_means", "means_score", "rvbbit_means_score", "clause_bool"])
        || rvbbit_resolve::is_semantic_func(oid) && !name_is(name, &["decide", "rvbbit_decide"])
    {
        // is_semantic_func is also true for catalog classify/extract. Prefer the name.
        if name_is(name, &["classify", "rvbbit_classify"]) {
            return Some((Op::Classify, false, 1.0, grouping));
        }
        if name_is(name, &["extract", "rvbbit_extract", "extract_typed", "extract_many"]) {
            return Some((Op::Extract, false, 1.0, grouping));
        }
        if name_is(name, &["rerank", "rerank_for", "rvbbit_rerank", "rvbbit_rerank_for"]) {
            return Some((Op::Rerank, false, 1.0, grouping));
        }
        if name_is(name, &["decide", "rvbbit_decide"]) {
            return Some((Op::Decide, false, 3.0, grouping));
        }
        if rvbbit_resolve::is_semantic_func(oid) {
            let ep = rvbbit_resolve::endpoint_for(oid);
            if ep.fn_name.eq_ignore_ascii_case("about") || name_is(name, &["about"]) {
                return Some((Op::Embed, in_qual, 1.0, grouping));
            }
            if ep.user_prompt.starts_with(rvbbit_resolve::TREE_PREFIX) {
                return Some((Op::Decide, false, 3.0, grouping));
            }
            if ep.labels.len() >= 2 {
                return Some((Op::Classify, false, 1.0, grouping));
            }
            if ep.return_type.eq_ignore_ascii_case("text") {
                return Some((Op::Extract, false, 1.0, grouping));
            }
            return Some((Op::Predict, in_qual || oid == CLAUSE_BOOL_OID, 1.0, grouping));
        }
        if oid == MEANS_OID || oid == MEANS_SCORE_OID || oid == CLAUSE_BOOL_OID || name_is(name, &["means", "rvbbit_means", "means_score", "rvbbit_means_score", "clause_bool", "consensus"])
        {
            return Some((Op::Predict, in_qual || oid == CLAUSE_BOOL_OID, 1.0, grouping));
        }
    }
    match oid {
        CLASSIFY_OID | CLASSIFY_LABELS_OID | CLASSIFY_ENUM_OID => Some((Op::Classify, false, 1.0, grouping)),
        EXTRACT_OID | EXTRACT_TYPED_OID | EXTRACT_MANY_OID | CLAUSE_TEXT_OID => {
            Some((Op::Extract, false, 1.0, grouping))
        }
        RERANK_OID | RERANK_FOR_OID | CLAUSE_SCORE_OID => Some((Op::Rerank, false, 1.0, grouping)),
        DECIDE_OID => Some((Op::Decide, false, 3.0, grouping)),
        SUMMARIZE_TEXT_OID | SUMMARIZE_FINAL_OID => Some((Op::Summarize, false, 1.0, Grouping::PerGroup)),
        _ if rvbbit_resolve::is_decide_func(oid) || name_is(name, &["decide", "rvbbit_decide"]) => {
            Some((Op::Decide, false, 3.0, grouping))
        }
        _ if name_is(name, &["classify", "classify_enum", "rvbbit_classify", "rvbbit_classify_labels", "rvbbit_classify_enum"]) => {
            Some((Op::Classify, false, 1.0, grouping))
        }
        _ if name_is(name, &["extract_typed", "extract_many", "rvbbit_extract_typed", "rvbbit_extract_many", "rvbbit_extract", "clause_text"]) => {
            Some((Op::Extract, false, 1.0, grouping))
        }
        _ if name_is(name, &["rerank", "rerank_for", "rvbbit_rerank", "rvbbit_rerank_for", "clause_score"]) => {
            Some((Op::Rerank, false, 1.0, grouping))
        }
        _ if name_is(name, &["summarize", "rvbbit_summarize", "rvbbit_summarize_text"]) => {
            Some((Op::Summarize, false, 1.0, Grouping::PerGroup))
        }
        _ if name_is(name, &["about"]) => Some((Op::Embed, in_qual, 1.0, grouping)),
        _ if name_is(name, &["semantic_similarity", "rvbbit_semantic_similarity"]) => {
            Some((Op::Embed, false, 2.0, grouping))
        }
        _ => None,
    }
}

fn name_is(name: &str, opts: &[&str]) -> bool {
    opts.iter().any(|o| name.eq_ignore_ascii_case(o))
}

fn model_for(oid: u32, name: &str) -> String {
    if rvbbit_resolve::is_semantic_func(oid) || rvbbit_resolve::is_decide_func(oid) || !name.is_empty() {
        let ep = rvbbit_resolve::endpoint_for(oid);
        if !ep.model.is_empty() {
            return ep.model;
        }
    }
    "jev".into()
}

fn proc_name(oid: u32) -> String {
    if !syscache_seams::pg_proc_proname::is_installed() {
        return String::new();
    }
    syscache_seams::pg_proc_proname::call(oid)
        .ok()
        .flatten()
        .map(|n| String::from_utf8_lossy(n.name_str()).into_owned())
        .unwrap_or_default()
}

fn clause_tail(func: &FuncExpr<'_>) -> Option<String> {
    let oid = func.funcid;
    let last = func.args.len().checked_sub(1)?;
    let read = func.args.iter().nth(last).and_then(const_text);
    if oid == CLAUSE_BOOL_OID || oid == CLAUSE_SCORE_OID || oid == CLAUSE_TEXT_OID {
        return read;
    }
    read.filter(|s| {
        let t = s.to_ascii_lowercase();
        t.contains("takes")
            || t.contains("budget")
            || t.contains("phras")
            || t.contains("fallback")
            || t.contains("model")
    })
}

fn peel_coerce(node: Node<'_>) -> Node<'_> {
    match node.node_tag() {
        NodeTag::T_RelabelType => node.as_relabel_type().map(|r| r.arg).unwrap_or(node),
        NodeTag::T_CoerceViaIO => node.as_coerce_via_io().map(|c| c.arg).unwrap_or(node),
        NodeTag::T_CollateExpr => node.as_collate_expr().map(|c| c.arg).unwrap_or(node),
        _ => node,
    }
}

fn const_text(node: Node<'_>) -> Option<String> {
    let node = peel_coerce(node);
    let c = node.as_const()?;
    if c.constisnull || c.constbyval {
        return None;
    }
    let ptr = c.constvalue.as_usize() as *const u8;
    if ptr.is_null() {
        return None;
    }
    // SAFETY: a plan Const text datum is a live varlena in the plan context.
    let bytes = unsafe { text_bytes(ptr) }?;
    String::from_utf8(bytes).ok()
}

unsafe fn text_bytes(ptr: *const u8) -> Option<Vec<u8>> {
    if types_tuple::varatt::varatt_is_1b_e(ptr) {
        return None;
    }
    if types_tuple::varatt::varatt_is_1b(ptr) {
        let size = types_tuple::varatt::varsize_1b(ptr);
        if size < 1 {
            return None;
        }
        Some(core::slice::from_raw_parts(ptr.add(1), size - 1).to_vec())
    } else {
        let size = types_tuple::varatt::varsize_4b(ptr);
        if size < 4 || size > 1_000_000 {
            return None;
        }
        Some(core::slice::from_raw_parts(ptr.add(4), size - 4).to_vec())
    }
}

/// Rows the semantic node will read.
///
/// An Agg prices its own group count. A scan with no leftover qual returns
/// the whole table: when that table is small or still un-analyzed, the
/// planner's 10-page floor is replaced by the counted row count stashed
/// for that scan. A scan that still has a cheap qual keeps `plan_rows`,
/// which already includes that qual. The scan node's own `plan_rows` is
/// not rewritten.
fn input_rows(node: Node<'_>) -> f64 {
    let Some(plan) = node.as_plan() else {
        return 0.0;
    };
    if node.node_tag() == NodeTag::T_Agg {
        return plan.plan_rows;
    }
    let Some(child_node) = plan.lefttree else {
        return plan.plan_rows;
    };
    let Some(child) = child_node.as_plan() else {
        return plan.plan_rows;
    };
    // Keyed by the scan node, not `plan_node_id`. Catalog SPI during
    // EXPLAIN reuses id 1 for a small catalog table and used to replace
    // the user scan's count, so a 40-row project estimated a handful of
    // calls.
    if child.qual.is_nil() {
        if let Some(rows) = counted_scan_rows(child_node) {
            return rows;
        }
    }
    child.plan_rows
}

thread_local! {
    static SCAN_ROWS: std::cell::RefCell<std::collections::HashMap<usize, f64>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

pub fn in_nested_planner() -> bool {
    rvbbit_cost::in_nested_planner()
}

pub fn clear_scan_rows() {
    SCAN_ROWS.with(|rows| rows.borrow_mut().clear());
}

/// Counted rows for one base-scan node. The key is the node's address.
/// `plan_node_id` is reused by every later plan, including catalog SPI
/// while EXPLAIN is printing. Ignored when the count is not finite.
pub fn remember_scan_rows(scan: Node<'_>, rows: f64) {
    remember_scan_key(scan.as_raw().as_ptr() as usize, rows);
}

pub(crate) fn remember_scan_key(key: usize, rows: f64) {
    if !rows.is_finite() || rows < 0.0 {
        return;
    }
    SCAN_ROWS.with(|slot| {
        slot.borrow_mut().insert(key, rows);
    });
}

fn counted_scan_rows(scan: Node<'_>) -> Option<f64> {
    counted_scan_key(scan.as_raw().as_ptr() as usize)
}

fn counted_scan_key(key: usize) -> Option<f64> {
    SCAN_ROWS.with(|slot| slot.borrow().get(&key).copied())
}

pub fn apply_costs(node: Node<'_>) {
    let Some(plan) = node.as_plan() else {
        return;
    };
    if let Some(est) = node_estimate(node) {
        let added = est.usd * 1.0e8;
        let rows = if est.filters {
            input_rows(node) * est.selectivity
        } else {
            plan.plan_rows
        };
        unsafe {
            let _ = node.with_plan_mut(|p| {
                p.total_cost += added;
                if est.filters {
                    p.plan_rows = rows;
                }
            });
        }
        account(&est);
    }
    let mut kids = Vec::new();
    if let Some(left) = plan.lefttree {
        kids.push(left);
    }
    if let Some(right) = plan.righttree {
        kids.push(right);
    }
    if let Some(append) = node.as_append() {
        kids.extend(append.appendplans.iter());
    }
    if let Some(merge) = node.as_merge_append() {
        kids.extend(merge.mergeplans.iter());
    }
    if let Some(sub) = node.as_subquery_scan().and_then(|s| s.subplan) {
        kids.push(sub);
    }
    for kid in kids {
        apply_costs(kid);
    }
}

struct Frame {
    usd: f64,
    budget: Option<f64>,
}

struct Acc {
    frames: Vec<Frame>,
    /// Planner re-entry while a statement is already executing (catalog
    /// SPI inside a semantic builtin). Those plans must not replace the
    /// armed pre-flight estimate.
    skip: u32,
}

static ACC: std::sync::Mutex<Acc> = std::sync::Mutex::new(Acc { frames: Vec::new(), skip: 0 });

fn acc() -> std::sync::MutexGuard<'static, Acc> {
    ACC.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn begin_statement_cost() {
    rvbbit_cost::refresh_history();
    let mut a = acc();
    // Catalog SPI plans while EXPLAIN reads this map. Those plans must
    // not clear it or replace the armed pre-flight estimate.
    if rvbbit_cost::execution_in_progress() || rvbbit_cost::in_nested_planner() {
        a.skip += 1;
        return;
    }
    clear_scan_rows();
    a.frames.push(Frame { usd: 0.0, budget: None });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counted_scan_rows_round_trip_and_clear() {
        clear_scan_rows();
        remember_scan_key(7, 12.0);
        remember_scan_key(8, f64::NAN);
        assert_eq!(counted_scan_key(7), Some(12.0));
        assert_eq!(counted_scan_key(8), None);
        clear_scan_rows();
        assert_eq!(counted_scan_key(7), None);
    }

    /// Two scans can both be `plan_node_id` 1 in successive plans. The
    /// user table's count has to survive a catalog scan that reuses that id.
    #[test]
    fn scan_counts_are_per_node_not_per_plan_id() {
        clear_scan_rows();
        remember_scan_key(0xA, 40.0);
        remember_scan_key(0xB, 4.0);
        assert_eq!(counted_scan_key(0xA), Some(40.0));
        assert_eq!(counted_scan_key(0xB), Some(4.0));
        clear_scan_rows();
    }
}

pub fn commit_statement_cost() {
    let mut a = acc();
    if a.skip > 0 {
        a.skip -= 1;
        return;
    }
    let Some(frame) = a.frames.pop() else {
        return;
    };
    if !a.frames.is_empty() {
        return;
    }
    if frame.usd > 0.0 || frame.budget.is_some() {
        rvbbit_cost::arm_query(frame.usd, frame.budget);
    } else {
        rvbbit_cost::disarm_query();
    }
}

fn account(est: &Shown) {
    let mut a = acc();
    if a.skip > 0 {
        return;
    }
    let Some(frame) = a.frames.last_mut() else {
        return;
    };
    frame.usd += est.usd;
    if let Some(b) = est.clause_budget {
        frame.budget = Some(frame.budget.map_or(b, |c| c.min(b)));
    }
}

pub fn semantic_result(node: Node<'_>) -> bool {
    let Some(plan) = node.as_plan() else {
        return false;
    };
    node.node_tag() == NodeTag::T_Result
        && (qual_contains_means(&plan.qual)
            || crate::tlist_contains_project(&plan.targetlist)
            || crate::tlist_contains_decide(&plan.targetlist))
}
