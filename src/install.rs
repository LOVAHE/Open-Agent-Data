//! `ai-usage install` / `uninstall`: the background service, plus telemetry
//! for every Claude Code and Codex run by this user, wherever it starts.

use crate::coverage::{CLAUDE_SINCE, CODEX_SINCE};
use crate::store::{Res, fmt_ts, home, logs_endpoint, state_dir};
use chrono::Utc;
use serde_json::{Map, Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const SERVICE: &str = "ai-usage.service";
const LAUNCHD_LABEL: &str = "io.github.lovahe.ai-usage";
const DESCRIPTION: &str = "ai-usage: local Claude Code and Codex usage ledger";

struct Opts {
    dry_run: bool,
    force: bool,
    claude: bool,
    codex: bool,
    service: bool,
}

impl Opts {
    fn parse(args: &[String]) -> Res<Self> {
        let mut o = Opts {
            dry_run: false,
            force: false,
            claude: true,
            codex: true,
            service: true,
        };
        for a in args {
            match a.as_str() {
                "--dry-run" => o.dry_run = true,
                "--force" => o.force = true,
                "--no-claude" => o.claude = false,
                "--no-codex" => o.codex = false,
                "--no-service" => o.service = false,
                other => return Err(format!("unknown option {other}").into()),
            }
        }
        Ok(o)
    }
}

pub fn install(args: &[String]) -> Res<()> {
    let o = Opts::parse(args)?;
    let mut restart_hint = false;
    if !o.dry_run {
        fs::create_dir_all(state_dir())?;
    }
    if o.claude {
        restart_hint |= edit(
            &claude_settings(),
            &o,
            |t| claude_enable(t, &logs_endpoint()),
            claude_strip,
            CLAUDE_SINCE,
        )?;
    }
    if o.codex {
        restart_hint |= edit(
            &codex_config(),
            &o,
            |t| codex_enable(t, &logs_endpoint()),
            codex_strip,
            CODEX_SINCE,
        )?;
    }
    if o.service {
        service_install(&o)?;
    }
    if restart_hint {
        println!("Sessions that were already running are not recorded until they restart.");
    }
    println!("Data is kept in {}", state_dir().display());
    Ok(())
}

pub fn uninstall(args: &[String]) -> Res<()> {
    let o = Opts::parse(args)?;
    if o.service {
        service_uninstall(&o)?;
    }
    if o.claude {
        edit(
            &claude_settings(),
            &o,
            |t| claude_disable(t, &logs_endpoint()),
            claude_strip,
            "",
        )?;
    }
    if o.codex {
        edit(
            &codex_config(),
            &o,
            |t| codex_disable(t, &logs_endpoint()),
            codex_strip,
            "",
        )?;
    }
    println!("Recorded data is left in {}", state_dir().display());
    Ok(())
}

fn claude_settings() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".claude"))
        .join("settings.json")
}

fn codex_config() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".codex"))
        .join("config.toml")
}

enum Change {
    Same,
    New(String),
    Conflict(String),
}

/// Applies `change` to one config file, keeping a one-time backup of the
/// original next to it. With --force, `strip` first removes whatever was in
/// the way. Returns whether the file changed.
fn edit(
    path: &Path,
    o: &Opts,
    change: impl Fn(&str) -> Res<Change>,
    strip: fn(&str) -> Res<String>,
    marker: &str,
) -> Res<bool> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("{}: {e}", path.display()).into()),
    };
    let new = match change(&text)? {
        Change::Same => {
            println!("{}: already set", path.display());
            return Ok(false);
        }
        Change::Conflict(why) if !o.force => {
            return Err(format!(
                "{}: {why}; leave it as is, or pass --force to replace it",
                path.display()
            )
            .into());
        }
        Change::Conflict(_) => match change(&strip(&text)?)? {
            Change::New(new) => new,
            _ => return Err(format!("{}: could not replace the existing settings", path.display()).into()),
        },
        Change::New(new) => new,
    };
    if o.dry_run {
        println!("{}: would change to\n{new}", path.display());
        return Ok(true);
    }
    let backup = path.with_file_name(format!(
        "{}.ai-usage-backup",
        path.file_name().unwrap().to_string_lossy()
    ));
    let backed_up = !marker.is_empty() && !text.is_empty() && !backup.exists();
    if backed_up {
        fs::write(&backup, &text)?;
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, new)?;
    if !marker.is_empty() {
        fs::write(state_dir().join(marker), fmt_ts(Utc::now()) + "\n")?;
    }
    if backed_up {
        println!("{}: updated (original saved as {})", path.display(), backup.display());
    } else {
        println!("{}: updated", path.display());
    }
    Ok(true)
}

fn claude_strip(text: &str) -> Res<String> {
    let mut root = settings_object(text)?;
    if let Some(env) = root.get_mut("env").and_then(Value::as_object_mut) {
        for (k, _) in claude_env("") {
            env.remove(k);
        }
    }
    Ok(serde_json::to_string_pretty(&root)? + "\n")
}

fn codex_strip(text: &str) -> Res<String> {
    let mut doc: toml_edit::DocumentMut = text.parse()?;
    doc.remove("otel");
    Ok(doc.to_string())
}

fn claude_env(endpoint: &str) -> [(&'static str, String); 4] {
    [
        ("CLAUDE_CODE_ENABLE_TELEMETRY", "1".into()),
        ("OTEL_LOGS_EXPORTER", "otlp".into()),
        ("OTEL_EXPORTER_OTLP_LOGS_PROTOCOL", "http/json".into()),
        ("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT", endpoint.into()),
    ]
}

fn settings_object(text: &str) -> Res<Map<String, Value>> {
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str(text)? {
        Value::Object(m) => Ok(m),
        _ => Err("settings.json is not a JSON object".into()),
    }
}

fn claude_enable(text: &str, endpoint: &str) -> Res<Change> {
    let mut root = settings_object(text)?;
    let env = root.entry("env").or_insert_with(|| json!({}));
    let Some(env) = env.as_object_mut() else {
        return Err("settings.json: \"env\" is not an object".into());
    };
    let mut changed = false;
    for (k, want) in claude_env(endpoint) {
        match env.get(k).and_then(Value::as_str) {
            Some(have) if have == want => {}
            Some(have) => return Ok(Change::Conflict(format!("env.{k} is already \"{have}\""))),
            None => {
                env.insert(k.into(), json!(want));
                changed = true;
            }
        }
    }
    Ok(if changed {
        Change::New(serde_json::to_string_pretty(&root)? + "\n")
    } else {
        Change::Same
    })
}

fn claude_disable(text: &str, endpoint: &str) -> Res<Change> {
    let mut root = settings_object(text)?;
    let Some(env) = root.get_mut("env").and_then(Value::as_object_mut) else {
        return Ok(Change::Same);
    };
    if env.get("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT").and_then(Value::as_str) != Some(endpoint) {
        return Ok(Change::Same);
    }
    for (k, want) in claude_env(endpoint) {
        if env.get(k).and_then(Value::as_str) == Some(want.as_str()) {
            env.remove(k);
        }
    }
    if env.is_empty() {
        root.remove("env");
    }
    Ok(Change::New(serde_json::to_string_pretty(&root)? + "\n"))
}

fn codex_enable(text: &str, endpoint: &str) -> Res<Change> {
    let doc: toml_edit::DocumentMut = text.parse()?;
    if let Some(otel) = doc.get("otel") {
        return Ok(if otel.to_string().contains(endpoint) {
            Change::Same
        } else {
            Change::Conflict("an [otel] section already exists".into())
        });
    }
    let section =
        format!("[otel]\nexporter = {{ otlp-http = {{ endpoint = \"{endpoint}\", protocol = \"json\" }} }}\n");
    let gap = match text.trim_end_matches(' ') {
        "" => "",
        t if t.ends_with("\n\n") => "",
        t if t.ends_with('\n') => "\n",
        _ => "\n\n",
    };
    let new = format!("{text}{gap}{section}");
    new.parse::<toml_edit::DocumentMut>()?;
    Ok(Change::New(new))
}

fn codex_disable(text: &str, endpoint: &str) -> Res<Change> {
    let mut doc: toml_edit::DocumentMut = text.parse()?;
    match doc.get("otel") {
        Some(otel) if otel.to_string().contains(endpoint) => {
            doc.remove("otel");
            Ok(Change::New(doc.to_string()))
        }
        _ => Ok(Change::Same),
    }
}

fn exe() -> Res<PathBuf> {
    Ok(std::env::current_exe()?.canonicalize()?)
}

fn service_install(o: &Opts) -> Res<()> {
    let exe = exe()?;
    let path = std::env::var("PATH").unwrap_or_default();
    if cfg!(target_os = "macos") {
        let plist = launchd_plist(&exe, &path, &state_dir());
        let file = home()
            .join("Library/LaunchAgents")
            .join(format!("{LAUNCHD_LABEL}.plist"));
        if !write_service(&file, &plist, o)? {
            return Ok(());
        }
        let domain = format!("gui/{}", uid()?);
        let _ = Command::new("launchctl")
            .args(["bootout", &format!("{domain}/{LAUNCHD_LABEL}")])
            .status();
        run_ok(Command::new("launchctl").args(["bootstrap", &domain]).arg(&file))?;
    } else {
        let unit = systemd_unit(&exe, &path);
        let file = dirs::config_dir()
            .unwrap_or_else(|| home().join(".config"))
            .join("systemd/user")
            .join(SERVICE);
        if !write_service(&file, &unit, o)? {
            return Ok(());
        }
        run_ok(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
        run_ok(Command::new("systemctl").args(["--user", "enable", "--now", SERVICE]))?;
    }
    println!("Service started; it records from now on.");
    Ok(())
}

/// Returns false when the file is already there with other content and
/// --force was not given.
fn write_service(file: &Path, content: &str, o: &Opts) -> Res<bool> {
    match fs::read_to_string(file) {
        Ok(have) if have == content => {
            println!("{}: already set", file.display());
            return Ok(!o.dry_run);
        }
        Ok(_) if !o.force => {
            println!(
                "{}: exists with other settings; kept (pass --force to replace it)",
                file.display()
            );
            return Ok(false);
        }
        _ => {}
    }
    if o.dry_run {
        println!("{}: would write\n{content}", file.display());
        return Ok(false);
    }
    fs::create_dir_all(file.parent().unwrap())?;
    fs::write(file, content)?;
    println!("{}: written", file.display());
    Ok(true)
}

fn service_uninstall(o: &Opts) -> Res<()> {
    let (file, stop): (PathBuf, Vec<Vec<String>>) = if cfg!(target_os = "macos") {
        let domain = format!("gui/{}/{LAUNCHD_LABEL}", uid()?);
        (
            home()
                .join("Library/LaunchAgents")
                .join(format!("{LAUNCHD_LABEL}.plist")),
            vec![vec!["launchctl".into(), "bootout".into(), domain]],
        )
    } else {
        (
            dirs::config_dir()
                .unwrap_or_else(|| home().join(".config"))
                .join("systemd/user")
                .join(SERVICE),
            vec![
                ["systemctl", "--user", "disable", "--now", SERVICE]
                    .map(String::from)
                    .to_vec(),
                ["systemctl", "--user", "daemon-reload"].map(String::from).to_vec(),
            ],
        )
    };
    let ours = fs::read_to_string(&file).is_ok_and(|t| t.contains(DESCRIPTION) || t.contains(LAUNCHD_LABEL));
    if !ours {
        println!("{}: not installed by ai-usage; left alone", file.display());
        return Ok(());
    }
    if o.dry_run {
        println!("{}: would stop and remove", file.display());
        return Ok(());
    }
    let (first, rest) = stop.split_first().unwrap();
    let _ = Command::new(&first[0]).args(&first[1..]).status();
    fs::remove_file(&file)?;
    for cmd in rest {
        let _ = Command::new(&cmd[0]).args(&cmd[1..]).status();
    }
    println!("{}: stopped and removed", file.display());
    Ok(())
}

fn systemd_unit(exe: &Path, path: &str) -> String {
    format!(
        "[Unit]\n\
         Description={DESCRIPTION}\n\
         \n\
         [Service]\n\
         ExecStart=\"{}\" serve\n\
         Environment=\"PATH={path}\"\n\
         Restart=always\n\
         RestartSec=5\n\
         MemoryMax=1G\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exe.display()
    )
}

fn launchd_plist(exe: &Path, path: &str, state: &Path) -> String {
    let x = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key><array><string>{}</string><string>serve</string></array>
  <key>EnvironmentVariables</key><dict><key>PATH</key><string>{}</string></dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardErrorPath</key><string>{}</string>
</dict>
</plist>
"#,
        x(&exe.display().to_string()),
        x(path),
        x(&state.join("serve.log").display().to_string())
    )
}

fn uid() -> Res<String> {
    let out = Command::new("id").arg("-u").output()?;
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

fn run_ok(cmd: &mut Command) -> Res<()> {
    let status = cmd.status().map_err(|e| format!("{:?}: {e}", cmd.get_program()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{:?} failed ({status})", cmd.get_program()).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EP: &str = "http://127.0.0.1:4318/v1/logs";

    fn new(c: Change) -> String {
        match c {
            Change::New(s) => s,
            Change::Same => panic!("expected a change, got none"),
            Change::Conflict(w) => panic!("unexpected conflict: {w}"),
        }
    }

    #[test]
    fn claude_settings_keep_other_keys_and_order() {
        let before = r#"{"theme": "dark", "env": {"FOO": "1"}, "permissions": {"allow": []}}"#;
        let after: Value = serde_json::from_str(&new(claude_enable(before, EP).unwrap())).unwrap();
        let keys: Vec<&String> = after.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["theme", "env", "permissions"]);
        assert_eq!(after["env"]["FOO"], "1");
        assert_eq!(after["env"]["OTEL_EXPORTER_OTLP_LOGS_ENDPOINT"], EP);
        assert!(matches!(claude_enable(&after.to_string(), EP).unwrap(), Change::Same));
        let back: Value = serde_json::from_str(&new(claude_disable(&after.to_string(), EP).unwrap())).unwrap();
        assert_eq!(back, serde_json::from_str::<Value>(before).unwrap());
    }

    #[test]
    fn claude_settings_never_override_someone_elses_collector() {
        let theirs = r#"{"env": {"OTEL_LOGS_EXPORTER": "console"}}"#;
        assert!(matches!(claude_enable(theirs, EP).unwrap(), Change::Conflict(_)));
        assert!(matches!(claude_enable("", EP).unwrap(), Change::New(_)));
    }

    #[test]
    fn codex_config_gains_and_loses_only_its_otel_section() {
        let before = "model = \"gpt-6-luna\"\n\n[profiles.fast]\nmodel = \"x\"\n";
        let after = new(codex_enable(before, EP).unwrap());
        assert!(after.starts_with(before));
        assert!(after.contains("[otel]") && after.contains(EP));
        assert!(matches!(codex_enable(&after, EP).unwrap(), Change::Same));
        assert_eq!(new(codex_disable(&after, EP).unwrap()), before);
        let theirs = "[otel]\nexporter = \"none\"\n";
        assert!(matches!(codex_enable(theirs, EP).unwrap(), Change::Conflict(_)));
        assert!(matches!(codex_disable(theirs, EP).unwrap(), Change::Same));
    }
}
