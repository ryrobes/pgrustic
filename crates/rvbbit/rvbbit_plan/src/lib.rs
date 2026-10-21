//! Lift `rvbbit_means` filters off base scans onto a Result node.
//!
//! The Result is the semantic batch node: its qual is only the MEANS
//! predicates, and the scan underneath keeps the cheap quals so they run
//! first. Columns the MEANS arguments read are added to the scan target
//! list (junk) when the parent did not already need them, so setrefs can
//! retarget the Result qual onto the scan output.

use mcx::Mcx;
use rvbbit_oids::MEANS_OID;
use types_error::PgResult;
use types_nodes::list::NodeList;
use types_nodes::plannodes::{Append, MergeAppend, Result as ResultPlan, SubqueryScan};
use types_nodes::primnodes::{FuncExpr, RelabelType, TargetEntry, Var};
use types_nodes::{Node, NodeTag};

pub fn qual_contains_means(qual: &NodeList<'_>) -> bool {
    qual.iter().any(expr_contains_means)
}

pub fn expr_contains_means(node: Node<'_>) -> bool {
    match node.node_tag() {
        NodeTag::T_FuncExpr => {
            let f = node.as_func_expr().expect("FuncExpr");
            f.funcid == MEANS_OID || f.args.iter().any(expr_contains_means)
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

/// Walk `plan` and wrap any base scan whose qual contains MEANS.
pub fn lift_semantic_filters<'mcx>(mcx: Mcx<'mcx>, plan: Node<'mcx>) -> PgResult<Node<'mcx>> {
    if plan.as_plan().is_none() {
        return Ok(plan);
    }
    if let Some(left) = plan.as_plan().and_then(|p| p.lefttree) {
        let new_left = lift_semantic_filters(mcx, left)?;
        // SAFETY: the plan tree was just built and is exclusively owned.
        unsafe { plan.with_plan_mut(|p| p.lefttree = Some(new_left)) }.expect("plan node");
    }
    if let Some(right) = plan.as_plan().and_then(|p| p.righttree) {
        let new_right = lift_semantic_filters(mcx, right)?;
        unsafe { plan.with_plan_mut(|p| p.righttree = Some(new_right)) }.expect("plan node");
    }
    lift_list_field(mcx, plan)?;
    if is_base_scan(plan) && qual_contains_means(&plan.as_plan().expect("plan").qual) {
        wrap_scan(mcx, plan)
    } else {
        Ok(plan)
    }
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

fn lift_list_field<'mcx>(mcx: Mcx<'mcx>, plan: Node<'mcx>) -> PgResult<()> {
    match plan.node_tag() {
        NodeTag::T_Append => {
            let children: Vec<Node<'mcx>> = plan.as_append().expect("Append").appendplans.iter().collect();
            let mut lifted = NodeList::nil();
            for child in children {
                lifted.lappend(mcx, lift_semantic_filters(mcx, child)?)?;
            }
            unsafe { plan.with_mut::<Append, _>(|a| a.appendplans = lifted) }.expect("Append");
        }
        NodeTag::T_MergeAppend => {
            let children: Vec<Node<'mcx>> =
                plan.as_merge_append().expect("MergeAppend").mergeplans.iter().collect();
            let mut lifted = NodeList::nil();
            for child in children {
                lifted.lappend(mcx, lift_semantic_filters(mcx, child)?)?;
            }
            unsafe { plan.with_mut::<MergeAppend, _>(|a| a.mergeplans = lifted) }.expect("MergeAppend");
        }
        NodeTag::T_SubqueryScan => {
            if let Some(sub) = plan.as_subquery_scan().expect("SubqueryScan").subplan {
                let new_sub = lift_semantic_filters(mcx, sub)?;
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

fn add_junk_inputs<'mcx>(
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
