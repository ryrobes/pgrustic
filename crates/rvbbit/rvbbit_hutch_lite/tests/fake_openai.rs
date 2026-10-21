//! Hutch-lite against a local OpenAI-compatible fake. No network provider.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rvbbit_contract::{
    client_cache_key_hex, BatchRequest, BatchResponse, CacheKeyParts, ItemRequest, Operation,
    CONTRACT_VERSION,
};
use rvbbit_hutch_lite::Config;

struct Harness {
    hutch: String,
    provider_hits: Arc<AtomicU64>,
}

fn harness(api_key: Option<&str>) -> Harness {
    let fake = TcpListener::bind("127.0.0.1:0").unwrap();
    let fake_port = fake.local_addr().unwrap().port();
    let hits = Arc::new(AtomicU64::new(0));
    let hits2 = Arc::clone(&hits);
    thread::spawn(move || serve_fake(fake, hits2));

    let hutch_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let hutch_port = hutch_listener.local_addr().unwrap().port();
    let cfg = Config {
        openai_base: format!("http://127.0.0.1:{fake_port}/v1"),
        api_key: api_key.map(|s| s.to_string()),
        default_model: "openai/gpt-4o-mini".into(),
        timeout: Duration::from_secs(5),
        concurrency: 4,
        tenant: "local".into(),
        default_ttl: Duration::from_secs(60),
        expect_auth: None,
    };
    thread::spawn(move || {
        let _ = rvbbit_hutch_lite::serve_listener(hutch_listener, cfg);
    });
    Harness {
        hutch: format!("http://127.0.0.1:{hutch_port}"),
        provider_hits: hits,
    }
}

fn serve_fake(listener: TcpListener, hits: Arc<AtomicU64>) {
    for conn in listener.incoming() {
        let Ok(mut stream) = conn else { continue };
        let hits = Arc::clone(&hits);
        thread::spawn(move || {
            let mut buf = Vec::new();
            let mut tmp = [0u8; 2048];
            let header_end;
            loop {
                let Ok(n) = stream.read(&mut tmp) else { return };
                if n == 0 {
                    return;
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    header_end = i;
                    break;
                }
            }
            let header = String::from_utf8_lossy(&buf[..header_end]).to_string();
            let mut len = 0usize;
            let mut auth = String::new();
            for line in header.lines().skip(1) {
                if let Some((n, v)) = line.split_once(':') {
                    if n.eq_ignore_ascii_case("content-length") {
                        len = v.trim().parse().unwrap_or(0);
                    } else if n.eq_ignore_ascii_case("authorization") {
                        auth = v.trim().to_string();
                    }
                }
            }
            let mut body = buf[header_end + 4..].to_vec();
            while body.len() < len {
                let Ok(n) = stream.read(&mut tmp) else { break };
                if n == 0 {
                    break;
                }
                body.extend_from_slice(&tmp[..n]);
            }
            body.truncate(len);
            hits.fetch_add(1, Ordering::SeqCst);
            let text = String::from_utf8_lossy(&body);
            if text.contains("SLOW") {
                thread::sleep(Duration::from_millis(250));
            }
            let content = if text.contains("BAD_JSON") {
                "not json at all".to_string()
            } else if text.contains("JSON_CASE") {
                "{\"creature\":\"sasquatch\",\"confidence\":0.8}".into()
            } else if text.contains("FALSE_CASE") {
                "false".into()
            } else if text.contains("TRUE_CASE") || text.contains("SLOW") {
                "true".into()
            } else {
                "false".into()
            };
            // Echo the bearer so a redact test can prove the key does not
            // come back out when we force an error via the marker LEAK.
            let (status, payload) = if text.contains("LEAK") {
                (
                    500,
                    format!("{{\"error\":{{\"message\":\"echo {auth}\"}}}}"),
                )
            } else {
                (
                    200,
                    format!(
                        "{{\"choices\":[{{\"message\":{{\"content\":{}}}}}]}}",
                        serde_json::to_string(&content).unwrap()
                    ),
                )
            };
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
            let _ = stream.write_all(resp.as_bytes());
        });
    }
}

fn item(input: &str, criterion: Option<&str>) -> ItemRequest {
    let inputs = match criterion {
        Some(c) => serde_json::json!({"input": input, "criterion": c}),
        None => serde_json::json!({"input": input}),
    };
    let cache_key = client_cache_key_hex(&CacheKeyParts {
        fn_name: "demo",
        fn_version: 1,
        system_prompt: "",
        user_prompt: "",
        return_schema: "text",
        model: "openai/gpt-4o-mini",
        temperature: 0.0,
        max_tokens: Some(32),
        take_index: 0,
        seed: "",
        inputs: &inputs,
    });
    ItemRequest {
        key: "0".into(),
        cache_key,
        inputs,
        criterion: criterion.map(|s| s.to_string()),
        system: None,
        prompt: None,
    }
}

fn post(url: &str, mode: &str, req: &BatchRequest) -> BatchResponse {
    let body = serde_json::to_string(req).unwrap();
    let resp = ureq::post(url)
        .set("Content-Type", "application/json")
        .set("X-Rvbbit-Cache", &format!("mode={mode}"))
        .send_string(&body)
        .unwrap();
    serde_json::from_str(&resp.into_string().unwrap()).unwrap()
}

fn predict(input: &str, criterion: &str) -> BatchRequest {
    let mut it = item(input, Some(criterion));
    it.key = "0".into();
    BatchRequest {
        contract: CONTRACT_VERSION.into(),
        operation: Operation::Predict,
        model: "openai/gpt-4o-mini".into(),
        temperature: 0.0,
        max_tokens: Some(16),
        items: vec![it],
    }
}

#[test]
fn predict_extract_cache_and_errors() {
    let h = harness(Some("test-key"));
    let yes = post(&format!("{}/v1/predict", h.hutch), "read-write", &predict("TRUE_CASE", "biped"));
    assert_eq!(yes.outputs[0].value, Some(serde_json::json!(true)));
    let no = post(&format!("{}/v1/predict", h.hutch), "read-write", &predict("FALSE_CASE", "biped"));
    assert_eq!(no.outputs[0].value, Some(serde_json::json!(false)));
    assert_eq!(h.provider_hits.load(Ordering::SeqCst), 2);

    let again = post(&format!("{}/v1/predict", h.hutch), "read-write", &predict("TRUE_CASE", "biped"));
    assert_eq!(again.outputs[0].value, Some(serde_json::json!(true)));
    assert_eq!(h.provider_hits.load(Ordering::SeqCst), 2, "second predict is a cache hit");

    let mut extract = predict("JSON_CASE", "ignored");
    extract.operation = Operation::Extract;
    extract.items[0].key = "e".into();
    let got = post(&format!("{}/v1/extract", h.hutch), "read-write", &extract);
    assert_eq!(got.outputs[0].value.as_ref().unwrap()["creature"], "sasquatch");

    let mut bad = extract.clone();
    bad.items[0] = item("BAD_JSON", None);
    bad.items[0].key = "bad".into();
    bad.items.insert(0, extract.items[0].clone());
    let mixed = post(&format!("{}/v1/extract", h.hutch), "off", &bad);
    assert!(mixed.outputs[0].error.is_none(), "sibling still succeeds");
    assert_eq!(mixed.outputs[1].error.as_ref().unwrap().code, "parse");

    let embed = post(&format!("{}/v1/embed", h.hutch), "off", &predict("TRUE_CASE", "x"));
    assert_eq!(embed.outputs[0].error.as_ref().unwrap().code, "unsupported");

    let before = h.provider_hits.load(Ordering::SeqCst);
    let _ = post(&format!("{}/v1/predict", h.hutch), "off", &predict("TRUE_CASE", "biped"));
    let _ = post(&format!("{}/v1/predict", h.hutch), "off", &predict("TRUE_CASE", "biped"));
    assert_eq!(h.provider_hits.load(Ordering::SeqCst), before + 2, "mode=off does not cache");
}

#[test]
fn single_flight_shares_one_provider_call() {
    let h = harness(Some("test-key"));
    let req = predict("SLOW", "biped");
    let url = format!("{}/v1/predict", h.hutch);
    let a = thread::spawn({
        let url = url.clone();
        let req = req.clone();
        move || post(&url, "read-write", &req)
    });
    let b = thread::spawn(move || post(&url, "read-write", &req));
    let ra = a.join().unwrap();
    let rb = b.join().unwrap();
    assert_eq!(ra.outputs[0].value, Some(serde_json::json!(true)));
    assert_eq!(rb.outputs[0].value, Some(serde_json::json!(true)));
    assert_eq!(h.provider_hits.load(Ordering::SeqCst), 1);
    let stats = ureq::get(&format!("{}/stats", h.hutch)).call().unwrap().into_string().unwrap();
    assert!(stats.contains("\"single_flight_waits\":1") || stats.contains("\"provider_calls\":1"), "{stats}");
}

#[test]
fn upstream_error_does_not_echo_the_api_key() {
    let h = harness(Some("super-secret-key"));
    let mut req = predict("LEAK", "biped");
    req.items[0] = item("LEAK", Some("biped"));
    let body = serde_json::to_string(&req).unwrap();
    let resp = ureq::post(&format!("{}/v1/predict", h.hutch))
        .set("Content-Type", "application/json")
        .set("X-Rvbbit-Cache", "mode=off")
        .send_string(&body)
        .unwrap()
        .into_string()
        .unwrap();
    assert!(!resp.contains("super-secret-key"), "{resp}");
    assert!(resp.contains("[redacted]"), "{resp}");
}

#[test]
fn missing_key_is_a_config_error_without_calling_upstream() {
    let h = harness(None);
    let body = serde_json::to_string(&predict("TRUE_CASE", "biped")).unwrap();
    let err = ureq::post(&format!("{}/v1/predict", h.hutch))
        .set("Content-Type", "application/json")
        .send_string(&body)
        .unwrap_err();
    let ureq::Error::Status(500, resp) = err else { panic!("{err}"); };
    let text = resp.into_string().unwrap();
    assert!(text.contains("upstream api key is not set"), "{text}");
    assert_eq!(h.provider_hits.load(Ordering::SeqCst), 0);
}

/// One real OpenRouter call. Opt in with `cargo test -p rvbbit_hutch_lite -- --ignored`.
/// Skips when `OPENROUTER_API_KEY` is absent. Does not print the key.
#[test]
#[ignore]
fn live_openrouter_means() {
    let key = match std::env::var("OPENROUTER_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => {
            eprintln!("skip live_openrouter_means: OPENROUTER_API_KEY absent");
            return;
        }
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let cfg = Config::from_env();
    assert!(cfg.api_key.is_some());
    thread::spawn(move || {
        let _ = rvbbit_hutch_lite::serve_listener(listener, cfg);
    });
    let req = predict("A large hairy biped crossed the logging road.", "a large biped was seen");
    let body = serde_json::to_string(&req).unwrap();
    let resp = ureq::post(&format!("http://127.0.0.1:{port}/v1/predict"))
        .set("Content-Type", "application/json")
        .set("X-Rvbbit-Cache", "mode=off")
        .send_string(&body);
    match resp {
        Ok(resp) => {
            let text = resp.into_string().unwrap();
            assert!(!text.contains(&key), "response echoed the api key");
            let parsed: BatchResponse = serde_json::from_str(&text).unwrap();
            assert!(parsed.outputs[0].error.is_none(), "{text}");
            assert_eq!(parsed.outputs[0].value, Some(serde_json::json!(true)));
        }
        Err(ureq::Error::Status(_, resp)) => {
            let text = resp.into_string().unwrap_or_default();
            assert!(!text.contains(&key), "error echoed the api key");
            panic!("live call failed: {text}");
        }
        Err(e) => panic!("live transport: {e}"),
    }
}
