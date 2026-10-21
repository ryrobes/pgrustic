//! Reserved identifiers for the rvbbit semantic-SQL spike.
//!
//! OID 9020 sits in PostgreSQL's fork-reserved window (9000–9999) and in
//! pgrust's `PGRUST_FOID_RANGE` (9000..=9099), clear of the oids already
//! claimed by the sqe census (9010/9011), the p8 census (9012), and the
//! janitor builtins (9001/9002/9005).

/// `rvbbit_means(text, text) -> bool`.
pub const MEANS_OID: u32 = 9020;

pub const MEANS_NAME: &str = "rvbbit_means";

/// Rows pulled into one semantic batch before an HTTP call.
pub const BATCH_ROWS: usize = 32;

/// In-flight hutch requests for a single backend, and the per-model
/// concurrency cap inside the dispatch pool.
pub const CONCURRENCY: usize = 4;

/// OS threads started with the server. Larger than [`CONCURRENCY`] so the
/// semaphore, not the thread count, is what limits a model.
pub const POOL_THREADS: usize = 8;
