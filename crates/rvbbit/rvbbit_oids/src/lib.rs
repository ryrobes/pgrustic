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
/// `rvbbit_extract(text) -> text`. Body for non-boolean semantic functions.
pub const EXTRACT_OID: u32 = 9024;

pub const DUMP_NAME: &str = "rvbbit_dump_sql";
pub const RESTORE_NAME: &str = "rvbbit_restore_sql";
pub const GETDEF_NAME: &str = "rvbbit_get_semfn_def";
pub const EXTRACT_NAME: &str = "rvbbit_extract";

/// Rows pulled into one semantic batch before an HTTP call.
pub const BATCH_ROWS: usize = 32;

/// In-flight hutch requests for a single backend, and the per-model
/// concurrency cap inside the dispatch pool.
pub const CONCURRENCY: usize = 4;

/// OS threads started with the server. Larger than [`CONCURRENCY`] so the
/// semaphore, not the thread count, is what limits a model.
pub const POOL_THREADS: usize = 8;
