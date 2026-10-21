fn main() {
    let port = std::env::args().nth(1).unwrap_or_else(|| {
        std::env::var("HUTCH_PORT").unwrap_or_else(|_| "8098".into())
    });
    let addr = if port.contains(':') {
        port
    } else {
        format!("127.0.0.1:{port}")
    };
    let cfg = rvbbit_hutch_lite::Config::from_env();
    // Never print the API key. Presence only.
    eprintln!(
        "hutch_lite listening on {addr} model={} openai_base={} api_key={}",
        cfg.default_model,
        cfg.openai_base,
        if cfg.api_key.is_some() { "set" } else { "missing" }
    );
    if let Err(e) = rvbbit_hutch_lite::serve(&addr, cfg) {
        eprintln!("hutch_lite: {e}");
        std::process::exit(1);
    }
}
