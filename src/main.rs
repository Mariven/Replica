//! Run resolution cycles on a timer, and serve the dashboard on loopback.
//!
//! `replica --config <path>` runs resolution cycles at the configured interval.
//! `--once` runs a single cycle and exits.
//! `--gate` starts with the global write gate closed, regardless of the config.

use std::collections::VecDeque;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local};

use replica::archive::Archive;
use replica::config::{self, DashboardConfig, expand_path, parse_duration, parse_size};
use replica::engine::{Engine, Event, Gates, Hook, Report, RowView};
use replica::policy::Op;
use replica::status::{Change, Status};
use replica::store::Store;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut config_path: Option<PathBuf> = None;
    let mut once = false;
    let mut gate_flag = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--config" => {
                i += 1;
                config_path = Some(expand_path(args.get(i).context("--config needs a path")?));
            }
            "--once" => once = true,
            "--gate" => gate_flag = true,
            other => bail!("unknown argument {other:?}"),
        }
        i += 1;
    }
    let config_path = config_path.context("usage: replica --config <path> [--once] [--gate]")?;

    let yaml = fs::read_to_string(&config_path).with_context(|| format!("reading {}", config_path.display()))?;
    let (settings, relations, logs) = config::load(&yaml)?;
    let interval = parse_duration(&settings.interval)?;
    let db_path = expand_path(settings.state_db.as_deref().context("the config file needs a top-level state_db")?);
    let store = Store::open(&db_path)?;

    // The archive is always on; config only moves or resizes it.
    let archive_settings = settings.archive.clone();
    let archive = Archive {
        dir: archive_settings.as_ref().and_then(|a| a.dir.as_deref()).map(expand_path)
            .unwrap_or_else(|| db_path.parent().unwrap_or(Path::new(".")).join("archive")),
        cap: parse_size(archive_settings.as_ref().map(|a| a.cap.as_str()).unwrap_or("100MB"))?,
    };

    // stderr hook, plus a file hook for each relation that configured a log path
    let mut hooks: Vec<Hook> = vec![Box::new(|e: &Event| eprintln!("{}", render_line(e)))];
    for (relation, path) in logs {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let file = Mutex::new(fs::OpenOptions::new().create(true).append(true).open(&path)?);
        hooks.push(Box::new(move |e: &Event| {
            if event_relation(e) == relation {
                let mut f = file.lock().unwrap();
                let _ = writeln!(f, "{}", render_line(e));
            }
        }));
    }

    let runtime = Arc::new(Runtime {
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION"),
        started: SystemTime::now(),
        config_path,
        db_path,
        interval,
        next_cycle: Mutex::new(None),
        last_cycle: Mutex::new(None),
        cycles: AtomicU64::new(0),
        ops_applied: AtomicU64::new(0),
        errors: AtomicU64::new(0),
        paused: AtomicBool::new(false),
    });

    let events: Arc<Mutex<VecDeque<Ev>>> = Arc::default();
    if let Some(dash) = &settings.dashboard {
        let sink = events.clone();
        let cap = dash.recent_events;
        let rt = runtime.clone();
        hooks.push(Box::new(move |e: &Event| {
            match e {
                Event::Resolved { ops, .. } => { rt.ops_applied.fetch_add(*ops as u64, Ordering::Relaxed); }
                Event::Errored { .. } | Event::StaleIntent { .. } => { rt.errors.fetch_add(1, Ordering::Relaxed); }
                _ => {}
            }
            let mut q = sink.lock().unwrap();
            if q.len() == cap.max(1) {
                q.pop_front();
            }
            q.push_back(Ev::from(e));
        }));
    }

    let mut eng = Engine::new(store, relations, hooks);
    eng.archive = Some(archive.clone());
    if settings.gate || gate_flag {
        eng.gates.set_global(true);
    }
    let gates = eng.gates.clone();
    let engine = Arc::new(Mutex::new(eng));
    if let Some(dash) = &settings.dashboard {
        serve_dashboard(engine.clone(), events, runtime.clone(), dash.clone(), gates, archive);
    }

    engine.lock().unwrap().recover()?;
    loop {
        let was_paused = runtime.paused.load(Ordering::Relaxed);
        if was_paused {
            *runtime.next_cycle.lock().unwrap() = None;
        } else {
            let begun = SystemTime::now();
            let timer = Instant::now();
            engine.lock().unwrap().cycle()?;
            runtime.cycles.fetch_add(1, Ordering::Relaxed);
            *runtime.last_cycle.lock().unwrap() = Some((begun, timer.elapsed()));
            if once {
                return Ok(());
            }
            *runtime.next_cycle.lock().unwrap() = Some(SystemTime::now() + interval);
        }
        // Sleep in short ticks so a resume from the dashboard cycles immediately.
        let deadline = Instant::now() + interval;
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(500));
            if was_paused && !runtime.paused.load(Ordering::Relaxed) {
                break;
            }
        }
    }
}

/// The dashboard thread reads these: pid, version, paths, and counters.
struct Runtime {
    pid: u32,
    version: &'static str,
    started: SystemTime,
    config_path: PathBuf,
    db_path: PathBuf,
    interval: Duration,
    next_cycle: Mutex<Option<SystemTime>>,
    last_cycle: Mutex<Option<(SystemTime, Duration)>>,
    cycles: AtomicU64,
    ops_applied: AtomicU64,
    errors: AtomicU64,
    paused: AtomicBool,
}

/// An event we kept, with the time it was emitted.
struct Ev {
    at: SystemTime,
    kind: &'static str,
    relation: String,
    text: String,
}

impl Ev {
    /// Turn an engine event into a dashboard row.
    fn from(e: &Event) -> Ev {
        let (kind, relation, text) = match e {
            Event::Resolved { relation, identifier, ops } => ("resolved", relation, format!("{identifier} ({ops} ops)")),
            Event::Held { relation, identifier, detail } => ("held", relation, format!("{identifier}: would {detail}")),
            Event::Skipped { relation, identifier, reason } => ("skipped", relation, format!("{identifier}: {reason}")),
            Event::Pending { relation, identifier, reason } => ("pending", relation, format!("{identifier}: {reason}")),
            Event::Errored { relation, identifier, error } => ("error", relation, format!("{identifier}: {error}")),
            Event::StaleIntent { relation, identifier, detail } => ("stale intent", relation, format!("{identifier}: {detail}")),
        };
        Ev { at: SystemTime::now(), kind, relation: relation.clone(), text }
    }
}

/// The relation this event belongs to.
fn event_relation(e: &Event) -> &str {
    match e {
        Event::Resolved { relation, .. }
        | Event::Held { relation, .. }
        | Event::Skipped { relation, .. }
        | Event::Pending { relation, .. }
        | Event::Errored { relation, .. }
        | Event::StaleIntent { relation, .. } => relation,
    }
}

/// Format an event as a single log line.
fn render_line(e: &Event) -> String {
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs();
    match e {
        Event::Resolved { relation, identifier, ops } => format!("{now} {relation} {identifier} resolved ({ops} ops)"),
        Event::Held { relation, identifier, detail } => format!("{now} {relation} {identifier} held by gate: would {detail}"),
        Event::Skipped { relation, identifier, reason } => format!("{now} {relation} {identifier} skipped: {reason}"),
        Event::Pending { relation, identifier, reason } => format!("{now} {relation} {identifier} pending: {reason}"),
        Event::Errored { relation, identifier, error } => format!("{now} {relation} {identifier} error: {error}"),
        Event::StaleIntent { relation, identifier, detail } => format!("{now} {relation} {identifier} stale intent (previous run died mid-operation): {detail}"),
    }
}

/// Loopback HTTP server with no extra crates. Each request reads the current state.
/// If a cycle is running, the request waits for the engine lock.
fn serve_dashboard(engine: Arc<Mutex<Engine>>, events: Arc<Mutex<VecDeque<Ev>>>, rt: Arc<Runtime>, dash: DashboardConfig, gates: Arc<Gates>, archive: Archive) {
    std::thread::spawn(move || {
        let listener = match TcpListener::bind(("127.0.0.1", dash.port)) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("dashboard: cannot bind 127.0.0.1:{}: {e}", dash.port);
                return;
            }
        };
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 2048];
            let n = stream.read(&mut buf).unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]);
            let mut parts = head.split_whitespace();
            let method = parts.next().unwrap_or("");
            let target = parts.next().unwrap_or("/");
            let (path, query) = match target.split_once('?') {
                Some((p, q)) => (p, q),
                None => (target, ""),
            };
            let param = |key: &str| -> Option<String> {
                query.split('&').find_map(|pair| {
                    let (k, v) = pair.split_once('=')?;
                    (k == key).then(|| url_decode(v))
                })
            };

            match (method, path) {
                ("POST", "/gate") => {
                    // ?to=open|closed: the global gate
                    // ?rel=<name>&to=open|closed: that relation's gate
                    let closed = param("to").as_deref() == Some("closed");
                    match param("rel") {
                        Some(rel) => gates.set_relation(&rel, closed),
                        None => gates.set_global(closed),
                    }
                    let _ = stream.write_all(b"HTTP/1.1 303 See Other\r\nLocation: /\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                }
                ("GET", "/history") => {
                    let rel = param("rel").unwrap_or_default();
                    let entries = engine.lock().unwrap().store.archive_entries(&rel).unwrap_or_default();
                    let body = render_history(&rel, &entries, &archive);
                    let _ = stream.write_all(response_for(&body).as_bytes());
                }
                ("GET", "/blob") => {
                    // ?h=<hash>&as=<filename>: download an archived version
                    let hex = param("h").unwrap_or_default();
                    let name = param("as").unwrap_or_else(|| format!("{hex}.bin"));
                    match archive.read_blob(&hex) {
                        Ok(bytes) => {
                            let name = name.replace(['"', '\r', '\n', '/'], "_");
                            let header = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                                 Content-Disposition: attachment; filename=\"{name}\"\r\n\
                                 Content-Length: {}\r\nConnection: close\r\n\r\n",
                                bytes.len()
                            );
                            let _ = stream.write_all(header.as_bytes());
                            let _ = stream.write_all(&bytes);
                        }
                        Err(e) => {
                            let body = format!("not found: {e}");
                            let _ = stream.write_all(format!(
                                "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(), body
                            ).as_bytes());
                        }
                    }
                }
                ("GET", "/style.css") => {
                    let _ = stream.write_all(response_with("text/css; charset=utf-8", CSS).as_bytes());
                }
                ("GET", "/favicon.svg") => {
                    let _ = stream.write_all(response_with("image/svg+xml", FAVICON).as_bytes());
                }
                ("GET", "/status.json") => {
                    let attention: usize = {
                        let eng = engine.lock().unwrap();
                        eng.report().relations.iter().flat_map(|r| r.rows.iter()).filter(|r| row_attention(r)).count()
                    };
                    let now = SystemTime::now();
                    let epoch = |t: SystemTime| t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs();
                    let last = *rt.last_cycle.lock().unwrap();
                    let next = *rt.next_cycle.lock().unwrap();
                    let body = format!(
                        "{{\"state\":\"{}\",\"pid\":{},\"version\":\"{}\",\"uptime_secs\":{},\
                         \"last_sync\":{},\"last_sync_ms\":{},\"next_sync\":{},\
                         \"cycles\":{},\"ops_applied\":{},\"errors\":{},\"awaiting_sync\":{}}}",
                        if rt.paused.load(Ordering::Relaxed) { "paused" } else { "running" },
                        rt.pid,
                        rt.version,
                        now.duration_since(rt.started).unwrap_or_default().as_secs(),
                        last.map(|(at, _)| epoch(at).to_string()).unwrap_or("null".into()),
                        last.map(|(_, took)| took.as_millis().to_string()).unwrap_or("null".into()),
                        next.map(|at| epoch(at).to_string()).unwrap_or("null".into()),
                        rt.cycles.load(Ordering::Relaxed),
                        rt.ops_applied.load(Ordering::Relaxed),
                        rt.errors.load(Ordering::Relaxed),
                        attention,
                    );
                    let _ = stream.write_all(response_with("application/json", &body).as_bytes());
                }
                ("POST", "/pause") => {
                    rt.paused.store(true, Ordering::Relaxed);
                    let _ = stream.write_all(b"HTTP/1.1 303 See Other\r\nLocation: /\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                }
                ("POST", "/resume") => {
                    rt.paused.store(false, Ordering::Relaxed);
                    let _ = stream.write_all(b"HTTP/1.1 303 See Other\r\nLocation: /\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                }
                ("POST", "/restart") => {
                    let body = notice_page("restarting", "Replica is restarting. This page reloads in 3 seconds.", true);
                    let _ = stream.write_all(response_for(&body).as_bytes());
                    let _ = stream.flush();
                    drop(stream);
                    // Take the engine lock first so we do not exit mid-cycle.
                    let _guard = engine.lock().unwrap();
                    eprintln!("dashboard: restart requested; exiting nonzero for launchd to relaunch");
                    std::process::exit(1);
                }
                ("POST", "/halt") => {
                    let body = notice_page("halted", "Replica has exited cleanly. launchd will leave it stopped until the next login, or until you start it with launchctl kickstart.", false);
                    let _ = stream.write_all(response_for(&body).as_bytes());
                    let _ = stream.flush();
                    drop(stream);
                    let _guard = engine.lock().unwrap();
                    eprintln!("dashboard: halt requested; exiting cleanly");
                    std::process::exit(0);
                }
                _ => {
                    let (report, db_stats, intents, arch_stats) = {
                        let eng = engine.lock().unwrap();
                        (
                            eng.report(),
                            eng.store.stats().ok(),
                            eng.store.stale_intents().unwrap_or_default(),
                            eng.store.archive_stats().unwrap_or((0, 0)),
                        )
                    };
                    let recent: Vec<EvView> = events.lock().unwrap().iter().rev().map(EvView::from).collect();
                    let body = render_page(&report, &recent, &rt, &dash, db_stats, &intents, &gates, &archive, arch_stats);
                    let _ = stream.write_all(response_for(&body).as_bytes());
                }
            }
        }
    });
}

/// A 200 response with an HTML body.
fn response_for(body: &str) -> String {
    response_with("text/html; charset=utf-8", body)
}

/// An HTTP 200 with the given content type and body.
fn response_with(content_type: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

/// The history page for a relation: versions we overwrote or deleted, and which are still on disk.
/// Restore is manual. Download the file and put it back at the path shown.
fn render_history(rel: &str, entries: &[replica::store::ArchiveEntry], archive: &Archive) -> String {
    let mut h = format!("{HEAD}</head><body>");
    h.push_str(&format!(
        "<header><a class=\"mlink\" href=\"/\">&larr; back</a><span class=\"name\">Replica</span>\
         <span class=\"dim\">history of</span><b>{}</b></header>",
        esc(rel)
    ));
    if entries.is_empty() {
        h.push_str("<section><div class=\"faint\">Nothing archived for this relation yet. Versions appear here when a sync overwrites or deletes a file.</div></section>");
        return h + "</body></html>";
    }
    h.push_str(&format!(
        "<section><div class=\"shead\"><b>Saved versions</b>\
         <span class=\"dim\">{} version{}, newest first. To restore one, download it and put it at the path shown.</span></div>",
        entries.len(),
        if entries.len() == 1 { "" } else { "s" },
    ));
    h.push_str("<table><tr><th>when</th><th>file</th><th>path</th><th>reason</th><th>content</th><th></th></tr>");
    for e in entries {
        let missing = !archive.dir.join(&e.hash).exists();
        h.push_str(&format!(
            "<tr><td title=\"{}\">{}</td><td>{}</td><td class=\"dim\">{}</td>\
             <td><span class=\"tag {}\">{}</span></td>\
             <td class=\"dim\" title=\"content hash {}\">{}, {}</td><td>{}</td></tr>",
            fmt_ago(e.at),
            fmt_stamp(e.at),
            esc(&e.ident),
            esc(&tilde_str(&e.path)),
            if e.reason == "deleted" { "bad" } else { "warn" },
            esc(&e.reason),
            esc(&e.hash),
            fmt_size(e.size),
            &e.hash[..8],
            if missing {
                "<span class=\"faint\">evicted</span>".to_string()
            } else {
                let name = e.path.rsplit('/').next().unwrap_or(&e.ident);
                format!(
                    "<a class=\"mlink\" href=\"/blob?h={}&as={}\">download</a>",
                    esc(&e.hash),
                    url_encode(name),
                )
            },
        ));
    }
    h.push_str("</table></section></body></html>");
    h
}

/// A short page shown after restart or halt.
fn notice_page(title: &str, text: &str, reload: bool) -> String {
    format!(
        "{HEAD}{}</head>\
         <body><header><span class=\"name\">Replica</span><span class=\"dim\">{}</span></header>\
         <section>{}</section></body></html>",
        if reload { "<meta http-equiv=\"refresh\" content=\"3;url=/\">" } else { "" },
        esc(title),
        esc(text),
    )
}

/// An event ready to print on the activity list.
struct EvView {
    age: String,
    kind: &'static str,
    relation: String,
    text: String,
}

impl EvView {
    /// Fill in the age, kind, relation, and text for the activity list.
    fn from(e: &Ev) -> EvView {
        EvView { age: fmt_ago(e.at), kind: e.kind, relation: e.relation.clone(), text: e.text.clone() }
    }
}

/// Escape `&`, `<`, and `>` for HTML text.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Decode `application/x-www-form-urlencoded` (query strings).
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    Ok(b) => { out.push(b); i += 2; }
                    Err(_) => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encode a filename for a query value.
fn url_encode(s: &str) -> String {
    s.bytes().map(|b| match b {
        b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
        _ => format!("%{b:02X}"),
    }).collect()
}

/// Display `p` with `$HOME` replaced by `~`.
fn tilde(p: &std::path::Path) -> String {
    tilde_str(&p.display().to_string())
}

/// Display a path string with `$HOME` replaced by `~`.
fn tilde_str(s: &str) -> String {
    match std::env::var("HOME") {
        Ok(h) if s.starts_with(&h) => format!("~{}", &s[h.len()..]),
        _ => s.into(),
    }
}

/// Local clock time, `HH:MM:SS`.
fn fmt_clock(t: SystemTime) -> String {
    DateTime::<Local>::from(t).format("%H:%M:%S").to_string()
}

/// Local date and time, `Mon D, HH:MM`.
fn fmt_stamp(t: SystemTime) -> String {
    DateTime::<Local>::from(t).format("%b %-d, %H:%M").to_string()
}

/// Compact duration (`2d 3h`, `4m 12s`, `800ms`).
fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs();
    let two = |big: u64, big_u: &str, small: u64, small_u: &str| {
        if small > 0 { format!("{big}{big_u} {small}{small_u}") } else { format!("{big}{big_u}") }
    };
    if s >= 86400 {
        two(s / 86400, "d", (s % 86400) / 3600, "h")
    } else if s >= 3600 {
        two(s / 3600, "h", (s % 3600) / 60, "m")
    } else if s >= 60 {
        two(s / 60, "m", s % 60, "s")
    } else if s > 0 {
        format!("{s}s")
    } else {
        format!("{}ms", d.as_millis())
    }
}

/// How long ago `t` was, or `now` if it is in the future.
fn fmt_ago(t: SystemTime) -> String {
    match SystemTime::now().duration_since(t) {
        Ok(d) => format!("{} ago", fmt_dur(d)),
        Err(_) => "now".into(),
    }
}

/// Human-readable byte count (`1.5 MB`).
fn fmt_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

/// First four bytes of a content hash, hex.
fn hash8(hash: &[u8; 32]) -> String {
    hash[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// Label alone if it is unique among these refs, otherwise `label (last/two/path/parts)`.
fn ref_short(refs: &[replica::engine::RefView], id: &str) -> String {
    match refs.iter().find(|r| r.id == id) {
        Some(r) => {
            if refs.iter().filter(|o| o.label == r.label).count() > 1 {
                let tail: PathBuf = r.root.iter().rev().take(2).collect::<Vec<_>>().into_iter().rev().collect();
                format!("{} ({})", r.label, tail.display())
            } else {
                r.label.clone()
            }
        }
        None => id.into(),
    }
}

/// Phrase a plan, e.g. `copy source → dest (a), dest (b); delete at dest (c)`.
fn describe_plan(refs: &[replica::engine::RefView], ops: &[Op]) -> String {
    let mut copies: Vec<(String, Vec<String>)> = Vec::new();
    let mut deletes: Vec<String> = Vec::new();
    for op in ops {
        match op {
            Op::Copy { from, to } => {
                let donor = ref_short(refs, from);
                let dest = ref_short(refs, to);
                match copies.iter_mut().find(|(d, _)| *d == donor) {
                    Some((_, dests)) => dests.push(dest),
                    None => copies.push((donor, vec![dest])),
                }
            }
            Op::Delete { at } => deletes.push(ref_short(refs, at)),
        }
    }
    let mut parts: Vec<String> = copies.into_iter()
        .map(|(donor, dests)| format!("copy {donor} \u{2192} {}", dests.join(", ")))
        .collect();
    if !deletes.is_empty() {
        parts.push(format!("delete at {}", deletes.join(", ")));
    }
    parts.join("; ")
}

/// This row needs attention: a ref changed, a ref is unreadable, or some refs have the file and others do not.
fn row_attention(row: &RowView) -> bool {
    let any_changed = row.cells.iter().any(|c| c.change != Change::Unchanged);
    let present = row.cells.iter().filter(|c| matches!(c.status, Status::Present(_))).count();
    let missing = row.cells.iter().filter(|c| c.status == Status::Missing).count();
    any_changed || (present > 0 && missing > 0)
}

const CSS: &str = include_str!("style.css");
const FAVICON: &str = include_str!("favicon.svg");

const HEAD: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>replica</title>\
    <link rel=\"icon\" type=\"image/svg+xml\" href=\"/favicon.svg\">\
    <link rel=\"stylesheet\" href=\"/style.css\">";

/// The writing / gated switch. Pass `None` for the global gate. The current side is lit and not clickable. The other side is the button.
fn gate_switch(action_rel: Option<&str>, gated: bool, disabled_note: Option<&str>) -> String {
    let target = |to: &str| match action_rel {
        Some(rel) => format!("/gate?rel={}&to={to}", url_encode(rel)),
        None => format!("/gate?to={to}"),
    };
    if let Some(note) = disabled_note {
        return format!("<span class=\"seg off\" title=\"{}\"><span class=\"segseat\">writing</span><span class=\"segseat\">gated</span></span>", esc(note));
    }
    let (writing, gated_seg) = if gated {
        (
            format!("<form method=\"post\" action=\"{}\"><button class=\"segbtn\" title=\"Open the gate. The next sync will apply pending changes.\">writing</button></form>", target("open")),
            "<span class=\"segseat lit warn\">gated</span>".to_string(),
        )
    } else {
        (
            "<span class=\"segseat lit ok\">writing</span>".to_string(),
            format!("<form method=\"post\" action=\"{}\"><button class=\"segbtn\" title=\"Close the gate. Replica will keep planning and reporting, but will not write files.\">gated</button></form>", target("closed")),
        )
    };
    format!("<span class=\"seg\">{writing}{gated_seg}</span>")
}

/// Build the HTML for the dashboard.
fn render_page(
    report: &Report,
    recent: &[EvView],
    rt: &Runtime,
    dash: &DashboardConfig,
    db_stats: Option<(u64, u64)>,
    intents: &[(String, Vec<String>, String)],
    gates: &Gates,
    archive: &Archive,
    arch_stats: (u64, u64),
) -> String {
    let now = SystemTime::now();
    let mut h = format!("{HEAD}</head><body>");

    let paused = rt.paused.load(Ordering::Relaxed);
    let globally_gated = gates.globally_gated();
    let pause_button = if paused {
        "<form method=\"post\" action=\"/resume\"><button class=\"btn\">resume</button></form>"
    } else {
        "<form method=\"post\" action=\"/pause\"><button class=\"btn warn\">pause</button></form>"
    };
    h.push_str(&format!(
        "<header><span class=\"name\">Replica</span><span class=\"faint\">{}</span>{}{}\
         <div class=\"ctl\"><span class=\"seglab\">all writes</span>{}\
         {pause_button}\
         <form method=\"post\" action=\"/restart\"><button class=\"btn\">restart</button></form>\
         <form method=\"post\" action=\"/halt\"><button class=\"btn bad\">halt</button></form></div></header>",
        rt.version,
        if paused { " <span class=\"tag warn\">paused</span>" } else { "" },
        if globally_gated { " <span class=\"tag warn\" title=\"No files will be written until the gate is opened.\">all writes gated</span>" } else { "" },
        gate_switch(None, globally_gated, None),
    ));

    // Status strip
    let db_size = fs::metadata(&rt.db_path).map(|m| fmt_size(m.len())).unwrap_or_else(|_| "?".into());
    let (rec_rows, int_rows) = db_stats.unwrap_or((0, 0));
    let uptime = now.duration_since(rt.started).unwrap_or_default();
    let ops = rt.ops_applied.load(Ordering::Relaxed);
    let errors = rt.errors.load(Ordering::Relaxed);
    let last = *rt.last_cycle.lock().unwrap();
    let next = *rt.next_cycle.lock().unwrap();
    let total_attention: usize = report.relations.iter().flat_map(|r| &r.rows).filter(|r| row_attention(r)).count();

    fn stat(k: &str, v: String) -> String {
        format!("<div class=\"stat\"><div class=\"k\">{k}</div><div class=\"v\">{v}</div></div>")
    }
    h.push_str("<div class=\"strip\">");
    h.push_str(&stat("Process", format!("pid {}, up {}", rt.pid, fmt_dur(uptime))));
    h.push_str(&stat("Configuration", esc(&tilde(&rt.config_path))));
    let intents_note = if int_rows > 0 { format!(", <span class=\"bad\">{int_rows} unfinished intents</span>") } else { String::new() };
    h.push_str(&stat("Database", format!("{}, {db_size}, {rec_rows} records{intents_note}", esc(&tilde(&rt.db_path)))));
    h.push_str(&stat("Last sync", match last {
        Some((at, took)) => format!("{}, took {}", fmt_clock(at), fmt_dur(took)),
        None => "not yet".into(),
    }));
    h.push_str(&stat("Next sync", match next {
        Some(at) => {
            let until = at.duration_since(now).map(fmt_dur).unwrap_or_else(|_| "0s".into());
            format!("{} <span class=\"dim\">(in {until})</span>", fmt_clock(at))
        }
        None => if paused { "<span class=\"warn\">paused</span>".into() } else { "running now".into() },
    }));
    let error_note = if errors > 0 { format!(", <span class=\"bad\">{errors} errors</span>") } else { String::new() };
    h.push_str(&stat("This run", format!("{ops} file operations{error_note}")));
    let (arch_rows, arch_bytes) = arch_stats;
    h.push_str(&stat("Archive", format!(
        "{arch_rows} saved version{}, {} of {}",
        if arch_rows == 1 { "" } else { "s" },
        fmt_size(arch_bytes),
        fmt_size(archive.cap),
    )));
    h.push_str(&stat("State", if total_attention > 0 {
        format!("<span class=\"tag warn\">{total_attention} awaiting sync</span>")
    } else {
        "<span class=\"tag ok\">everything in sync</span>".into()
    }));
    h.push_str("</div>");

    // Unfinished intents are evidence of a crash mid-write.
    if !intents.is_empty() {
        h.push_str("<section><div class=\"shead\"><b class=\"bad\">Unfinished operations</b>\
                    <span class=\"dim\">A previous run stopped mid-write.</span></div>");
        for (rel, ident, detail) in intents {
            h.push_str(&format!("<div class=\"bad\">{} {}: {}</div>", esc(rel), esc(&ident.join("/")), esc(detail)));
        }
        h.push_str("</section>");
    }

    for rel in &report.relations {
        let attention = rel.rows.iter().filter(|r| row_attention(r)).count();
        let effective_gated = globally_gated || rel.gated;
        let n_idents = rel.rows.len();
        let n_refs = rel.refs.len();
        let state = if rel.error.is_some() {
            String::new()
        } else if attention > 0 && effective_gated {
            format!("<span class=\"tag warn\">{attention} held by gate</span>")
        } else if attention > 0 {
            format!("<span class=\"tag warn\">{attention} awaiting sync</span>")
        } else {
            "<span class=\"tag ok\">in sync</span>".into()
        };
        let controls = format!(
            "<span class=\"sctl\"><a class=\"mlink\" href=\"/history?rel={}\" title=\"Overwritten and deleted versions kept in the archive.\">history</a>{}</span>",
            url_encode(&rel.name),
            gate_switch(
                Some(&rel.name),
                rel.gated,
                globally_gated.then_some("The global gate is closed, so this switch has no effect until the global gate opens."),
            ),
        );
        h.push_str(&format!(
            "<section><div class=\"shead\"><b>{}</b>\
             <span class=\"dim\">{n_idents} file{} across {n_refs} location{}</span>{state}{controls}</div>",
            esc(&rel.name),
            if n_idents == 1 { "" } else { "s" },
            if n_refs == 1 { "" } else { "s" },
        ));
        if let Some(e) = &rel.error {
            h.push_str(&format!("<div class=\"bad\">{}</div></section>", esc(e)));
            continue;
        }

        // Labels and their refs, flowing inline: "label path, path   label path"
        h.push_str("<div class=\"locs\">");
        for label in &rel.labels {
            let mut notes = Vec::new();
            if let Some(leaf) = label.mapping.strip_prefix("fixed(").and_then(|s| s.strip_suffix(')')) {
                notes.push(format!("stored as {leaf}"));
            }
            if label.shape.starts_with("many") || label.shape.starts_with("scan") {
                notes.push(label.shape.clone());
            }
            let note = if notes.is_empty() { String::new() } else { format!(" <span class=\"faint\">({})</span>", esc(&notes.join(", "))) };
            let paths: Vec<String> = rel.refs.iter().filter(|r| r.label == label.name).map(|rf| {
                let shown = match &rf.pattern {
                    Some(p) => tilde_str(p),
                    None => tilde(&rf.root),
                };
                format!(
                    "{}{}",
                    esc(&shown),
                    if rf.root_exists { "" } else { " <span class=\"warn\">(folder does not exist)</span>" },
                )
            }).collect();
            h.push_str(&format!(
                "<span class=\"loc\"><b>{}</b>{note} <span class=\"path\">{}</span></span>",
                esc(&label.name),
                paths.join(", "),
            ));
        }
        h.push_str("</div>");

        // A row per file and a column per location. Pending rows come first.
        if rel.rows.is_empty() {
            h.push_str("<div class=\"faint\" style=\"margin-top:6px\">no files yet</div></section>");
            continue;
        }
        h.push_str("<table><tr><th>file</th>");
        for rf in &rel.refs {
            let tail: PathBuf = rf.root.iter().rev().take(2).collect::<Vec<_>>().into_iter().rev().collect();
            h.push_str(&format!("<th>{} <span class=\"faint\">{}</span></th>", esc(&rf.label), esc(&tail.display().to_string())));
        }
        h.push_str("</tr>");

        const SYNCED_ROW_CAP: usize = 50;
        let (pending, in_sync): (Vec<&RowView>, Vec<&RowView>) = rel.rows.iter().partition(|r| row_attention(r));
        let shown = in_sync.len().min(SYNCED_ROW_CAP);
        for row in pending.iter().chain(in_sync.iter().take(SYNCED_ROW_CAP)) {
            // The exact plan the next ungated sync applies.
            let plan_html = if !row.plan.is_empty() {
                let verb = if effective_gated { "held" } else { "next sync" };
                format!(
                    "<div class=\"plan {}\"><span class=\"plab\">{verb}</span>{}</div>",
                    if effective_gated { "warn" } else { "" },
                    esc(&describe_plan(&rel.refs, &row.plan)),
                )
            } else if let Some(note) = &row.note {
                format!("<div class=\"plan\"><span class=\"plab\">waiting</span>{}</div>", esc(note))
            } else {
                String::new()
            };
            h.push_str(&format!("<tr><td>{}{plan_html}</td>", esc(&row.identifier)));
            for cell in &row.cells {
                match &cell.status {
                    Status::Present(snap) => {
                        let (word, cls) = match cell.change {
                            Change::Unchanged => ("in sync", "ok"),
                            Change::Edited => ("edited", "warn"),
                            Change::Appeared => ("new", "warn"),
                            _ => ("changed", "warn"),
                        };
                        h.push_str(&format!(
                            "<td title=\"content hash {}\"><span class=\"tag {cls}\">{word}</span>\
                             <div class=\"meta\">{}, {}</div></td>",
                            hash8(&snap.hash),
                            fmt_size(snap.size),
                            fmt_stamp(snap.mtime),
                        ));
                    }
                    Status::Missing => {
                        if cell.change == Change::Disappeared {
                            h.push_str("<td><span class=\"tag bad\">deleted</span></td>");
                        } else {
                            h.push_str("<td><span class=\"faint\">absent</span></td>");
                        }
                    }
                    Status::Unreadable(e) => {
                        h.push_str(&format!("<td><span class=\"tag bad\">unreadable</span><div class=\"meta\">{}</div></td>", esc(e)));
                    }
                }
            }
            h.push_str("</tr>");
        }
        h.push_str("</table>");
        if in_sync.len() > shown {
            h.push_str(&format!("<div class=\"more\">and {} more in sync</div>", in_sync.len() - shown));
        }
        h.push_str("</section>");
    }

    h.push_str("<section><div class=\"shead\"><b>Activity</b></div>");
    if recent.is_empty() {
        h.push_str("<div class=\"faint\" style=\"margin-top:4px\">nothing yet</div>");
    } else {
        for ev in recent {
            let cls = match ev.kind {
                "resolved" => "ok",
                "error" | "stale intent" => "bad",
                "pending" => "warn",
                _ => if ev.text.contains("conflict") { "warn" } else { "dim" },
            };
            h.push_str(&format!(
                "<div class=\"ev\"><span class=\"age\">{}</span><span class=\"kind tag {cls}\">{}</span>\
                 <span class=\"rel\">{}</span><span class=\"dim\">{}</span></div>",
                esc(&ev.age), esc(ev.kind), esc(&ev.relation), esc(&ev.text)
            ));
        }
    }
    h.push_str("</section>");

    // Refresh by swapping the body in place: no page load, so no tab flash.
    h.push_str(&format!(
        "<script>setInterval(async()=>{{try{{\
         const r=await fetch('/',{{cache:'no-store'}});\
         const d=new DOMParser().parseFromString(await r.text(),'text/html');\
         document.body.innerHTML=d.body.innerHTML;\
         }}catch(e){{}}}},{});</script>",
        dash.refresh.max(1) * 1000
    ));
    h.push_str("</body></html>");
    h
}
