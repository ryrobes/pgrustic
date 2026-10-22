//! Move semantic work to where it sees fewer rows.
//!
//! A filter stays under an inner join when the join multiplies its input,
//! and moves above the join when the join emits fewer rows. A project or
//! decision tree that sits under `LIMIT` moves above it, so the limit
//! runs on the raw columns. A filter never crosses `LIMIT`.

use mcx::Mcx;
use types_error::PgResult;
use types_nodes::jointype::JoinType;
use types_nodes::{Node, NodeTag};

use crate::estimate::semantic_result;
use crate::qual_contains_means;

pub fn place<'mcx>(mcx: Mcx<'mcx>, plan: Node<'mcx>) -> PgResult<Node<'mcx>> {
    if plan.as_plan().is_none() {
        return Ok(plan);
    }
    if let Some(left) = plan.as_plan().and_then(|p| p.lefttree) {
        let new_left = place(mcx, left)?;
        unsafe { plan.with_plan_mut(|p| p.lefttree = Some(new_left)) }.expect("plan");
    }
    if let Some(right) = plan.as_plan().and_then(|p| p.righttree) {
        let new_right = place(mcx, right)?;
        unsafe { plan.with_plan_mut(|p| p.righttree = Some(new_right)) }.expect("plan");
    }
    crate::lift_list_children(mcx, plan)?;
    if plan.node_tag() == NodeTag::T_Limit {
        return hoist_project(mcx, plan);
    }
    if is_join(plan) {
        return hoist_filter(mcx, plan);
    }
    Ok(plan)
}

fn is_join(node: Node<'_>) -> bool {
    matches!(
        node.node_tag(),
        NodeTag::T_NestLoop | NodeTag::T_HashJoin | NodeTag::T_MergeJoin
    )
}

fn join_type(node: Node<'_>) -> JoinType {
    if let Some(n) = node.as_nest_loop() {
        n.join.jointype
    } else if let Some(n) = node.as_merge_join() {
        n.join.jointype
    } else if let Some(n) = node.as_hash_join() {
        n.join.jointype
    } else {
        JoinType::JOIN_INNER
    }
}

fn hoist_project<'mcx>(mcx: Mcx<'mcx>, limit: Node<'mcx>) -> PgResult<Node<'mcx>> {
    let Some(child) = limit.as_plan().and_then(|p| p.lefttree) else {
        return Ok(limit);
    };
    if !semantic_result(child) || qual_contains_means(&child.as_plan().expect("result").qual) {
        return Ok(limit);
    }
    let Some(grand) = child.as_plan().and_then(|p| p.lefttree) else {
        return Ok(limit);
    };
    let grand_tlist = grand.as_plan().expect("child").targetlist.clone_in(mcx)?;
    let limit_rows = limit.as_plan().expect("limit").plan_rows;
    unsafe {
        limit.with_plan_mut(|p| {
            p.lefttree = Some(grand);
            p.targetlist = grand_tlist;
        })
    }
    .expect("limit");
    unsafe {
        child.with_plan_mut(|p| {
            p.lefttree = Some(limit);
            p.plan_rows = limit_rows;
        })
    }
    .expect("project");
    Ok(child)
}

fn hoist_filter<'mcx>(mcx: Mcx<'mcx>, join: Node<'mcx>) -> PgResult<Node<'mcx>> {
    if join_type(join) != JoinType::JOIN_INNER {
        return Ok(join);
    }
    if let Some(rewritten) = hoist_side(mcx, join, true)? {
        return Ok(rewritten);
    }
    if let Some(rewritten) = hoist_side(mcx, join, false)? {
        return Ok(rewritten);
    }
    Ok(join)
}

fn hoist_side<'mcx>(mcx: Mcx<'mcx>, join: Node<'mcx>, left: bool) -> PgResult<Option<Node<'mcx>>> {
    let plan = join.as_plan().expect("join");
    let child = if left { plan.lefttree } else { plan.righttree };
    let Some(child) = child else {
        return Ok(None);
    };
    if child.node_tag() != NodeTag::T_Result || !qual_contains_means(&child.as_plan().expect("result").qual) {
        return Ok(None);
    }
    let Some(scan) = child.as_plan().and_then(|p| p.lefttree) else {
        return Ok(None);
    };
    let input_rows = scan.as_plan().map(|p| p.plan_rows).unwrap_or(0.0);
    let join_rows = plan.plan_rows;
    if !(join_rows + 0.5 < input_rows) {
        return Ok(None);
    }
    let output = plan.targetlist.clone_in(mcx)?;
    let mut tlist = output.clone_in(mcx)?;
    let qual = child.as_plan().expect("filter").qual.clone_in(mcx)?;
    crate::add_junk_inputs(mcx, &qual, &mut tlist)?;
    unsafe {
        join.with_plan_mut(|p| {
            if left {
                p.lefttree = Some(scan);
            } else {
                p.righttree = Some(scan);
            }
            p.targetlist = tlist;
        })
    }
    .expect("join");
    unsafe {
        child.with_plan_mut(|p| {
            p.lefttree = Some(join);
            p.righttree = None;
            p.targetlist = output;
        })
    }
    .expect("filter");
    Ok(Some(child))
}
