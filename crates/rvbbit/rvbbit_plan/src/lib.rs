//! Lift semantic calls off base scans onto a Result node.
//!
//! A MEANS or ABOUT qual becomes the filter variant: the Result qual is
//! only those predicates, and the scan underneath keeps the cheap quals.
//! A top-level native `CLASSIFY` or typed `EXTRACT` becomes the project
//! variant when the scan has no MEANS qual and no `rvbbit.decide`. Columns
//! the call reads are added to the scan target list (junk) when the parent
//! did not already need them, so setrefs can retarget the Result onto the
//! scan output. Nested calls and a mix with MEANS or a decision tree stay
//! on the row-at-a-time builtin.

mod estimate;
mod place;

use mcx::Mcx;
use types_error::PgResult;
use types_nodes::list::NodeList;
use types_nodes::jointype::JoinType;
use types_nodes::plannodes::{Append, HashJoin, MergeAppend, MergeJoin, NestLoop, Result as ResultPlan, SubqueryScan};
use types_nodes::primnodes::{FuncExpr, RelabelType, TargetEntry, Var};
use types_nodes::{Node, NodeTag};

pub fn qual_contains_means(qual: &NodeList<'_>) -> bool {
    qual.iter().any(expr_contains_means)
}

/// `about` when the qual embeds, otherwise `means`.
pub fn qual_semantic_name(qual: &NodeList<'_>) -> &'static str {
    if qual.iter().any(expr_contains_about) {
        "about"
    } else {
        "means"
    }
}

pub fn expr_contains_means(node: Node<'_>) -> bool {
    match node.node_tag() {
        NodeTag::T_FuncExpr => {
            let f = node.as_func_expr().expect("FuncExpr");
            rvbbit_resolve::is_semantic_func(f.funcid)
                || func_is_clause_bool(f.funcid)
                || f.args.iter().any(expr_contains_means)
        }
        NodeTag::T_BoolExpr => node
            .as_bool_expr()
            .expect("BoolExpr")
            .args
            .iter()
            .any(expr_contains_means),
        NodeTag::T_RelabelType => expr_contains_means(node.as_relabel_type().expect("Relabel").arg),
        NodeTag::T_CoerceViaIO => expr_contains_means(node.as_coerce_via_io().expect("CoerceViaIO").arg),
        NodeTag::T_OpExpr => node
            .as_op_expr()
            .expect("OpExpr")
            .args
            .iter()
            .any(expr_contains_means),
        _ => false,
    }
}

/// Fill the tool and user-function caches. Called from `pg_plan_query`,
/// before `planner`. SPI from inside `planner` reenters it and deadlocks.
/// A cold cache left `count(*)` over a subquery on the serial scan path.
pub fn load_installed_cache() -> PgResult<()> {
    rvbbit_resolve::load_installed()
}

/// Walk `plan` and wrap any base scan whose qual contains MEANS.
pub fn lift_semantic_filters<'mcx>(mcx: Mcx<'mcx>, plan: Node<'mcx>) -> PgResult<Node<'mcx>> {
    let plan = lift_tree(mcx, plan)?;
    place::place(mcx, plan)
}

pub use estimate::{
    apply_costs, begin_statement_cost, commit_statement_cost, in_nested_planner, node_estimate,
    remember_scan_rows,
    tree_estimate, Shown,
};
pub use rvbbit_cost::{mcp_batches, node_actual, query_actual};

fn lift_tree<'mcx>(mcx: Mcx<'mcx>, plan: Node<'mcx>) -> PgResult<Node<'mcx>> {
    if plan.as_plan().is_none() {
        return Ok(plan);
    }
    if let Some(left) = plan.as_plan().and_then(|p| p.lefttree) {
        let new_left = lift_tree(mcx, left)?;
        // SAFETY: the plan tree was just built and is exclusively owned.
        unsafe { plan.with_plan_mut(|p| p.lefttree = Some(new_left)) }.expect("plan node");
    }
    if let Some(right) = plan.as_plan().and_then(|p| p.righttree) {
        let new_right = lift_tree(mcx, right)?;
        unsafe { plan.with_plan_mut(|p| p.righttree = Some(new_right)) }.expect("plan node");
    }
    lift_list_field(mcx, plan)?;
    // MEANS stays a filter on a Result. A top-level `rvbbit.decide` in a
    // scan target list is the same kind of Result: the scan emits the
    // columns, and the Result evaluates the tree. Leaving the call on the
    // scan would make setrefs turn the Result expression into a plain Var
    // of an already-computed column, and the batcher would never see it.
    let plan = if let Some(wrapped) = lift_about_join(mcx, plan)? {
        wrapped
    } else if is_base_scan(plan) && qual_contains_means(&plan.as_plan().expect("plan").qual) {
        wrap_scan(mcx, plan)?
    } else if is_base_scan(plan) && tlist_contains_decide(&plan.as_plan().expect("plan").targetlist) {
        wrap_decide(mcx, plan)?
    } else if is_base_scan(plan) && tlist_contains_project(&plan.as_plan().expect("plan").targetlist) {
        wrap_project(mcx, plan)?
    } else if is_join(plan) && tlist_contains_project(&plan.as_plan().expect("plan").targetlist) {
        wrap_project(mcx, plan)?
    } else if plan.node_tag() == NodeTag::T_Agg {
        sink_agg_project(mcx, plan)?
    } else {
        plan
    };
    strip_child_decide(mcx, plan)?;
    strip_child_project(mcx, plan)?;
    Ok(plan)
}

fn is_join(plan: Node<'_>) -> bool {
    matches!(
        plan.node_tag(),
        NodeTag::T_NestLoop | NodeTag::T_HashJoin | NodeTag::T_MergeJoin
    )
}

/// Move an inner join's ABOUT quals onto a Result so the pair list is
/// embedded in one batch. Outer joins keep the qual on the join: lifting
/// it would drop the preserved side.
fn lift_about_join<'mcx>(mcx: Mcx<'mcx>, plan: Node<'mcx>) -> PgResult<Option<Node<'mcx>>> {
    if !is_join(plan) {
        return Ok(None);
    }
    let (jointype, clauses) = match plan.node_tag() {
        NodeTag::T_NestLoop => {
            let join = &plan.as_nest_loop().expect("NestLoop").join;
            (join.jointype, join.joinqual.iter().collect::<Vec<_>>())
        }
        NodeTag::T_HashJoin => {
            let join = &plan.as_hash_join().expect("HashJoin").join;
            (join.jointype, join.joinqual.iter().collect::<Vec<_>>())
        }
        NodeTag::T_MergeJoin => {
            let join = &plan.as_merge_join().expect("MergeJoin").join;
            (join.jointype, join.joinqual.iter().collect::<Vec<_>>())
        }
        _ => return Ok(None),
    };
    if jointype != JoinType::JOIN_INNER || !clauses.iter().copied().any(expr_contains_about) {
        return Ok(None);
    }
    let mut keep = NodeList::nil();
    let mut about = NodeList::nil();
    for clause in clauses {
        if expr_contains_about(clause) {
            about.lappend(mcx, clause)?;
        } else {
            keep.lappend(mcx, clause)?;
        }
    }
    if about.is_nil() {
        return Ok(None);
    }
    match plan.node_tag() {
        NodeTag::T_NestLoop => {
            unsafe { plan.with_mut::<NestLoop, _>(|n| n.join.joinqual = keep) }.expect("NestLoop");
        }
        NodeTag::T_HashJoin => {
            unsafe { plan.with_mut::<HashJoin, _>(|n| n.join.joinqual = keep) }.expect("HashJoin");
        }
        NodeTag::T_MergeJoin => {
            unsafe { plan.with_mut::<MergeJoin, _>(|n| n.join.joinqual = keep) }.expect("MergeJoin");
        }
        _ => return Ok(None),
    }
    Ok(Some(wrap_filter(mcx, plan, about)?))
}

fn expr_contains_about(node: Node<'_>) -> bool {
    match node.node_tag() {
        NodeTag::T_FuncExpr => {
            let func = node.as_func_expr().expect("FuncExpr");
            func_is_about(func) || func.args.iter().any(expr_contains_about)
        }
        NodeTag::T_BoolExpr => node
            .as_bool_expr()
            .expect("BoolExpr")
            .args
            .iter()
            .any(expr_contains_about),
        NodeTag::T_RelabelType => expr_contains_about(node.as_relabel_type().expect("Relabel").arg),
        NodeTag::T_CoerceViaIO => expr_contains_about(node.as_coerce_via_io().expect("CoerceViaIO").arg),
        NodeTag::T_OpExpr => node
            .as_op_expr()
            .expect("OpExpr")
            .args
            .iter()
            .any(expr_contains_about),
        _ => false,
    }
}

fn func_is_about(func: &FuncExpr<'_>) -> bool {
    if rvbbit_resolve::is_semantic_func(func.funcid) {
        let ep = rvbbit_resolve::endpoint_for(func.funcid);
        if ep.fn_name.eq_ignore_ascii_case("about") {
            return true;
        }
    }
    if !syscache_seams::pg_proc_proname::is_installed() {
        return false;
    }
    let Ok(Some(name)) = syscache_seams::pg_proc_proname::call(func.funcid) else {
        return false;
    };
    name.name_str() == b"about"
}

fn wrap_filter<'mcx>(mcx: Mcx<'mcx>, child: Node<'mcx>, semantic: NodeList<'mcx>) -> PgResult<Node<'mcx>> {
    let base = child.as_plan().expect("plan");
    let result_tlist = base.targetlist.clone_in(mcx)?;
    let mut child_tlist = base.targetlist.clone_in(mcx)?;
    add_junk_inputs(mcx, &semantic, &mut child_tlist)?;
    let startup_cost = base.startup_cost;
    let total_cost = base.total_cost;
    let plan_rows = base.plan_rows;
    let plan_width = base.plan_width;
    unsafe {
        child.with_plan_mut(|p| {
            p.targetlist = child_tlist;
            p.parallel_safe = false;
        })
    }
    .expect("plan");
    let mut result = Node::build::<ResultPlan<'mcx>>(mcx)?;
    result.plan.targetlist = result_tlist;
    result.plan.qual = semantic;
    result.plan.lefttree = Some(child);
    result.plan.startup_cost = startup_cost;
    result.plan.total_cost = total_cost;
    result.plan.plan_rows = plan_rows;
    result.plan.plan_width = plan_width;
    result.plan.parallel_aware = false;
    result.plan.parallel_safe = false;
    result.resconstantqual = None;
    Ok(result.seal())
}

fn is_base_scan(plan: Node<'_>) -> bool {
    matches!(
        plan.node_tag(),
        NodeTag::T_SeqScan
            | NodeTag::T_SampleScan
            | NodeTag::T_IndexScan
            | NodeTag::T_IndexOnlyScan
            | NodeTag::T_BitmapHeapScan
            | NodeTag::T_TidScan
            | NodeTag::T_TidRangeScan
            | NodeTag::T_FunctionScan
            | NodeTag::T_ValuesScan
            | NodeTag::T_SubqueryScan
    )
}

pub(crate) fn lift_list_children<'mcx>(mcx: Mcx<'mcx>, plan: Node<'mcx>) -> PgResult<()> {
    // Placement runs after the lift, so list children are already lifted.
    // Re-walk them so a project under a Limit inside an Append still moves.
    match plan.node_tag() {
        NodeTag::T_Append => {
            let children: Vec<Node<'mcx>> = plan.as_append().expect("Append").appendplans.iter().collect();
            let mut placed = NodeList::nil();
            for child in children {
                placed.lappend(mcx, place::place(mcx, child)?)?;
            }
            unsafe { plan.with_mut::<Append, _>(|a| a.appendplans = placed) }.expect("Append");
        }
        NodeTag::T_MergeAppend => {
            let children: Vec<Node<'mcx>> =
                plan.as_merge_append().expect("MergeAppend").mergeplans.iter().collect();
            let mut placed = NodeList::nil();
            for child in children {
                placed.lappend(mcx, place::place(mcx, child)?)?;
            }
            unsafe { plan.with_mut::<MergeAppend, _>(|a| a.mergeplans = placed) }.expect("MergeAppend");
        }
        NodeTag::T_SubqueryScan => {
            if let Some(sub) = plan.as_subquery_scan().expect("SubqueryScan").subplan {
                let new_sub = place::place(mcx, sub)?;
                unsafe { plan.with_mut::<SubqueryScan, _>(|s| s.subplan = Some(new_sub)) }
                    .expect("SubqueryScan");
            }
        }
        _ => {}
    }
    Ok(())
}

fn lift_list_field<'mcx>(mcx: Mcx<'mcx>, plan: Node<'mcx>) -> PgResult<()> {
    match plan.node_tag() {
        NodeTag::T_Append => {
            let children: Vec<Node<'mcx>> = plan.as_append().expect("Append").appendplans.iter().collect();
            let mut lifted = NodeList::nil();
            for child in children {
                lifted.lappend(mcx, lift_tree(mcx, child)?)?;
            }
            unsafe { plan.with_mut::<Append, _>(|a| a.appendplans = lifted) }.expect("Append");
        }
        NodeTag::T_MergeAppend => {
            let children: Vec<Node<'mcx>> =
                plan.as_merge_append().expect("MergeAppend").mergeplans.iter().collect();
            let mut lifted = NodeList::nil();
            for child in children {
                lifted.lappend(mcx, lift_tree(mcx, child)?)?;
            }
            unsafe { plan.with_mut::<MergeAppend, _>(|a| a.mergeplans = lifted) }.expect("MergeAppend");
        }
        NodeTag::T_SubqueryScan => {
            if let Some(sub) = plan.as_subquery_scan().expect("SubqueryScan").subplan {
                let new_sub = lift_tree(mcx, sub)?;
                unsafe { plan.with_mut::<SubqueryScan, _>(|s| s.subplan = Some(new_sub)) }
                    .expect("SubqueryScan");
            }
        }
        _ => {}
    }
    Ok(())
}

fn wrap_scan<'mcx>(mcx: Mcx<'mcx>, scan: Node<'mcx>) -> PgResult<Node<'mcx>> {
    let base = scan.as_plan().expect("scan plan");
    let mut cheap = NodeList::nil();
    let mut semantic = NodeList::nil();
    for clause in base.qual.iter() {
        if expr_contains_means(clause) {
            semantic.lappend(mcx, clause)?;
        } else {
            cheap.lappend(mcx, clause)?;
        }
    }
    if semantic.is_nil() {
        return Ok(scan);
    }

    // Two list spines over the same target-entry nodes. setrefs replaces
    // the Result list with fresh OUTER_VAR entries and leaves these nodes
    // for the scan, which it then fixes on its own.
    let result_tlist = base.targetlist.clone_in(mcx)?;
    let mut scan_tlist = base.targetlist.clone_in(mcx)?;
    add_junk_inputs(mcx, &semantic, &mut scan_tlist)?;

    let startup_cost = base.startup_cost;
    let total_cost = base.total_cost;
    let plan_rows = base.plan_rows;
    let plan_width = base.plan_width;

    unsafe {
        scan.with_plan_mut(|p| {
            p.qual = cheap;
            p.targetlist = scan_tlist;
            p.parallel_safe = false;
        })
    }
    .expect("scan plan");

    let mut result = Node::build::<ResultPlan<'mcx>>(mcx)?;
    result.plan.targetlist = result_tlist;
    result.plan.qual = semantic;
    result.plan.lefttree = Some(scan);
    result.plan.startup_cost = startup_cost;
    result.plan.total_cost = total_cost;
    result.plan.plan_rows = plan_rows;
    result.plan.plan_width = plan_width;
    result.plan.parallel_aware = false;
    result.plan.parallel_safe = false;
    result.resconstantqual = None;
    Ok(result.seal())
}

pub(crate) fn add_junk_inputs<'mcx>(
    mcx: Mcx<'mcx>,
    semantic: &NodeList<'mcx>,
    tlist: &mut NodeList<'mcx>,
) -> PgResult<()> {
    let mut vars = Vec::new();
    for clause in semantic.iter() {
        collect_vars(clause, &mut vars);
    }
    let mut next_resno = 1i16;
    for tle in tlist.iter() {
        let te = tle.as_target_entry().expect("TargetEntry");
        if te.resno >= next_resno {
            next_resno = te.resno.saturating_add(1);
        }
    }
    for var_node in vars {
        let var = var_node.as_var().expect("Var");
        if tlist_has_var(tlist, var.varno, var.varattno) {
            continue;
        }
        let copied = Node::mk(
            mcx,
            Var {
                varno: var.varno,
                varattno: var.varattno,
                vartype: var.vartype,
                vartypmod: var.vartypmod,
                varcollid: var.varcollid,
                varnullingrels: var.varnullingrels.clone_in(mcx)?,
                varlevelsup: var.varlevelsup,
                varreturningtype: var.varreturningtype,
                varnosyn: var.varnosyn,
                varattnosyn: var.varattnosyn,
                location: var.location,
            },
        )?;
        let tle = Node::mk(
            mcx,
            TargetEntry {
                expr: copied,
                resno: next_resno,
                resname: None,
                ressortgroupref: 0,
                resorigtbl: 0,
                resorigcol: 0,
                resjunk: true,
            },
        )?;
        tlist.lappend(mcx, tle)?;
        next_resno = next_resno.saturating_add(1);
    }
    Ok(())
}

/// `pg_proc` name lookup. The decide oid is not a fixed builtin number, and
/// a fresh backend has not loaded the rvbbit cache yet. Missing seams (planner
/// unit tests) count as "not decide" so ordinary plans are unchanged.
/// `rvbbit.clause_bool` is the WHERE form of `MEANS ... WITH BUDGET`.
/// It is not a catalog MEANS operator, so the oid cache does not know it.
fn func_is_clause_bool(funcid: types_core::Oid) -> bool {
    if funcid == rvbbit_oids::CLAUSE_BOOL_OID {
        return true;
    }
    if !syscache_seams::pg_proc_proname::is_installed()
        || !syscache_seams::lookup_pg_proc_shape::is_installed()
        || !syscache_seams::pg_namespace_nspname::is_installed()
    {
        return false;
    }
    let Ok(Some(name)) = syscache_seams::pg_proc_proname::call(funcid) else {
        return false;
    };
    if name.name_str() != b"clause_bool" {
        return false;
    }
    let Ok(Some(shape)) = syscache_seams::lookup_pg_proc_shape::call(funcid) else {
        return false;
    };
    let Ok(Some(ns)) = syscache_seams::pg_namespace_nspname::call(shape.pronamespace) else {
        return false;
    };
    ns.name_str() == b"rvbbit"
}

fn func_is_decide(funcid: types_core::Oid) -> bool {
    if rvbbit_resolve::is_decide_func(funcid) {
        return true;
    }
    if !syscache_seams::pg_proc_proname::is_installed()
        || !syscache_seams::lookup_pg_proc_shape::is_installed()
        || !syscache_seams::pg_namespace_nspname::is_installed()
    {
        return false;
    }
    let Ok(Some(name)) = syscache_seams::pg_proc_proname::call(funcid) else {
        return false;
    };
    if name.name_str() != b"decide" {
        return false;
    }
    let Ok(Some(shape)) = syscache_seams::lookup_pg_proc_shape::call(funcid) else {
        return false;
    };
    let Ok(Some(ns)) = syscache_seams::pg_namespace_nspname::call(shape.pronamespace) else {
        return false;
    };
    ns.name_str() == b"rvbbit"
}

fn expr_contains_decide(node: Node<'_>) -> bool {
    match node.node_tag() {
        NodeTag::T_FuncExpr => {
            let f = node.as_func_expr().expect("FuncExpr");
            func_is_decide(f.funcid) || f.args.iter().any(expr_contains_decide)
        }
        NodeTag::T_BoolExpr => node
            .as_bool_expr()
            .expect("BoolExpr")
            .args
            .iter()
            .any(expr_contains_decide),
        NodeTag::T_RelabelType => expr_contains_decide(node.as_relabel_type().expect("Relabel").arg),
        NodeTag::T_CoerceViaIO => expr_contains_decide(node.as_coerce_via_io().expect("CoerceViaIO").arg),
        NodeTag::T_OpExpr => node
            .as_op_expr()
            .expect("OpExpr")
            .args
            .iter()
            .any(expr_contains_decide),
        NodeTag::T_List => node.as_list().is_some_and(|list| list.iter().any(expr_contains_decide)),
        _ => false,
    }
}

pub(crate) fn tlist_contains_decide(tlist: &NodeList<'_>) -> bool {
    tlist.iter().any(|tle| {
        tle.as_target_entry()
            .is_some_and(|te| expr_contains_decide(te.expr))
    })
}

fn tlist_without_decide<'mcx>(
    mcx: Mcx<'mcx>,
    tlist: &NodeList<'mcx>,
) -> PgResult<NodeList<'mcx>> {
    let mut kept = NodeList::nil();
    let mut decide_exprs = NodeList::nil();
    let mut resno = 1i16;
    for tle_node in tlist.iter() {
        let tle = tle_node.as_target_entry().expect("TargetEntry");
        if expr_contains_decide(tle.expr) {
            decide_exprs.lappend(mcx, tle.expr)?;
        } else {
            kept.lappend(mcx, copy_target_entry(mcx, tle, resno)?)?;
            resno = resno.saturating_add(1);
        }
    }
    add_junk_inputs(mcx, &decide_exprs, &mut kept)?;
    Ok(kept)
}

fn wrap_decide<'mcx>(mcx: Mcx<'mcx>, scan: Node<'mcx>) -> PgResult<Node<'mcx>> {
    let base = scan.as_plan().expect("scan plan");
    let result_tlist = base.targetlist.clone_in(mcx)?;
    let scan_tlist = tlist_without_decide(mcx, &base.targetlist)?;
    let startup_cost = base.startup_cost;
    let total_cost = base.total_cost;
    let plan_rows = base.plan_rows;
    let plan_width = base.plan_width;
    unsafe {
        scan.with_plan_mut(|p| {
            p.targetlist = scan_tlist;
            p.parallel_safe = false;
        })
    }
    .expect("scan plan");
    let mut result = Node::build::<ResultPlan<'mcx>>(mcx)?;
    result.plan.targetlist = result_tlist;
    result.plan.qual = NodeList::nil();
    result.plan.lefttree = Some(scan);
    result.plan.startup_cost = startup_cost;
    result.plan.total_cost = total_cost;
    result.plan.plan_rows = plan_rows;
    result.plan.plan_width = plan_width;
    result.plan.parallel_aware = false;
    result.plan.parallel_safe = false;
    result.resconstantqual = None;
    Ok(result.seal())
}

fn func_is_native_project(func: &FuncExpr<'_>) -> bool {
    let nargs = func.args.len();
    if nargs != 2 && nargs != 3 {
        return false;
    }
    if !syscache_seams::pg_proc_proname::is_installed()
        || !syscache_seams::lookup_pg_proc_shape::is_installed()
        || !syscache_seams::pg_namespace_nspname::is_installed()
    {
        return false;
    }
    let Ok(Some(name)) = syscache_seams::pg_proc_proname::call(func.funcid) else {
        return false;
    };
    let native = match (name.name_str(), nargs) {
        (b"classify" | b"classify_enum" | b"extract_typed" | b"extract_many", 3) => true,
        (b"rerank" | b"rerank_for", 2 | 3) => true,
        (b"semantic_similarity", 2) => true,
        _ => false,
    };
    if !native {
        return false;
    }
    let Ok(Some(shape)) = syscache_seams::lookup_pg_proc_shape::call(func.funcid) else {
        return false;
    };
    let Ok(Some(ns)) = syscache_seams::pg_namespace_nspname::call(shape.pronamespace) else {
        return false;
    };
    ns.name_str() == b"rvbbit"
}

/// An imported MCP tool. Lifted onto a Result so a subquery or aggregate
/// batches it the same way a native project call does. Set-returning tools
/// stay on the scan.
fn func_is_mcp_project(func: &FuncExpr<'_>) -> bool {
    if func.funcretset {
        return false;
    }
    if let Some(tool) = rvbbit_resolve::mcp_tool_by_oid(func.funcid) {
        return !tool.returns_table;
    }
    // An inlined SQL body is `rvbbit.mcp_invoke_text`. The tool's own
    // function is recognized from the cache `load_installed` fills
    // before this walk. Do not look up prosrc here: that seam needs a
    // live memory context, and a cold cache is filled by the load.
    if !syscache_seams::pg_proc_proname::is_installed()
        || !syscache_seams::lookup_pg_proc_shape::is_installed()
        || !syscache_seams::pg_namespace_nspname::is_installed()
    {
        return false;
    }
    let Ok(Some(name)) = syscache_seams::pg_proc_proname::call(func.funcid) else {
        return false;
    };
    if name.name_str() != b"mcp_invoke_text" {
        return false;
    }
    let Ok(Some(shape)) = syscache_seams::lookup_pg_proc_shape::call(func.funcid) else {
        return false;
    };
    let Ok(Some(ns)) = syscache_seams::pg_namespace_nspname::call(shape.pronamespace) else {
        return false;
    };
    ns.name_str() == b"rvbbit"
}

/// A catalog function whose body is `rvbbit_user`. The Result batcher
/// prefetches it. Pipe workflows stay out: the planner inlines those.
fn func_is_user_project(func: &FuncExpr<'_>) -> bool {
    let nargs = func.args.len();
    if !(1..=8).contains(&nargs) {
        return false;
    }
    if !rvbbit_resolve::is_semantic_func(func.funcid) {
        return false;
    }
    let ep = rvbbit_resolve::endpoint_for(func.funcid);
    ep.user_fn && ep.system_prompt != rvbbit_resolve::PIPE_BODY
}

/// True when the expression is a native project call or contains one.
/// `round(rvbbit.rerank(...))` and a cast of that call are included, so
/// the scan target moves onto the Result and the batcher sees the call.
fn expr_contains_project(node: Node<'_>) -> bool {
    match node.node_tag() {
        NodeTag::T_RelabelType => expr_contains_project(node.as_relabel_type().expect("Relabel").arg),
        NodeTag::T_CoerceViaIO => expr_contains_project(node.as_coerce_via_io().expect("CoerceViaIO").arg),
        NodeTag::T_FuncExpr => {
            let func = node.as_func_expr().expect("FuncExpr");
            func_is_native_project(func)
                || func_is_user_project(func)
                || func_is_mcp_project(func)
                || func.args.iter().any(expr_contains_project)
        }
        NodeTag::T_OpExpr => node
            .as_op_expr()
            .expect("OpExpr")
            .args
            .iter()
            .any(expr_contains_project),
        NodeTag::T_BoolExpr => node
            .as_bool_expr()
            .expect("BoolExpr")
            .args
            .iter()
            .any(expr_contains_project),
        _ => false,
    }
}

pub(crate) fn tlist_contains_project(tlist: &NodeList<'_>) -> bool {
    tlist.iter().any(|tle| {
        tle.as_target_entry()
            .is_some_and(|te| expr_contains_project(te.expr))
    })
}

/// Scan target list with project calls removed.
///
/// `NodeList::clone_in` is shallow, so renumbering a cloned entry also
/// renumbers the Result list that still owns that node. Projection writes
/// by `resno` and the tuple descriptor is positional, so a kept column
/// then lands on a semantic column (the label shows the row text, or a
/// numeric slot is read as text). The scan gets new `TargetEntry` nodes.
/// The Result keeps the original resnos. The expression pointer is shared;
/// setrefs builds a fresh tree for the Result before it fixes the scan.
fn tlist_without_project<'mcx>(
    mcx: Mcx<'mcx>,
    tlist: &NodeList<'mcx>,
) -> PgResult<NodeList<'mcx>> {
    let mut kept = NodeList::nil();
    let mut project_exprs = NodeList::nil();
    let mut resno = 1i16;
    for tle_node in tlist.iter() {
        let tle = tle_node.as_target_entry().expect("TargetEntry");
        if expr_contains_project(tle.expr) {
            project_exprs.lappend(mcx, tle.expr)?;
        } else {
            kept.lappend(mcx, copy_target_entry(mcx, tle, resno)?)?;
            resno = resno.saturating_add(1);
        }
    }
    add_junk_inputs(mcx, &project_exprs, &mut kept)?;
    Ok(kept)
}

fn copy_target_entry<'mcx>(
    mcx: Mcx<'mcx>,
    src: &TargetEntry<'mcx>,
    resno: i16,
) -> PgResult<Node<'mcx>> {
    Node::mk(
        mcx,
        TargetEntry {
            expr: src.expr,
            resno,
            resname: src.resname,
            ressortgroupref: src.ressortgroupref,
            resorigtbl: src.resorigtbl,
            resorigcol: src.resorigcol,
            resjunk: src.resjunk,
        },
    )
}

/// `count(*)` over a subquery can leave a volatile tool on the Aggregate
/// target list as resjunk. That is not a Result, so the MCP batcher never
/// sees it and each row opens its own stdio process. Move those resjunk
/// calls onto a Result under the aggregate. The aggregate still counts
/// every child row, and it does not evaluate the tool a second time.
fn sink_agg_project<'mcx>(mcx: Mcx<'mcx>, agg: Node<'mcx>) -> PgResult<Node<'mcx>> {
    let base = agg.as_plan().expect("agg");
    if !tlist_contains_project(&base.targetlist) {
        return Ok(agg);
    }
    let Some(child) = base.lefttree else {
        return Ok(agg);
    };
    if !is_base_scan(child) && child.node_tag() != NodeTag::T_Result {
        return Ok(agg);
    }
    let extras: Vec<Node<'mcx>> = base
        .targetlist
        .iter()
        .filter_map(|tle_node| {
            let tle = tle_node.as_target_entry()?;
            if tle.resjunk && expr_contains_project(tle.expr) {
                Some(tle.expr)
            } else {
                None
            }
        })
        .collect();
    if extras.is_empty() {
        return Ok(agg);
    }
    let mut child_tlist = child.as_plan().expect("child").targetlist.clone_in(mcx)?;
    let mut resno = 1i16;
    for tle in child_tlist.iter() {
        let te = tle.as_target_entry().expect("TargetEntry");
        if te.resno >= resno {
            resno = te.resno.saturating_add(1);
        }
    }
    for expr in extras {
        let tle = Node::mk(
            mcx,
            TargetEntry {
                expr,
                resno,
                resname: None,
                ressortgroupref: 0,
                resorigtbl: 0,
                resorigcol: 0,
                resjunk: true,
            },
        )?;
        child_tlist.lappend(mcx, tle)?;
        resno = resno.saturating_add(1);
    }
    unsafe { child.with_plan_mut(|p| p.targetlist = child_tlist) }.expect("child");
    let wrapped = if is_base_scan(child) {
        wrap_project(mcx, child)?
    } else {
        child
    };
    unsafe { agg.with_plan_mut(|p| p.lefttree = Some(wrapped)) }.expect("agg");
    let mut kept = NodeList::nil();
    for tle_node in agg.as_plan().expect("agg").targetlist.iter() {
        let tle = tle_node.as_target_entry().expect("TargetEntry");
        if tle.resjunk && expr_contains_project(tle.expr) {
            continue;
        }
        kept.lappend(mcx, tle_node)?;
    }
    unsafe { agg.with_plan_mut(|p| p.targetlist = kept) }.expect("agg");
    Ok(agg)
}

fn wrap_project<'mcx>(mcx: Mcx<'mcx>, scan: Node<'mcx>) -> PgResult<Node<'mcx>> {
    let base = scan.as_plan().expect("scan plan");
    let result_tlist = base.targetlist.clone_in(mcx)?;
    let scan_tlist = tlist_without_project(mcx, &base.targetlist)?;
    let startup_cost = base.startup_cost;
    let total_cost = base.total_cost;
    let plan_rows = base.plan_rows;
    let plan_width = base.plan_width;
    unsafe {
        scan.with_plan_mut(|p| {
            p.targetlist = scan_tlist;
            p.parallel_safe = false;
        })
    }
    .expect("scan plan");
    let mut result = Node::build::<ResultPlan<'mcx>>(mcx)?;
    result.plan.targetlist = result_tlist;
    result.plan.qual = NodeList::nil();
    result.plan.lefttree = Some(scan);
    result.plan.startup_cost = startup_cost;
    result.plan.total_cost = total_cost;
    result.plan.plan_rows = plan_rows;
    result.plan.plan_width = plan_width;
    result.plan.parallel_aware = false;
    result.plan.parallel_safe = false;
    result.resconstantqual = None;
    Ok(result.seal())
}

/// A MEANS or decide Result was wrapped around a scan that still projects
/// a native CLASSIFY or EXTRACT. Drop that call from the scan so setrefs
/// leaves it on the Result. The filter or tree batcher then evaluates it
/// one row at a time. A project-only scan is wrapped by [`wrap_project`].
fn strip_child_project<'mcx>(mcx: Mcx<'mcx>, plan: Node<'mcx>) -> PgResult<()> {
    if plan.node_tag() != NodeTag::T_Result {
        return Ok(());
    }
    let Some(child) = plan.as_plan().and_then(|p| p.lefttree) else {
        return Ok(());
    };
    if !is_base_scan(child) {
        return Ok(());
    }
    let tlist = &child.as_plan().expect("scan").targetlist;
    if !tlist_contains_project(tlist) {
        return Ok(());
    }
    let new_tlist = tlist_without_project(mcx, tlist)?;
    unsafe { child.with_plan_mut(|p| p.targetlist = new_tlist) }.expect("scan");
    Ok(())
}

/// A MEANS Result was wrapped around a scan that still projects `decide`.
/// Drop that call from the scan so setrefs leaves it on the Result.
fn strip_child_decide<'mcx>(mcx: Mcx<'mcx>, plan: Node<'mcx>) -> PgResult<()> {
    if plan.node_tag() != NodeTag::T_Result {
        return Ok(());
    }
    let Some(child) = plan.as_plan().and_then(|p| p.lefttree) else {
        return Ok(());
    };
    if !is_base_scan(child) {
        return Ok(());
    }
    let tlist = &child.as_plan().expect("scan").targetlist;
    if !tlist_contains_decide(tlist) {
        return Ok(());
    }
    let new_tlist = tlist_without_decide(mcx, tlist)?;
    unsafe { child.with_plan_mut(|p| p.targetlist = new_tlist) }.expect("scan");
    Ok(())
}

fn tlist_has_var(tlist: &NodeList<'_>, varno: i32, varattno: i16) -> bool {
    tlist.iter().any(|tle| {
        tle.as_target_entry()
            .and_then(|te| te.expr.as_var())
            .is_some_and(|v| v.varno == varno && v.varattno == varattno)
    })
}

fn collect_vars<'mcx>(node: Node<'mcx>, out: &mut Vec<Node<'mcx>>) {
    match node.node_tag() {
        NodeTag::T_Var => out.push(node),
        NodeTag::T_FuncExpr => {
            let f: &FuncExpr<'_> = node.as_func_expr().expect("FuncExpr");
            for arg in f.args.iter() {
                collect_vars(arg, out);
            }
        }
        NodeTag::T_RelabelType => {
            let r: &RelabelType<'_> = node.as_relabel_type().expect("RelabelType");
            collect_vars(r.arg, out);
        }
        NodeTag::T_CoerceViaIO => collect_vars(node.as_coerce_via_io().expect("CoerceViaIO").arg, out),
        NodeTag::T_BoolExpr => {
            for arg in node.as_bool_expr().expect("BoolExpr").args.iter() {
                collect_vars(arg, out);
            }
        }
        NodeTag::T_OpExpr => {
            for arg in node.as_op_expr().expect("OpExpr").args.iter() {
                collect_vars(arg, out);
            }
        }
        _ => {}
    }
}
