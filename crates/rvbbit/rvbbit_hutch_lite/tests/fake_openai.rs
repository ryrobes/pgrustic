//! Hutch-lite against a local OpenAI-compatible fake. No network provider.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rvbbit_contract::{
    client_cache_key_hex, BatchRequest, BatchResponse, CacheKeyParts, DecisionStack, ItemRequest,
    Operation, CONTRACT_VERSION,
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
    let openai_base = format!("http://127.0.0.1:{fake_port}/v1");
    let cfg = Config {
        decisions_url: rvbbit_hutch_lite::decisions_url_from_base(&openai_base),
        openai_base,
        api_key: api_key.map(|s| s.to_string()),
        default_model: "openai/gpt-4o-mini".into(),
        decision_model: "typesafe/jev-1.14".into(),
        decision_takes: 1,
        decision_combine: "mean".into(),
        decision_threshold: 0.5,
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
            let request_line = header.lines().next().unwrap_or("").to_string();
            // A generation lookup is accounting, not an inference call.
            // Still counted here so a test can see that the fallback fired.
            hits.fetch_add(1, Ordering::SeqCst);
            if request_line.starts_with("GET ") && request_line.contains("/generation") {
                let payload = if request_line.contains("id=gen-nocost-1") {
                    r#"{"data":{"id":"gen-nocost-1","model":"openai/gpt-4o-mini","total_cost":0.000009,"tokens_prompt":50,"tokens_completion":10,"usage":0.000009}}"#
                } else {
                    r#"{"error":{"message":"generation not found"}}"#
                };
                let status = if request_line.contains("id=gen-nocost-1") { 200 } else { 404 };
                let resp = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = stream.write_all(resp.as_bytes());
                return;
            }
            let text = String::from_utf8_lossy(&body);
            if text.contains("SLOW") {
                thread::sleep(Duration::from_millis(250));
            }
            // Echo the bearer so a redact test can prove the key does not
            // come back out when we force an error via the marker LEAK.
            let (status, payload) = fake_payload(&header, &text, &auth);
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
        labels: None,
        label_descriptions: None,
        node: None,
        extract_mode: None,
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

fn fake_payload(header: &str, text: &str, auth: &str) -> (u16, String) {
    if text.contains("LEAK") {
        return (500, format!("{{\"error\":{{\"message\":\"echo {auth}\"}}}}"));
    }
    let decisions = header.lines().next().unwrap_or("").contains("decisions");
    if decisions {
        let payload = if text.contains("\"type\":\"choice\"") {
            match choice_json(text) {
                Ok(body) => body,
                Err(message) => {
                    return (
                        400,
                        format!(r#"{{"error":{{"message":"{message}"}}}}"#),
                    );
                }
            }
        } else {
            if let Err(message) = check_noul(text) {
                return (
                    400,
                    format!(r#"{{"error":{{"message":"{message}"}}}}"#),
                );
            }
            format!(
                r#"{{"answers":{{"matches":{{"type":"noul","noul":{}}}}}}}"#,
                noul_for(text)
            )
        };
        return (200, decorate_decision(&payload, text));
    }
    let content = if text.contains("BAD_JSON") {
        "not json at all".to_string()
    } else if text.contains("JSON_CASE") {
        "{\"creature\":\"sasquatch\",\"confidence\":0.8}".into()
    } else if text.contains("PLAIN_CASE") {
        "sasquatch".into()
    } else if text.contains("FREEFORM") {
        "forest".into()
    } else if text.contains("FALSE_CASE") {
        "false".into()
    } else if text.contains("TRUE_CASE") || text.contains("SLOW") {
        "true".into()
    } else {
        "false".into()
    };
    (200, chat_body(&content, text))
}

fn requested_model(text: &str) -> String {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(|s| s.to_string()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "openai/gpt-4o-mini".into())
}

/// Decisions `usage.cost` is USD. 80 input tokens at $0.042 / 1M.
fn decorate_decision(body: &str, request: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(body) else {
        return body.to_string();
    };
    let model = requested_model(request);
    if request.contains("NOCOST") {
        value["id"] = serde_json::json!("gen-nocost-1");
        value["model"] = serde_json::json!(model);
    } else {
        value["id"] = serde_json::json!("gen-dec-1");
        value["model"] = serde_json::json!(model);
        value["usage"] = serde_json::json!({
            "input_tokens": 80,
            "output_tokens": 4,
            "cost": 0.00000336
        });
    }
    value.to_string()
}

/// Chat `usage.cost` is the account charge. 100 prompt tokens at $0.15 / 1M
/// plus 20 completion tokens at $0.60 / 1M.
fn chat_body(content: &str, request: &str) -> String {
    let model = requested_model(request);
    let mut value = serde_json::json!({
        "id": if request.contains("NOCOST") { "gen-nocost-1" } else { "gen-chat-1" },
        "model": model,
        "choices": [{"message": {"content": content}}],
    });
    if !request.contains("NOCOST") {
        value["usage"] = serde_json::json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "total_tokens": 120,
            "cost": 0.000027
        });
    }
    value.to_string()
}

fn near(got: Option<f64>, expect: f64) -> bool {
    got.is_some_and(|n| (n - expect).abs() < 1e-12)
}

fn check_noul(text: &str) -> Result<(), String> {
    let body: serde_json::Value = serde_json::from_str(text).map_err(|e| format!("noul body is not json: {e}"))?;
    let state = body.get("state").ok_or("noul question has no state")?;
    if !state.is_string() {
        return Err("noul state must be the input string".into());
    }
    if body.pointer("/questions/matches/criteria").is_some() {
        return Err("noul criteria must be omitted".into());
    }
    let instructions = body
        .pointer("/questions/matches/instructions")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if instructions.is_empty() {
        return Err("noul instructions are empty".into());
    }
    Ok(())
}

fn noul_for(text: &str) -> &'static str {
    if text.contains("ALIAS") {
        if text.contains("~typesafe/jev-latest") && !text.contains("typesafe/jev-1.14") {
            "0.75"
        } else {
            "0.0"
        }
    } else if text.contains("MOCKMODEL") {
        if text.contains("typesafe/jev-1.14") { "0.75" } else { "0.0" }
    } else if text.contains("typesafe/high") {
        "0.75"
    } else if text.contains("typesafe/low") || text.contains("typesafe/repeat") {
        "0.25"
    } else if text.contains("FALSE_CASE") {
        "0.25"
    } else if text.contains("TRUE_CASE") || text.contains("SLOW") {
        "0.75"
    } else {
        "0.25"
    }
}

fn choice_json(text: &str) -> Result<String, String> {
    let body: serde_json::Value = serde_json::from_str(text).map_err(|e| format!("choice body is not json: {e}"))?;
    let criteria = body
        .pointer("/questions/label/criteria")
        .ok_or("choice question has no criteria")?;
    let Some(map) = criteria.as_object() else {
        return Err("questions.label.criteria must be an object".into());
    };
    if map.values().any(|v| !v.is_string()) {
        return Err("questions.label.criteria values must be strings".into());
    }
    if text.contains("CUSTOMDESC") {
        let got = map.get("bigfoot").and_then(|v| v.as_str()).unwrap_or("");
        if got != "CUSTOMDESC" {
            return Err(format!("bigfoot description was {got}"));
        }
    }
    if text.contains("typesafe/high") {
        return Ok(r#"{"answers":{"label":{"type":"choice","choice":"bigfoot","probabilities":{"bigfoot":1.0,"known animal":0.0,"unclear":0.0},"confidence":0.5}}}"#.into());
    }
    if text.contains("typesafe/low") {
        return Ok(r#"{"answers":{"label":{"type":"choice","choice":"known animal","probabilities":{"bigfoot":0.0,"known animal":1.0,"unclear":0.0},"confidence":0.5}}}"#.into());
    }
    if text.contains("STRAY") {
        return Ok(r#"{"answers":{"label":{"type":"choice","choice":"cryptid","probabilities":{"bigfoot":0.25,"known animal":0.5,"unclear":0.25,"cryptid":1.0},"confidence":0.5}}}"#.into());
    }
    if text.contains("CONFONLY") {
        return Ok(r#"{"answers":{"label":{"type":"choice","choice":"unclear","confidence":0.5}}}"#.into());
    }
    let bigfoot = map.get("bigfoot").and_then(|v| v.as_str()).unwrap_or("");
    if bigfoot != "bigfoot" && !text.contains("CUSTOMDESC") {
        return Err(format!("default bigfoot description was {bigfoot}"));
    }
    let state = body.get("state").ok_or("choice question has no state")?;
    if !state.is_string() {
        return Err("choice state must be the input string".into());
    }
    Ok(r#"{"answers":{"label":{"type":"choice","choice":"bigfoot","probabilities":{"bigfoot":0.75,"known animal":0.25,"unclear":0.0},"confidence":0.5}}}"#.into())
}

fn labels() -> Vec<String> {
    vec!["bigfoot".into(), "known animal".into(), "unclear".into()]
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
        decision: None,
    }
}

#[test]
fn predict_extract_cache_and_errors() {
    let h = harness(Some("test-key"));
    let mut first = predict("TRUE_CASE", "biped");
    first.items[0].node = Some("n1:credible?".into());
    let yes = post(&format!("{}/v1/predict", h.hutch), "read-write", &first);
    assert_eq!(yes.outputs[0].value, Some(serde_json::json!(true)));
    assert_eq!(yes.outputs[0].score, Some(0.75));
    assert!(yes.outputs[0].scores.is_none());
    let call = &yes.outputs[0].calls.as_ref().unwrap()[0];
    assert_eq!(call.op, "predict");
    assert_eq!(call.node.as_deref(), Some("n1:credible?"));
    assert_eq!(call.cache, "miss");
    assert_eq!(call.items, 1);
    assert_eq!(call.tokens_in, Some(80));
    assert_eq!(call.tokens_out, Some(4));
    assert!(near(call.cost_usd, 0.00000336), "{:?}", call.cost_usd);
    assert!(near(call.cost_saved_usd, 0.0));
    assert_eq!(call.generation_id.as_deref(), Some("gen-dec-1"));
    assert_eq!(call.tenant, "local");
    let no = post(&format!("{}/v1/predict", h.hutch), "read-write", &predict("FALSE_CASE", "biped"));
    assert_eq!(no.outputs[0].value, Some(serde_json::json!(false)));
    assert_eq!(no.outputs[0].score, Some(0.25));
    assert_eq!(h.provider_hits.load(Ordering::SeqCst), 2);

    let again = post(&format!("{}/v1/predict", h.hutch), "read-write", &predict("TRUE_CASE", "biped"));
    assert_eq!(again.outputs[0].value, Some(serde_json::json!(true)));
    assert_eq!(h.provider_hits.load(Ordering::SeqCst), 2, "second predict is a cache hit");
    let hit = &again.outputs[0].calls.as_ref().unwrap()[0];
    assert_eq!(hit.cache, "hit");
    assert!(near(hit.cost_usd, 0.0));
    assert!(near(hit.cost_saved_usd, 0.00000336), "{:?}", hit.cost_saved_usd);
    assert!(hit.provider_latency_ms.is_none());
    assert_eq!(hit.tokens_in, Some(0));
    let stats = ureq::get(&format!("{}/stats", h.hutch)).call().unwrap().into_string().unwrap();
    let stats: serde_json::Value = serde_json::from_str(&stats).unwrap();
    assert!(near(stats["total_cost_usd"].as_f64(), 0.00000336 * 2.0), "{stats}");
    assert!(near(stats["cost_saved_usd"].as_f64(), 0.00000336), "{stats}");
    assert_eq!(stats["hits"], 1, "contract-cache reuse is a hit: {stats}");
    assert!(stats["latency_by_op_model"].as_array().unwrap().iter().any(|row| {
        row["op"] == "predict"
            && row["calls"].as_u64() == Some(2)
            && row["hits"].as_u64() == Some(1)
    }), "{stats}");
    let recent = ureq::get(&format!("{}/v1/calls?limit=1", h.hutch))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    let recent: serde_json::Value = serde_json::from_str(&recent).unwrap();
    assert_eq!(recent["calls"].as_array().unwrap().len(), 1);
    assert_eq!(recent["calls"][0]["cache"], "hit");

    let mut extract = predict("JSON_CASE", "ignored");
    extract.operation = Operation::Extract;
    extract.items[0].key = "e".into();
    let got = post(&format!("{}/v1/extract", h.hutch), "read-write", &extract);
    assert_eq!(got.outputs[0].value.as_ref().unwrap()["creature"], "sasquatch");

    let mut scalar = predict("PLAIN_CASE", "ignored");
    scalar.operation = Operation::Extract;
    scalar.items[0].key = "s".into();
    scalar.items[0].extract_mode = Some("scalar".into());
    let plain = post(&format!("{}/v1/extract", h.hutch), "read-write", &scalar);
    assert_eq!(plain.outputs[0].value, Some(serde_json::json!("sasquatch")));
    assert!(plain.outputs[0].error.is_none(), "{:?}", plain.outputs[0].error);
    let before_scalar = h.provider_hits.load(Ordering::SeqCst);
    let again = post(&format!("{}/v1/extract", h.hutch), "read-write", &scalar);
    assert_eq!(again.outputs[0].value, Some(serde_json::json!("sasquatch")));
    assert_eq!(
        h.provider_hits.load(Ordering::SeqCst),
        before_scalar,
        "a scalar extract answer is cached"
    );
    let mut as_json = scalar.clone();
    as_json.items[0].extract_mode = None;
    as_json.items[0].cache_key = format!("{}json", as_json.items[0].cache_key);
    let rejected = post(&format!("{}/v1/extract", h.hutch), "off", &as_json);
    assert_eq!(rejected.outputs[0].error.as_ref().unwrap().code, "parse");

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
fn decision_stack_alias_and_classify() {
    let h = harness(Some("test-key"));
    let mut mean = predict("STACK", "biped");
    mean.decision = Some(DecisionStack {
        models: vec!["typesafe/high".into(), "typesafe/low".into()],
        takes: None,
        combine: Some("mean".into()),
        threshold: None,
    });
    let got = post(&format!("{}/v1/predict", h.hutch), "off", &mean);
    assert_eq!(got.outputs[0].value, Some(serde_json::json!(true)));
    assert_eq!(got.outputs[0].score, Some(0.5));
    assert_eq!(h.provider_hits.load(Ordering::SeqCst), 2);

    let mut vote = mean.clone();
    vote.decision.as_mut().unwrap().combine = Some("vote".into());
    vote.decision.as_mut().unwrap().threshold = Some(0.5);
    let tied = post(&format!("{}/v1/decide", h.hutch), "off", &vote);
    assert_eq!(tied.outputs[0].value, Some(serde_json::json!(true)), "tie breaks on the mean");
    assert_eq!(tied.outputs[0].score, Some(0.5));

    vote.decision.as_mut().unwrap().threshold = Some(0.6);
    let below = post(&format!("{}/v1/decide", h.hutch), "off", &vote);
    assert_eq!(below.outputs[0].value, Some(serde_json::json!(false)));
    assert_eq!(below.outputs[0].score, Some(0.5));

    let mut repeats = predict("REPEAT", "biped");
    repeats.decision = Some(DecisionStack {
        models: vec!["typesafe/repeat".into()],
        takes: Some(3),
        combine: Some("mean".into()),
        threshold: Some(0.5),
    });
    let before = h.provider_hits.load(Ordering::SeqCst);
    let thrice = post(&format!("{}/v1/predict", h.hutch), "off", &repeats);
    assert_eq!(thrice.outputs[0].value, Some(serde_json::json!(false)));
    assert_eq!(thrice.outputs[0].score, Some(0.25));
    assert_eq!(h.provider_hits.load(Ordering::SeqCst), before + 3);

    let mut mocked = predict("MOCKMODEL", "biped");
    mocked.model = "mock".into();
    let resolved = post(&format!("{}/v1/predict", h.hutch), "off", &mocked);
    assert_eq!(resolved.outputs[0].score, Some(0.75), "mock resolves to jev-1.14");

    let mut alias = predict("ALIAS", "biped");
    alias.model = "~typesafe/jev-latest".into();
    let passed = post(&format!("{}/v1/predict", h.hutch), "off", &alias);
    assert_eq!(passed.outputs[0].score, Some(0.75), "alias is not rewritten");

    let mut shared = predict("TRUE_CASE", "memo");
    shared.items[0].cache_key = "a".repeat(8);
    let mut other = shared.clone();
    other.items[0].cache_key = "b".repeat(8);
    other.items[0].key = "1".into();
    let hits = h.provider_hits.load(Ordering::SeqCst);
    let first = post(&format!("{}/v1/predict", h.hutch), "read-write", &shared);
    let second = post(&format!("{}/v1/predict", h.hutch), "read-write", &other);
    assert_eq!(
        h.provider_hits.load(Ordering::SeqCst),
        hits + 1,
        "identical decision bodies share one provider call"
    );
    assert_eq!(first.outputs[0].calls.as_ref().unwrap()[0].cache, "miss");
    let memo_hit = &second.outputs[0].calls.as_ref().unwrap()[0];
    assert_eq!(memo_hit.cache, "hit");
    assert!(memo_hit.provider_latency_ms.is_none());
    assert!(near(memo_hit.cost_usd, 0.0));
    assert!(near(memo_hit.cost_saved_usd, 0.00000336), "{:?}", memo_hit.cost_saved_usd);
    let stats = ureq::get(&format!("{}/stats", h.hutch)).call().unwrap().into_string().unwrap();
    let stats: serde_json::Value = serde_json::from_str(&stats).unwrap();
    assert_eq!(stats["hits"], 1, "decision-memo reuse counts as a hit: {stats}");
    assert!(stats["latency_by_op_model"].as_array().unwrap().iter().any(|row| {
        row["op"] == "predict" && row["hits"].as_u64() == Some(1)
    }), "{stats}");

    let mut kind = predict("BIGFOOT tracks", "ignored");
    kind.operation = Operation::Classify;
    kind.model = "mock".into();
    kind.items[0].labels = Some(labels());
    let classified = post(&format!("{}/v1/classify", h.hutch), "off", &kind);
    assert_eq!(classified.outputs[0].value, Some(serde_json::json!("bigfoot")));
    assert_eq!(classified.outputs[0].score, Some(0.75), "winning score is the probability");
    assert_ne!(classified.outputs[0].score, Some(0.5));
    let scores = classified.outputs[0].scores.as_ref().unwrap();
    assert_eq!(scores["known animal"], serde_json::json!(0.25));
    assert_eq!(scores["unclear"], serde_json::json!(0.0));
    assert!(scores.get("cryptid").is_none());

    let mut custom = kind.clone();
    custom.items[0] = item("CUSTOMDESC tracks", None);
    custom.items[0].labels = Some(labels());
    custom.items[0].label_descriptions = Some(vec![
        "CUSTOMDESC".into(),
        "a recognized species".into(),
        "not enough to tell".into(),
    ]);
    let custom_got = post(&format!("{}/v1/classify", h.hutch), "off", &custom);
    assert!(custom_got.outputs[0].error.is_none(), "{:?}", custom_got.outputs[0].error);
    assert_eq!(custom_got.outputs[0].value, Some(serde_json::json!("bigfoot")));

    let mut stray = kind.clone();
    stray.items[0] = item("STRAY", None);
    stray.items[0].labels = Some(labels());
    let stray_got = post(&format!("{}/v1/classify", h.hutch), "off", &stray);
    assert_eq!(stray_got.outputs[0].value, Some(serde_json::json!("known animal")));
    assert_eq!(stray_got.outputs[0].score, Some(0.5));
    assert!(stray_got.outputs[0].scores.as_ref().unwrap().get("cryptid").is_none());

    let mut conf = kind.clone();
    conf.items[0] = item("CONFONLY", None);
    conf.items[0].labels = Some(labels());
    let conf_got = post(&format!("{}/v1/classify", h.hutch), "off", &conf);
    assert_eq!(conf_got.outputs[0].value, Some(serde_json::json!("unclear")));
    assert_eq!(conf_got.outputs[0].score, Some(0.5));

    let mut stacked = kind.clone();
    stacked.items[0] = item("STACKCHOICE", None);
    stacked.items[0].labels = Some(labels());
    stacked.decision = Some(DecisionStack {
        models: vec!["typesafe/high".into(), "typesafe/low".into()],
        takes: Some(1),
        combine: None,
        threshold: None,
    });
    let averaged = post(&format!("{}/v1/classify", h.hutch), "off", &stacked);
    assert_eq!(averaged.outputs[0].value, Some(serde_json::json!("bigfoot")));
    assert_eq!(averaged.outputs[0].score, Some(0.5));
    assert_eq!(
        averaged.outputs[0].scores.as_ref().unwrap()["known animal"],
        serde_json::json!(0.5)
    );

    let mut one = kind.clone();
    one.items[0].labels = Some(vec!["only".into()]);
    let before = h.provider_hits.load(Ordering::SeqCst);
    let bad = post(&format!("{}/v1/classify", h.hutch), "off", &one);
    assert_eq!(bad.outputs[0].error.as_ref().unwrap().code, "invalid");
    assert!(bad.outputs[0].error.as_ref().unwrap().message.contains("at least two"));
    assert_eq!(h.provider_hits.load(Ordering::SeqCst), before);

    let mut free = predict("FREEFORM", "ignored");
    free.operation = Operation::Classify;
    let open = post(&format!("{}/v1/classify", h.hutch), "off", &free);
    assert_eq!(open.outputs[0].value, Some(serde_json::json!("forest")));
    assert!(open.outputs[0].score.is_none());
    assert!(open.outputs[0].scores.is_none());
    let chat = &open.outputs[0].calls.as_ref().unwrap()[0];
    assert_eq!(chat.op, "classify");
    assert_eq!(chat.cache, "miss");
    assert_eq!(chat.tokens_in, Some(100));
    assert_eq!(chat.tokens_out, Some(20));
    assert!(near(chat.cost_usd, 0.000027), "{:?}", chat.cost_usd);
    assert_eq!(chat.generation_id.as_deref(), Some("gen-chat-1"));
}

#[test]
fn missing_inline_cost_uses_the_generation_endpoint() {
    let h = harness(Some("test-key"));
    let mut req = predict("NOCOST", "ignored");
    req.operation = Operation::Extract;
    req.items[0] = item("NOCOST creature", None);
    req.items[0].key = "n".into();
    let got = post(&format!("{}/v1/extract", h.hutch), "off", &req);
    assert!(got.outputs[0].error.is_none(), "{:?}", got.outputs[0].error);
    let call = &got.outputs[0].calls.as_ref().unwrap()[0];
    assert_eq!(call.cache, "miss");
    assert_eq!(call.op, "extract");
    assert!(near(call.cost_usd, 0.000009), "{:?}", call.cost_usd);
    assert_eq!(call.tokens_in, Some(50));
    assert_eq!(call.tokens_out, Some(10));
    assert_eq!(call.generation_id.as_deref(), Some("gen-nocost-1"));
    assert!(call.latency_ms >= call.provider_latency_ms.expect("miss has provider latency"));
    let stats = ureq::get(&format!("{}/stats", h.hutch)).call().unwrap().into_string().unwrap();
    let stats: serde_json::Value = serde_json::from_str(&stats).unwrap();
    assert_eq!(stats["provider_calls"], 1, "generation lookup is not a provider call");
    assert!(near(stats["total_cost_usd"].as_f64(), 0.000009), "{stats}");
    assert_eq!(h.provider_hits.load(Ordering::SeqCst), 2, "inference plus generation GET");
    let contract = ureq::get(&format!("{}/v1/cache/stats", h.hutch))
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert!(!contract.contains("total_cost_usd"), "{contract}");
    assert!(!contract.contains("provider_calls"), "{contract}");
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
    let mut req = predict("A large hairy biped crossed the logging road.", "a large biped was seen");
    // Empty or `mock` selects HUTCH_DECISION_MODEL. A chat slug is not a
    // decision model and would 400 on the Decisions API.
    req.model = "mock".into();
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
            let value = parsed.outputs[0].value.as_ref().expect("value");
            assert!(value.is_boolean(), "{text}");
            let score = parsed.outputs[0].score.expect("score");
            assert!((0.0..=1.0).contains(&score), "{score}");
        }
        Err(ureq::Error::Status(_, resp)) => {
            let text = resp.into_string().unwrap_or_default();
            assert!(!text.contains(&key), "error echoed the api key");
            panic!("live call failed: {text}");
        }
        Err(e) => panic!("live transport: {e}"),
    }
}
