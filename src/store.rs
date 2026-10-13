use chrono::{DateTime, Datelike, SecondsFormat, Utc};
use serde_json::Value;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

pub type Res<T> = Result<T, Box<dyn std::error::Error>>;

pub fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("AI_USAGE_STATE") {
        return PathBuf::from(dir);
    }
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(|| home().join(".local/state"))
        .join("ai-usage")
}

pub fn listen_addr() -> String {
    std::env::var("AI_USAGE_LISTEN").unwrap_or_else(|_| "127.0.0.1:4318".into())
}

pub fn logs_endpoint() -> String {
    format!("http://{}/v1/logs", listen_addr())
}

/// Finds a CLI on PATH, then in the places its installers usually put it
/// (background services often start with a minimal PATH).
pub fn find_tool(name: &str) -> Option<PathBuf> {
    let on_path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|dir| dir.join(name));
    let usual = [".local/bin", ".claude/local", ".npm-global/bin", ".bun/bin"]
        .iter()
        .map(|d| home().join(d).join(name))
        .chain(
            ["/opt/homebrew/bin", "/usr/local/bin"]
                .iter()
                .map(|d| PathBuf::from(d).join(name)),
        );
    on_path.chain(usual).find(|p| p.is_file())
}

pub fn fmt_ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, false)
}

pub fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc))
}

pub fn read_since(name: &str) -> Option<DateTime<Utc>> {
    parse_ts(fs::read_to_string(state_dir().join(name)).ok()?.trim())
}

pub fn month_file(t: DateTime<Utc>) -> String {
    format!("requests-{:04}-{:02}.jsonl", t.year(), t.month())
}

pub fn append(name: &str, rows: &[Value]) -> std::io::Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let mut buf = String::new();
    for r in rows {
        buf.push_str(&r.to_string());
        buf.push('\n');
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(state_dir().join(name))?
        .write_all(buf.as_bytes())
}

pub fn read_jsonl(prefix: &str) -> Vec<Value> {
    let mut paths: Vec<PathBuf> = fs::read_dir(state_dir())
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(prefix) && n.ends_with(".jsonl"))
        })
        .collect();
    paths.sort();
    let mut rows = Vec::new();
    for p in paths {
        let Ok(f) = fs::File::open(&p) else { continue };
        for line in BufReader::new(f).lines().map_while(Result::ok) {
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                rows.push(v);
            }
        }
    }
    rows
}

pub fn num(v: &Value) -> f64 {
    match v {
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        Value::String(s) => s.parse().unwrap_or(0.0),
        _ => 0.0,
    }
}
