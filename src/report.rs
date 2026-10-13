use crate::share::median;
use crate::store::{Res, num, parse_ts, read_jsonl};
use chrono::{DateTime, Duration, Local, NaiveDate, TimeZone, Timelike, Utc};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

const EDGE_MIN: i64 = 10;
const MIN_TPS_OUTPUT: u64 = 200;
const MIN_GEN_MS: u64 = 1000;
const TOOLS: [(&str, &str); 2] = [("claude", "Claude Code"), ("codex", "Codex")];
const WINDOWS: [(&str, &str, i64); 2] = [("five_hour", "5-hour", 5 * 60), ("week", "week", 7 * 24 * 60)];

pub struct Req {
    pub ts: DateTime<Utc>,
    pub tool: String,
    pub instance: String,
    pub session: String,
    pub model: String,
    pub effort: String,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost: f64,
    pub duration_ms: u64,
    pub ttft_ms: Option<u64>,
}

impl Req {
    fn tokens(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }
    pub fn gen_tps(&self) -> Option<f64> {
        let ttft = self.ttft_ms?;
        (self.output >= MIN_TPS_OUTPUT && self.duration_ms >= ttft + MIN_GEN_MS)
            .then(|| self.output as f64 / ((self.duration_ms - ttft) as f64 / 1000.0))
    }
    fn e2e_tps(&self) -> Option<f64> {
        (self.output >= MIN_TPS_OUTPUT && self.duration_ms > 0)
            .then(|| self.output as f64 / (self.duration_ms as f64 / 1000.0))
    }
}

struct Sample {
    ts: DateTime<Utc>,
    tool: String,
    covered: bool,
    windows: Vec<(String, f64, Option<DateTime<Utc>>, String)>,
}

struct Window {
    start: DateTime<Utc>,
    reset: DateTime<Utc>,
    pct: f64,
    delta: f64,
    unc: f64,
    spans: Vec<(DateTime<Utc>, DateTime<Utc>)>,
    full: bool,
}

fn local(t: DateTime<Utc>, fmt: &str) -> String {
    t.with_timezone(&Local).format(fmt).to_string()
}

fn md(t: DateTime<Utc>) -> String {
    local(t, "%m-%d %H:%M")
}

fn fmt_tok(n: f64) -> String {
    if n >= 1e6 {
        format!("{:.2}M", n / 1e6)
    } else {
        format!("{:.1}K", n / 1e3)
    }
}

fn s(v: &Value, k: &str) -> String {
    v[k].as_str().unwrap_or_default().to_string()
}

pub fn load_requests() -> Vec<Req> {
    let mut reqs: Vec<Req> = read_jsonl("requests-")
        .iter()
        .filter_map(|r| {
            let u = |k: &str| num(&r[k]).max(0.0) as u64;
            Some(Req {
                ts: parse_ts(r["ts"].as_str()?)?,
                tool: r["tool"].as_str().unwrap_or("claude").to_string(),
                instance: r["instance"].as_str().unwrap_or("other").to_string(),
                session: r["session"].as_str().unwrap_or_default().to_string(),
                model: r["model"].as_str().unwrap_or("?").to_string(),
                effort: r["effort"].as_str().unwrap_or("-").to_string(),
                input: u("input_tokens"),
                output: u("output_tokens"),
                cache_read: u("cache_read_tokens"),
                cache_write: u("cache_creation_tokens"),
                cost: num(&r["cost_usd"]),
                duration_ms: u("duration_ms"),
                ttft_ms: r.get("ttft_ms").map(|v| num(v).max(0.0) as u64),
            })
        })
        .collect();
    for r in reqs.iter_mut().filter(|r| r.tool == "codex" && r.cost == 0.0) {
        r.cost = codex_cost(r);
    }
    reqs.sort_by_key(|r| r.ts);
    reqs
}

const CODEX_LONG_CONTEXT: u64 = 272_000;

struct Price {
    model: &'static str,
    usd_per_mtok: [f64; 3],
    long_context: Option<[f64; 3]>,
}

const CODEX_PRICES: [Price; 4] = [
    Price {
        model: "gpt-6.1-sol",
        usd_per_mtok: [2.00, 0.10, 10.00],
        long_context: Some([4.00, 0.20, 15.00]),
    },
    Price {
        model: "gpt-6-astra",
        usd_per_mtok: [10.00, 1.00, 50.00],
        long_context: None,
    },
    Price {
        model: "gpt-6-luna",
        usd_per_mtok: [0.10, 0.01, 0.50],
        long_context: Some([0.20, 0.02, 0.75]),
    },
    Price {
        model: "gpt-5.6-luna",
        usd_per_mtok: [0.20, 0.02, 1.20],
        long_context: None,
    },
];

fn codex_cost(r: &Req) -> f64 {
    let Some(p) = CODEX_PRICES.iter().find(|p| p.model == r.model) else {
        return 0.0;
    };
    let uncached = r.input + r.cache_write;
    let [input, cached, output] = match p.long_context {
        Some(l) if uncached + r.cache_read >= CODEX_LONG_CONTEXT => l,
        _ => p.usd_per_mtok,
    };
    (uncached as f64 * input + r.cache_read as f64 * cached + r.output as f64 * output) / 1e6
}

fn round10(t: DateTime<Utc>) -> DateTime<Utc> {
    Utc.timestamp_opt((t.timestamp() as f64 / 600.0).round() as i64 * 600, 0)
        .unwrap()
}

pub fn parse_claude_reset(text: &str, reference: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let body = text.strip_suffix(" (UTC)")?;
    let (date, time) = body.split_once(", ")?;
    let (mon, day) = date.split_once(' ')?;
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|m| *m == mon)? as u32
        + 1;
    let (clock, pm) = match (time.strip_suffix("am"), time.strip_suffix("pm")) {
        (Some(c), _) => (c, false),
        (_, Some(c)) => (c, true),
        _ => return None,
    };
    let (h, m) = clock.split_once(':').unwrap_or((clock, "0"));
    let hour = h.parse::<u32>().ok()? % 12 + if pm { 12 } else { 0 };
    let at = |year| {
        NaiveDate::from_ymd_opt(year, month, day.parse().ok()?)?
            .and_hms_opt(hour, m.parse().ok()?, 0)
            .map(|d| d.and_utc())
    };
    let mut t = at(reference.format("%Y").to_string().parse().ok()?)?;
    if t < reference - Duration::days(30) {
        t = at(reference.format("%Y").to_string().parse::<i32>().ok()? + 1)?;
    }
    Some(round10(t))
}

fn load_samples() -> Vec<Sample> {
    let mut out: Vec<Sample> = read_jsonl("usage.jsonl")
        .iter()
        .filter_map(|r| {
            let ts = parse_ts(r["ts"].as_str()?)?;
            let windows = r
                .as_object()?
                .iter()
                .filter(|(_, v)| v.get("pct").is_some())
                .map(|(k, v)| {
                    let reset = match v.get("reset_at") {
                        Some(e) => Utc.timestamp_opt(num(e) as i64, 0).single().map(round10),
                        None => v["resets"].as_str().and_then(|t| parse_claude_reset(t, ts)),
                    };
                    let key = if k == "week_all_models" {
                        "week".to_string()
                    } else {
                        k.clone()
                    };
                    (key, num(&v["pct"]), reset, s(v, "resets"))
                })
                .collect();
            Some(Sample {
                ts,
                tool: r["tool"].as_str().unwrap_or("claude").to_string(),
                covered: r["covered"].as_bool().unwrap_or(false),
                windows,
            })
        })
        .collect();
    out.sort_by_key(|s| s.ts);
    out
}

fn between<'a>(reqs: &'a [Req], tool: &str, a: DateTime<Utc>, b: DateTime<Utc>) -> impl Iterator<Item = &'a Req> {
    let lo = reqs.partition_point(|r| r.ts <= a);
    let hi = reqs.partition_point(|r| r.ts <= b);
    reqs[lo..hi.max(lo)].iter().filter(move |r| r.tool == tool)
}

fn windows_for(samples: &[&Sample], key: &str, minutes: i64, now: DateTime<Utc>) -> Vec<Window> {
    let edge = Duration::minutes(EDGE_MIN);
    let mut groups: BTreeMap<DateTime<Utc>, Vec<(&Sample, f64)>> = BTreeMap::new();
    for s in samples {
        for (k, pct, reset, _) in &s.windows {
            if k == key
                && let Some(r) = reset
            {
                groups.entry(*r).or_default().push((s, *pct));
            }
        }
    }
    let mut out = Vec::new();
    let mut prev_last: Option<DateTime<Utc>> = None;
    for (reset, group) in groups {
        let mut start = reset - Duration::minutes(minutes);
        if let Some(p) = prev_last.filter(|p| *p > start) {
            start = p;
        }
        prev_last = Some(group.last().unwrap().0.ts);
        let before = samples
            .iter()
            .any(|s| s.covered && s.ts >= start - edge && s.ts <= start);
        let mut segments: Vec<Vec<(DateTime<Utc>, f64)>> = Vec::new();
        let mut cur: Option<Vec<(DateTime<Utc>, f64)>> = (before && group[0].0.covered).then(|| vec![(start, 0.0)]);
        for (s, pct) in &group {
            if s.covered {
                cur.get_or_insert_with(Vec::new).push((s.ts, *pct));
            } else if let Some(c) = cur.take() {
                segments.push(c);
            }
        }
        segments.extend(cur);
        let (mut delta, mut unc, mut spans) = (0.0, 0.0, Vec::new());
        for seg in segments.iter().filter(|s| s.len() >= 2) {
            delta += seg.last().unwrap().1 - seg[0].1;
            unc += if seg[0].0 == start { 0.5 } else { 1.0 };
            spans.push((seg[0].0, seg.last().unwrap().0));
        }
        let full = spans.len() == 1 && spans[0].0 == start && spans[0].1 >= reset.min(now) - edge;
        out.push(Window {
            start,
            reset,
            pct: group.last().unwrap().1,
            delta,
            unc,
            spans,
            full,
        });
    }
    out
}

fn totals(rows: &[&Req]) -> String {
    let f = |g: fn(&Req) -> u64| rows.iter().map(|r| g(r) as f64).sum::<f64>();
    let cost: f64 = rows.iter().map(|r| r.cost).sum();
    format!(
        "{} req, {} tokens, ${:.2} (in {} / out {} / cache r {} / cache w {})",
        rows.len(),
        fmt_tok(f(Req::tokens)),
        cost,
        fmt_tok(f(|r| r.input)),
        fmt_tok(f(|r| r.output)),
        fmt_tok(f(|r| r.cache_read)),
        fmt_tok(f(|r| r.cache_write)),
    )
}

fn per_pct(rows: &[&Req], delta: f64, unc: f64) -> String {
    if delta <= 0.0 {
        return "n/a (quota % has not moved yet)".into();
    }
    let cost: f64 = rows.iter().map(|r| r.cost).sum();
    if cost > 0.0 {
        let hi = cost / (delta - unc).max(0.5);
        let lo = cost / (delta + unc);
        return format!("${:.2} per 1%  (rounding range ${lo:.2}-${hi:.2})", cost / delta);
    }
    let out: f64 = rows.iter().map(|r| r.output as f64).sum();
    let all: f64 = rows.iter().map(|r| r.tokens() as f64).sum();
    format!(
        "per 1%: {} output tokens, {} total tokens",
        fmt_tok(out / delta),
        fmt_tok(all / delta)
    )
}

fn label(key: &str) -> String {
    WINDOWS
        .iter()
        .find(|w| w.0 == key)
        .map(|w| w.1.to_string())
        .unwrap_or_else(|| key.replace('_', " "))
}

fn quota_section(tool: &str, name: &str, samples: &[Sample], reqs: &[Req], n: usize, now: DateTime<Utc>) {
    let mine: Vec<&Sample> = samples
        .iter()
        .filter(|s| s.tool == tool && !s.windows.is_empty())
        .collect();
    println!("\n#### {name}");
    let Some(last) = mine.last() else {
        println!("   no quota samples yet");
        return;
    };
    let cov = if last.covered {
        "complete"
    } else {
        "NOT covering every running process"
    };
    println!("Quota now (sampled {}, telemetry {cov}):", md(last.ts));
    for (k, pct, reset, raw) in &last.windows {
        let when = reset.map(md).unwrap_or_else(|| raw.clone());
        println!("   {:<12} {pct:>5}%   resets {when}", label(k));
    }
    for (key, lbl, minutes) in WINDOWS {
        let wins = windows_for(&mine, key, minutes, now);
        if wins.is_empty() {
            continue;
        }
        println!("== {lbl} windows");
        for w in wins.iter().rev().take(n) {
            let state = if w.reset > now { "open" } else { "closed" };
            let cov = if w.full {
                "full"
            } else if w.spans.is_empty() {
                "no telemetry"
            } else {
                "partial"
            };
            println!(
                "   {} -> {} [{state}] quota {}%  measured: +{}% over {cov} coverage",
                md(w.start),
                md(w.reset),
                w.pct,
                (w.delta * 10.0).round() / 10.0
            );
            let rows: Vec<&Req> = w.spans.iter().flat_map(|(a, b)| between(reqs, tool, *a, *b)).collect();
            if !rows.is_empty() {
                println!("     {}", totals(&rows));
                println!("     {}", per_pct(&rows, w.delta, w.unc));
            }
            if cov == "partial" {
                for (a, b) in &w.spans {
                    println!("     covered {} -> {}", md(*a), md(*b));
                }
            }
        }
    }
}

fn pctl(v: &mut [f64], q: f64) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[((v.len() - 1) as f64 * q).round() as usize]
}

fn speed_table(rows: &[&Req]) {
    let mut groups: BTreeMap<(String, String), Vec<&Req>> = BTreeMap::new();
    for r in rows {
        if r.e2e_tps().is_some() {
            groups
                .entry((r.tool.clone(), format!("{}/{}", r.model, r.effort)))
                .or_default()
                .push(r);
        }
    }
    if groups.is_empty() {
        println!("   no requests with ≥{MIN_TPS_OUTPUT} output tokens and timing data");
        return;
    }
    println!(
        "   {:<7} {:<36} {:>5}  {:>9} {:>14}  {:>8}  {:>9}",
        "tool", "model/effort", "req", "gen tok/s", "p10-p90", "e2e p50", "ttft p50"
    );
    let mut sorted: Vec<_> = groups.into_iter().collect();
    sorted.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
    for ((tool, model), v) in sorted {
        let mut e2e: Vec<f64> = v.iter().filter_map(|r| r.e2e_tps()).collect();
        let mut gen_tps: Vec<f64> = v.iter().filter_map(|r| r.gen_tps()).collect();
        let mut ttft: Vec<f64> = v.iter().filter_map(|r| r.ttft_ms.map(|t| t as f64 / 1000.0)).collect();
        let (g50, range) = if gen_tps.is_empty() {
            ("-".to_string(), "-".to_string())
        } else {
            let p50 = pctl(&mut gen_tps, 0.5);
            (
                format!("{p50:.1}"),
                format!("{:.0}-{:.0}", pctl(&mut gen_tps, 0.1), pctl(&mut gen_tps, 0.9)),
            )
        };
        let t50 = if ttft.is_empty() {
            "-".into()
        } else {
            format!("{:.1}s", pctl(&mut ttft, 0.5))
        };
        println!(
            "   {tool:<7} {model:<36} {:>5}  {g50:>9} {range:>14}  {:>8.1}  {t50:>9}",
            v.len(),
            pctl(&mut e2e, 0.5)
        );
    }
}

fn breakdown(rows: &[&Req]) {
    let mut groups: BTreeMap<(String, String, String), (u64, f64, usize)> = BTreeMap::new();
    for r in rows {
        let g = groups
            .entry((r.tool.clone(), r.instance.clone(), format!("{}/{}", r.model, r.effort)))
            .or_default();
        g.0 += r.tokens();
        g.1 += r.cost;
        g.2 += 1;
    }
    let mut sorted: Vec<_> = groups.into_iter().collect();
    sorted.sort_by(|a, b| b.1.1.total_cmp(&a.1.1).then(b.1.0.cmp(&a.1.0)));
    for ((tool, inst, model), (t, c, n)) in sorted {
        println!(
            "     {tool:<7} {inst:<18} {model:<36} {n:>5} req {:>9}  ${c:.2}",
            fmt_tok(t as f64)
        );
    }
}

pub fn parse_flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

pub fn summary(args: &[String]) -> Res<()> {
    let n: usize = parse_flag(args, "-n").map(|v| v.parse()).transpose()?.unwrap_or(6);
    let only = parse_flag(args, "--tool");
    let now = Utc::now();
    let reqs = load_requests();
    let samples = load_samples();
    for (tool, name) in TOOLS {
        if only.as_deref().is_none_or(|o| o == tool) {
            quota_section(tool, name, &samples, &reqs, n, now);
        }
    }
    let keep = |r: &&Req| only.as_deref().is_none_or(|o| o == r.tool);
    let day: Vec<&Req> = between_all(&reqs, now - Duration::hours(24), now)
        .filter(keep)
        .collect();
    println!("\n== speed, last 24h (output tok/s; gen excludes time to first token)");
    speed_table(&day);
    let midnight = local_midnight(now);
    let today: Vec<&Req> = between_all(&reqs, midnight, now).filter(keep).collect();
    println!("\n== today {}: {}", local(midnight, "%Y-%m-%d"), totals(&today));
    breakdown(&today);
    if let Some(first) = reqs.first() {
        println!(
            "\nRecording since {}. Only this machine is recorded; usage elsewhere still moves the quota %.",
            local(first.ts, "%Y-%m-%d %H:%M")
        );
    }
    Ok(())
}

fn local_midnight(now: DateTime<Utc>) -> DateTime<Utc> {
    now.with_timezone(&Local)
        .with_hour(0)
        .and_then(|t| t.with_minute(0))
        .and_then(|t| t.with_second(0))
        .unwrap()
        .with_timezone(&Utc)
}

fn between_all(reqs: &[Req], a: DateTime<Utc>, b: DateTime<Utc>) -> impl Iterator<Item = &Req> {
    let lo = reqs.partition_point(|r| r.ts <= a);
    let hi = reqs.partition_point(|r| r.ts <= b);
    reqs[lo..hi.max(lo)].iter()
}

pub fn tps(args: &[String]) -> Res<()> {
    let days: i64 = parse_flag(args, "--days").map(|v| v.parse()).transpose()?.unwrap_or(7);
    let only = parse_flag(args, "--tool");
    let now = Utc::now();
    let reqs = load_requests();
    let rows: Vec<&Req> = between_all(&reqs, now - Duration::days(days), now)
        .filter(|r| only.as_deref().is_none_or(|o| o == r.tool))
        .collect();
    println!("== speed, last {days} days (output tok/s; gen excludes time to first token)");
    speed_table(&rows);
    if args.iter().any(|a| a == "--hourly") {
        println!("\n== median gen tok/s by local hour");
        let mut by: BTreeMap<(String, String), BTreeMap<u32, Vec<f64>>> = BTreeMap::new();
        for r in &rows {
            if let Some(v) = r.gen_tps() {
                let hour = r.ts.with_timezone(&Local).hour();
                by.entry((r.tool.clone(), r.model.clone()))
                    .or_default()
                    .entry(hour)
                    .or_default()
                    .push(v);
            }
        }
        for ((tool, model), hours) in by {
            println!("   {tool} {model}");
            for (h, mut v) in hours {
                println!("     {h:02}:00  {:>6.1}  ({} req)", pctl(&mut v, 0.5), v.len());
            }
        }
    }
    Ok(())
}

/// Speed series for the private page: the same ranges and steps as the
/// public curves, from every local request.
const SNAPSHOT_RANGES: [(&str, i64, i64); 5] = [
    ("24h", 86_400, 300),
    ("48h", 2 * 86_400, 1800),
    ("72h", 3 * 86_400, 3600),
    ("1w", 7 * 86_400, 3600),
    ("1m", 30 * 86_400, 3600),
];

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn speed_series(rows: &[&Req], now: i64) -> Value {
    let mut out = Map::new();
    for (name, span, step) in SNAPSHOT_RANGES {
        let mut buckets: BTreeMap<i64, (Vec<f64>, Vec<f64>)> = BTreeMap::new();
        for r in rows.iter().filter(|r| r.ts.timestamp() >= now - span) {
            if let (Some(tps), Some(ttft)) = (r.gen_tps(), r.ttft_ms) {
                let b = buckets.entry(r.ts.timestamp().div_euclid(step) * step).or_default();
                b.0.push(tps);
                b.1.push(ttft as f64);
            }
        }
        if buckets.is_empty() {
            continue;
        }
        let (mut t, mut tps, mut ttft) = (Vec::new(), Vec::new(), Vec::new());
        for (k, (mut a, mut b)) in buckets {
            t.push(k);
            tps.push(round1(median(&mut a)));
            ttft.push(median(&mut b).round());
        }
        out.insert(name.into(), json!({"step": step, "t": t, "tps": tps, "ttft": ttft}));
    }
    Value::Object(out)
}

/// Everything the private page shows. It is encrypted on this machine
/// before it leaves.
pub fn snapshot(now: DateTime<Utc>) -> Value {
    let reqs = load_requests();
    let samples = load_samples();
    let midnight = local_midnight(now);
    let mut tools = Map::new();
    for (tool, _) in TOOLS {
        let mine: Vec<&Sample> = samples
            .iter()
            .filter(|s| s.tool == tool && !s.windows.is_empty())
            .collect();
        let rows: Vec<&Req> = reqs.iter().filter(|r| r.tool == tool).collect();
        if mine.is_empty() && rows.is_empty() {
            continue;
        }
        let quota: Vec<Value> = mine
            .last()
            .map(|s| {
                s.windows
                    .iter()
                    .map(|(k, pct, reset, _)| json!({"window": k, "pct": pct, "resets_at": reset.map(|r| r.timestamp())}))
                    .collect()
            })
            .unwrap_or_default();
        let mut windows = Vec::new();
        for (key, _, minutes) in WINDOWS {
            for w in windows_for(&mine, key, minutes, now).iter().rev().take(6) {
                let used: Vec<&Req> = w.spans.iter().flat_map(|(a, b)| between(&reqs, tool, *a, *b)).collect();
                let cost: f64 = used.iter().map(|r| r.cost).sum();
                let tokens: u64 = used.iter().map(|r| r.tokens()).sum();
                let per = |x: f64| (w.delta > 0.0).then(|| x / w.delta);
                windows.push(json!({
                    "window": key, "start": w.start.timestamp(), "reset": w.reset.timestamp(), "pct": w.pct,
                    "delta": round1(w.delta), "full": w.full, "cost_usd": round2(cost), "tokens": tokens,
                    "usd_per_pct": per(cost).map(round2), "tokens_per_pct": per(tokens as f64).map(f64::round),
                }));
            }
        }
        let today: Vec<&&Req> = rows.iter().filter(|r| r.ts >= midnight).collect();
        let mut by_model: BTreeMap<&str, Vec<&Req>> = BTreeMap::new();
        for r in rows.iter().filter(|r| r.gen_tps().is_some()) {
            by_model.entry(&r.model).or_default().push(r);
        }
        let mut models: Vec<(usize, Value)> = by_model
            .into_iter()
            .map(|(model, rs)| {
                let last = rs.last().unwrap();
                let entry = json!({
                    "model": model, "tps": last.gen_tps().map(round1), "ttft": last.ttft_ms,
                    "ranges": speed_series(&rs, now.timestamp()),
                });
                (rs.len(), entry)
            })
            .collect();
        models.sort_by_key(|m| std::cmp::Reverse(m.0));
        tools.insert(
            tool.into(),
            json!({
                "covered": mine.last().map(|s| s.covered),
                "quota": quota,
                "windows": windows,
                "today": {
                    "requests": today.len(),
                    "tokens": today.iter().map(|r| r.tokens()).sum::<u64>(),
                    "cost_usd": round2(today.iter().map(|r| r.cost).sum()),
                },
                "models": models.into_iter().map(|(_, m)| m).collect::<Vec<_>>(),
            }),
        );
    }
    json!({"v": 1, "generated_at": now.timestamp(), "tools": tools})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_reset_text_rolls_into_next_year() {
        let reference = Utc.with_ymd_and_hms(2026, 12, 30, 12, 0, 0).unwrap();
        let t = parse_claude_reset("Jan 2, 3:59pm (UTC)", reference).unwrap();
        assert_eq!(t, Utc.with_ymd_and_hms(2027, 1, 2, 16, 0, 0).unwrap());
        assert_eq!(
            parse_claude_reset("Oct 13, 9am (UTC)", reference)
                .unwrap()
                .format("%m-%d %H:%M")
                .to_string(),
            "10-13 09:00"
        );
        assert!(parse_claude_reset("Oct 13, 9am", reference).is_none());
    }
}
