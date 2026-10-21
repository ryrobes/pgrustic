//! Pool against a local mock. The pool reads `RVBBIT_HUTCH_URL` once, so
//! this file is a single test.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

#[test]
fn batch_match_and_rate_limit() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let url = format!("http://127.0.0.1:{port}/v1/means");
    // The pool is process-global and latches the URL on first start.
    // set_var is unsafe because it races other threads; this test is the
    // only thread that reads the variables, and it does so inside start().
    unsafe {
        std::env::set_var("RVBBIT_HUTCH_URL", &url);
        std::env::set_var("RVBBIT_HUTCH_TIMEOUT_MS", "2000");
    }
    thread::spawn(move || {
        let cfg = hutch_mock::Config {
            latency: Duration::from_millis(5),
            fail_every: 2,
            timeout_every: 0,
        };
        let _ = hutch_mock::serve_listener(listener, cfg);
    });

    rvbbit_dispatch::start();
    let yes = rvbbit_dispatch::means_pair("Hello World", "hello").expect("match");
    assert!(yes, "substring match");
    let no = rvbbit_dispatch::means_pair("abc", "zzz").expect("miss");
    assert!(!no, "non-match");

    let stats = http_get(&format!("http://127.0.0.1:{port}/stats"));
    // fail_every=2 rejects every other call, counted before the increment.
    // Each means_pair therefore takes a 429 plus a success: 4 HTTP calls,
    // 2 of which carry a row. The 429s are recorded with 0 rows, so
    // max_batch stays 1.
    assert!(stats.contains("\"calls\":4"), "stats {stats}");
    assert!(stats.contains("\"rows\":2"), "stats {stats}");
    assert!(stats.contains("\"max_batch\":1"), "stats {stats}");
}

fn http_get(url: &str) -> String {
    let rest = url.trim_start_matches("http://");
    let (host, path) = rest.split_once('/').expect("url");
    let mut stream = std::net::TcpStream::connect(host).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("timeout");
    let req = format!("GET /{path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).expect("write");
    let mut buf = String::new();
    stream.read_to_string(&mut buf).expect("read");
    buf.split("\r\n\r\n").nth(1).unwrap_or("").to_string()
}
