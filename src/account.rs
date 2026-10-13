//! GitHub login through the site, the local signing key, and signed requests.

use crate::store::{Res, home};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub fn site() -> String {
    std::env::var("AI_USAGE_SITE")
        .unwrap_or_else(|_| "https://ai.catjuly.dev".into())
        .trim_end_matches('/')
        .to_string()
}

fn client() -> String {
    format!("ai-usage/{}", env!("CARGO_PKG_VERSION"))
}

fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| home().join(".config"))
        .join("ai-usage")
}

fn account_file() -> PathBuf {
    config_dir().join("account.json")
}

pub fn random<const N: usize>() -> Res<[u8; N]> {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).map_err(|e| format!("no system randomness: {e}"))?;
    Ok(b)
}

pub struct Account {
    pub id: String,
    key: SigningKey,
    pub link_key: [u8; 32],
    /// Unix time of the login; nothing recorded before it is uploaded.
    pub since: i64,
}

impl Account {
    pub fn load() -> Option<Self> {
        let v: Value = serde_json::from_str(&fs::read_to_string(account_file()).ok()?).ok()?;
        let bytes = |k: &str| -> Option<[u8; 32]> { STANDARD.decode(v[k].as_str()?).ok()?.try_into().ok() };
        Some(Account {
            id: v["contributor_id"].as_str()?.to_string(),
            key: SigningKey::from_bytes(&bytes("signing_key")?),
            link_key: bytes("link_key")?,
            since: v["since"].as_i64()?,
        })
    }

    pub fn save(&self) -> Res<()> {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(config_dir())?;
        let body = json!({
            "contributor_id": self.id,
            "signing_key": STANDARD.encode(self.key.to_bytes()),
            "link_key": STANDARD.encode(self.link_key),
            "since": self.since,
        });
        let tmp = account_file().with_extension("tmp");
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(serde_json::to_string_pretty(&body)?.as_bytes())?;
        fs::rename(tmp, account_file())?;
        Ok(())
    }

    pub fn request(&self, method: &str, path: &str, body: Option<&Value>) -> Res<Value> {
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        let time = chrono::Utc::now().timestamp().to_string();
        let digest: String = Sha256::digest(body.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let message = format!("ai-usage-v1\n{method}\n{path}\n{time}\n{digest}");
        let signature = STANDARD.encode(self.key.sign(message.as_bytes()).to_bytes());
        let headers = [
            ("X-AIU-Key", self.id.clone()),
            ("X-AIU-Time", time),
            ("X-AIU-Signature", signature),
        ];
        expect_ok(send(method, path, body.as_bytes(), &headers)?)
    }
}

fn send(method: &str, path: &str, body: &[u8], headers: &[(&str, String)]) -> Res<(u16, Value)> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .user_agent(client())
        .build()
        .into();
    let url = format!("{}{path}", site());
    let mut resp = match method {
        "GET" | "DELETE" => {
            let mut r = if method == "GET" {
                agent.get(&url)
            } else {
                agent.delete(&url)
            };
            for (k, v) in headers {
                r = r.header(*k, v);
            }
            r.call()?
        }
        _ => {
            let mut r = if method == "PUT" {
                agent.put(&url)
            } else {
                agent.post(&url)
            };
            r = r.header("Content-Type", "application/json");
            for (k, v) in headers {
                r = r.header(*k, v);
            }
            r.send(body)?
        }
    };
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().unwrap_or_default();
    let value = match text.trim() {
        "" => Value::Null,
        t => serde_json::from_str(t).unwrap_or_else(|_| json!({ "error": t })),
    };
    Ok((status, value))
}

fn expect_ok((status, value): (u16, Value)) -> Res<Value> {
    if (200..300).contains(&status) {
        return Ok(value);
    }
    let why = value["error"]
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| format!("HTTP {status}"));
    Err(why.into())
}

fn post(path: &str, body: Value) -> Res<(u16, Value)> {
    send("POST", path, body.to_string().as_bytes(), &[])
}

pub fn login(_args: &[String]) -> Res<()> {
    let key = SigningKey::from_bytes(&random::<32>()?);
    let public_key = STANDARD.encode(key.verifying_key().to_bytes());
    let start = expect_ok(post(
        "/api/v1/login",
        json!({ "public_key": public_key, "client": client() }),
    )?)?;
    let login_id = start["login_id"].as_str().ok_or("unexpected reply from the site")?;
    println!(
        "Open {} and enter the code {}",
        start["verification_uri"]
            .as_str()
            .unwrap_or("https://github.com/login/device"),
        start["user_code"].as_str().unwrap_or("?")
    );
    let interval = Duration::from_secs(start["interval"].as_u64().unwrap_or(5).max(1));
    let deadline = Instant::now() + Duration::from_secs(start["expires_in"].as_u64().unwrap_or(900));
    let done = loop {
        if Instant::now() > deadline {
            return Err("the code expired; run `ai-usage login` again".into());
        }
        std::thread::sleep(interval);
        match post("/api/v1/login/poll", json!({ "login_id": login_id }))? {
            (202, _) => continue,
            reply => break expect_ok(reply)?,
        }
    };
    let account = Account {
        id: done["contributor_id"]
            .as_str()
            .ok_or("unexpected reply from the site")?
            .to_string(),
        key,
        link_key: random()?,
        since: chrono::Utc::now().timestamp(),
    };
    account.save()?;
    println!("Logged in as {}.", done["github"]["login"].as_str().unwrap_or("?"));
    if done["public"].as_bool().unwrap_or(true) {
        println!(
            "Your GitHub avatar and name appear on the contributor list; `ai-usage profile --anonymous` hides them."
        );
    }
    println!(
        "From now on the service uploads your speed data every hour; `ai-usage share --dry-run` shows what it sends."
    );
    Ok(())
}

pub fn logout() -> Res<()> {
    let Some(account) = Account::load() else {
        println!("Not logged in.");
        return Ok(());
    };
    if let Err(e) = account.request("DELETE", "/api/v1/key", None) {
        eprintln!("The site did not confirm ({e}); the key is removed here anyway.");
    }
    fs::remove_file(account_file())?;
    println!("Logged out. Nothing more is uploaded.");
    Ok(())
}

pub fn profile(args: &[String]) -> Res<()> {
    let account = Account::load().ok_or("not logged in; run `ai-usage login`")?;
    let public = match args.first().map(String::as_str) {
        Some("--anonymous") => Some(false),
        Some("--public") => Some(true),
        None => None,
        Some(other) => return Err(format!("unknown option {other}").into()),
    };
    if let Some(public) = public {
        account.request("POST", "/api/v1/profile", Some(&json!({ "public": public })))?;
    }
    let me = account.request("GET", "/api/v1/me", None)?;
    let shown = if me["public"].as_bool().unwrap_or(true) {
        "public"
    } else {
        "anonymous"
    };
    println!(
        "{} ({shown} on the contributor list)",
        me["github"]["login"].as_str().unwrap_or("?")
    );
    for range in ["24h", "7d", "30d"] {
        if let Some(score) = me["score"][range].as_f64() {
            println!("   score {range:>3}  {score:.0}");
        }
    }
    Ok(())
}
