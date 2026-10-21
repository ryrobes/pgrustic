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

/// Rows pulled into one semantic batch before an HTTP call.
pub const BATCH_ROWS: usize = 32;

/// In-flight hutch requests for a single backend, and the per-model
/// concurrency cap inside the dispatch pool.
pub const CONCURRENCY: usize = 4;

/// OS threads started with the server. Larger than [`CONCURRENCY`] so the
/// semaphore, not the thread count, is what limits a model.
pub const POOL_THREADS: usize = 8;
