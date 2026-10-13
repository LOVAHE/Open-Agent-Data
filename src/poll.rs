use crate::coverage::{Tool, covered};
use crate::store::{append, find_tool, fmt_ts, num, state_dir};
use chrono::Utc;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const INTERVAL: Duration = Duration::from_secs(300);
const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const COMMUNITY_INTERVAL: Duration = Duration::from_secs(3600);

pub fn run_forever() {
    let mut bench = BenchJob::default();
    let mut community = Community::default();
    loop {
        let started = Instant::now();
        poll_once();
        bench.tick();
        community.tick();
        std::thread::sleep(INTERVAL.saturating_sub(started.elapsed()));
    }
}

/// Hourly upload of speed points and the private snapshot, once logged in.
#[derive(Default)]
struct Community {
    last: Option<Instant>,
}

impl Community {
    fn tick(&mut self) {
        if self.last.is_some_and(|t| t.elapsed() < COMMUNITY_INTERVAL) {
            return;
        }
        let Some(account) = crate::account::Account::load() else {
            return;
        };
        self.last = Some(Instant::now());
        if let Err(e) = crate::share::upload(&account) {
            eprintln!("share: {e}");
        }
        if let Err(e) = crate::link::upload(&account) {
            eprintln!("private page: {e}");
        }
    }
}

/// Scheduled benchmark rounds spend quota, so they only run when the
/// service is started with AI_USAGE_BENCH=on.
#[derive(Default)]
struct BenchJob {
    child: Option<Child>,
    spawned: Option<Instant>,
}

impl BenchJob {
    fn tick(&mut self) {
        if std::env::var("AI_USAGE_BENCH").as_deref() != Ok("on") {
            return;
        }
        if let Some(c) = self.child.as_mut() {
            if let Ok(None) = c.try_wait() {
                return;
            }
            self.child = None;
        }
        if self.spawned.is_some_and(|t| t.elapsed() < crate::bench::INTERVAL) {
            return;
        }
        let due = chrono::Duration::from_std(crate::bench::INTERVAL).unwrap();
        if crate::bench::last_start().is_some_and(|t| Utc::now() - t < due) {
            return;
        }
        let Ok(exe) = std::env::current_exe() else {
            return eprintln!("bench: cannot locate own binary");
        };
        let mut cmd = Command::new(exe);
        cmd.arg("bench").current_dir(state_dir()).stdin(Stdio::null());
        match cmd.spawn() {
            Ok(c) => {
                self.child = Some(c);
                self.spawned = Some(Instant::now());
            }
            Err(e) => eprintln!("bench: spawn failed: {e}"),
        }
    }
}

pub fn poll_once() {
    let rows: Vec<Value> = [poll_claude(), poll_codex()].into_iter().flatten().collect();
    if let Err(e) = append("usage.jsonl", &rows) {
        eprintln!("write usage.jsonl: {e}");
    }
}

fn run(mut cmd: Command, timeout: Duration) -> Option<String> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| eprintln!("spawn {:?}: {e}", cmd.get_program()))
        .ok()?;
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(200)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                eprintln!("{:?} timed out", cmd.get_program());
                return None;
            }
        }
    }
    let mut out = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut out).ok()?;
    Some(out)
}

fn poll_claude() -> Option<Value> {
    let mut cmd = Command::new(find_tool("claude")?);
    cmd.args(["-p", "/usage", "--no-session-persistence"])
        .current_dir(state_dir());
    for (k, _) in std::env::vars() {
        if k.starts_with("CLAUDE_CODE_") || k.starts_with("OTEL_") {
            cmd.env_remove(k);
        }
    }
    let out = run(cmd, Duration::from_secs(120))?;
    let mut row = json!({"ts": fmt_ts(Utc::now()), "tool": "claude", "covered": covered(Tool::Claude)});
    let mut found = false;
    for line in out.lines() {
        if let Some((key, pct, resets)) = parse_usage_line(line.trim()) {
            row[key] = json!({"pct": pct, "resets": resets});
            found = true;
        }
    }
    if !found {
        row["raw"] = json!(out.trim().chars().take(500).collect::<String>());
    }
    Some(row)
}

pub fn parse_usage_line(line: &str) -> Option<(String, f64, Option<String>)> {
    let (label, rest) = line.strip_prefix("Current ")?.split_once(':')?;
    let key = if label == "session" {
        "five_hour".to_string()
    } else {
        let inner = label.strip_prefix("week (")?.strip_suffix(')')?;
        format!("week_{}", inner.to_lowercase().replace(' ', "_"))
    };
    let (pct, resets) = match rest.split_once('·') {
        Some((p, r)) => (p, r.trim().strip_prefix("resets ").map(str::to_string)),
        None => (rest, None),
    };
    let pct = pct.trim().strip_suffix("% used")?.trim().parse().ok()?;
    Some((key, pct, resets))
}

fn poll_codex() -> Option<Value> {
    let result = codex_rpc("account/rateLimits/read")?;
    let rl = &result["rateLimits"];
    let mut row = json!({"ts": fmt_ts(Utc::now()), "tool": "codex", "covered": covered(Tool::Codex)});
    if let Some(plan) = rl["planType"].as_str() {
        row["plan"] = json!(plan);
    }
    for w in [&rl["primary"], &rl["secondary"]] {
        if w.is_null() {
            continue;
        }
        let mins = num(&w["windowDurationMins"]) as i64;
        let key = match mins {
            300 => "five_hour".to_string(),
            10080 => "week".to_string(),
            m => format!("window_{m}m"),
        };
        row[key] = json!({"pct": num(&w["usedPercent"]), "reset_at": num(&w["resetsAt"]) as i64, "window_mins": mins});
    }
    Some(row)
}

/// One request to a short-lived `codex app-server` over stdio. Read-only
/// account calls like this one never reach a model.
fn codex_rpc(method: &str) -> Option<Value> {
    let mut child = Command::new(find_tool("codex")?)
        .arg("app-server")
        .current_dir(state_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| eprintln!("spawn codex app-server: {e}"))
        .ok()?;
    let stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(msg) = serde_json::from_str::<Value>(&line)
                && msg["id"] == json!(2)
            {
                let _ = tx.send(msg);
                return;
            }
        }
    });
    let client = json!({"name": "ai-usage", "title": "ai-usage", "version": env!("CARGO_PKG_VERSION")});
    let requests = [
        json!({"id": 1, "method": "initialize", "params": {"clientInfo": client}}),
        json!({"method": "initialized"}),
        json!({"id": 2, "method": method}),
    ];
    let mut stdin = child.stdin.take()?;
    let sent = requests.iter().all(|r| writeln!(stdin, "{r}").is_ok());
    let reply = if sent { rx.recv_timeout(RPC_TIMEOUT).ok() } else { None };
    drop(stdin);
    let deadline = Instant::now() + Duration::from_secs(5);
    while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    match reply {
        Some(msg) if msg.get("error").is_none() => msg.get("result").cloned(),
        Some(msg) => {
            eprintln!("codex {method}: {}", msg["error"]);
            None
        }
        None => {
            eprintln!("codex {method}: no reply");
            None
        }
    }
}
