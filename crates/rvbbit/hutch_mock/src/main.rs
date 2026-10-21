fn main() {
    let port = std::env::args().nth(1).unwrap_or_else(|| {
        std::env::var("HUTCH_PORT").unwrap_or_else(|_| "8099".into())
    });
    let addr = if port.contains(':') {
        port
    } else {
        format!("127.0.0.1:{port}")
    };
    let cfg = hutch_mock::Config::from_env();
    eprintln!(
        "hutch_mock listening on {addr} latency_ms={} fail_every={} timeout_every={}",
        cfg.latency.as_millis(),
        cfg.fail_every,
        cfg.timeout_every
    );
    if let Err(e) = hutch_mock::serve(&addr, cfg) {
        eprintln!("hutch_mock: {e}");
        std::process::exit(1);
    }
}
