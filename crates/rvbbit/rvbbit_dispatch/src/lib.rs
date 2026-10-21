//! Blocking HTTP dispatch from backend threads to the hutch service.
//!
//! A fixed OS thread pool is started at server boot. Backend threads hand
//! work over a bounded queue and wait on a reply channel, polling
//! `InterruptPending` so query cancel and `statement_timeout` abort the
//! wait. Workers use `ureq` (no tokio). A semaphore caps in-flight calls
//! per model; the spike has one model, so the cap is [`CONCURRENCY`].

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use rvbbit_oids::{CONCURRENCY, POOL_THREADS};
use serde::Serialize;
use types_error::{PgError, PgResult};

const WAIT_SLICE: Duration = Duration::from_millis(20);

struct Job {
    body: String,
    expect: usize,
    reply: mpsc::Sender<Result<BatchReply, String>>,
}

struct Limiter {
    free: Mutex<usize>,
    cv: Condvar,
}

impl Limiter {
    fn new(n: usize) -> Self {
        Self {
            free: Mutex::new(n),
            cv: Condvar::new(),
        }
    }

    fn acquire(&self) {
        let mut guard = self.free.lock().unwrap_or_else(|e| e.into_inner());
        while *guard == 0 {
            guard = self.cv.wait(guard).unwrap_or_else(|e| e.into_inner());
        }
        *guard -= 1;
    }

    fn release(&self) {
        let mut guard = self.free.lock().unwrap_or_else(|e| e.into_inner());
        *guard += 1;
        self.cv.notify_one();
    }
}

struct Pool {
    tx: SyncSender<Job>,
}

fn pool() -> &'static Pool {
    static POOL: std::sync::OnceLock<Pool> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        let (tx, rx) = mpsc::sync_channel::<Job>(64);
        let rx = Arc::new(Mutex::new(rx));
        let limiter = Arc::new(Limiter::new(CONCURRENCY));
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_millis(timeout_ms()))
            .build();
        let url = hutch_url();
        for i in 0..POOL_THREADS {
            let rx = Arc::clone(&rx);
            let limiter = Arc::clone(&limiter);
            let agent = agent.clone();
            let url = url.clone();
            thread::Builder::new()
                .name(format!("rvbbit-hutch-{i}"))
                .spawn(move || worker(rx, limiter, agent, url))
                .expect("spawn rvbbit dispatch worker");
        }
        Pool { tx }
    })
}

fn worker(
    rx: Arc<Mutex<Receiver<Job>>>,
    limiter: Arc<Limiter>,
    agent: ureq::Agent,
    url: String,
) {
    loop {
        let job = {
            let guard = rx.lock().unwrap_or_else(|e| e.into_inner());
            guard.recv()
        };
        let Ok(job) = job else {
            break;
        };
        limiter.acquire();
        let result = post_with_retry(&agent, &url, &job.body, job.expect);
        limiter.release();
        let _ = job.reply.send(result);
    }
}

fn hutch_url() -> String {
    std::env::var("RVBBIT_HUTCH_URL").unwrap_or_else(|_| "http://127.0.0.1:8099/v1/means".to_string())
}

fn timeout_ms() -> u64 {
    std::env::var("RVBBIT_HUTCH_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30_000)
}

/// Start the pool. Safe to call more than once; later calls are no-ops.
/// Reads `RVBBIT_HUTCH_URL` and `RVBBIT_HUTCH_TIMEOUT_MS` on the first call.
pub fn start() {
    let _ = pool();
}

pub struct Ticket {
    rx: Receiver<Result<BatchReply, String>>,
}

/// One hutch round-trip's worth of answers. `http_calls` counts attempts,
/// including 429 retries, so EXPLAIN lines up with the mock's `/stats`.
pub struct BatchReply {
    pub matched: Vec<bool>,
    pub http_calls: u32,
}

#[derive(Serialize)]
struct ReqItem<'a> {
    key: String,
    input: &'a str,
    criterion: &'a str,
}

/// Queue one batch. The caller waits with [`wait`], which honors query cancel.
pub fn submit(items: &[(String, String)]) -> PgResult<Ticket> {
    if items.is_empty() {
        let (tx, rx) = mpsc::channel();
        let _ = tx.send(Ok(BatchReply {
            matched: Vec::new(),
            http_calls: 0,
        }));
        return Ok(Ticket { rx });
    }
    let body_items: Vec<ReqItem<'_>> = items
        .iter()
        .enumerate()
        .map(|(i, (input, criterion))| ReqItem {
            key: i.to_string(),
            input,
            criterion,
        })
        .collect();
    let body = serde_json::to_string(&body_items).map_err(|e| pg(format!("hutch encode: {e}")))?;
    let (reply, rx) = mpsc::channel();
    let job = Job {
        body,
        expect: items.len(),
        reply,
    };
    let tx = &pool().tx;
    let mut job = Some(job);
    loop {
        check_cancel()?;
        match tx.try_send(job.take().expect("job")) {
            Ok(()) => break,
            Err(TrySendError::Full(returned)) => {
                job = Some(returned);
                thread::sleep(WAIT_SLICE);
            }
            Err(TrySendError::Disconnected(_)) => {
                return Err(pg("rvbbit dispatch pool is gone"));
            }
        }
    }
    Ok(Ticket { rx })
}

/// Block until the batch returns, or the backend is canceled.
pub fn wait(ticket: Ticket) -> PgResult<BatchReply> {
    loop {
        check_cancel()?;
        match ticket.rx.recv_timeout(WAIT_SLICE) {
            Ok(Ok(v)) => return Ok(v),
            Ok(Err(e)) => return Err(pg(e)),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                return Err(pg("rvbbit dispatch worker dropped the reply"));
            }
        }
    }
}

/// One pair through the pool. Used by the per-row builtin fallback.
pub fn means_pair(input: &str, criterion: &str) -> PgResult<bool> {
    let ticket = submit(&[(input.to_string(), criterion.to_string())])?;
    let got = wait(ticket)?;
    Ok(got.matched.first().copied().unwrap_or(false))
}

fn check_cancel() -> PgResult<()> {
    if init_small::globals::InterruptPending() {
        postgres_seams::check_for_interrupts::call()?;
    }
    Ok(())
}

fn post_with_retry(
    agent: &ureq::Agent,
    url: &str,
    body: &str,
    expect: usize,
) -> Result<BatchReply, String> {
    let mut last = String::from("hutch: no attempt");
    for attempt in 1..=5 {
        match post_once(agent, url, body, expect) {
            Ok(matched) => {
                return Ok(BatchReply {
                    matched,
                    http_calls: attempt,
                })
            }
            Err(e) if e.starts_with("429") && attempt < 5 => {
                last = e;
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

fn post_once(agent: &ureq::Agent, url: &str, body: &str, expect: usize) -> Result<Vec<bool>, String> {
    let response = agent
        .post(url)
        .set("Content-Type", "application/json")
        .send_string(body);
    let text = match response {
        Ok(resp) => resp.into_string().map_err(|e| e.to_string())?,
        Err(ureq::Error::Status(code, resp)) => {
            let detail = resp.into_string().unwrap_or_default();
            if code == 429 {
                return Err(format!("429 {detail}"));
            }
            return Err(format!("hutch HTTP {code}: {detail}"));
        }
        Err(e) => return Err(format!("hutch transport: {e}")),
    };
    let values: Vec<bool> = serde_json::from_str(&text).map_err(|e| format!("hutch decode: {e}: {text}"))?;
    if values.len() != expect {
        return Err(format!(
            "hutch returned {} results for {expect} items",
            values.len()
        ));
    }
    Ok(values)
}

fn pg(message: impl Into<String>) -> Box<PgError> {
    Box::new(PgError::error(message))
}
