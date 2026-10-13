use crate::report::parse_flag;
use crate::store::{Res, append, find_tool, fmt_ts, logs_endpoint, parse_ts, read_jsonl};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub const FILE: &str = "bench.jsonl";
pub const INTERVAL: Duration = Duration::from_secs(3 * 3600);
// claude-haiku-5-5 works although Claude Code 2.1.285 does not list it yet
// (it logs `unrecognized_model` and its cost basis is "unknown").
pub const CLAUDE_MODELS: [&str; 3] = ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"];
pub const CODEX_MODELS: [&str; 3] = ["gpt-6-astra", "gpt-6.1-sol", "gpt-6-luna"];
pub const EFFORT: &str = "medium";
// A code-writing task: real Claude Code / Codex traffic is mostly code, which
// decodes faster than prose (a story prompt measured Opus at 63 tok/s against
// ~104 organic). Fixed text so runs stay comparable; ~500-700 output tokens.
const PROMPT: &str = "Write a self-contained Python module that implements an LRU cache class with get, put and delete methods, type hints and docstrings, followed by four pytest test functions. Reply with the code only, about 60 lines.";
const SYSTEM_PROMPT: &str = "You are a senior software engineer. Reply with the requested code only.";
// Claude Code switches that keep CLAUDE.md files, auto-memory, skills and
// other scaffolding out of the request: 4.4K -> 0.36K prompt tokens.
const CLAUDE_LEAN_ENV: [&str; 13] = [
    "CLAUDE_CODE_DISABLE_CLAUDE_MDS",
    "CLAUDE_CODE_DISABLE_AUTO_MEMORY",
    "CLAUDE_CODE_DISABLE_BUNDLED_SKILLS",
    "CLAUDE_CODE_DISABLE_CLAUDE_API_SKILL",
    "CLAUDE_CODE_DISABLE_CLAUDE_CODE_SKILL",
    "CLAUDE_CODE_DISABLE_GIT_INSTRUCTIONS",
    "CLAUDE_CODE_DISABLE_ATTACHMENTS",
    "CLAUDE_CODE_DISABLE_WORKFLOWS",
    "CLAUDE_CODE_DISABLE_ARTIFACT",
    "CLAUDE_CODE_DISABLE_EXPLORE_PLAN_AGENTS",
    "CLAUDE_CODE_DISABLE_CRON",
    "CLAUDE_CODE_DISABLE_BACKGROUND_TASKS",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
];
// Codex features whose tools or instructions are not needed for a plain
// answer; with the overrides in codex_run: 21.1K -> 8.9K prompt tokens.
const CODEX_OFF_FEATURES: [&str; 26] = [
    "apps",
    "browser_use",
    "browser_use_external",
    "computer_use",
    "image_generation",
    "multi_agent",
    "multi_agent_v2",
    "plugins",
    "remote_plugin",
    "skill_search",
    "sleep_tool",
    "tool_suggest",
    "goals",
    "hooks",
    "realtime_conversation",
    "mentions_v2",
    "in_app_browser",
    "in_app_chat",
    "in_app_dictation",
    "in_app_local_automation",
    "shell_snapshot",
    "skill_mcp_dependency_install",
    "code_mode_host",
    "shell_tool",
    "unified_exec",
    "request_permissions_tool",
];
const CLAUDE_TIMEOUT: Duration = Duration::from_secs(180);
const CODEX_TIMEOUT: Duration = Duration::from_secs(240);

/// Sessions of benchmark runs; they are measured differently, so they stay
/// out of the community data.
pub fn sessions() -> std::collections::HashSet<String> {
    read_jsonl(FILE)
        .iter()
        .filter_map(|r| Some(r["session"].as_str()?.to_string()))
        .collect()
}

pub fn last_start() -> Option<DateTime<Utc>> {
    read_jsonl(FILE)
        .iter()
        .filter_map(|r| parse_ts(r["ts"].as_str()?))
        .max()
}

struct Outcome {
    ok: bool,
    session: String,
    error: Option<String>,
}

pub fn run(args: &[String]) -> Res<()> {
    let tool = parse_flag(args, "--tool");
    let model = parse_flag(args, "--model");
    if tool.as_deref().is_some_and(|t| t != "claude" && t != "codex") {
        return Err("--tool must be claude or codex".into());
    }
    let mut plan: Vec<(&str, String)> = CLAUDE_MODELS
        .iter()
        .map(|m| ("claude", m.to_string()))
        .chain(CODEX_MODELS.iter().map(|m| ("codex", m.to_string())))
        .filter(|(t, _)| tool.as_deref().is_none_or(|o| o == *t))
        .collect();
    if let Some(m) = model {
        plan.retain(|(_, pm)| *pm == m);
        if plan.is_empty() {
            match tool.as_deref() {
                Some("claude") => plan.push(("claude", m)),
                Some("codex") => plan.push(("codex", m)),
                _ => return Err(format!("{m} is not in the bench list; pass --tool to run it anyway").into()),
            }
        }
    }
    // outside $HOME so no project CLAUDE.md / AGENTS.md is discovered
    let dir = std::env::temp_dir().join(format!("ai-usage-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let instructions = dir.join("instructions.md");
    std::fs::write(&instructions, format!("{SYSTEM_PROMPT}\n"))?;
    for (tool, model) in plan {
        let ts = Utc::now();
        let started = Instant::now();
        let out = match tool {
            "claude" => claude_run(&model, &dir),
            _ => codex_run(&model, &dir, &instructions),
        };
        let secs = (started.elapsed().as_secs_f64() * 10.0).round() / 10.0;
        let mut line =
            json!({"ts": fmt_ts(ts), "tool": tool, "model": model, "session": out.session, "ok": out.ok, "secs": secs});
        if let Some(e) = &out.error {
            line["error"] = json!(e);
        }
        append(FILE, &[line])?;
        println!(
            "bench {tool} {model}: {} in {secs}s",
            out.error.as_deref().unwrap_or("ok")
        );
    }
    let _ = std::fs::remove_file(&instructions);
    let _ = std::fs::remove_dir(&dir);
    Ok(())
}

fn clean_env(cmd: &mut Command) {
    for (k, _) in std::env::vars_os() {
        let name = k.to_string_lossy();
        if name.starts_with("CLAUDE_CODE_") || name.starts_with("OTEL_") {
            cmd.env_remove(&k);
        }
    }
}

fn tool_cmd(name: &str) -> Command {
    Command::new(find_tool(name).unwrap_or_else(|| name.into()))
}

fn claude_cmd(model: &str, session: &str, effort: bool, dir: &Path) -> Command {
    let mut cmd = tool_cmd("claude");
    cmd.args(["-p", PROMPT, "--model", model]);
    if effort {
        cmd.args(["--effort", EFFORT]);
    }
    cmd.args([
        "--system-prompt",
        SYSTEM_PROMPT,
        "--tools",
        "",
        "--strict-mcp-config",
        "--no-session-persistence",
    ])
    .args(["--session-id", session, "--output-format", "json"])
    .current_dir(dir);
    clean_env(&mut cmd);
    for k in CLAUDE_LEAN_ENV {
        cmd.env(k, "1");
    }
    cmd.env("LANG", "C.UTF-8")
        .env("CLAUDE_CODE_ENABLE_TELEMETRY", "1")
        .env("OTEL_METRICS_EXPORTER", "none")
        .env("OTEL_LOGS_EXPORTER", "otlp")
        .env("OTEL_EXPORTER_OTLP_LOGS_PROTOCOL", "http/json")
        .env("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT", logs_endpoint())
        .env("OTEL_RESOURCE_ATTRIBUTES", "service.instance.id=bench");
    cmd
}

fn claude_run(model: &str, dir: &Path) -> Outcome {
    let mut effort = true;
    loop {
        let session = uuid4();
        let res = capture(claude_cmd(model, &session, effort, dir), CLAUDE_TIMEOUT);
        let parsed: Value = serde_json::from_str(res.stdout.trim()).unwrap_or(Value::Null);
        let ok = res.status.is_some_and(|s| s.success()) && parsed["is_error"] == json!(false);
        if ok {
            return Outcome {
                ok,
                session,
                error: None,
            };
        }
        let error = match res.status {
            None => "timeout".to_string(),
            Some(s) => {
                let detail = parsed["result"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| res.stderr.clone());
                short(&format!("{}: {}", exit_text(s), detail))
            }
        };
        let rejected_effort = res.status.is_some() && error.to_lowercase().contains("effort");
        if effort && rejected_effort {
            effort = false;
            continue;
        }
        return Outcome {
            ok,
            session,
            error: Some(error),
        };
    }
}

fn codex_run(model: &str, dir: &Path, instructions: &Path) -> Outcome {
    let mut cmd = tool_cmd("codex");
    // --ignore-user-config drops ~/.codex/config.toml (MCP servers, profiles,
    // [otel]), so telemetry is passed back in with -c.
    cmd.args([
        "exec",
        "--ephemeral",
        "--skip-git-repo-check",
        "--ignore-user-config",
        "-s",
        "read-only",
        "-C",
    ])
    .arg(dir)
    .args(["-m", model, "-c", &format!("model_reasoning_effort=\"{EFFORT}\"")])
    .args([
        "-c",
        &format!(
            "otel.exporter={{otlp-http={{endpoint=\"{}\",protocol=\"json\"}}}}",
            logs_endpoint()
        ),
    ])
    .args([
        "-c",
        "project_doc_max_bytes=0",
        "-c",
        "include_environment_context=false",
    ])
    .args([
        "-c",
        "include_apps_instructions=false",
        "-c",
        "include_permissions_instructions=false",
    ])
    .args([
        "-c",
        "include_collaboration_mode_instructions=false",
        "-c",
        "web_search=\"disabled\"",
        "-c",
        "tools.view_image=false",
    ])
    .args(["-c", &format!("model_instructions_file=\"{}\"", instructions.display())]);
    for f in CODEX_OFF_FEATURES {
        cmd.args(["-c", &format!("features.{f}=false")]);
    }
    cmd.args(["--json", PROMPT]).current_dir(dir);
    clean_env(&mut cmd);
    let res = capture(cmd, CODEX_TIMEOUT);
    let (mut session, mut completed, mut failure) = (String::new(), false, None);
    for line in res.stdout.lines() {
        let Ok(ev) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match ev["type"].as_str().unwrap_or_default() {
            "thread.started" => session = ev["thread_id"].as_str().unwrap_or_default().to_string(),
            "turn.completed" => completed = true,
            "turn.failed" => failure = ev["error"]["message"].as_str().map(str::to_string),
            "error" => failure = failure.or_else(|| ev["message"].as_str().map(str::to_string)),
            _ => {}
        }
    }
    let ok = res.status.is_some_and(|s| s.success()) && completed && failure.is_none();
    let error = (!ok).then(|| match res.status {
        None => "timeout".to_string(),
        Some(s) => short(&format!("{}: {}", exit_text(s), failure.unwrap_or(res.stderr))),
    });
    Outcome { ok, session, error }
}

fn exit_text(s: ExitStatus) -> String {
    s.code().map(|c| format!("exit {c}")).unwrap_or_else(|| "killed".into())
}

fn short(s: &str) -> String {
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    one.chars().take(200).collect()
}

fn uuid4() -> String {
    let mut b = [0u8; 16];
    if std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .is_err()
    {
        let n = Utc::now().timestamp_nanos_opt().unwrap_or_default() as u128 ^ (std::process::id() as u128) << 64;
        b = n.to_le_bytes();
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

struct Captured {
    status: Option<ExitStatus>,
    stdout: String,
    stderr: String,
}

fn capture(mut cmd: Command, timeout: Duration) -> Captured {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Captured {
                status: None,
                stdout: String::new(),
                stderr: format!("spawn: {e}"),
            };
        }
    };
    let (tx, rx) = mpsc::channel();
    let pipes: [Option<Box<dyn Read + Send>>; 2] = [
        child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
        child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
    ];
    for (i, pipe) in pipes.into_iter().enumerate() {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut s = String::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_string(&mut s);
            }
            let _ = tx.send((i, s));
        });
    }
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(200)),
            _ => {
                let _ = Command::new("kill")
                    .args(["-KILL", "--", &format!("-{}", child.id())])
                    .status();
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let (mut stdout, mut stderr) = (String::new(), String::new());
    for _ in 0..2 {
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok((0, s)) => stdout = s,
            Ok((_, s)) => stderr = s,
            Err(_) => break,
        }
    }
    Captured { status, stdout, stderr }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_kills_the_process_group() {
        let started = Instant::now();
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo started; sleep 30 & sleep 30"]);
        let res = capture(cmd, Duration::from_secs(1));
        assert!(res.status.is_none());
        assert_eq!(res.stdout, "started\n");
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[test]
    fn uuid_is_v4() {
        let u = uuid4();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        assert!("89ab".contains(&u[19..20]));
    }
}
