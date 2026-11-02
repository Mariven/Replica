//! The body of `status.json`, with the process state, the gate, counters, timing, the watch state, and an entry for each relation.

use std::sync::atomic::Ordering;
use std::time::SystemTime;

use crate::engine::Report;
use crate::store::to_epoch_ms;

use super::http::json_str;
use super::{Runtime, Snapshot};

/// Seconds since the Unix epoch.
pub fn epoch_secs(t: SystemTime) -> i64 {
    to_epoch_ms(t) / 1000
}

/// Count the rows awaiting sync and the rows held by a gate, summed over all relations.
pub fn totals(report: &Report) -> (usize, usize) {
    report.relations.iter().map(|r| r.counts(report.globally_gated)).fold((0, 0), |(a, h), (x, y)| (a + x, h + y))
}

/// Build `status.json` from a snapshot and the process counters. `stale` marks a report left over from the previous cycle.
pub fn status_json(snap: &Snapshot, stale: bool, rt: &Runtime) -> String {
    let report = &snap.report;
    let now = SystemTime::now();
    let last = *rt.last_cycle.lock().unwrap();
    let next = *rt.next_cycle.lock().unwrap();
    let globally_gated = report.globally_gated;
    let (awaiting, held) = totals(report);
    let dirty: Vec<String> = rt.dirty.lock().unwrap().iter().map(|r| json_str(r)).collect();
    let watched: Vec<String> = rt.watched_roots.lock().unwrap().iter().map(|p| json_str(&p.display().to_string())).collect();
    let relations: Vec<String> = report.relations.iter().map(|r| {
        let state = r.state(globally_gated);
        let (awaiting, held) = r.counts(globally_gated);
        format!(
            "{{\"name\":{},\"state\":{},\"awaiting\":{awaiting},\"held\":{held},\"gated\":{},\"files\":{},\"replicas\":{}}}",
            json_str(&r.name), json_str(state.word()), globally_gated || r.gated, r.rows.len(), r.replicas.len(),
        )
    }).collect();
    let opt = |v: Option<String>| v.unwrap_or_else(|| "null".into());
    format!(
        "{{\"state\":\"{}\",\"gate\":\"{}\",\"pid\":{},\"version\":\"{}\",\"uptime_secs\":{},\
         \"last_sync\":{},\"last_sync_took_ms\":{},\"next_sync\":{},\"report_at\":{},\"report_stale\":{stale},\
         \"cycles\":{},\"ops_applied\":{},\"errors\":{},\"awaiting_sync\":{awaiting},\"held\":{held},\
         \"watch\":{{\"enabled\":{},\"roots\":[{}],\"dirty\":[{}]}},\"relations\":[{}]}}",
        if rt.paused.load(Ordering::Relaxed) { "paused" } else { "running" },
        if globally_gated { "closed" } else { "open" },
        rt.pid,
        rt.version,
        now.duration_since(rt.started).unwrap_or_default().as_secs(),
        opt(last.map(|(at, _)| epoch_secs(at).to_string())),
        opt(last.map(|(_, took)| took.as_millis().to_string())),
        opt(next.map(|at| epoch_secs(at).to_string())),
        epoch_secs(snap.at),
        rt.cycles.load(Ordering::Relaxed),
        rt.ops_applied.load(Ordering::Relaxed),
        rt.errors.load(Ordering::Relaxed),
        rt.watching.load(Ordering::Relaxed),
        watched.join(","),
        dirty.join(","),
        relations.join(","),
    )
}
