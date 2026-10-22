//! Batch scalar MCP tool calls in a Result target list.
//!
//! Compiled as `execmain::mcpbatch` via `#[path]`. A window of child rows
//! is sent through `hutch-mcp` once per server. The SQL function then
//! reads `rvbbit.mcp_invoke_text`, which finds the memo and does not call
//! again. Set-returning tools, jsonb arguments, and a Result that the
//! semantic project batcher already owns stay per-row. Stdio concurrency
//! is 1 inside the gateway; HTTP overlaps.

use std::collections::{BTreeMap, VecDeque};

use execexpr::{exec_eval_expr, ExprState};
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
    if funcs.is_empty() || !eligible {
        return Ok(None);
    }
    rvbbit_resolve::ensure()?;
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
        sites.push(Site {
            nsp: tool.nsp,
            name: tool.name,
            tool: tool.tool,
            server_key,
            arg_types: tool.arg_types,
            arg_keys: tool.arg_keys,
            args,
        });
    }
    if sites.is_empty() {
        return Ok(None);
    }
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
                concurrency: if spec_tool.transport == "stdio" { 1 } else { 4 },
            };
            let reply = rvbbit_dispatch::mcp_batch_for_tool(&spec_tool, calls.clone(), &opts).map_err(err)?;
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
                rvbbit_dispatch::remember_scalar(key, input, hit);
                forget.0.push((key.clone(), input.clone()));
                cursor += 1;
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
            let projected = semantic::snapshot_slot(estate, node.ps.ps_ResultTupleSlot.expect("result slot"))?;
            self.emit.push_back(projected);
        }
        drop(forget);
        Ok(())
    }
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
