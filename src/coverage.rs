//! Whether every Claude Code / Codex process on this machine reports to
//! ai-usage. Quota deltas only count while this holds, so $ per 1% never
//! includes usage that went unrecorded.

use crate::store::read_since;
use std::ffi::OsString;
use sysinfo::{ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System, UpdateKind};

pub const CLAUDE_SINCE: &str = "telemetry-since";
pub const CODEX_SINCE: &str = "codex-telemetry-since";

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Tool {
    Claude,
    Codex,
}

struct Proc {
    tool: Tool,
    started: u64,
    cmd: Vec<OsString>,
    environ: Vec<OsString>,
}

pub fn covered(tool: Tool) -> bool {
    let since = read_since(match tool {
        Tool::Claude => CLAUDE_SINCE,
        Tool::Codex => CODEX_SINCE,
    })
    .map(|t| t.timestamp().max(0) as u64);
    scan().iter().filter(|p| p.tool == tool).all(|p| reports(p, since))
}

fn scan() -> Vec<Proc> {
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_cmd(UpdateKind::Always)
            .with_environ(UpdateKind::Always),
    );
    sys.processes()
        .values()
        .filter(|p| p.thread_kind().is_none() && p.status() != ProcessStatus::Zombie)
        .filter_map(|p| {
            Some(Proc {
                tool: tool_of(&p.name().to_string_lossy(), p.cmd())?,
                started: p.start_time(),
                cmd: p.cmd().to_vec(),
                environ: p.environ().to_vec(),
            })
        })
        .collect()
}

fn tool_of(name: &str, cmd: &[OsString]) -> Option<Tool> {
    match name {
        "claude" => Some(Tool::Claude),
        "codex" => Some(Tool::Codex),
        n if n.starts_with("node") || n.starts_with("bun") => {
            let script = cmd.get(1)?.to_string_lossy();
            (script.ends_with("/claude") || script.contains("@anthropic-ai/claude-code")).then_some(Tool::Claude)
        }
        _ => None,
    }
}

fn reports(p: &Proc, since: Option<u64>) -> bool {
    let arg = |s: &str| p.cmd.iter().any(|a| a.to_string_lossy().contains(s));
    let after = since.is_some_and(|s| p.started >= s);
    match p.tool {
        Tool::Claude => p.environ.iter().any(|e| e == "CLAUDE_CODE_ENABLE_TELEMETRY=1") || after,
        Tool::Codex if arg("pid-update-loop") => true,
        Tool::Codex if arg("--ignore-user-config") => arg("otel.exporter"),
        Tool::Codex => after,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(tool: Tool, started: u64, cmd: &[&str], environ: &[&str]) -> Proc {
        Proc {
            tool,
            started,
            cmd: cmd.iter().map(OsString::from).collect(),
            environ: environ.iter().map(OsString::from).collect(),
        }
    }

    #[test]
    fn claude_counts_if_its_env_or_start_time_says_so() {
        let old_with_env = proc(Tool::Claude, 10, &["claude"], &["CLAUDE_CODE_ENABLE_TELEMETRY=1"]);
        let old_without = proc(Tool::Claude, 10, &["claude"], &[]);
        let new_without = proc(Tool::Claude, 200, &["claude"], &[]);
        assert!(reports(&old_with_env, Some(100)));
        assert!(!reports(&old_without, Some(100)));
        assert!(reports(&new_without, Some(100)));
        assert!(!reports(&new_without, None));
    }

    #[test]
    fn codex_without_user_config_needs_its_own_exporter() {
        let bare = proc(Tool::Codex, 200, &["codex", "exec", "--ignore-user-config"], &[]);
        let passed = proc(
            Tool::Codex,
            200,
            &["codex", "exec", "--ignore-user-config", "-c", "otel.exporter={}"],
            &[],
        );
        assert!(!reports(&bare, Some(100)));
        assert!(reports(&passed, Some(100)));
        assert!(!reports(&proc(Tool::Codex, 10, &["codex"], &[]), Some(100)));
    }

    #[test]
    fn recognises_node_installs() {
        let cmd = |s: &[&str]| s.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            tool_of(
                "node",
                &cmd(&["node", "/usr/lib/node_modules/@anthropic-ai/claude-code/cli.js"])
            ),
            Some(Tool::Claude)
        );
        assert_eq!(
            tool_of("node", &cmd(&["node", "/home/u/.npm-global/bin/claude"])),
            Some(Tool::Claude)
        );
        assert_eq!(tool_of("node", &cmd(&["node", "server.js"])), None);
    }
}
