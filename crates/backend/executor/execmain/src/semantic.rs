//! Semantic batch execution for a Result node whose qual is `rvbbit_means`.
//!
//! Rows are pulled from the child (which already applied the cheap quals),
//! projected, and grouped into batches (32 by default, from the backend
//! row). Up to that backend's concurrency (4 by default) are in flight.
//! Identical `(input, criterion)` pairs in the same query are memoized
//! and not sent again.

use std::collections::{HashMap, HashSet, VecDeque};

use execexpr::{exec_eval_expr, exec_init_expr_subplans, ExprState};
use executils::{EStateData, ExecSlotId};
use mcx::{Mcx, PgBox};
use rvbbit_dispatch::{self, BatchReply};
use types_error::{PgError, PgResult};
use types_nodes::plannodes::Result as ResultPlan;
use types_nodes::Node;
use types_nodes::NodeTag;
use crate::noderesult::{lane_result_gate, lane_result_project, ResultState};
use crate::procnode::{exec_proc_node, with_eval_slots};

#[derive(Clone, Copy, Debug, Default)]
pub struct SemanticStats {
    pub batches: u64,
    pub http_calls: u64,
    pub memo_hits: u64,
    pub rows_in: u64,
    pub rows_out: u64,
}

static STATS: std::sync::LazyLock<std::sync::Mutex<HashMap<i32, SemanticStats>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

pub fn clear_stats() {
    STATS.lock().unwrap_or_else(|e| e.into_inner()).clear();
    crate::treebatch::clear_stats();
}

pub fn semantic_stats(plan_node_id: i32) -> Option<SemanticStats> {
    STATS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&plan_node_id)
        .copied()
}

struct MeansArgs<'mcx> {
    input: PgBox<'mcx, ExprState<'mcx>>,
    criterion: PgBox<'mcx, ExprState<'mcx>>,
}

pub(crate) enum Field {
    Null,
    ByVal(usize),
    /// Plain or still-toasted varlena image (header included).
    Varlena(Vec<u8>),
    /// Fixed-length by-reference payload (`attlen > 0`).
    Fixed(Vec<u8>),
}

pub(crate) struct SavedTuple {
    fields: Vec<Field>,
}

struct RowWork {
    saved: SavedTuple,
    /// `None` means a NULL argument: the strict predicate is false, no HTTP.
    pairs: Vec<Option<(String, String)>>,
}

struct Inflight {
    rows: Vec<RowWork>,
    ticket: Option<rvbbit_dispatch::Ticket>,
    sent: Vec<(String, String)>,
}

pub struct SemanticState<'mcx> {
    args: Vec<MeansArgs<'mcx>>,
    func_oid: u32,
    batch_rows: usize,
    concurrency: usize,
    memo: HashMap<(String, String), bool>,
    emit: VecDeque<SavedTuple>,
    inflight: VecDeque<Inflight>,
    held: Option<SavedTuple>,
    child_done: bool,
    batches: u64,
    http_calls: u64,
    memo_hits: u64,
    rows_in: u64,
    rows_out: u64,
}

impl<'mcx> SemanticState<'mcx> {
    fn snapshot(&self) -> SemanticStats {
        SemanticStats {
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
) -> PgResult<Option<Box<SemanticState<'mcx>>>> {
    if !rvbbit_plan::qual_contains_means(&plan.plan.qual) {
        return Ok(None);
    }
    // A mixed clause (`means OR id = 1`, a comparison nested inside the
    // predicate) stays on the ordinary Result qual, which calls the
    // per-row builtin. Batching only implements an implicit AND of MEANS.
    if !plan.plan.qual.iter().all(clause_is_pure_means) {
        return Ok(None);
    }
    let mut funcs = Vec::new();
    for clause in plan.plan.qual.iter() {
        collect_means(clause, &mut funcs);
    }
    if funcs.is_empty() {
        return Ok(None);
    }
    let func_oid = funcs[0].as_func_expr().expect("FuncExpr").funcid;
    let mcx = estate.es_query_cxt;
    let params = estate.param_bind();
    let mut args = Vec::with_capacity(funcs.len());
    for func in funcs {
        let f = func.as_func_expr().expect("FuncExpr");
        if f.args.len() != 2 {
            return Err(err("rvbbit_means expects 2 arguments"));
        }
        let input = compile_expr(estate, mcx, params, f.args.nth(0))?;
        let criterion = compile_expr(estate, mcx, params, f.args.nth(1))?;
        args.push(MeansArgs { input, criterion });
    }
    Ok(Some(Box::new(SemanticState {
        args,
        func_oid,
        batch_rows: rvbbit_resolve::batch_rows().max(1),
        concurrency: rvbbit_resolve::max_concurrent().max(1),
        memo: HashMap::new(),
        emit: VecDeque::new(),
        inflight: VecDeque::new(),
        held: None,
        child_done: false,
        batches: 0,
        http_calls: 0,
        memo_hits: 0,
        rows_in: 0,
        rows_out: 0,
    })))
}

pub(crate) fn compile_expr<'mcx>(
    estate: &mut EStateData<'mcx>,
    mcx: Mcx<'mcx>,
    params: execexpr::ParamBind<'mcx>,
    node: Node<'mcx>,
) -> PgResult<PgBox<'mcx, ExprState<'mcx>>> {
    ::executils::with_subplan_compile_env(estate, |env| {
        exec_init_expr_subplans(mcx, Some(node), params, env)
    })?
    .ok_or_else(|| err("MEANS argument did not compile"))
}

fn clause_is_pure_means(node: Node<'_>) -> bool {
    match node.node_tag() {
        NodeTag::T_FuncExpr => {
            rvbbit_resolve::is_semantic_func(node.as_func_expr().expect("FuncExpr").funcid)
        }
        NodeTag::T_RelabelType => {
            clause_is_pure_means(node.as_relabel_type().expect("Relabel").arg)
        }
        NodeTag::T_CoerceViaIO => {
            clause_is_pure_means(node.as_coerce_via_io().expect("CoerceViaIO").arg)
        }
        NodeTag::T_BoolExpr => {
            let b = node.as_bool_expr().expect("BoolExpr");
            b.boolop == types_nodes::primnodes::BoolExprType::AND_EXPR
                && b.args.iter().all(clause_is_pure_means)
        }
        _ => false,
    }
}

fn collect_means<'mcx>(node: Node<'mcx>, out: &mut Vec<Node<'mcx>>) {
    match node.node_tag() {
        NodeTag::T_FuncExpr => {
            let f = node.as_func_expr().expect("FuncExpr");
            if rvbbit_resolve::is_semantic_func(f.funcid) {
                out.push(node);
            } else {
                for arg in f.args.iter() {
                    collect_means(arg, out);
                }
            }
        }
        NodeTag::T_BoolExpr => {
            for arg in node.as_bool_expr().expect("BoolExpr").args.iter() {
                collect_means(arg, out);
            }
        }
        NodeTag::T_List => {
            if let Some(list) = node.as_list() {
                for arg in list.iter() {
                    collect_means(arg, out);
                }
            }
        }
        _ => {}
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
    let Some(mut sem) = node.rvbbit.take() else {
        return Err(err("semantic Result without batch state"));
    };
    let result = sem.next(node, estate);
    sem.publish(node.ps.plan.plan_node_id);
    node.rvbbit = Some(sem);
    result
}

impl<'mcx> SemanticState<'mcx> {
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
        restore_tuple(estate, slot, &row)?;
        // Moving the Vec does not move its heap buffer, so the datums just
        // stored stay valid until `held` is replaced on the next pull.
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
        self.inflight.clear();
        self.memo.clear();
        self.held = None;
        self.child_done = false;
        Ok(())
    }

    fn ensure_emit(
        &mut self,
        node: &mut ResultState<'mcx>,
        estate: &mut EStateData<'mcx>,
    ) -> PgResult<()> {
        while self.emit.is_empty() {
            self.fill_window(node, estate)?;
            if self.inflight.is_empty() {
                break;
            }
            self.reap_front()?;
        }
        Ok(())
    }

    fn fill_window(
        &mut self,
        node: &mut ResultState<'mcx>,
        estate: &mut EStateData<'mcx>,
    ) -> PgResult<()> {
        while self.inflight.len() < self.concurrency && !self.child_done {
            let rows = self.pull_batch(node, estate)?;
            if rows.is_empty() {
                self.child_done = true;
                break;
            }
            self.submit_batch(rows)?;
        }
        Ok(())
    }

    fn pull_batch(
        &mut self,
        node: &mut ResultState<'mcx>,
        estate: &mut EStateData<'mcx>,
    ) -> PgResult<Vec<RowWork>> {
        let mut rows = Vec::with_capacity(self.batch_rows);
        let ecxt = node.ps.ps_ExprContext.expect("Result ExprContext");
        while rows.len() < self.batch_rows && !self.child_done {
            crate::cfi()?;
            estate.reset_expr_context(ecxt);
            let outer = node.outer.as_deref_mut().expect("semantic Result has a child");
            let Some(outer_slot) = exec_proc_node(outer, estate)? else {
                self.child_done = true;
                break;
            };
            estate.ecxt_mut(ecxt).ecxt_outertuple = Some(outer_slot);
            let mut pairs = Vec::with_capacity(self.args.len());
            for arg in &mut self.args {
                let input = eval_text(estate, ecxt, &mut arg.input)?;
                let criterion = eval_text(estate, ecxt, &mut arg.criterion)?;
                pairs.push(match (input, criterion) {
                    (Some(i), Some(c)) => Some((i, c)),
                    _ => None,
                });
            }
            lane_result_project(&mut node.ps, estate)?;
            let saved = snapshot_slot(estate, node.ps.ps_ResultTupleSlot.expect("result slot"))?;
            self.rows_in += 1;
            rows.push(RowWork { saved, pairs });
        }
        Ok(rows)
    }

    fn submit_batch(&mut self, rows: Vec<RowWork>) -> PgResult<()> {
        self.batches += 1;
        let mut sent = Vec::new();
        let mut seen = HashSet::new();
        for row in &rows {
            for pair in &row.pairs {
                let Some(pair) = pair else { continue };
                if self.memo.contains_key(pair) || !seen.insert(pair.clone()) {
                    self.memo_hits += 1;
                    continue;
                }
                sent.push(pair.clone());
            }
        }
        if sent.is_empty() {
            self.enqueue_ready(rows, &BatchReply { matched: Vec::new(), http_calls: 0 });
            return Ok(());
        }
        let ticket = rvbbit_dispatch::submit(self.func_oid, &sent)?;
        self.inflight.push_back(Inflight {
            rows,
            ticket: Some(ticket),
            sent,
        });
        Ok(())
    }

    fn reap_front(&mut self) -> PgResult<()> {
        let Some(mut batch) = self.inflight.pop_front() else {
            return Ok(());
        };
        let reply = match batch.ticket.take() {
            Some(ticket) => rvbbit_dispatch::wait(ticket)?,
            None => BatchReply {
                matched: Vec::new(),
                http_calls: 0,
            },
        };
        if reply.matched.len() != batch.sent.len() {
            return Err(err(format!(
                "hutch returned {} results for {} items",
                reply.matched.len(),
                batch.sent.len()
            )));
        }
        self.http_calls += reply.http_calls as u64;
        for (pair, bit) in batch.sent.iter().zip(reply.matched.iter()) {
            self.memo.insert(pair.clone(), *bit);
        }
        self.enqueue_ready(batch.rows, &reply);
        Ok(())
    }

    fn enqueue_ready(&mut self, rows: Vec<RowWork>, _reply: &BatchReply) {
        for row in rows {
            if row_passes(&row, &self.memo) {
                self.emit.push_back(row.saved);
            }
        }
    }
}

fn row_passes(row: &RowWork, memo: &HashMap<(String, String), bool>) -> bool {
    if row.pairs.is_empty() {
        return false;
    }
    row.pairs.iter().all(|pair| {
        pair.as_ref()
            .and_then(|p| memo.get(p))
            .copied()
            .unwrap_or(false)
    })
}

pub(crate) fn eval_text<'mcx>(
    estate: &mut EStateData<'mcx>,
    ecxt: executils::EcxtId,
    expr: &mut ExprState<'mcx>,
) -> PgResult<Option<String>> {
    let nd = with_eval_slots(estate, ecxt, None, |slots, _, _| exec_eval_expr(expr, slots))?;
    if nd.isnull {
        return Ok(None);
    }
    Ok(Some(datum_to_text(estate.es_query_cxt, nd.value)?))
}

fn datum_to_text<'mcx>(mcx: Mcx<'mcx>, datum: datum::Datum) -> PgResult<String> {
    let ptr = datum.as_usize() as *const u8;
    if ptr.is_null() {
        return Ok(String::new());
    }
    // SAFETY: a text datum points at a live varlena for this evaluation.
    let len = unsafe { types_tuple::varatt::varsize_any(ptr) };
    if len > isize::MAX as usize {
        return Err(err("text datum length overflow"));
    }
    let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
    let plain = detoast::detoast_attr(mcx, bytes)?;
    if plain.len() < types_tuple::varatt::VARHDRSZ {
        return Err(err("detoasted text is shorter than its header"));
    }
    Ok(String::from_utf8_lossy(&plain[types_tuple::varatt::VARHDRSZ..]).into_owned())
}

pub(crate) fn snapshot_slot<'mcx>(estate: &mut EStateData<'mcx>, slot: ExecSlotId) -> PgResult<SavedTuple> {
    let mut fields = Vec::new();
    {
        let data = estate.slot(slot);
        let base = data.base();
        let desc = base
            .tts_tupleDescriptor
            .as_ref()
            .expect("result slot descriptor");
        let natts = desc.natts as usize;
        for i in 0..natts {
            if base.tts_isnull[i] {
                fields.push(Field::Null);
                continue;
            }
            let att = desc.attr(i);
            let word = base.tts_values[i].as_usize();
            if att.attbyval {
                fields.push(Field::ByVal(word));
            } else if att.attlen == -1 {
                let ptr = word as *const u8;
                let len = unsafe { types_tuple::varatt::varsize_any(ptr) };
                let bytes = unsafe { core::slice::from_raw_parts(ptr, len) }.to_vec();
                fields.push(Field::Varlena(bytes));
            } else if att.attlen > 0 {
                let ptr = word as *const u8;
                let bytes = unsafe { core::slice::from_raw_parts(ptr, att.attlen as usize) }.to_vec();
                fields.push(Field::Fixed(bytes));
            } else {
                return Err(err(format!("unsupported result attribute length {}", att.attlen)));
            }
        }
    }
    let mcx = estate.es_query_cxt;
    for field in &mut fields {
        if let Field::Varlena(bytes) = field {
            let plain = detoast::detoast_attr(mcx, bytes)?;
            *bytes = plain.to_vec();
        }
    }
    Ok(SavedTuple { fields })
}

pub(crate) fn restore_tuple<'mcx>(
    estate: &mut EStateData<'mcx>,
    slot_id: ExecSlotId,
    row: &SavedTuple,
) -> PgResult<()> {
    let mcx = estate.es_query_cxt;
    let slot = estate.slot_mut(slot_id);
    ::exectuples::exec_clear_tuple(slot, mcx);
    let base = slot.base_mut();
    if row.fields.len() != base.tts_values.len() {
        return Err(err(format!(
            "saved tuple has {} attributes, slot has {}",
            row.fields.len(),
            base.tts_values.len()
        )));
    }
    for (i, field) in row.fields.iter().enumerate() {
        match field {
            Field::Null => {
                base.tts_isnull[i] = true;
                base.tts_values[i] = datum::Datum::null();
            }
            Field::ByVal(word) => {
                base.tts_isnull[i] = false;
                base.tts_values[i] = datum::Datum::from_usize(*word);
            }
            Field::Varlena(bytes) | Field::Fixed(bytes) => {
                base.tts_isnull[i] = false;
                base.tts_values[i] = datum::Datum::from_usize(bytes.as_ptr() as usize);
            }
        }
    }
    base.tts_nvalid = row.fields.len() as i16;
    base.mark_not_empty();
    Ok(())
}

fn err(message: impl Into<String>) -> Box<PgError> {
    Box::new(PgError::error(message))
}
