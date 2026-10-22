//! Reserved identifiers for the rvbbit semantic-SQL spike.
//!
//! OID 9020 sits in PostgreSQL's fork-reserved window (9000–9999) and in
//! pgrust's `PGRUST_FOID_RANGE` (9000..=9099), clear of the oids already
//! claimed by the sqe census (9010/9011), the p8 census (9012), and the
//! janitor builtins (9001/9002/9005).

/// `rvbbit_means(text, text) -> bool`. Fallback oid when the catalog has
/// not been created yet. Catalog `MEANS` gets a real `pg_proc` oid.
pub const MEANS_OID: u32 = 9020;

pub const MEANS_NAME: &str = "rvbbit_means";

/// Internal bodies for dump/restore. Not `pg_proc` oids; `CREATE FUNCTION
/// ... AS 'name' LANGUAGE internal` resolves them by name.
pub const DUMP_OID: u32 = 9021;
pub const RESTORE_OID: u32 = 9022;
pub const GETDEF_OID: u32 = 9023;
/// `rvbbit_extract(text) -> text`. Body for text semantic functions that
/// have no label set.
pub const EXTRACT_OID: u32 = 9024;
/// `rvbbit_means_score(text, text) -> float8`. Same predict call as MEANS;
/// returns the yes-probability.
pub const MEANS_SCORE_OID: u32 = 9025;
/// `rvbbit_classify(text) -> text`. Fixed-label classify. The SQL result
/// is JSON text `{label, score, scores}`.
pub const CLASSIFY_OID: u32 = 9026;
/// `rvbbit_call_log() -> text`. JSON array behind the `rvbbit.calls` view.
pub const CALL_LOG_OID: u32 = 9027;
/// `rvbbit_decide(text, text) -> text`. Internal name for `rvbbit.decide`.
/// The plan's `pg_proc` oid is different; [`rvbbit_resolve::is_decide_func`]
/// is that oid.
pub const DECIDE_OID: u32 = 9028;
/// `rvbbit_classify_labels(text, text, text) -> text`. Native
/// `CLASSIFY(x INTO ...)`. `labels` is U+001F-separated. `fallback` may be null.
pub const CLASSIFY_LABELS_OID: u32 = 9029;
/// `rvbbit_extract_typed(text, text, text) -> text`. Native
/// `EXTRACT(type FROM x)` when `type` is not a datetime field.
pub const EXTRACT_TYPED_OID: u32 = 9030;
/// `rvbbit_classify_enum(text, text, text) -> text`. Native
/// `CLASSIFY(x AS enum)`.
pub const CLASSIFY_ENUM_OID: u32 = 9031;
/// `rvbbit_apply_clauses(text, text, text, text) -> text`.
/// Clause tail on a semantic call. The SQL result is a JSON report.
pub const APPLY_CLAUSES_OID: u32 = 9032;
/// `rvbbit_summarize_trans(text, text, text) -> text`.
pub const SUMMARIZE_TRANS_OID: u32 = 9033;
/// `rvbbit_summarize_final(text) -> text`.
pub const SUMMARIZE_FINAL_OID: u32 = 9034;
/// `rvbbit_consensus_trans(text, text, text, text) -> text`.
pub const CONSENSUS_TRANS_OID: u32 = 9035;
/// `rvbbit_consensus_final(text) -> bool`.
pub const CONSENSUS_FINAL_OID: u32 = 9036;
/// `rvbbit_rerank(text, text) -> float8`.
pub const RERANK_OID: u32 = 9037;
/// `rvbbit_extract_many(text, text, text) -> text`. JSON array.
pub const EXTRACT_MANY_OID: u32 = 9038;
/// `rvbbit_summarize_text(text, text) -> text`. Per-row summary (the pipe).
pub const SUMMARIZE_TEXT_OID: u32 = 9039;
/// `rvbbit_clause_bool(text, text, text, text) -> bool`. Natural type of a
/// clause tail on MEANS or CONSENSUS. The JSON report stays on
/// `rvbbit_apply_clauses` and on `rvbbit.calls`.
pub const CLAUSE_BOOL_OID: u32 = 9040;
/// `rvbbit_clause_score(text, text, text, text) -> float8`. Natural type of
/// a clause tail on RERANK.
pub const CLAUSE_SCORE_OID: u32 = 9041;
/// `rvbbit_clause_text(text, text, text, text) -> text`. Natural type of a
/// clause tail on CLASSIFY, EXTRACT, EXTRACT MANY, or a text summary.
pub const CLAUSE_TEXT_OID: u32 = 9042;
/// `rvbbit_rerank_for(text, text, text) -> float8`. Same score as
/// `rvbbit_rerank`, with the model slug in the third argument. Empty
/// slug keeps the jev default.
pub const RERANK_FOR_OID: u32 = 9043;
/// `rvbbit_user(...)`. Body for a user semantic function that is not the
/// 2-argument means/score operator: a prompt template, `RETURNS label`,
/// a 1-argument bool or score, or `AS DECISION TREE`. Arity comes from
/// `pg_proc`, not from this oid.
pub const USER_OID: u32 = 9044;
/// `rvbbit_semantic_similarity(text, text) -> float8`. Cosine of the two
/// embedding vectors. The SQL name is `rvbbit.semantic_similarity`.
pub const SIMILARITY_OID: u32 = 9045;
/// `rvbbit_flow(text, text) -> text`. Runs the latest `CREATE WORKFLOW`
/// body. A third `int` argument selects the version. The SQL name is
/// `rvbbit.flow`.
pub const FLOW_OID: u32 = 9046;
/// `rvbbit_mcp_invoke_text(nsp, name, args, use_fallback, fallback) -> text`.
/// The SQL name is `rvbbit.mcp_invoke_text`. Not strict: `ON ERROR NULL`
/// passes a null fallback.
pub const MCP_INVOKE_OID: u32 = 9047;
/// `rvbbit_last_actual() -> text`. `calls,usd` captured from the previous
/// statement when `semantic.capture_actual` was on.
pub const LAST_ACTUAL_OID: u32 = 9048;

pub const DUMP_NAME: &str = "rvbbit_dump_sql";
pub const RESTORE_NAME: &str = "rvbbit_restore_sql";
pub const GETDEF_NAME: &str = "rvbbit_get_semfn_def";
pub const EXTRACT_NAME: &str = "rvbbit_extract";
pub const MEANS_SCORE_NAME: &str = "rvbbit_means_score";
pub const CLASSIFY_NAME: &str = "rvbbit_classify";
pub const CALL_LOG_NAME: &str = "rvbbit_call_log";
pub const DECIDE_NAME: &str = "rvbbit_decide";
pub const CLASSIFY_LABELS_NAME: &str = "rvbbit_classify_labels";
pub const EXTRACT_TYPED_NAME: &str = "rvbbit_extract_typed";
pub const CLASSIFY_ENUM_NAME: &str = "rvbbit_classify_enum";
pub const APPLY_CLAUSES_NAME: &str = "rvbbit_apply_clauses";
pub const SUMMARIZE_TRANS_NAME: &str = "rvbbit_summarize_trans";
pub const SUMMARIZE_FINAL_NAME: &str = "rvbbit_summarize_final";
pub const CONSENSUS_TRANS_NAME: &str = "rvbbit_consensus_trans";
pub const CONSENSUS_FINAL_NAME: &str = "rvbbit_consensus_final";
pub const RERANK_NAME: &str = "rvbbit_rerank";
pub const EXTRACT_MANY_NAME: &str = "rvbbit_extract_many";
pub const SUMMARIZE_TEXT_NAME: &str = "rvbbit_summarize_text";
pub const CLAUSE_BOOL_NAME: &str = "rvbbit_clause_bool";
pub const CLAUSE_SCORE_NAME: &str = "rvbbit_clause_score";
pub const CLAUSE_TEXT_NAME: &str = "rvbbit_clause_text";
pub const RERANK_FOR_NAME: &str = "rvbbit_rerank_for";
pub const USER_NAME: &str = "rvbbit_user";
pub const SIMILARITY_NAME: &str = "rvbbit_semantic_similarity";
pub const FLOW_NAME: &str = "rvbbit_flow";
pub const MCP_INVOKE_NAME: &str = "rvbbit_mcp_invoke_text";
pub const LAST_ACTUAL_NAME: &str = "rvbbit_last_actual";

/// Rows pulled into one semantic batch before an HTTP call.
pub const BATCH_ROWS: usize = 32;

/// In-flight hutch requests for a single backend, and the per-model
/// concurrency cap inside the dispatch pool.
pub const CONCURRENCY: usize = 4;

/// OS threads started with the server. Larger than [`CONCURRENCY`] so the
/// semaphore, not the thread count, is what limits a model.
pub const POOL_THREADS: usize = 8;
