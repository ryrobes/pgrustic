//! Batch scalar MCP tool calls in a Result target list.
//!
//! Compiled as `execmain::mcpbatch` via `#[path]`. A window of child rows
//! is sent through `hutch-mcp` once per server. The SQL function then
//! reads `rvbbit.mcp_invoke_text`, which finds the memo and does not call
//! again. Set-returning tools, jsonb arguments, and a Result that the
//! semantic project batcher already owns stay per-row. Stdio and HTTP
//! both overlap on one session. A qual-free Seq Scan or Values Scan
//! with the same scalar tools is batched here too.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ptr::NonNull;

use execexpr::{exec_eval_expr, exec_project_prearmed, EvalSlots, ExprState};
use executils::{EStateData, ExecSlotId};
use lsyscache::get_func_prosrc;
use mcx::{Mcx, PgBox};
use types_core::Oid;
use types_error::{PgError, PgResult};
use types_nodes::plannodes::Result as ResultPlan;
use types_nodes::Node;
use types_nodes::NodeTag;

use crate::noderesult::{lane_result_gate, lane_result_project, ResultState};
use crate::procnode::{exec_proc_node, with_eval_slots};
use crate::semantic::{self, SavedTuple};

pub struct McpState<'mcx> {
    sites: Vec<Site<'mcx>>,
    batch_rows: usize,
    emit: VecDeque<SavedTuple>,
    held: Option<SavedTuple>,
    child_done: bool,
    tuple_bound: Option<u64>,
    child_pulled: u64,
}

struct Site<'mcx> {
    nsp: String,
    name: String,
    tool: String,
    server_key: String,
    arg_types: Vec<String>,
    arg_keys: Vec<String>,
    args: Vec<PgBox<'mcx, ExprState<'mcx>>>,
    price: Option<f64>,
    model: String,
}

struct Pending {
    outer: Option<SavedTuple>,
    outer_slot: Option<ExecSlotId>,
    calls: Vec<PendingCall>,
}

struct PendingCall {
    key: String,
    input: String,
    server_key: String,
    tool: String,
    arguments: String,
}

pub fn note_tuple_bound(state: &mut McpState<'_>, tuples_needed: i64) {
    state.tuple_bound = if tuples_needed < 0 { None } else { Some(tuples_needed as u64) };
    state.child_pulled = 0;
}

pub fn compile<'mcx>(
    estate: &mut EStateData<'mcx>,
    plan: &'mcx ResultPlan<'mcx>,
) -> PgResult<Option<Box<McpState<'mcx>>>> {
    let mcx = estate.es_query_cxt;
    let mut funcs = Vec::new();
    let mut eligible = true;
    for item in plan.plan.targetlist.iter() {
        let Some(entry) = item.as_target_entry() else { continue };
        walk(mcx, entry.expr, &mut funcs, &mut eligible)?;
    }
    let Some(sites) = sites_from(estate, funcs, eligible)? else {
        return Ok(None);
    };
    Ok(Some(Box::new(McpState {
        sites,
        batch_rows: rvbbit_resolve::batch_rows().max(1),
        emit: VecDeque::new(),
        held: None,
        child_done: false,
        tuple_bound: None,
        child_pulled: 0,
    })))
}

fn walk<'mcx>(mcx: Mcx<'mcx>, node: Node<'mcx>, out: &mut Vec<Node<'mcx>>, eligible: &mut bool) -> PgResult<()> {
    match node.node_tag() {
        NodeTag::T_RelabelType => walk(mcx, node.as_relabel_type().expect("Relabel").arg, out, eligible),
        NodeTag::T_CoerceViaIO => walk(mcx, node.as_coerce_via_io().expect("CoerceViaIO").arg, out, eligible),
        NodeTag::T_FuncExpr => {
            let func = node.as_func_expr().expect("FuncExpr");
            if is_tool_body(mcx, func.funcid)? {
                if func.funcretset {
                    *eligible = false;
                }
                out.push(node);
            }
            for arg in func.args.iter() {
                walk(mcx, arg, out, eligible)?;
            }
            Ok(())
        }
        NodeTag::T_OpExpr => {
            for arg in node.as_op_expr().expect("OpExpr").args.iter() {
                walk(mcx, arg, out, eligible)?;
            }
            Ok(())
        }
        NodeTag::T_BoolExpr => {
            for arg in node.as_bool_expr().expect("BoolExpr").args.iter() {
                walk(mcx, arg, out, eligible)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn is_tool_body<'mcx>(mcx: Mcx<'mcx>, oid: Oid) -> PgResult<bool> {
    let Some(src) = get_func_prosrc(mcx, oid)? else {
        return Ok(false);
    };
    Ok(src.as_str().contains("rvbbit.mcp_invoke_text"))
}

fn sites_from<'mcx>(
    estate: &mut EStateData<'mcx>,
    funcs: Vec<Node<'mcx>>,
    eligible: bool,
) -> PgResult<Option<Vec<Site<'mcx>>>> {
    if funcs.is_empty() || !eligible {
        return Ok(None);
    }
    rvbbit_resolve::ensure()?;
    let mcx = estate.es_query_cxt;
    let params = estate.param_bind();
    let mut sites = Vec::new();
    for func in funcs {
        let f = func.as_func_expr().expect("FuncExpr");
        let Some(tool) = rvbbit_resolve::mcp_tool_by_oid(f.funcid) else {
            return Ok(None);
        };
        if tool.returns_table || f.funcretset || f.args.len() != tool.arg_types.len() {
            return Ok(None);
        }
        if tool.arg_types.iter().any(|t| !scalar_type(t)) {
            return Ok(None);
        }
        let mut args = Vec::with_capacity(f.args.len());
        for arg in f.args.iter() {
            let arg = bare_named(arg);
            args.push(semantic::compile_expr(estate, mcx, params, arg)?);
        }
        let server_key = format!(
            "{}|{}|{}|{}|{}|{}",
            tool.server,
            tool.read_only,
            tool.cache_ttl_ms,
            tool.timeout_ms,
            tool.max_cost.unwrap_or(-1.0),
            tool.retries
        );
        let model = format!("mcp:{}/{}", tool.server, tool.tool);
        sites.push(Site {
            nsp: tool.nsp,
            name: tool.name,
            tool: tool.tool,
            server_key,
            arg_types: tool.arg_types,
            arg_keys: tool.arg_keys,
            args,
            price: tool.cost_usd,
            model,
        });
    }
    if sites.is_empty() {
        return Ok(None);
    }
    Ok(Some(sites))
}

fn priced_calls(
    sites: &[Site<'_>],
    server_key: &str,
    calls: &[(String, serde_json::Value)],
) -> PgResult<Vec<(String, serde_json::Value, f64)>> {
    let mut out = Vec::with_capacity(calls.len());
    for (tool, args) in calls {
        let site = sites.iter().find(|site| site.server_key == server_key && site.tool == *tool);
        if let Some(site) = site {
            rvbbit_cost::reject_unpriced_mcp(&site.model, site.price.is_some()).map_err(err)?;
            out.push((tool.clone(), args.clone(), site.price.unwrap_or(0.0)));
        } else {
            out.push((tool.clone(), args.clone(), 0.0));
        }
    }
    Ok(out)
}

fn bare_named<'mcx>(node: Node<'mcx>) -> Node<'mcx> {
    if let Some(named) = node.as_named_arg_expr() {
        named.arg.unwrap_or(node)
    } else {
        node
    }
}

fn scalar_type(sql: &str) -> bool {
    matches!(
        sql.trim().to_ascii_lowercase().as_str(),
        "boolean" | "bool" | "smallint" | "int2" | "integer" | "int" | "int4" | "bigint" | "int8"
            | "real" | "float4" | "double precision" | "float8" | "text" | "varchar"
            | "character varying" | "char" | "bpchar"
    )
}

pub fn exec<'mcx>(node: &mut ResultState<'mcx>, estate: &mut EStateData<'mcx>) -> PgResult<Option<ExecSlotId>> {
    crate::cfi()?;
    if node.rs_checkqual && !lane_result_gate(node, estate)? {
        return Ok(None);
    }
    if node.rs_done {
        return Ok(None);
    }
    let Some(mut state) = node.mcp.take() else {
        return Err(err("mcp Result without batch state"));
    };
    let result = state.next(node, estate);
    node.mcp = Some(state);
    result
}

impl<'mcx> McpState<'mcx> {
    fn next(&mut self, node: &mut ResultState<'mcx>, estate: &mut EStateData<'mcx>) -> PgResult<Option<ExecSlotId>> {
        self.note_rescan(node, estate)?;
        self.ensure_emit(node, estate)?;
        let Some(row) = self.emit.pop_front() else {
            node.rs_done = true;
            return Ok(None);
        };
        let slot = node.ps.ps_ResultTupleSlot.expect("Result slot");
        semantic::restore_tuple(estate, slot, &row)?;
        self.held = Some(row);
        Ok(Some(slot))
    }

    fn note_rescan(&mut self, node: &mut ResultState<'mcx>, estate: &mut EStateData<'mcx>) -> PgResult<()> {
        if node.outer_chg.is_empty() {
            return Ok(());
        }
        let chg = core::mem::replace(&mut node.outer_chg, ::types_nodes::bitmapset::Bitmapset::empty());
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

    fn ensure_emit(&mut self, node: &mut ResultState<'mcx>, estate: &mut EStateData<'mcx>) -> PgResult<()> {
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

    fn pull_batch(&mut self, node: &mut ResultState<'mcx>, estate: &mut EStateData<'mcx>) -> PgResult<Vec<Pending>> {
        let ecxt = node.ps.ps_ExprContext.expect("Result ExprContext");
        if node.outer.is_none() {
            if self.child_done {
                return Ok(Vec::new());
            }
            self.child_done = true;
            estate.reset_expr_context(ecxt);
            let calls = self.eval_sites(estate, ecxt)?;
            return Ok(vec![Pending { outer: None, outer_slot: None, calls }]);
        }
        let mut rows = Vec::new();
        while rows.len() < self.batch_rows && !self.child_done {
            if self.tuple_bound.is_some_and(|bound| self.child_pulled >= bound) {
                self.child_done = true;
                break;
            }
            crate::cfi()?;
            estate.reset_expr_context(ecxt);
            let outer = node.outer.as_deref_mut().expect("mcp Result has a child");
            let Some(outer_slot) = exec_proc_node(outer, estate)? else {
                self.child_done = true;
                break;
            };
            estate.ecxt_mut(ecxt).ecxt_outertuple = Some(outer_slot);
            let calls = self.eval_sites(estate, ecxt)?;
            let saved = semantic::snapshot_slot(estate, outer_slot)?;
            self.child_pulled += 1;
            rows.push(Pending { outer: Some(saved), outer_slot: Some(outer_slot), calls });
        }
        Ok(rows)
    }

    fn eval_sites(&mut self, estate: &mut EStateData<'mcx>, ecxt: executils::EcxtId) -> PgResult<Vec<PendingCall>> {
        let mut calls = Vec::new();
        for site in &mut self.sites {
            let mut items = Vec::with_capacity(site.args.len());
            for (expr, typ) in site.args.iter_mut().zip(site.arg_types.iter()) {
                items.push(eval_json(estate, ecxt, expr, typ)?);
            }
            let positional = serde_json::Value::Array(items);
            let input = positional.to_string();
            let arguments = rvbbit_dispatch::mcp_arguments(&site.arg_keys, &input).unwrap_or(positional).to_string();
            calls.push(PendingCall {
                key: format!("mcp\u{1f}{}\u{1f}{}", site.nsp, site.name),
                input,
                server_key: site.server_key.clone(),
                tool: site.tool.clone(),
                arguments,
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
        let mut grouped: BTreeMap<String, Vec<(String, serde_json::Value)>> = BTreeMap::new();
        let mut index: Vec<(String, String, String)> = Vec::new();
        for row in &rows {
            for call in &row.calls {
                let args: serde_json::Value = serde_json::from_str(&call.arguments).unwrap_or(serde_json::json!({}));
                grouped.entry(call.server_key.clone()).or_default().push((call.tool.clone(), args));
                index.push((call.server_key.clone(), call.key.clone(), call.input.clone()));
            }
        }
        let mut forget = Forget(Vec::new());
        for (server_key, calls) in &grouped {
            let Some(tool) = self.sites.iter().find(|s| s.server_key == *server_key) else {
                continue;
            };
            let Some(spec_tool) = rvbbit_resolve::mcp_tool(&tool.nsp, &tool.name) else {
                continue;
            };
            let opts = rvbbit_dispatch::McpCallOpts {
                timeout_ms: spec_tool.timeout_ms,
                retries: spec_tool.retries,
                cache_ttl_ms: spec_tool.cache_ttl_ms,
                read_only: spec_tool.read_only,
                max_cost: spec_tool.max_cost,
                concurrency: fanout(),
                cost_usd: spec_tool.cost_usd.unwrap_or(0.0),
            };
            let priced = priced_calls(&self.sites, server_key, calls)?;
            let reply = rvbbit_dispatch::mcp_batch_for_tool(&spec_tool, priced, &opts).map_err(err)?;
            let mut cursor = 0usize;
            for (key_server, key, input) in &index {
                if key_server != server_key {
                    continue;
                }
                let hit = match reply.get(cursor) {
                    Some(Ok(value)) => {
                        let text = serde_json::to_string(value).unwrap_or_else(|_| "null".into());
                        rvbbit_dispatch::ScalarHit::Value(text)
                    }
                    Some(Err(message)) => rvbbit_dispatch::ScalarHit::Failed(message.clone()),
                    None => rvbbit_dispatch::ScalarHit::Failed("mcp batch truncated".into()),
                };
                rvbbit_dispatch::remember_delivery(key, input, hit);
                forget.0.push((key.clone(), input.clone()));
                cursor += 1;
            }
            rvbbit_cost::note_mcp_batch(node.ps.plan.plan_node_id);
        }
        let ecxt = node.ps.ps_ExprContext.expect("Result ExprContext");
        for row in &rows {
            estate.reset_expr_context(ecxt);
            if let (Some(saved), Some(slot)) = (&row.outer, row.outer_slot) {
                semantic::restore_tuple(estate, slot, saved)?;
                estate.ecxt_mut(ecxt).ecxt_outertuple = Some(slot);
            }
            lane_result_project(&mut node.ps, estate)?;
            let projected = semantic::snapshot_slot(estate, node.ps.ps_ResultTupleSlot.expect("result slot"))?;
            self.emit.push_back(projected);
        }
        drop(forget);
        Ok(())
    }
}

fn fanout() -> usize {
    rvbbit_resolve::max_concurrent().max(1)
}

fn eval_json<'mcx>(
    estate: &mut EStateData<'mcx>,
    ecxt: executils::EcxtId,
    expr: &mut ExprState<'mcx>,
    sql_type: &str,
) -> PgResult<serde_json::Value> {
    let typ = sql_type.trim().to_ascii_lowercase();
    if matches!(typ.as_str(), "text" | "varchar" | "character varying" | "char" | "bpchar") {
        return Ok(match semantic::eval_text(estate, ecxt, expr)? {
            Some(text) => serde_json::Value::String(text),
            None => serde_json::Value::Null,
        });
    }
    let nd = with_eval_slots(estate, ecxt, None, |slots, _, _| exec_eval_expr(expr, slots))?;
    if nd.isnull {
        return Ok(serde_json::Value::Null);
    }
    let value = match typ.as_str() {
        "boolean" | "bool" => serde_json::Value::Bool(nd.value.as_bool()),
        "smallint" | "int2" | "integer" | "int" | "int4" => serde_json::Value::from(nd.value.as_i32()),
        "bigint" | "int8" => serde_json::Value::from(nd.value.as_i64()),
        "real" | "float4" => serde_json::json!(nd.value.as_f32()),
        "double precision" | "float8" => serde_json::json!(nd.value.as_f64()),
        _ => serde_json::Value::Null,
    };
    Ok(value)
}

struct Forget(Vec<(String, String)>);

impl Drop for Forget {
    fn drop(&mut self) {
        rvbbit_dispatch::forget_scalar(&self.0);
    }
}

fn err(message: impl Into<String>) -> Box<PgError> {
    Box::new(PgError::error(message.into()))
}

thread_local! {
    static SCAN_EXPRS: RefCell<HashMap<(u32, i32), Vec<usize>>> = RefCell::new(HashMap::new());
    static SCAN_OWNS: RefCell<HashMap<(u32, i32), bool>> = RefCell::new(HashMap::new());
    static SCAN_BOUNDS: RefCell<HashMap<(u32, i32), Option<u64>>> = RefCell::new(HashMap::new());
    static SCAN_DRIVES: RefCell<HashMap<(u32, i32), ScanDrive>> = RefCell::new(HashMap::new());
}

struct ScanDrive {
    declined: bool,
    done: bool,
    pulled: u64,
    bound: Option<u64>,
    emit: VecDeque<semantic::SavedTuple>,
    held: Option<semantic::SavedTuple>,
}

fn drive_key(plan_node_id: i32) -> (u32, i32) {
    (rvbbit_cost::exec_depth(), plan_node_id)
}

/// Plan target-list expressions for a scan. The list itself is not a
/// `Node`, so the expression pointers are what later execution walks.
pub fn register_scan_tlist(plan_node_id: i32, tlist: &types_nodes::list::NodeList<'_>) {
    let mut exprs = Vec::new();
    for item in tlist.iter() {
        if let Some(entry) = item.as_target_entry() {
            exprs.push(entry.expr.as_raw().as_ptr() as usize);
        }
    }
    SCAN_EXPRS.with(|regs| {
        regs.borrow_mut().insert(drive_key(plan_node_id), exprs);
    });
}

pub fn note_scan_bound(plan_node_id: i32, tuples_needed: i64) {
    let bound = if tuples_needed < 0 { None } else { Some(tuples_needed as u64) };
    let key = drive_key(plan_node_id);
    SCAN_BOUNDS.with(|regs| {
        regs.borrow_mut().insert(key, bound);
    });
    SCAN_DRIVES.with(|regs| {
        if let Some(drive) = regs.borrow_mut().get_mut(&key) {
            drive.bound = bound;
        }
    });
}

pub fn rescan_scan(plan_node_id: i32) {
    let key = drive_key(plan_node_id);
    SCAN_DRIVES.with(|regs| {
        if let Some(drive) = regs.borrow_mut().get_mut(&key) {
            drive.done = false;
            drive.pulled = 0;
            drive.emit.clear();
            drive.held = None;
        }
    });
}

pub fn clear_drives() {
    SCAN_EXPRS.with(|regs| regs.borrow_mut().clear());
    SCAN_OWNS.with(|regs| regs.borrow_mut().clear());
    SCAN_BOUNDS.with(|regs| regs.borrow_mut().clear());
    SCAN_DRIVES.with(|regs| regs.borrow_mut().clear());
}

pub fn on_executor_end() {
    let depth = rvbbit_cost::exec_depth();
    let keep = |key: &(u32, i32)| key.0 <= depth;
    SCAN_EXPRS.with(|regs| regs.borrow_mut().retain(|key, _| keep(key)));
    SCAN_OWNS.with(|regs| regs.borrow_mut().retain(|key, _| keep(key)));
    SCAN_BOUNDS.with(|regs| regs.borrow_mut().retain(|key, _| keep(key)));
    SCAN_DRIVES.with(|regs| regs.borrow_mut().retain(|key, _| keep(key)));
}

pub fn arm_targetlist<'mcx>(
    plan_node_id: i32,
    tlist: &types_nodes::list::NodeList<'mcx>,
    mcx: Mcx<'mcx>,
) -> PgResult<Option<rvbbit_cost::McpNodeGuard>> {
    let nodes: Vec<Node<'mcx>> = tlist.iter().filter_map(|item| item.as_target_entry().map(|entry| entry.expr)).collect();
    arm_nodes(plan_node_id, &nodes, mcx)
}

pub fn arm_registered<'mcx>(plan_node_id: i32, mcx: Mcx<'mcx>) -> PgResult<Option<rvbbit_cost::McpNodeGuard>> {
    let nodes = registered_nodes(plan_node_id);
    arm_nodes(plan_node_id, &nodes, mcx)
}

fn arm_nodes<'mcx>(
    plan_node_id: i32,
    nodes: &[Node<'mcx>],
    mcx: Mcx<'mcx>,
) -> PgResult<Option<rvbbit_cost::McpNodeGuard>> {
    let key = drive_key(plan_node_id);
    let known = SCAN_OWNS.with(|regs| regs.borrow().get(&key).copied());
    let owns = match known {
        Some(owns) => owns,
        None => {
            let mut funcs = Vec::new();
            let mut eligible = true;
            for node in nodes {
                walk(mcx, *node, &mut funcs, &mut eligible)?;
            }
            let owns = !funcs.is_empty();
            SCAN_OWNS.with(|regs| {
                regs.borrow_mut().insert(key, owns);
            });
            owns
        }
    };
    if owns {
        Ok(Some(rvbbit_cost::push_mcp_node(plan_node_id)))
    } else {
        Ok(None)
    }
}

enum Queued {
    Fresh,
    Fill,
    Decline,
    End,
    Row(semantic::SavedTuple),
}

/// `Some(slot)` is this batcher's answer, including end of scan.
/// `None` means the ordinary per-tuple scan should run.
pub fn scan_pull<'mcx, N: ::execscan::ScanNode<'mcx>>(
    plan_node_id: i32,
    node: &mut N,
    estate: &mut EStateData<'mcx>,
) -> PgResult<Option<Option<ExecSlotId>>> {
    let key = drive_key(plan_node_id);
    let mut filling = false;
    let queued = SCAN_DRIVES.with(|regs| {
        let mut drives = regs.borrow_mut();
        let Some(drive) = drives.get_mut(&key) else {
            return Queued::Fresh;
        };
        if let Some(row) = drive.emit.pop_front() {
            Queued::Row(row)
        } else if drive.done {
            Queued::End
        } else if drive.declined {
            Queued::Decline
        } else {
            Queued::Fill
        }
    });
    match queued {
        Queued::Row(row) => {
            let slot = result_slot(node)?;
            semantic::restore_tuple(estate, slot, &row)?;
            SCAN_DRIVES.with(|regs| {
                if let Some(drive) = regs.borrow_mut().get_mut(&key) {
                    drive.held = Some(row);
                }
            });
            return Ok(Some(Some(slot)));
        }
        Queued::End => return Ok(Some(None)),
        Queued::Decline => return Ok(None),
        Queued::Fresh => filling = false,
        Queued::Fill => filling = true,
    }
    if !filling && !scan_can_batch(node, estate) {
        mark_declined(key);
        return Ok(None);
    }
    let exprs = registered_nodes(plan_node_id);
    let mcx = estate.es_query_cxt;
    let mut funcs = Vec::new();
    let mut eligible = true;
    for expr in &exprs {
        walk(mcx, *expr, &mut funcs, &mut eligible)?;
    }
    let Some(mut sites) = sites_from(estate, funcs, eligible)? else {
        mark_declined(key);
        return Ok(None);
    };
    let batch_rows = rvbbit_resolve::batch_rows().max(1);
    let mut drive = SCAN_DRIVES.with(|regs| regs.borrow_mut().remove(&key)).unwrap_or_else(|| ScanDrive {
        declined: false,
        done: false,
        pulled: 0,
        bound: SCAN_BOUNDS.with(|regs| regs.borrow().get(&key).copied().flatten()),
        emit: VecDeque::new(),
        held: None,
    });
    fill_scan_batch(plan_node_id, node, estate, &mut sites, batch_rows, &mut drive)?;
    let first = drive.emit.pop_front();
    let slot = if let Some(row) = first {
        let slot = result_slot(node)?;
        semantic::restore_tuple(estate, slot, &row)?;
        drive.held = Some(row);
        Some(slot)
    } else {
        None
    };
    SCAN_DRIVES.with(|regs| {
        regs.borrow_mut().insert(key, drive);
    });
    Ok(Some(slot))
}

fn scan_can_batch<'mcx, N: ::execscan::ScanNode<'mcx>>(node: &mut N, estate: &EStateData<'mcx>) -> bool {
    if estate.es_epq_active || node.ss_mut().qual.is_some() || node.ss_mut().ps_ProjInfo.is_none() {
        return false;
    }
    matches!(estate.es_direction, ::types_scan::ScanDirection::ForwardScanDirection)
}

fn mark_declined(key: (u32, i32)) {
    SCAN_DRIVES.with(|regs| {
        regs.borrow_mut().insert(
            key,
            ScanDrive {
                declined: true,
                done: false,
                pulled: 0,
                bound: None,
                emit: VecDeque::new(),
                held: None,
            },
        );
    });
}

fn registered_nodes<'mcx>(plan_node_id: i32) -> Vec<Node<'mcx>> {
    let ptrs = SCAN_EXPRS.with(|regs| regs.borrow().get(&drive_key(plan_node_id)).cloned().unwrap_or_default());
    ptrs.into_iter()
        .filter_map(|ptr| NonNull::new(ptr as *mut ()))
        .map(|ptr| unsafe { Node::from_raw(ptr) })
        .collect()
}

fn result_slot<'mcx, N: ::execscan::ScanNode<'mcx>>(node: &mut N) -> PgResult<ExecSlotId> {
    node.ss_mut()
        .ps_ProjInfo
        .as_ref()
        .map(|proj| proj.pi_result_slot)
        .ok_or_else(|| err("mcp scan batch has no projection"))
}

fn fill_scan_batch<'mcx, N: ::execscan::ScanNode<'mcx>>(
    plan_node_id: i32,
    node: &mut N,
    estate: &mut EStateData<'mcx>,
    sites: &mut [Site<'mcx>],
    batch_rows: usize,
    drive: &mut ScanDrive,
) -> PgResult<()> {
    let room = drive.bound.map(|bound| bound.saturating_sub(drive.pulled)).unwrap_or(u64::MAX);
    let limit = (batch_rows as u64).min(room) as usize;
    if limit == 0 {
        drive.done = true;
        return Ok(());
    }
    let ecxt = node.ss_mut().ps_ExprContext;
    let mut rows: Vec<(semantic::SavedTuple, ExecSlotId, Vec<PendingCall>)> = Vec::new();
    while rows.len() < limit {
        crate::cfi()?;
        estate.ecxt_mut(ecxt).reset();
        let Some(scan_id) = ::execscan::fetch_scan_tuple(node, estate)? else {
            drive.done = true;
            break;
        };
        estate.ecxt_mut(ecxt).ecxt_scantuple = Some(scan_id);
        let calls = eval_site_list(sites, estate, ecxt)?;
        let saved = semantic::snapshot_slot(estate, scan_id)?;
        drive.pulled += 1;
        rows.push((saved, scan_id, calls));
    }
    if rows.is_empty() {
        drive.done = true;
        return Ok(());
    }
    let forget = launch_calls(plan_node_id, sites, &rows)?;
    for (saved, scan_id, _) in &rows {
        estate.ecxt_mut(ecxt).reset();
        semantic::restore_tuple(estate, *scan_id, saved)?;
        estate.ecxt_mut(ecxt).ecxt_scantuple = Some(*scan_id);
        let result_id = project_scan(node, estate, *scan_id)?;
        let projected = semantic::snapshot_slot(estate, result_id)?;
        drive.emit.push_back(projected);
    }
    drop(forget);
    Ok(())
}

fn eval_site_list<'mcx>(
    sites: &mut [Site<'mcx>],
    estate: &mut EStateData<'mcx>,
    ecxt: executils::EcxtId,
) -> PgResult<Vec<PendingCall>> {
    let mut calls = Vec::new();
    for site in sites.iter_mut() {
        let mut items = Vec::with_capacity(site.args.len());
        for (expr, typ) in site.args.iter_mut().zip(site.arg_types.iter()) {
            items.push(eval_json(estate, ecxt, expr, typ)?);
        }
        let positional = serde_json::Value::Array(items);
        let input = positional.to_string();
        let arguments = rvbbit_dispatch::mcp_arguments(&site.arg_keys, &input).unwrap_or(positional).to_string();
        calls.push(PendingCall {
            key: format!("mcp\u{1f}{}\u{1f}{}", site.nsp, site.name),
            input,
            server_key: site.server_key.clone(),
            tool: site.tool.clone(),
            arguments,
        });
    }
    Ok(calls)
}

fn launch_calls<'mcx>(
    plan_node_id: i32,
    sites: &[Site<'mcx>],
    rows: &[(semantic::SavedTuple, ExecSlotId, Vec<PendingCall>)],
) -> PgResult<Forget> {
    let mut grouped: BTreeMap<String, Vec<(String, serde_json::Value)>> = BTreeMap::new();
    let mut index: Vec<(String, String, String)> = Vec::new();
    for (_, _, calls) in rows {
        for call in calls {
            let args: serde_json::Value = serde_json::from_str(&call.arguments).unwrap_or(serde_json::json!({}));
            grouped.entry(call.server_key.clone()).or_default().push((call.tool.clone(), args));
            index.push((call.server_key.clone(), call.key.clone(), call.input.clone()));
        }
    }
    let mut forget = Forget(Vec::new());
    for (server_key, calls) in &grouped {
        let Some(tool) = sites.iter().find(|site| site.server_key == *server_key) else {
            continue;
        };
        let Some(spec_tool) = rvbbit_resolve::mcp_tool(&tool.nsp, &tool.name) else {
            continue;
        };
        let opts = rvbbit_dispatch::McpCallOpts {
            timeout_ms: spec_tool.timeout_ms,
            retries: spec_tool.retries,
            cache_ttl_ms: spec_tool.cache_ttl_ms,
            read_only: spec_tool.read_only,
            max_cost: spec_tool.max_cost,
            concurrency: fanout(),
            cost_usd: spec_tool.cost_usd.unwrap_or(0.0),
        };
        let priced = priced_calls(sites, server_key, calls)?;
        let reply = rvbbit_dispatch::mcp_batch_for_tool(&spec_tool, priced, &opts).map_err(err)?;
        let mut cursor = 0usize;
        for (key_server, key, input) in &index {
            if key_server != server_key {
                continue;
            }
            let hit = match reply.get(cursor) {
                Some(Ok(value)) => {
                    let text = serde_json::to_string(value).unwrap_or_else(|_| "null".into());
                    rvbbit_dispatch::ScalarHit::Value(text)
                }
                Some(Err(message)) => rvbbit_dispatch::ScalarHit::Failed(message.clone()),
                None => rvbbit_dispatch::ScalarHit::Failed("mcp scan batch truncated".into()),
            };
            rvbbit_dispatch::remember_delivery(key, input, hit);
            forget.0.push((key.clone(), input.clone()));
            cursor += 1;
        }
        rvbbit_cost::note_mcp_batch(plan_node_id);
    }
    Ok(forget)
}

fn project_scan<'mcx, N: ::execscan::ScanNode<'mcx>>(
    node: &mut N,
    estate: &mut EStateData<'mcx>,
    scan_id: ExecSlotId,
) -> PgResult<ExecSlotId> {
    let ss = node.ss_mut();
    let ecxt = ss.ps_ExprContext;
    let result_id = ss.ps_ProjInfo.as_ref().expect("projection").pi_result_slot;
    let subplan = {
        let proj = ss.ps_ProjInfo.as_ref().expect("projection");
        proj.pi_state.has_subplan() || !proj.pi_state.param_exec_deps().is_empty()
    };
    if subplan {
        let proj = node.ss_mut().ps_ProjInfo.as_mut().expect("projection");
        executils::exec_project_with_subplans(&mut proj.pi_state, estate, ecxt, result_id)?;
        return Ok(result_id);
    }
    unsafe {
        let per_tuple = estate.ecxt(ecxt).per_tuple_mcx();
        node.ss_mut().ps_ProjInfo.as_mut().expect("projection").pi_state.arm_result_mcx_raw(per_tuple);
    }
    let mcx = estate.es_query_cxt;
    let (scan_slot, result_slot) = ::execscan::slot_pair(estate, scan_id, result_id);
    let proj = node.ss_mut().ps_ProjInfo.as_mut().expect("projection");
    let mut slots = EvalSlots { scan: Some(scan_slot), inner: None, outer: None };
    exec_project_prearmed(&mut proj.pi_state, &mut slots, result_slot, mcx)?;
    Ok(result_id)
}
