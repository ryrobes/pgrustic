//! Project batcher for native `CLASSIFY`, typed `EXTRACT`, and `EXTRACT MANY`.
//!
//! Compiled as `execmain::projbatch` via `#[path]`. Not a separate crate.
//!
//! A window of child rows is grouped by the scalar memo key, which is the
//! means backend URL, the model (`typesafe/jev-1.14` for classify, `mock`
//! for extract so chat uses `HUTCH_MODEL`), and the label set or extract
//! kind. Duplicate inputs in a group are sent once. One HTTP call carries
//! the unique inputs, in first-seen order. Answers are remembered and the
//! builtin returns them while the Result projects, including `ON ERROR`.
//! The memo is dropped after the window is snapshotted.
//!
//! A MEANS qual or a top-level `rvbbit.decide` keeps its own batcher. The
//! project calls in that query stay on the builtin. A call nested under
//! `round` or a cast is still batched: the Result remembers the score,
//! then the outer function reads it back.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

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
pub struct ProjectStats {
    pub batches: u64,
    pub http_calls: u64,
    pub memo_hits: u64,
    pub rows_in: u64,
    pub rows_out: u64,
}

/// Where a native 3-argument classify or extract sits in a Result target list.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProjectShape {
    pub batched: bool,
    pub present: bool,
}

static STATS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<i32, ProjectStats>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

pub fn clear_stats() {
    STATS.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

pub fn project_stats(plan_node_id: i32) -> Option<ProjectStats> {
    STATS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&plan_node_id)
        .copied()
}

pub fn project_shape<'mcx>(mcx: Mcx<'mcx>, tlist: &NodeList<'mcx>) -> PgResult<ProjectShape> {
    let mut shape = ProjectShape::default();
    for item in tlist.iter() {
        let Some(entry) = item.as_target_entry() else {
            continue;
        };
        walk(mcx, entry.expr, &mut shape)?;
    }
    Ok(shape)
}

fn walk<'mcx>(mcx: Mcx<'mcx>, node: Node<'mcx>, shape: &mut ProjectShape) -> PgResult<()> {
    match node.node_tag() {
        NodeTag::T_RelabelType => {
            let arg = node.as_relabel_type().expect("Relabel").arg;
            walk(mcx, arg, shape)?;
        }
        NodeTag::T_CoerceViaIO => {
            let arg = node.as_coerce_via_io().expect("CoerceViaIO").arg;
            walk(mcx, arg, shape)?;
        }
        NodeTag::T_FuncExpr => {
            let func = node.as_func_expr().expect("FuncExpr");
            if let Some(kind) = project_kind(mcx, func.funcid)? {
                if accepts_project(kind, func.args.len()) {
                    // Nested calls (`round(rerank(...))`, casts) batch too.
                    // The outer function runs after the memo is filled.
                    shape.present = true;
                    shape.batched = true;
                }
            }
            for arg in func.args.iter() {
                walk(mcx, arg, shape)?;
            }
        }
        NodeTag::T_OpExpr => {
            let op = node.as_op_expr().expect("OpExpr");
            for arg in op.args.iter() {
                walk(mcx, arg, shape)?;
            }
        }
        NodeTag::T_BoolExpr => {
            let b = node.as_bool_expr().expect("BoolExpr");
            for arg in b.args.iter() {
                walk(mcx, arg, shape)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Kind {
    Classify,
    Enum,
    Extract,
    ExtractMany,
    Rerank,
    User,
    Similarity,
}

fn accepts_project(kind: Kind, nargs: usize) -> bool {
    match kind {
        Kind::Rerank => nargs == 2 || nargs == 3,
        Kind::User => (1..=8).contains(&nargs),
        Kind::Similarity => nargs == 2,
        _ => nargs == 3,
    }
}

fn project_kind<'mcx>(mcx: Mcx<'mcx>, funcid: Oid) -> PgResult<Option<Kind>> {
    let Some(name) = get_func_name(mcx, funcid)? else {
        return Ok(None);
    };
    let named = match name.as_str() {
        "classify" => Some(Kind::Classify),
        "classify_enum" => Some(Kind::Enum),
        "extract_typed" => Some(Kind::Extract),
        "extract_many" => Some(Kind::ExtractMany),
        "rerank" | "rerank_for" => Some(Kind::Rerank),
        "semantic_similarity" => Some(Kind::Similarity),
        _ => None,
    };
    let nsp = get_func_namespace(funcid)?;
    let Some(nsname) = get_namespace_name(mcx, nsp)? else {
        return Ok(None);
    };
    if nsname.as_str() != "rvbbit" {
        return Ok(None);
    }
    if let Some(kind) = named {
        return Ok(Some(kind));
    }
    let ep = rvbbit_resolve::endpoint_for(funcid);
    if ep.user_fn && ep.system_prompt != rvbbit_resolve::PIPE_BODY {
        return Ok(Some(Kind::User));
    }
    Ok(None)
}

struct Site<'mcx> {
    kind: Kind,
    oid: u32,
    args: Vec<PgBox<'mcx, ExprState<'mcx>>>,
}

enum PendingCall {
    Classify { key: String, labels: Vec<String>, input: String },
    Extract { key: String, kind: String, input: String },
    ExtractMany { key: String, kind: String, input: String },
    Rerank { key: String, model: String, query: String, input: String },
    User { oid: u32, values: Vec<String> },
    Similarity { left: String, right: String },
}

struct Pending {
    outer: Option<SavedTuple>,
    outer_slot: Option<ExecSlotId>,
    calls: Vec<Option<PendingCall>>,
}

struct Forget(Vec<(String, String)>);

impl Drop for Forget {
    fn drop(&mut self) {
        if !self.0.is_empty() {
            rvbbit_dispatch::forget_scalar(&self.0);
        }
    }
}

pub struct ProjectState<'mcx> {
    sites: Vec<Site<'mcx>>,
    batch_rows: usize,
    emit: VecDeque<SavedTuple>,
    held: Option<SavedTuple>,
    child_done: bool,
    /// Parent LIMIT count plus offset. A Sort under this Result is bounded
    /// to the same number and errors if read once more, so pulls stop here.
    tuple_bound: Option<u64>,
    child_pulled: u64,
    batches: u64,
    http_calls: u64,
    memo_hits: u64,
    rows_in: u64,
    rows_out: u64,
}

impl ProjectState<'_> {
    fn snapshot(&self) -> ProjectStats {
        ProjectStats {
            batches: self.batches,
            http_calls: self.http_calls,
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
) -> PgResult<Option<Box<ProjectState<'mcx>>>> {
    let mcx = estate.es_query_cxt;
    let shape = project_shape(mcx, &plan.plan.targetlist)?;
    if !shape.batched {
        return Ok(None);
    }
    rvbbit_resolve::ensure()?;
    let mut funcs = Vec::new();
    for item in plan.plan.targetlist.iter() {
        let Some(entry) = item.as_target_entry() else {
            continue;
        };
        collect_projects(mcx, entry.expr, &mut funcs)?;
    }
    if funcs.is_empty() {
        return Ok(None);
    }
    let params = estate.param_bind();
    let mut sites = Vec::with_capacity(funcs.len());
    for func in funcs {
        let f = func.as_func_expr().expect("FuncExpr");
        let kind = project_kind(mcx, f.funcid)?.ok_or_else(|| err("project function lost its name"))?;
        if !accepts_project(kind, f.args.len()) {
            return Err(err("native CLASSIFY, EXTRACT, and RERANK have the wrong arity"));
        }
        let n = match kind {
            Kind::User => f.args.len(),
            Kind::ExtractMany => 3,
            Kind::Rerank if f.args.len() == 3 => 3,
            _ => 2,
        };
        let mut args = Vec::with_capacity(n);
        for i in 0..n {
            args.push(semantic::compile_expr(estate, mcx, params, f.args.nth(i))?);
        }
        sites.push(Site { kind, oid: f.funcid, args });
    }
    Ok(Some(Box::new(ProjectState {
        sites,
        batch_rows: rvbbit_resolve::batch_rows().max(1),
        emit: VecDeque::new(),
        held: None,
        child_done: false,
        tuple_bound: None,
        child_pulled: 0,
        batches: 0,
        http_calls: 0,
        memo_hits: 0,
        rows_in: 0,
        rows_out: 0,
    })))
}

/// A parent `LIMIT` counted these tuples (count + offset). Negative means
/// the sort underneath is not bounded. The pull stops at this count so it
/// never reads the extra tuple a bounded sort rejects.
pub fn note_tuple_bound(state: &mut ProjectState<'_>, tuples_needed: i64) {
    state.tuple_bound = if tuples_needed < 0 {
        None
    } else {
        Some(tuples_needed as u64)
    };
    state.child_pulled = 0;
}

/// Every native project call in the target, including one nested under
/// `round`, a cast, or another function. The outer expression still runs
/// in the projection; it reads the memo this batch fills.
fn collect_projects<'mcx>(
    mcx: Mcx<'mcx>,
    node: Node<'mcx>,
    out: &mut Vec<Node<'mcx>>,
) -> PgResult<()> {
    match node.node_tag() {
        NodeTag::T_RelabelType => {
            collect_projects(mcx, node.as_relabel_type().expect("Relabel").arg, out)
        }
        NodeTag::T_CoerceViaIO => {
            collect_projects(mcx, node.as_coerce_via_io().expect("CoerceViaIO").arg, out)
        }
        NodeTag::T_FuncExpr => {
            let func = node.as_func_expr().expect("FuncExpr");
            if project_kind(mcx, func.funcid)?.is_some_and(|kind| accepts_project(kind, func.args.len()))
            {
                out.push(node);
            }
            for arg in func.args.iter() {
                collect_projects(mcx, arg, out)?;
            }
            Ok(())
        }
        NodeTag::T_OpExpr => {
            for arg in node.as_op_expr().expect("OpExpr").args.iter() {
                collect_projects(mcx, arg, out)?;
            }
            Ok(())
        }
        NodeTag::T_BoolExpr => {
            for arg in node.as_bool_expr().expect("BoolExpr").args.iter() {
                collect_projects(mcx, arg, out)?;
            }
            Ok(())
        }
        _ => Ok(()),
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
    let Some(mut state) = node.proj.take() else {
        return Err(err("semantic project Result without batch state"));
    };
    let result = state.next(node, estate);
    state.publish(node.ps.plan.plan_node_id);
    node.proj = Some(state);
    result
}

impl<'mcx> ProjectState<'mcx> {
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
        self.child_pulled = 0;
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
        // Stop on the first NULL. An exhausted seq scan returns a row
        // again if it is pulled after that NULL, and a bounded sort then
        // either spins or errors with "retrieved too many tuples".
        // Also stop once a parent LIMIT bound is met: the Sort under this
        // Result keeps that many tuples and errors on the next read.
        while rows.len() < self.batch_rows && !self.child_done {
            if self.tuple_bound.is_some_and(|bound| self.child_pulled >= bound) {
                self.child_done = true;
                break;
            }
            crate::cfi()?;
            estate.reset_expr_context(ecxt);
            let outer = node.outer.as_deref_mut().expect("semantic project Result has a child");
            let Some(outer_slot) = exec_proc_node(outer, estate)? else {
                self.child_done = true;
                break;
            };
            estate.ecxt_mut(ecxt).ecxt_outertuple = Some(outer_slot);
            let calls = self.eval_sites(estate, ecxt)?;
            let saved = semantic::snapshot_slot(estate, outer_slot)?;
            self.child_pulled += 1;
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
    ) -> PgResult<Vec<Option<PendingCall>>> {
        let mut calls = Vec::with_capacity(self.sites.len());
        for site in &mut self.sites {
            let mut texts = Vec::with_capacity(site.args.len());
            for expr in &mut site.args {
                texts.push(semantic::eval_text(estate, ecxt, expr)?);
            }
            let first = texts.first().cloned().flatten();
            let second = texts.get(1).cloned().flatten();
            let extra = texts.get(2).cloned().flatten().unwrap_or_default();
            if matches!(site.kind, Kind::Similarity) {
                match (texts.first().cloned().flatten(), texts.get(1).cloned().flatten()) {
                    (Some(left), Some(right)) => {
                        calls.push(Some(PendingCall::Similarity { left, right }));
                    }
                    _ => calls.push(None),
                }
                continue;
            }
            if matches!(site.kind, Kind::User) {
                if texts.iter().any(|text| text.is_none()) {
                    calls.push(None);
                } else {
                    let values = texts.into_iter().flatten().collect();
                    calls.push(Some(PendingCall::User { oid: site.oid, values }));
                }
                continue;
            }
            let call = match (site.kind, first, second) {
                (Kind::Classify, Some(input), Some(raw)) => {
                    let labels: Vec<String> = raw
                        .split('\u{1f}')
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string())
                        .collect();
                    if labels.len() < 2 {
                        None
                    } else {
                        let key = rvbbit_dispatch::scalar_key_classify(&labels);
                        Some(PendingCall::Classify { key, labels, input })
                    }
                }
                (Kind::Enum, Some(input), Some(type_name)) => match rvbbit_resolve::enum_labels(&type_name)
                {
                    Ok(labels) if labels.len() >= 2 => {
                        let key = rvbbit_dispatch::scalar_key_classify(&labels);
                        Some(PendingCall::Classify { key, labels, input })
                    }
                    _ => None,
                },
                (Kind::Extract, Some(kind), Some(input)) => {
                    let key = rvbbit_dispatch::scalar_key_extract(&kind);
                    Some(PendingCall::Extract { key, kind, input })
                }
                // A clause tail is a different call (takes, phrasings) and
                // stays on the per-row builtin. An empty tail is one batch
                // POST of JSON arrays, remembered for the builtin.
                (Kind::ExtractMany, Some(kind), Some(input)) if extra.trim().is_empty() => {
                    let key = rvbbit_dispatch::scalar_key_extract_many(&kind);
                    Some(PendingCall::ExtractMany { key, kind, input })
                }
                (Kind::Rerank, Some(document), Some(query)) => {
                    let model = extra.clone();
                    let key = rvbbit_dispatch::scalar_key_rerank(&model, &query);
                    Some(PendingCall::Rerank { key, model, query, input: document })
                }
                _ => None,
            };
            calls.push(call);
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
        let mut grouped: BTreeMap<String, Group> = BTreeMap::new();
        for row in &rows {
            for call in &row.calls {
                let Some(call) = call else { continue };
                match call {
                    PendingCall::Classify { key, labels, input } => {
                        let group = grouped.entry(key.clone()).or_insert_with(|| Group {
                            labels: Some(labels.clone()),
                            kind: None,
                            lists: false,
                            rerank: None,
                            inputs: Vec::new(),
                            user_rows: Vec::new(),
                        });
                        group.inputs.push(input.clone());
                    }
                    PendingCall::Extract { key, kind, input } => {
                        let group = grouped.entry(key.clone()).or_insert_with(|| Group {
                            labels: None,
                            kind: Some(kind.clone()),
                            lists: false,
                            rerank: None,
                            inputs: Vec::new(),
                            user_rows: Vec::new(),
                        });
                        group.inputs.push(input.clone());
                    }
                    PendingCall::ExtractMany { key, kind, input } => {
                        let group = grouped.entry(key.clone()).or_insert_with(|| Group {
                            labels: None,
                            kind: Some(kind.clone()),
                            lists: true,
                            rerank: None,
                            inputs: Vec::new(),
                            user_rows: Vec::new(),
                        });
                        group.inputs.push(input.clone());
                    }
                    PendingCall::Rerank { key, model, query, input } => {
                        let group = grouped.entry(key.clone()).or_insert_with(|| Group {
                            labels: None,
                            kind: None,
                            lists: false,
                            rerank: Some((model.clone(), query.clone())),
                            inputs: Vec::new(),
                            user_rows: Vec::new(),
                        });
                        group.inputs.push(input.clone());
                    }
                    PendingCall::Similarity { left, right } => {
                        let key = rvbbit_dispatch::scalar_key_similarity();
                        let group = grouped.entry(key).or_insert_with(|| Group {
                            labels: None,
                            kind: None,
                            lists: false,
                            rerank: None,
                            inputs: Vec::new(),
                            user_rows: Vec::new(),
                        });
                        group.user_rows.push(vec![left.clone(), right.clone()]);
                    }
                    PendingCall::User { oid, values } => {
                        let key = rvbbit_dispatch::user_memo_key(*oid);
                        let group = grouped.entry(key).or_insert_with(|| Group {
                            labels: None,
                            kind: None,
                            lists: false,
                            rerank: None,
                            inputs: Vec::new(),
                            user_rows: Vec::new(),
                        });
                        group.user_rows.push(values.clone());
                    }
                }
            }
        }
        let mut forget = Forget(Vec::new());
        for (key, group) in &grouped {
            if !group.user_rows.is_empty() {
                let oid = key
                    .rsplit('\u{1e}')
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                let mut seen = BTreeSet::new();
                let mut unique = Vec::new();
                for row in &group.user_rows {
                    if seen.insert(row.join("\u{1e}")) {
                        unique.push(row.clone());
                    }
                }
                self.memo_hits += (group.user_rows.len() - unique.len()) as u64;
                if key.starts_with("sim\u{1e}") {
                    let pairs: Vec<(String, String)> = unique
                        .iter()
                        .map(|row| (row.first().cloned().unwrap_or_default(), row.get(1).cloned().unwrap_or_default()))
                        .collect();
                    let reply = rvbbit_dispatch::similarity_many(&pairs)?;
                    self.http_calls += reply.http_calls as u64;
                    if reply.values.len() != unique.len() {
                        return Err(err(format!(
                            "semantic batch returned {} values for {} inputs",
                            reply.values.len(),
                            unique.len()
                        )));
                    }
                    for (row, value) in unique.iter().zip(reply.values.into_iter()) {
                        let input = row.join("\u{1e}");
                        let hit = match value {
                            Ok(text) => rvbbit_dispatch::ScalarHit::Value(text),
                            Err(message) => rvbbit_dispatch::ScalarHit::Failed(message),
                        };
                        rvbbit_dispatch::remember_scalar(key, &input, hit);
                        forget.0.push((key.clone(), input));
                    }
                    continue;
                }
                let reply = rvbbit_dispatch::user_eval_many(oid, &unique)?;
                self.http_calls += reply.http_calls as u64;
                if reply.values.len() != unique.len() {
                    return Err(err(format!(
                        "semantic batch returned {} values for {} inputs",
                        reply.values.len(),
                        unique.len()
                    )));
                }
                for (row, value) in unique.iter().zip(reply.values.into_iter()) {
                    let input = row.join("\u{1e}");
                    rvbbit_dispatch::remember_scalar(key, &input, rvbbit_dispatch::ScalarHit::Value(value));
                    forget.0.push((key.clone(), input));
                }
                continue;
            }
            let mut seen = BTreeSet::new();
            let mut unique = Vec::new();
            for input in &group.inputs {
                if seen.insert(input.clone()) {
                    unique.push(input.clone());
                }
            }
            self.memo_hits += (group.inputs.len() - unique.len()) as u64;
            let reply = if group.lists {
                let kind = group.kind.as_deref().unwrap_or("");
                rvbbit_dispatch::extract_lists(kind, &unique)?
            } else if let Some((model, query)) = &group.rerank {
                rvbbit_dispatch::rerank_many(model, query, &unique)?
            } else if let Some(labels) = &group.labels {
                rvbbit_dispatch::classify_many(labels, &unique)?
            } else {
                let kind = group.kind.as_deref().unwrap_or("");
                rvbbit_dispatch::extract_many(kind, &unique)?
            };
            self.http_calls += reply.http_calls as u64;
            if reply.values.len() != unique.len() {
                return Err(err(format!(
                    "semantic batch returned {} values for {} inputs",
                    reply.values.len(),
                    unique.len()
                )));
            }
            for (input, value) in unique.iter().zip(reply.values.into_iter()) {
                let hit = match value {
                    Ok(raw) => match &group.kind {
                        Some(_) if group.lists => rvbbit_dispatch::ScalarHit::Value(raw),
                        Some(_) if raw == rvbbit_dispatch::EXTRACT_ABSENT => {
                            rvbbit_dispatch::ScalarHit::Missing
                        }
                        Some(kind) => match rvbbit_dispatch::coerce_extracted(kind, &raw) {
                            Ok(text) => rvbbit_dispatch::ScalarHit::Value(text),
                            Err(message) => rvbbit_dispatch::ScalarHit::Failed(message),
                        },
                        None => rvbbit_dispatch::ScalarHit::Value(raw),
                    },
                    Err(message) => rvbbit_dispatch::ScalarHit::Failed(message),
                };
                rvbbit_dispatch::remember_scalar(key, input, hit);
                forget.0.push((key.clone(), input.clone()));
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

struct Group {
    labels: Option<Vec<String>>,
    kind: Option<String>,
    lists: bool,
    /// `(model, query)` for a rerank batch. Documents are `inputs`.
    rerank: Option<(String, String)>,
    inputs: Vec<String>,
    user_rows: Vec<Vec<String>>,
}

fn err(message: impl Into<String>) -> Box<PgError> {
    Box::new(PgError::error(message))
}
