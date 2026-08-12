use std::collections::VecDeque;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local};

use replica::config::{self, DashboardConfig, expand_path, parse_duration};
use replica::engine::{Engine, Event, Hook, Report, RowView};
use replica::status::{Change, Status};
use replica::store::Store;

// Usage: replica --config <path> [--once]
// Runs resolution cycles at the configured interval; --once runs a single cycle and exits.
fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut config_path: Option<PathBuf> = None;
    let mut once = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--config" => {
                i += 1;
                config_path = Some(expand_path(args.get(i).context("--config needs a path")?));
            }
            "--once" => once = true,
            other => bail!("unknown argument {other:?}"),
        }
        i += 1;
    }
    let config_path = config_path.context("usage: replica --config <path> [--once]")?;

    let yaml = fs::read_to_string(&config_path).with_context(|| format!("reading {}", config_path.display()))?;
    let (settings, relations, logs) = config::load(&yaml)?;
    let interval = parse_duration(&settings.interval)?;
    let db_path = expand_path(settings.state_db.as_deref().context("config needs a top-level state_db")?);
    let store = Store::open(&db_path)?;

    // stderr hook plus one file hook per relation that configured a log path
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

    let engine = Arc::new(Mutex::new(Engine::new(store, relations, hooks)));
    if let Some(dash) = &settings.dashboard {
        serve_dashboard(engine.clone(), events, runtime.clone(), dash.clone());
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

// Facts about the running process, shared with the dashboard thread.
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

// One remembered event, timestamped at emission.
struct Ev {
    at: SystemTime,
    kind: &'static str,
    relation: String,
    text: String,
}

impl Ev {
    fn from(e: &Event) -> Ev {
        let (kind, relation, text) = match e {
            Event::Resolved { relation, identifier, ops } => ("resolved", relation, format!("{identifier} ({ops} ops)")),
            Event::Skipped { relation, identifier, reason } => ("skipped", relation, format!("{identifier}: {reason}")),
            Event::Pending { relation, identifier, reason } => ("pending", relation, format!("{identifier}: {reason}")),
            Event::Errored { relation, identifier, error } => ("error", relation, format!("{identifier}: {error}")),
            Event::StaleIntent { relation, identifier, detail } => ("stale intent", relation, format!("{identifier}: {detail}")),
        };
        Ev { at: SystemTime::now(), kind, relation: relation.clone(), text }
    }
}

fn event_relation(e: &Event) -> &str {
    match e {
        Event::Resolved { relation, .. }
        | Event::Skipped { relation, .. }
        | Event::Pending { relation, .. }
        | Event::Errored { relation, .. }
        | Event::StaleIntent { relation, .. } => relation,
    }
}

fn render_line(e: &Event) -> String {
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs();
    match e {
        Event::Resolved { relation, identifier, ops } => format!("{now} {relation} {identifier} resolved ({ops} ops)"),
        Event::Skipped { relation, identifier, reason } => format!("{now} {relation} {identifier} skipped: {reason}"),
        Event::Pending { relation, identifier, reason } => format!("{now} {relation} {identifier} pending: {reason}"),
        Event::Errored { relation, identifier, error } => format!("{now} {relation} {identifier} error: {error}"),
        Event::StaleIntent { relation, identifier, detail } => format!("{now} {relation} {identifier} STALE INTENT (previous run died mid-operation): {detail}"),
    }
}

// A loopback-only, dependency-free HTTP server: every request observes the world fresh.
// Locking the engine means a request during a running cycle waits for it to finish.
fn serve_dashboard(engine: Arc<Mutex<Engine>>, events: Arc<Mutex<VecDeque<Ev>>>, rt: Arc<Runtime>, dash: DashboardConfig) {
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
            let path = parts.next().unwrap_or("/");

            match (method, path) {
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
                    let body = notice_page("restarting", "replica is restarting; this page reloads in 3 seconds.", true);
                    let _ = stream.write_all(response_for(&body).as_bytes());
                    let _ = stream.flush();
                    drop(stream);
                    // Taking the engine lock first means never dying mid-cycle.
                    let _guard = engine.lock().unwrap();
                    eprintln!("dashboard: restart requested; exiting nonzero for launchd to relaunch");
                    std::process::exit(1);
                }
                ("POST", "/halt") => {
                    let body = notice_page("halted", "replica has exited cleanly. launchd will not restart it until next login; start it manually with launchctl kickstart.", false);
                    let _ = stream.write_all(response_for(&body).as_bytes());
                    let _ = stream.flush();
                    drop(stream);
                    let _guard = engine.lock().unwrap();
                    eprintln!("dashboard: halt requested; exiting cleanly");
                    std::process::exit(0);
                }
                _ => {
                    let (report, db_stats, intents) = {
                        let eng = engine.lock().unwrap();
                        (eng.report(), eng.store.stats().ok(), eng.store.stale_intents().unwrap_or_default())
                    };
                    let recent: Vec<EvView> = events.lock().unwrap().iter().rev().map(EvView::from).collect();
                    let body = render_page(&report, &recent, &rt, &dash, db_stats, &intents);
                    let _ = stream.write_all(response_for(&body).as_bytes());
                }
            }
        }
    });
}

fn response_for(body: &str) -> String {
    response_with("text/html; charset=utf-8", body)
}

fn response_with(content_type: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

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

struct EvView {
    age: String,
    kind: &'static str,
    relation: String,
    text: String,
}

impl EvView {
    fn from(e: &Ev) -> EvView {
        EvView { age: fmt_ago(e.at), kind: e.kind, relation: e.relation.clone(), text: e.text.clone() }
    }
}

// ---- formatting helpers ----

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn tilde(p: &std::path::Path) -> String {
    tilde_str(&p.display().to_string())
}

fn tilde_str(s: &str) -> String {
    match std::env::var("HOME") {
        Ok(h) if s.starts_with(&h) => format!("~{}", &s[h.len()..]),
        _ => s.into(),
    }
}

fn fmt_clock(t: SystemTime) -> String {
    DateTime::<Local>::from(t).format("%H:%M:%S").to_string()
}

fn fmt_stamp(t: SystemTime) -> String {
    DateTime::<Local>::from(t).format("%b %-d, %H:%M").to_string()
}

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

fn fmt_ago(t: SystemTime) -> String {
    match SystemTime::now().duration_since(t) {
        Ok(d) => format!("{} ago", fmt_dur(d)),
        Err(_) => "now".into(),
    }
}

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

fn hash8(hash: &[u8; 32]) -> String {
    hash[..4].iter().map(|b| format!("{b:02x}")).collect()
}

// A row needs attention when any ref changed against the record, is unreadable,
// or the refs disagree about existence.
fn row_attention(row: &RowView) -> bool {
    let any_changed = row.cells.iter().any(|c| c.change != Change::Unchanged);
    let present = row.cells.iter().filter(|c| matches!(c.status, Status::Present(_))).count();
    let missing = row.cells.iter().filter(|c| c.status == Status::Missing).count();
    any_changed || (present > 0 && missing > 0)
}

// ---- page rendering ----

const CSS: &str = include_str!("style.css");
const FAVICON: &str = include_str!("favicon.svg");

const HEAD: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>replica</title>\
    <link rel=\"icon\" type=\"image/svg+xml\" href=\"/favicon.svg\">\
    <link rel=\"stylesheet\" href=\"/style.css\">";

fn render_page(
    report: &Report,
    recent: &[EvView],
    rt: &Runtime,
    dash: &DashboardConfig,
    db_stats: Option<(u64, u64)>,
    intents: &[(String, Vec<String>, String)],
) -> String {
    let now = SystemTime::now();
    let mut h = format!("{HEAD}</head><body>");

    let paused = rt.paused.load(Ordering::Relaxed);
    let pause_button = if paused {
        "<form method=\"post\" action=\"/resume\"><button class=\"btn\">resume</button></form>"
    } else {
        "<form method=\"post\" action=\"/pause\"><button class=\"btn warn\">pause</button></form>"
    };
    h.push_str(&format!(
        "<header><span class=\"name\">Replica</span><span class=\"faint\">{}</span>{}\
         <div class=\"ctl\">{pause_button}\
         <form method=\"post\" action=\"/restart\"><button class=\"btn\">restart</button></form>\
         <form method=\"post\" action=\"/halt\"><button class=\"btn bad\">halt</button></form></div></header>",
        rt.version,
        if paused { " <span class=\"tag warn\">paused</span>" } else { "" },
    ));

    // status strip
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
    h.push_str(&stat("State", if total_attention > 0 {
        format!("<span class=\"tag warn\">{total_attention} awaiting sync</span>")
    } else {
        "<span class=\"tag ok\">everything in sync</span>".into()
    }));
    h.push_str("</div>");

    // unfinished intents are evidence of a crash mid-write
    if !intents.is_empty() {
        h.push_str("<section><div class=\"shead\"><b class=\"bad\">Unfinished operations</b>\
                    <span class=\"dim\">a previous run stopped mid-write</span></div>");
        for (rel, ident, detail) in intents {
            h.push_str(&format!("<div class=\"bad\">{} {} — {}</div>", esc(rel), esc(&ident.join("/")), esc(detail)));
        }
        h.push_str("</section>");
    }

    // relations
    for rel in &report.relations {
        let attention = rel.rows.iter().filter(|r| row_attention(r)).count();
        let n_idents = rel.rows.len();
        let n_refs = rel.refs.len();
        let state = if rel.error.is_some() {
            String::new()
        } else if attention > 0 {
            format!("<span class=\"tag warn\">{attention} awaiting sync</span>")
        } else {
            "<span class=\"tag ok\">in sync</span>".into()
        };
        h.push_str(&format!(
            "<section><div class=\"shead\"><b>{}</b>\
             <span class=\"dim\">{n_idents} file{} across {n_refs} location{}</span>{state}</div>",
            esc(&rel.name),
            if n_idents == 1 { "" } else { "s" },
            if n_refs == 1 { "" } else { "s" },
        ));
        if let Some(e) = &rel.error {
            h.push_str(&format!("<div class=\"bad\">{}</div></section>", esc(e)));
            continue;
        }

        // labels and their refs, flowing inline: "label path, path   label path"
        h.push_str("<div class=\"locs\">");
        for label in &rel.labels {
            let mut notes = Vec::new();
            if let Some(leaf) = label.mapping.strip_prefix("fixed(").and_then(|s| s.strip_suffix(')')) {
                notes.push(format!("stored as {leaf}"));
            }
            if label.shape.starts_with("many") || label.shape.starts_with("scan") {
                notes.push(label.shape.clone());
            }
            let note = if notes.is_empty() { String::new() } else { format!(" <span class=\"faint\">— {}</span>", esc(&notes.join(", "))) };
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

        // one row per file, one column per location; pending rows first
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
            h.push_str(&format!("<tr><td>{}</td>", esc(&row.identifier)));
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

    // events
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
