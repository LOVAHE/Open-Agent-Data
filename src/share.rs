//! The community upload: finished 5-minute slots, one point per tool and
//! model, recorded after `ai-usage login`.

use crate::account::Account;
use crate::report::{Req, load_requests};
use crate::store::{Res, state_dir};
use chrono::Utc;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::fs;

pub const SLOT: i64 = 300;
/// OTLP exporters batch for a few seconds; a slot is closed this long after
/// it ends.
const GRACE: i64 = 60;
const BATCH: usize = 200;
const CURSOR: &str = "share-cursor";

/// Speeds and TTFTs of the requests in one slot.
type Samples = (Vec<f64>, Vec<f64>);

#[derive(Debug, PartialEq)]
pub struct Point {
    pub tool: String,
    pub model: String,
    pub t: i64,
    pub n: usize,
    pub tps: f64,
    pub ttft: i64,
}

impl Point {
    fn json(&self) -> Value {
        json!({"tool": self.tool, "model": self.model, "t": self.t, "n": self.n, "tps": self.tps, "ttft": self.ttft})
    }
}

pub fn median(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    let m = v.len() / 2;
    if v.len() % 2 == 1 {
        v[m]
    } else {
        (v[m - 1] + v[m]) / 2.0
    }
}

pub fn points(reqs: &[Req], bench: &HashSet<String>, from: i64, to: i64) -> Vec<Point> {
    let mut slots: BTreeMap<(i64, String, String), Samples> = BTreeMap::new();
    for r in reqs {
        let ts = r.ts.timestamp();
        if ts < from || ts >= to || r.instance == "bench" || bench.contains(&r.session) {
            continue;
        }
        let (Some(tps), Some(ttft)) = (r.gen_tps(), r.ttft_ms) else {
            continue;
        };
        let slot = slots
            .entry((ts.div_euclid(SLOT) * SLOT, r.tool.clone(), r.model.clone()))
            .or_default();
        slot.0.push(tps);
        slot.1.push(ttft as f64);
    }
    slots
        .into_iter()
        .map(|((t, tool, model), (mut tps, mut ttft))| Point {
            tool,
            model,
            t,
            n: tps.len(),
            tps: (median(&mut tps) * 10.0).round() / 10.0,
            ttft: median(&mut ttft).round() as i64,
        })
        .collect()
}

fn cursor() -> Option<i64> {
    fs::read_to_string(state_dir().join(CURSOR)).ok()?.trim().parse().ok()
}

/// The upload due now: from the cursor (or the login) to the last closed slot.
fn pending(account: &Account) -> (i64, i64, Vec<Point>) {
    let first = account.since.div_euclid(SLOT) * SLOT + SLOT;
    let from = cursor().map_or(first, |c| c.max(first));
    let to = (Utc::now().timestamp() - GRACE).div_euclid(SLOT) * SLOT;
    let pts = if to > from {
        points(&load_requests(), &crate::bench::sessions(), from, to)
    } else {
        Vec::new()
    };
    (from, to, pts)
}

pub fn upload(account: &Account) -> Res<usize> {
    let (from, to, pts) = pending(account);
    if to <= from {
        return Ok(0);
    }
    let client = format!("ai-usage/{}", env!("CARGO_PKG_VERSION"));
    let mut accepted = 0;
    for chunk in pts.chunks(BATCH) {
        let body = json!({"client": client, "points": chunk.iter().map(Point::json).collect::<Vec<_>>()});
        let reply = account.request("POST", "/api/v1/points", Some(&body))?;
        accepted += reply["accepted"].as_u64().unwrap_or(0) as usize;
    }
    fs::write(state_dir().join(CURSOR), format!("{to}\n"))?;
    Ok(accepted)
}

pub fn run(args: &[String]) -> Res<()> {
    let account = Account::load().ok_or("not logged in; run `ai-usage login` to start sharing")?;
    if args.iter().any(|a| a == "--dry-run") {
        let (_, _, pts) = pending(&account);
        let body = json!({"points": pts.iter().map(Point::json).collect::<Vec<_>>()});
        println!("{}", serde_json::to_string_pretty(&body)?);
        return Ok(());
    }
    println!("{} points accepted", upload(&account)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn req(ts: i64, model: &str, output: u64, gen_ms: u64, ttft: u64) -> Req {
        Req {
            ts: Utc.timestamp_opt(ts, 0).unwrap(),
            tool: "claude".into(),
            instance: "other".into(),
            session: String::new(),
            model: model.into(),
            effort: "-".into(),
            input: 0,
            output,
            cache_read: 0,
            cache_write: 0,
            cost: 0.0,
            duration_ms: ttft + gen_ms,
            ttft_ms: Some(ttft),
        }
    }

    #[test]
    fn slots_keep_their_own_speed() {
        let base = 1_000_000_200;
        let reqs = [
            req(base + 10, "opus", 1000, 10_000, 800),
            req(base + 20, "opus", 3000, 10_000, 1200),
            req(base + 40, "opus", 100, 10_000, 500),
            req(base + 2400, "opus", 6000, 10_000, 700),
        ];
        let pts = points(&reqs, &HashSet::new(), base, base + 3600);
        assert_eq!(pts.len(), 2);
        assert_eq!((pts[0].t, pts[0].n, pts[0].tps, pts[0].ttft), (base, 2, 200.0, 1000));
        assert_eq!((pts[1].t, pts[1].n, pts[1].tps), (base + 2400, 1, 600.0));
    }

    #[test]
    fn bench_runs_and_out_of_range_rows_stay_home() {
        let base = 1_000_000_200;
        let mut benched = req(base + 10, "opus", 1000, 10_000, 800);
        benched.session = "s1".into();
        let mut tagged = req(base + 20, "opus", 1000, 10_000, 800);
        tagged.instance = "bench".into();
        let late = req(base + 400, "opus", 1000, 10_000, 800);
        let bench: HashSet<String> = ["s1".to_string()].into();
        assert!(points(&[benched, tagged, late], &bench, base, base + SLOT).is_empty());
    }
}
