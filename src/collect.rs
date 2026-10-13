use crate::store::{Res, append, fmt_ts, month_file, num, parse_ts, state_dir};
use chrono::{DateTime, Duration, TimeZone, Utc};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::io::Read;

const MAX_BODY: u64 = 32 << 20;

#[derive(Default)]
pub struct CodexState {
    pending: HashMap<String, DateTime<Utc>>,
}

pub fn serve() -> Res<()> {
    std::fs::create_dir_all(state_dir())?;
    let addr = crate::store::listen_addr();
    let server = tiny_http::Server::http(&addr).map_err(|e| format!("bind {addr}: {e}"))?;
    eprintln!("listening on {addr}");
    std::thread::spawn(crate::poll::run_forever);
    let mut codex = CodexState::default();
    let json_header: tiny_http::Header = "Content-Type: application/json".parse().unwrap();
    for mut req in server.incoming_requests() {
        let status = match handle(&mut req, &mut codex) {
            Ok(()) => 200,
            Err(e) => {
                eprintln!("rejected {}: {e}", req.url());
                400
            }
        };
        let resp = tiny_http::Response::from_string("{}")
            .with_status_code(status)
            .with_header(json_header.clone());
        let _ = req.respond(resp);
    }
    Ok(())
}

fn handle(req: &mut tiny_http::Request, codex: &mut CodexState) -> Res<()> {
    if *req.method() != tiny_http::Method::Post || !req.url().trim_end_matches('/').ends_with("/v1/logs") {
        return Ok(());
    }
    let gzip = req
        .headers()
        .iter()
        .any(|h| h.field.equiv("Content-Encoding") && h.value.as_str().eq_ignore_ascii_case("gzip"));
    let mut body = Vec::new();
    req.as_reader().take(MAX_BODY).read_to_end(&mut body)?;
    if gzip {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&body[..])
            .take(MAX_BODY * 4)
            .read_to_end(&mut out)?;
        body = out;
    }
    let payload: Value = serde_json::from_slice(&body)?;
    let mut by_month: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for (t, row) in extract(&payload, codex) {
        by_month.entry(month_file(t)).or_default().push(row);
    }
    for (file, rows) in by_month {
        append(&file, &rows)?;
    }
    Ok(())
}

fn attrs(list: &Value) -> Map<String, Value> {
    let mut m = Map::new();
    for a in list.as_array().into_iter().flatten() {
        let Some(k) = a["key"].as_str() else { continue };
        let v = &a["value"];
        let val = ["stringValue", "intValue", "doubleValue", "boolValue"]
            .iter()
            .find_map(|k| v.get(*k).cloned())
            .unwrap_or(Value::Null);
        m.insert(k.to_string(), val);
    }
    m
}

fn text<'a>(a: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    a.get(k).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn int(a: &Map<String, Value>, k: &str) -> i64 {
    a.get(k).map(num).unwrap_or(0.0) as i64
}

fn record_time(lr: &Value, a: &Map<String, Value>) -> DateTime<Utc> {
    if let Some(t) = text(a, "event.timestamp").and_then(parse_ts) {
        return t;
    }
    for k in ["timeUnixNano", "observedTimeUnixNano"] {
        let ns = num(&lr[k]) as i64;
        if ns > 0 {
            return Utc.timestamp_nanos(ns);
        }
    }
    Utc::now()
}

fn extract(payload: &Value, codex: &mut CodexState) -> Vec<(DateTime<Utc>, Value)> {
    let mut out = Vec::new();
    for rl in payload["resourceLogs"].as_array().into_iter().flatten() {
        let res = attrs(&rl["resource"]["attributes"]);
        for sl in rl["scopeLogs"].as_array().into_iter().flatten() {
            for lr in sl["logRecords"].as_array().into_iter().flatten() {
                let a = attrs(&lr["attributes"]);
                let name = text(&a, "event.name")
                    .or_else(|| lr["body"]["stringValue"].as_str())
                    .unwrap_or("");
                let t = record_time(lr, &a);
                let row = match name {
                    "api_request" | "claude_code.api_request" => Some(claude_row(t, &res, &a)),
                    "codex.websocket_request" | "codex.api_request" => {
                        codex_request_start(codex, t, &a);
                        None
                    }
                    "codex.sse_event" => codex_completed(codex, t, &res, &a),
                    _ => None,
                };
                if let Some(row) = row {
                    out.push((t, row));
                }
            }
        }
    }
    out
}

fn claude_row(t: DateTime<Utc>, res: &Map<String, Value>, a: &Map<String, Value>) -> Value {
    let instance = ["service.instance.id", "rc.instance"]
        .iter()
        .find_map(|k| text(res, k).or_else(|| text(a, k)));
    let mut row = json!({
        "ts": fmt_ts(t),
        "tool": "claude",
        "instance": instance.unwrap_or("other"),
        "session": text(a, "session.id"),
        "cost_usd": a.get("cost_usd").map(num).unwrap_or(0.0),
    });
    for k in [
        "input_tokens",
        "output_tokens",
        "cache_read_tokens",
        "cache_creation_tokens",
        "duration_ms",
    ] {
        row[k] = json!(int(a, k));
    }
    if a.contains_key("ttft_ms") {
        row["ttft_ms"] = json!(int(a, "ttft_ms"));
    }
    for k in ["model", "effort", "query_source", "speed", "request_id"] {
        if let Some(v) = text(a, k) {
            row[k] = json!(v);
        }
    }
    row
}

fn codex_request_start(codex: &mut CodexState, t: DateTime<Utc>, a: &Map<String, Value>) {
    let Some(conv) = text(a, "conversation.id") else { return };
    if codex.pending.len() > 10_000 {
        codex.pending.clear();
    }
    codex
        .pending
        .insert(conv.to_string(), t - Duration::milliseconds(int(a, "duration_ms")));
}

fn codex_completed(
    codex: &mut CodexState,
    t: DateTime<Utc>,
    res: &Map<String, Value>,
    a: &Map<String, Value>,
) -> Option<Value> {
    if text(a, "event.kind") != Some("response.completed") || a.contains_key("error.message") {
        return None;
    }
    let conv = text(a, "conversation.id").unwrap_or("");
    let start = codex.pending.remove(conv);
    let output = int(a, "output_token_count");
    if output <= 0 {
        return None;
    }
    let input = int(a, "input_token_count");
    let cached = int(a, "cached_token_count");
    let cache_write = int(a, "cache_write_token_count");
    let mut row = json!({
        "ts": fmt_ts(t),
        "tool": "codex",
        "instance": text(a, "originator").or_else(|| text(res, "service.name")).unwrap_or("codex"),
        "session": conv,
        "cost_usd": 0.0,
        "model": text(a, "model").unwrap_or("?"),
        "input_tokens": (input - cached - cache_write).max(0),
        "output_tokens": output,
        "reasoning_tokens": int(a, "reasoning_token_count"),
        "cache_read_tokens": cached,
        "cache_creation_tokens": cache_write,
    });
    if let Some(e) = text(a, "model_reasoning_effort").or_else(|| text(a, "reasoning_effort")) {
        row["effort"] = json!(e);
    }
    if let Some(s) = start.filter(|s| *s <= t && t - *s < Duration::hours(1)) {
        row["duration_ms"] = json!((t - s).num_milliseconds());
    }
    if a.contains_key("ttft_ms") {
        row["ttft_ms"] = json!(int(a, "ttft_ms"));
    }
    Some(row)
}
