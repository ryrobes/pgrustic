fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--mock-stdio") => {
            if let Err(e) = hutch_mcp::mock_stdio() {
                eprintln!("hutch-mcp mock stdio: {e}");
                std::process::exit(1);
            }
        }
        Some("--mock-http") => {
            let addr = args.next().unwrap_or_else(|| "127.0.0.1:8120".into());
            if let Err(e) = hutch_mcp::mock_http(&addr) {
                eprintln!("hutch-mcp mock http: {e}");
                std::process::exit(1);
            }
        }
        Some(addr) => serve(addr),
        None => {
            let port = std::env::var("HUTCH_MCP_PORT").unwrap_or_else(|_| "8111".into());
            let addr = if port.contains(':') {
                port
            } else {
                format!("127.0.0.1:{port}")
            };
            serve(&addr);
        }
    }
}

fn serve(addr: &str) {
    eprintln!("hutch-mcp listening on {addr}");
    if let Err(e) = hutch_mcp::serve(addr) {
        eprintln!("hutch-mcp: {e}");
        std::process::exit(1);
    }
}
