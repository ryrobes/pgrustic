//! Batch executor for `rvbbit.decide(tree, input)` on a Result target list.
//!
//! A window of child rows is evaluated for the tree name and the input
//! only. Those pairs are grouped by tree and walked by
//! [`rvbbit_dispatch::decide_texts`]: one HTTP batch per node per level,
//! with duplicate inputs at a node served from the walker's memo.
//! Answers are remembered, then each saved input tuple is projected.
//! The strict builtin returns the remembered text and does not call
//! hutch again. The memo entries are dropped after the window is
//! snapshotted.
//!
//! A MEANS qual keeps the semantic batcher. `decide` in that query stays
//! on the builtin, one row at a time. A `decide` nested under another
//! expression stays on the builtin too. Only a top-level target entry
//! (after relabel and coerce-via-io) is batched.
//!
//! `CREATE BACKEND` or any earlier rvbbit statement in this session fills
//! the process cache. A fresh session whose first statement is the
//! `SELECT` still resolves `rvbbit.decide` by name and loads the cache
//! here, so the batcher runs without a prior statement in-process.

use std::collections::{BTreeMap, VecDeque};

use execexpr::ExprState;
use executils::{EStateData, ExecSlotId};
use lsyscache::{get_func_name, get_func_namespace, get_namespace_name};
use mcx::{Mcx, PgBox};
use types_core::Oid;
use types_error::{PgError, PgResult};
use types_nodes::plannodes::Result as ResultPlan;
use types_nodes::Node;
use types_nodes::NodeTag;
use types_nodes::list::NodeList;

use crate::noderesult::{lane_result_gate, lane_result_project, ResultState};
use crate::procnode::exec_proc_node;
use crate::semantic::{self, SavedTuple};

#[derive(Clone, Copy, Debug, Default)]
pub struct TreeStats {
    pub batches: u64,
    pub http_calls: u64,
    pub node_calls: u64,
    pub memo_hits: u64,
    pub rows_in: u64,
    pub rows_out: u64,
}

/// Where `rvbbit.decide` sits in a Result target list.
#[derive(Clone, Copy, Debug, Default)]
pub struct DecideShape {
    /// At least one top-level `rvbbit.decide`. The batcher can run.
    pub batched: bool,
    /// Any `rvbbit.decide`, including one nested in another expression.
    pub present: bool,
}

static STATS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<i32, TreeStats>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

pub fn clear_stats() {
    STATS.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

pub fn tree_stats(plan_node_id: i32) -> Option<TreeStats> {
    STATS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&plan_node_id)
        .copied()
}

pub fn decide_shape<'mcx>(mcx: Mcx<'mcx>, tlist: &NodeList<'mcx>) -> PgResult<DecideShape> {
    let mut shape = DecideShape::default();
    for item in tlist.iter() {
        let Some(entry) = item.as_target_entry() else {
            continue;
        };
        walk(mcx, entry.expr, true, &mut shape)?;
    }
    Ok(shape)
}

fn walk<'mcx>(mcx: Mcx<'mcx>, node: Node<'mcx>, top: bool, shape: &mut DecideShape) -> PgResult<()> {
    match node.node_tag() {
        NodeTag::T_RelabelType => {
            let arg = node.as_relabel_type().expect("Relabel").arg;
            walk(mcx, arg, top, shape)?;
        }
        NodeTag::T_CoerceViaIO => {
            let arg = node.as_coerce_via_io().expect("CoerceViaIO").arg;
            walk(mcx, arg, top, shape)?;
        }
        NodeTag::T_FuncExpr => {
            let func = node.as_func_expr().expect("FuncExpr");
            if is_decide(mcx, func.funcid)? {
                shape.present = true;
                if top {
                    shape.batched = true;
                }
            }
            for arg in func.args.iter() {
                walk(mcx, arg, false, shape)?;
            }
        }
        NodeTag::T_OpExpr => {
            let op = node.as_op_expr().expect("OpExpr");
            for arg in op.args.iter() {
                walk(mcx, arg, false, shape)?;
            }
        }
        NodeTag::T_BoolExpr => {
            let b = node.as_bool_expr().expect("BoolExpr");
            for arg in b.args.iter() {
                walk(mcx, arg, false, shape)?;
            }
        }
        NodeTag::T_CaseExpr => {
            let case = node.as_case_expr().expect("CaseExpr");
            if let Some(arg) = case.arg {
                walk(mcx, arg, false, shape)?;
            }
            for arg in case.args.iter() {
                walk(mcx, arg, false, shape)?;
            }
            if let Some(arg) = case.defresult {
                walk(mcx, arg, false, shape)?;
            }
        }
        NodeTag::T_List => {
            if let Some(list) = node.as_list() {
                for arg in list.iter() {
                    walk(mcx, arg, false, shape)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn is_decide<'mcx>(mcx: Mcx<'mcx>, funcid: Oid) -> PgResult<bool> {
    if rvbbit_resolve::is_decide_func(funcid) {
        return Ok(true);
    }
    let Some(name) = get_func_name(mcx, funcid)? else {
        return Ok(false);
    };
    if name.as_str() != "decide" {
        return Ok(false);
    }
    let nsp = get_func_namespace(funcid)?;
    let Some(nsname) = get_namespace_name(mcx, nsp)? else {
        return Ok(false);
    };
    Ok(nsname.as_str() == "rvbbit")
}

struct DecideSite<'mcx> {
    tree_name: PgBox<'mcx, ExprState<'mcx>>,
    input: PgBox<'mcx, ExprState<'mcx>>,
}

struct Pending {
    outer: Option<SavedTuple>,
    outer_slot: Option<ExecSlotId>,
    /// One entry per site. `None` is a NULL argument: the strict function
    /// returns NULL and this row does not join the HTTP batch.
    calls: Vec<Option<(String, String)>>,
}

struct Forget(Vec<(String, String)>);

impl Drop for Forget {
    fn drop(&mut self) {
        if !self.0.is_empty() {
            rvbbit_dispatch::forget_decide(&self.0);
        }
    }
}

pub struct TreeState<'mcx> {
    sites: Vec<DecideSite<'mcx>>,
    batch_rows: usize,
    emit: VecDeque<SavedTuple>,
    held: Option<SavedTuple>,
    child_done: bool,
    batches: u64,
    http_calls: u64,
    node_calls: u64,
    memo_hits: u64,
    rows_in: u64,
    rows_out: u64,
}

impl<'mcx> TreeState<'mcx> {
    fn snapshot(&self) -> TreeStats {
        TreeStats {
            batches: self.batches,
            http_calls: self.http_calls,
            node_calls: self.node_calls,
            memo_hits: self.memo_hits,
            rows_in: self.rows_in,
            rows_out: self.rows_out,
        }
    }

    fn publish(&self, plan_node_id: i32) {
        STATS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(plan_node_id, self.snapshot());
    }
}

pub fn compile<'mcx>(
    estate: &mut EStateData<'mcx>,
    plan: &'mcx ResultPlan<'mcx>,
) -> PgResult<Option<Box<TreeState<'mcx>>>> {
    let mcx = estate.es_query_cxt;
    let shape = decide_shape(mcx, &plan.plan.targetlist)?;
    if !shape.batched {
        return Ok(None);
    }
    // The function exists, so the rvbbit schema exists. This loads trees
    // and the pg_proc oid on a fresh backend. It does not run for queries
    // that never mention rvbbit.decide.
    rvbbit_resolve::ensure()?;
    let mut funcs = Vec::new();
    for item in plan.plan.targetlist.iter() {
        let Some(entry) = item.as_target_entry() else {
            continue;
        };
        if let Some(func) = top_decide(entry.expr) {
            funcs.push(func);
        }
    }
    if funcs.is_empty() {
        return Ok(None);
    }
    let params = estate.param_bind();
    let mut sites = Vec::with_capacity(funcs.len());
    for func in funcs {
        let f = func.as_func_expr().expect("FuncExpr");
        if f.args.len() != 2 {
            return Err(err("rvbbit.decide expects 2 arguments"));
        }
        let tree_name = semantic::compile_expr(estate, mcx, params, f.args.nth(0))?;
        let input = semantic::compile_expr(estate, mcx, params, f.args.nth(1))?;
        sites.push(DecideSite { tree_name, input });
    }
    Ok(Some(Box::new(TreeState {
        sites,
        batch_rows: rvbbit_resolve::batch_rows().max(1),
        emit: VecDeque::new(),
        held: None,
        child_done: false,
        batches: 0,
        http_calls: 0,
        node_calls: 0,
        memo_hits: 0,
        rows_in: 0,
        rows_out: 0,
    })))
}

fn top_decide(node: Node<'_>) -> Option<Node<'_>> {
    match node.node_tag() {
        NodeTag::T_RelabelType => top_decide(node.as_relabel_type().expect("Relabel").arg),
        NodeTag::T_CoerceViaIO => top_decide(node.as_coerce_via_io().expect("CoerceViaIO").arg),
        NodeTag::T_FuncExpr => {
            let func = node.as_func_expr().expect("FuncExpr");
            rvbbit_resolve::is_decide_func(func.funcid).then_some(node)
        }
        _ => None,
    }
}

pub fn exec<'mcx>(
    node: &mut ResultState<'mcx>,
    estate: &mut EStateData<'mcx>,
) -> PgResult<Option<ExecSlotId>> {
    crate::cfi()?;
    if node.rs_checkqual && !lane_result_gate(node, estate)? {
        return Ok(None);
    }
    if node.rs_done {
        return Ok(None);
    }
    let Some(mut tree) = node.tree.take() else {
        return Err(err("decision-tree Result without batch state"));
    };
    let result = tree.next(node, estate);
    tree.publish(node.ps.plan.plan_node_id);
    node.tree = Some(tree);
    result
}

impl<'mcx> TreeState<'mcx> {
    fn next(
        &mut self,
        node: &mut ResultState<'mcx>,
        estate: &mut EStateData<'mcx>,
    ) -> PgResult<Option<ExecSlotId>> {
        self.note_rescan(node, estate)?;
        self.ensure_emit(node, estate)?;
        let Some(row) = self.emit.pop_front() else {
            node.rs_done = true;
            return Ok(None);
        };
        let slot = node.ps.ps_ResultTupleSlot.expect("Result slot");
        semantic::restore_tuple(estate, slot, &row)?;
        self.held = Some(row);
        self.rows_out += 1;
        Ok(Some(slot))
    }

    fn note_rescan(
        &mut self,
        node: &mut ResultState<'mcx>,
        estate: &mut EStateData<'mcx>,
    ) -> PgResult<()> {
        if node.outer_chg.is_empty() {
            return Ok(());
        }
        let chg = core::mem::replace(
            &mut node.outer_chg,
            ::types_nodes::bitmapset::Bitmapset::empty(),
        );
        let outer_plan = node.ps.plan.lefttree.expect("Result outer plan");
        if let Some(outer) = node.outer.as_deref_mut() {
            crate::execami::exec_re_scan_with_chg(outer, outer_plan, estate, &chg)?;
        }
        self.emit.clear();
        self.held = None;
        self.child_done = false;
        Ok(())
    }

    fn ensure_emit(
        &mut self,
        node: &mut ResultState<'mcx>,
        estate: &mut EStateData<'mcx>,
    ) -> PgResult<()> {
        while self.emit.is_empty() && !self.child_done {
            let rows = self.pull_batch(node, estate)?;
            if rows.is_empty() {
                self.child_done = true;
                break;
            }
            self.run_batch(node, estate, rows)?;
        }
        Ok(())
    }

    fn pull_batch(
        &mut self,
        node: &mut ResultState<'mcx>,
        estate: &mut EStateData<'mcx>,
    ) -> PgResult<Vec<Pending>> {
        if node.outer.is_none() {
            if self.child_done {
                return Ok(Vec::new());
            }
            self.child_done = true;
            let ecxt = node.ps.ps_ExprContext.expect("Result ExprContext");
            estate.reset_expr_context(ecxt);
            let calls = self.eval_sites(estate, ecxt)?;
            self.rows_in += 1;
            return Ok(vec![Pending {
                outer: None,
                outer_slot: None,
                calls,
            }]);
        }
        let mut rows = Vec::with_capacity(self.batch_rows);
        let ecxt = node.ps.ps_ExprContext.expect("Result ExprContext");
        while rows.len() < self.batch_rows {
            crate::cfi()?;
            estate.reset_expr_context(ecxt);
            let outer = node.outer.as_deref_mut().expect("decision tree Result has a child");
            let Some(outer_slot) = exec_proc_node(outer, estate)? else {
                break;
            };
            estate.ecxt_mut(ecxt).ecxt_outertuple = Some(outer_slot);
            let calls = self.eval_sites(estate, ecxt)?;
            let saved = semantic::snapshot_slot(estate, outer_slot)?;
            self.rows_in += 1;
            rows.push(Pending {
                outer: Some(saved),
                outer_slot: Some(outer_slot),
                calls,
            });
        }
        Ok(rows)
    }

    fn eval_sites(
        &mut self,
        estate: &mut EStateData<'mcx>,
        ecxt: executils::EcxtId,
    ) -> PgResult<Vec<Option<(String, String)>>> {
        let mut calls = Vec::with_capacity(self.sites.len());
        for site in &mut self.sites {
            let tree_name = semantic::eval_text(estate, ecxt, &mut site.tree_name)?;
            let input = semantic::eval_text(estate, ecxt, &mut site.input)?;
            calls.push(match (tree_name, input) {
                (Some(name), Some(text)) => Some((name, text)),
                _ => None,
            });
        }
        Ok(calls)
    }

    fn run_batch(
        &mut self,
        node: &mut ResultState<'mcx>,
        estate: &mut EStateData<'mcx>,
        rows: Vec<Pending>,
    ) -> PgResult<()> {
        self.batches += 1;
        let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for row in &rows {
            for call in &row.calls {
                let Some((name, input)) = call else { continue };
                grouped.entry(name.clone()).or_default().push(input.clone());
            }
        }
        let mut forget = Forget(Vec::new());
        for (name, inputs) in &grouped {
            let tree = rvbbit_resolve::tree(name).ok_or_else(|| {
                err(format!("decision tree \"{name}\" does not exist"))
            })?;
            let reply = rvbbit_dispatch::decide_texts(&tree.endpoint, &tree.nodes, inputs)?;
            if reply.values.len() != inputs.len() {
                return Err(err(format!(
                    "decision tree \"{name}\" returned {} values for {} inputs",
                    reply.values.len(),
                    inputs.len()
                )));
            }
            self.http_calls += reply.http_calls as u64;
            self.node_calls += reply.node_calls as u64;
            self.memo_hits += reply.memo_hits as u64;
            for (input, value) in inputs.iter().zip(reply.values.iter()) {
                rvbbit_dispatch::remember_decide(name, input, value);
                forget.0.push((name.clone(), input.clone()));
            }
        }
        let ecxt = node.ps.ps_ExprContext.expect("Result ExprContext");
        for row in &rows {
            estate.reset_expr_context(ecxt);
            if let (Some(saved), Some(slot)) = (&row.outer, row.outer_slot) {
                semantic::restore_tuple(estate, slot, saved)?;
                estate.ecxt_mut(ecxt).ecxt_outertuple = Some(slot);
            }
            lane_result_project(&mut node.ps, estate)?;
            let projected = semantic::snapshot_slot(
                estate,
                node.ps.ps_ResultTupleSlot.expect("result slot"),
            )?;
            self.emit.push_back(projected);
        }
        drop(forget);
        Ok(())
    }
}

fn err(message: impl Into<String>) -> Box<PgError> {
    Box::new(PgError::error(message))
}
