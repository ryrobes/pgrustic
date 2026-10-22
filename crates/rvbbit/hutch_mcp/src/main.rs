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
        Some("--inbound-stdio") => inbound_stdio(),
        Some("--inbound") => {
            let addr = args.next().unwrap_or_else(|| {
                let port = std::env::var("HUTCH_MCP_INBOUND_PORT").unwrap_or_else(|_| "8113".into());
                if port.contains(':') {
                    port
                } else {
                    format!("127.0.0.1:{port}")
                }
            });
            inbound(&addr);
        }
        Some("--issue-key") => {
            let role = args.next().unwrap_or_default();
            let name = args.next().unwrap_or_else(|| "api".into());
            if role.is_empty() {
                eprintln!("usage: hutch-mcp --issue-key ROLE NAME");
                std::process::exit(1);
            }
            match inbound_config("127.0.0.1:0").and_then(|cfg| hutch_mcp::issue_key(&cfg, &role, &name)) {
                Ok(key) => println!("{key}"),
                Err(e) => {
                    eprintln!("hutch-mcp issue-key: {e}");
                    std::process::exit(1);
                }
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

fn inbound(addr: &str) {
    eprintln!("hutch-mcp inbound on {addr}");
    match inbound_config(addr).and_then(|cfg| hutch_mcp::serve_inbound(cfg)) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("hutch-mcp inbound: {e}");
            std::process::exit(1);
        }
    }
}

fn inbound_stdio() {
    match inbound_config("stdio").and_then(|cfg| hutch_mcp::serve_inbound_stdio(cfg)) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("hutch-mcp inbound stdio: {e}");
            std::process::exit(1);
        }
    }
}

fn inbound_config(bind: &str) -> Result<hutch_mcp::InboundConfig, String> {
    hutch_mcp::InboundConfig::from_env(bind.to_string())
}

fn serve(addr: &str) {
    eprintln!("hutch-mcp listening on {addr}");
    if let Err(e) = hutch_mcp::serve(addr) {
        eprintln!("hutch-mcp: {e}");
        std::process::exit(1);
    }
}
