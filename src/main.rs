//! Execute scheduled resolution cycles while hosting the loopback status dashboard.
//! Pass `--config <path>` (or `--config=<path>`) to run cycles at the configured interval.
//! Pass `--once` to run a cycle before exiting.
//! Pass `--gate` to start with the global write gate closed.
//! Pass `--check` to load the config, expand its refs, run the overlap check, and exit.

use std::collections::VecDeque;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local};

use replica::archive::Archive;
use replica::config::{self, DashboardConfig, expand_path};
use replica::engine::{self, Engine, Event, Gates, Hook, RelationState, Report, RowView, STOP, Tone};
use replica::identifier::Identifier;
use replica::status::{Change, Status};
use replica::store::{Store, to_epoch_ms};

const USAGE: &str = "usage: replica --config <path> [--once] [--gate] [--check]\n\
  --config <path>  configuration file (also --config=<path>)\n\
  --once           run one cycle, then exit\n\
  --gate           start with the global write gate closed\n\
  --check          load the config, expand refs, check for overlapping roots, then exit\n\
  --help           print this text\n\
  --version        print the version";

/// Exit status the main loop uses once STOP is set: 0 for halt and signals, 1 for restart so launchd relaunches.
static EXIT_CODE: AtomicI32 = AtomicI32::new(0);

/// Command-line options.
#[derive(Debug)]
struct Args {
    config: PathBuf,
    once: bool,
    gate: bool,
    check: bool,
}

/// Parse the command line. Returns None after printing help or the version.
fn parse_args(args: &[String]) -> Result<Option<Args>> {
    let mut config = None;
    let mut once = false;
    let mut gate = false;
    let mut check = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(None);
            }
            "--version" | "-V" => {
                println!("replica {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--config" => {
                i += 1;
                config = Some(expand_path(args.get(i).with_context(|| format!("--config needs a path\n{USAGE}"))?));
            }
            _ if arg.starts_with("--config=") => config = Some(expand_path(&arg["--config=".len()..])),
            "--once" => once = true,
            "--gate" => gate = true,
            "--check" => check = true,
            other => bail!("unknown argument {other:?}\n{USAGE}"),
        }
        i += 1;
    }
    let config = config.with_context(|| USAGE.to_string())?;
    Ok(Some(Args { config, once, gate, check }))
}

/// Requests from the dashboard to the main loop.
enum Command {
    Sync,
    Pause,
    Resume,
    Stop,
}

/// Signal handler: only an atomic store, which is safe inside a handler. The engine checks the flag between identifiers.
extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

/// Stop cleanly on SIGTERM (launchctl bootout, logout) and SIGINT (Ctrl-C) instead of dying mid-write.
fn install_signal_handlers() {
    let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    // SAFETY: the handler touches only an atomic, and signal() with a plain function pointer is the documented use.
    unsafe {
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGINT, handler);
    }
}

/// Everything the daemon needs from a config file, resolved and checked the same way at startup, by `--check`, and before a restart.
struct Loaded {
    settings: config::Settings,
    relations: Vec<engine::Relation>,
    logs: Vec<(String, PathBuf)>,
    db_path: PathBuf,
    archive: Archive,
    reserved: Vec<PathBuf>,
    /// Problems the cycle reports and works around (expansion failures, roots nested in other roots).
    problems: Vec<String>,
}

/// Load and validate a config. A root that contains a file replica itself writes is refused here, so neither startup nor a restart proceeds on it.
fn load_config(config_path: &Path) -> Result<Loaded> {
    let yaml = fs::read_to_string(config_path).with_context(|| format!("reading {}", config_path.display()))?;
    let (settings, relations, logs) = config::load(&yaml)?;
    let db_path = settings.state_db.clone().context("the config file needs a top-level state_db")?;
    let archive = Archive {
        dir: settings.archive_dir.clone().context("the config file needs a top-level state_db")?,
        cap: settings.archive_cap,
    };
    // Paths the daemon writes itself. A root containing one of these would sync replica's own files.
    let mut reserved: Vec<PathBuf> = vec![db_path.clone(), archive.dir.clone()];
    reserved.extend(logs.iter().map(|(_, p)| p.clone()));
    let (fatal, problems): (Vec<_>, Vec<_>) = engine::check(&relations, &reserved).into_iter().partition(|p| p.reserved);
    if !fatal.is_empty() {
        bail!("{}", fatal.into_iter().map(|p| p.message).collect::<Vec<_>>().join("\n"));
    }
    Ok(Loaded { settings, relations, logs, db_path, archive, reserved, problems: problems.into_iter().map(|p| p.message).collect() })
}

/// Initialize runtime services from configuration before driving reconciliation cycles.
fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let Some(args) = parse_args(&argv)? else { return Ok(()) };
    let config_path = args.config;

    if args.check {
        let loaded = load_config(&config_path)?;
        for p in &loaded.problems {
            eprintln!("{p}");
        }
        if loaded.problems.is_empty() {
            println!("ok: {} from {}", plural(loaded.relations.len(), "relation"), config_path.display());
            return Ok(());
        }
        std::process::exit(1);
    }
    let Loaded { settings, relations, logs, db_path, archive, reserved, problems } = load_config(&config_path)?;
    let interval = settings.interval;
    // Stop cleanly from here on; the handler only stores an atomic that the engine checks between identifiers.
    install_signal_handlers();

    // Refuse to run beside another daemon on the same database, whose intent rows this one would otherwise wipe.
    let lock_path = PathBuf::from(format!("{}.lock", db_path.display()));
    if let Some(dir) = lock_path.parent() {
        fs::create_dir_all(dir)?;
    }
    let lock = fs::File::create(&lock_path).with_context(|| format!("creating {}", lock_path.display()))?;
    if let Err(e) = lock.try_lock() {
        bail!("another replica is running against {} (lock {}): {e}", db_path.display(), lock_path.display());
    }
    let _lock = lock;

    let store = Store::open(&db_path)?;

    let (tx, rx) = mpsc::channel::<Command>();
    let runtime = Arc::new(Runtime {
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION"),
        started: SystemTime::now(),
        config_path: config_path.clone(),
        db_path: db_path.clone(),
        next_cycle: Mutex::new(None),
        last_cycle: Mutex::new(None),
        cycles: AtomicU64::new(0),
        ops_applied: AtomicU64::new(0),
        errors: AtomicU64::new(0),
        paused: AtomicBool::new(false),
        cleared_intents: Mutex::new(Vec::new()),
        snapshot: Mutex::new(None),
        commands: tx,
    });

    // Stream events to standard error, count them, and attach dedicated file loggers for configured relation paths.
    let mut hooks: Vec<Hook> = vec![Box::new(|e: &Event| eprintln!("{}", render_line(e)))];
    {
        let rt = runtime.clone();
        hooks.push(Box::new(move |e: &Event| match e {
            Event::Resolved { ops, .. } => { rt.ops_applied.fetch_add(*ops as u64, Ordering::Relaxed); }
            Event::Errored { .. } | Event::StaleIntent { .. } => { rt.errors.fetch_add(1, Ordering::Relaxed); }
            _ => {}
        }));
    }
    for (relation, path) in &logs {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let file = Mutex::new(fs::OpenOptions::new().create(true).append(true).open(path).with_context(|| format!("opening log {}", path.display()))?);
        let relation = relation.clone();
        hooks.push(Box::new(move |e: &Event| {
            if e.relation() == relation {
                let mut f = file.lock().unwrap();
                let _ = writeln!(f, "{}", render_line(e));
            }
        }));
    }
    let events: Arc<Mutex<VecDeque<Ev>>> = Arc::default();
    if let Some(dash) = &settings.dashboard {
        let sink = events.clone();
        let cap = dash.recent_events;
        hooks.push(Box::new(move |e: &Event| {
            let mut q = sink.lock().unwrap();
            if q.len() == cap.max(1) {
                q.pop_front();
            }
            q.push_back(Ev::from(e));
        }));
    }

    // Build the engine with initial gate states before starting the dashboard listener.
    let mut eng = Engine::new(store, relations, hooks);
    eng.archive = Some(archive.clone());
    eng.reserved = reserved;
    if settings.gate || args.gate {
        eng.gates.set_global(true);
    }
    let gates = eng.gates.clone();
    let n_relations = eng.relations.len();
    let engine = Arc::new(Mutex::new(eng));
    eprintln!(
        "replica {} pid {}: {} from {}, state in {}, gate {}{}",
        runtime.version, runtime.pid, plural(n_relations, "relation"), tilde(&config_path), tilde(&db_path),
        if gates.globally_gated() { "closed" } else { "open" },
        match &settings.dashboard {
            Some(d) => format!(", dashboard at http://localhost:{}", d.port),
            None => String::new(),
        },
    );
    for problem in &problems {
        eprintln!("startup: {problem}");
    }
    let has_dashboard = settings.dashboard.is_some();
    if let Some(dash) = &settings.dashboard {
        serve_dashboard(Arc::new(Dashboard {
            engine: engine.clone(),
            events,
            rt: runtime.clone(),
            config: dash.clone(),
            gates,
            archive,
        }));
    }

    // Recover interrupted state before entering the reconciliation loop, keeping the cleared rows visible on the dashboard.
    let cleared = engine.lock().unwrap().recover()?;
    *runtime.cleared_intents.lock().unwrap() = cleared;

    let exit = |rt: &Runtime| -> ! {
        let code = EXIT_CODE.load(Ordering::SeqCst);
        eprintln!("replica pid {}: stopping with status {code}", rt.pid);
        std::process::exit(code)
    };
    // Run one cycle, publish a report snapshot for the dashboard, and log a summary when anything happened. Returns the error count.
    let run_cycle = |paused: bool| -> Result<u64> {
        let begun = SystemTime::now();
        let timer = Instant::now();
        let (ops_before, errors_before) = (runtime.ops_applied.load(Ordering::Relaxed), runtime.errors.load(Ordering::Relaxed));
        let eng = engine.lock().unwrap();
        eng.cycle()?;
        let n = runtime.cycles.fetch_add(1, Ordering::Relaxed) + 1;
        let took = timer.elapsed();
        *runtime.last_cycle.lock().unwrap() = Some((begun, took));
        *runtime.next_cycle.lock().unwrap() = if paused || args.once { None } else { Some(SystemTime::now() + interval) };
        if has_dashboard && !args.once {
            *runtime.snapshot.lock().unwrap() = Some(Arc::new(take_snapshot(&eng)));
        }
        drop(eng);
        let ops = runtime.ops_applied.load(Ordering::Relaxed) - ops_before;
        let errors = runtime.errors.load(Ordering::Relaxed) - errors_before;
        if ops > 0 || errors > 0 {
            eprintln!("cycle {n} done in {}: {}, {}", fmt_dur(took), plural(ops as usize, "file operation"), plural(errors as usize, "error"));
        }
        Ok(errors)
    };
    let mut paused = false;
    // A command received while draining duplicate sync requests, to be handled before waiting again.
    let mut pending: Option<Command> = None;
    loop {
        if STOP.load(Ordering::SeqCst) {
            exit(&runtime);
        }
        if !paused {
            let errors = run_cycle(false)?;
            if args.once {
                if errors > 0 {
                    eprintln!("replica: {} in the only cycle", plural(errors as usize, "error"));
                    std::process::exit(1);
                }
                return Ok(());
            }
            if STOP.load(Ordering::SeqCst) {
                exit(&runtime);
            }
            // Sync requests that piled up during the cycle are satisfied by the cycle that just ran.
            while pending.is_none() {
                match rx.try_recv() {
                    Ok(Command::Sync) => {}
                    Ok(other) => pending = Some(other),
                    Err(_) => break,
                }
            }
        }
        // Wait for the next cycle, a dashboard command, or a stop signal. Signals set STOP alone, so wake at least once a second to notice them.
        let deadline = Instant::now() + interval;
        let mut sync_now = false;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if !paused && remaining.is_zero() {
                break;
            }
            let timeout = if paused { Duration::from_secs(1) } else { remaining.min(Duration::from_secs(1)) };
            let received = match pending.take() {
                Some(cmd) => Ok(cmd),
                None => rx.recv_timeout(timeout),
            };
            match received {
                Ok(Command::Sync) => {
                    eprintln!("dashboard: sync requested");
                    sync_now = true;
                    break;
                }
                Ok(Command::Pause) => {
                    paused = true;
                    runtime.paused.store(true, Ordering::Relaxed);
                    *runtime.next_cycle.lock().unwrap() = None;
                    eprintln!("dashboard: paused");
                }
                Ok(Command::Resume) => {
                    paused = false;
                    runtime.paused.store(false, Ordering::Relaxed);
                    eprintln!("dashboard: resumed");
                    break;
                }
                Ok(Command::Stop) | Err(RecvTimeoutError::Disconnected) => exit(&runtime),
                Err(RecvTimeoutError::Timeout) => {
                    if STOP.load(Ordering::SeqCst) {
                        exit(&runtime);
                    }
                }
            }
        }
        if paused && sync_now {
            // A manual sync while paused runs one cycle, then keeps waiting.
            run_cycle(true)?;
        }
    }
}

/// Process telemetry and cycle timing shared with the dashboard server.
struct Runtime {
    pid: u32,
    version: &'static str,
    started: SystemTime,
    config_path: PathBuf,
    db_path: PathBuf,
    next_cycle: Mutex<Option<SystemTime>>,
    last_cycle: Mutex<Option<(SystemTime, Duration)>>,
    cycles: AtomicU64,
    ops_applied: AtomicU64,
    errors: AtomicU64,
    paused: AtomicBool,
    /// Intent rows found and cleared at startup, kept for the life of the process so the dashboard can show them.
    cleared_intents: Mutex<Vec<(String, Identifier, String)>>,
    /// Report published after each cycle, served while a cycle holds the engine.
    snapshot: Mutex<Option<Arc<Snapshot>>>,
    commands: Sender<Command>,
}

/// Everything a page or status body reads from the engine, taken at one moment.
struct Snapshot {
    at: SystemTime,
    report: Report,
    db_stats: std::result::Result<(u64, u64), String>,
    intents: std::result::Result<Vec<(String, Identifier, String)>, String>,
    arch_stats: std::result::Result<(u64, u64), String>,
}

/// Read a report plus store statistics from the engine.
fn take_snapshot(eng: &Engine) -> Snapshot {
    Snapshot {
        at: SystemTime::now(),
        report: eng.report(),
        db_stats: eng.store.stats().map_err(|e| format!("{e:#}")),
        intents: eng.store.stale_intents().map_err(|e| format!("{e:#}")),
        arch_stats: eng.store.archive_stats().map_err(|e| format!("{e:#}")),
    }
}

/// A fresh snapshot when the engine is idle; otherwise the one published after the last cycle, marked stale.
/// Only before the first cycle has published anything does a request wait for the engine.
fn current_snapshot(ctx: &Dashboard) -> (Arc<Snapshot>, bool) {
    match ctx.engine.try_lock() {
        Ok(eng) => {
            let snap = Arc::new(take_snapshot(&eng));
            *ctx.rt.snapshot.lock().unwrap() = Some(snap.clone());
            (snap, false)
        }
        Err(std::sync::TryLockError::WouldBlock) => {
            // Clone out of the snapshot mutex in its own statement: holding that guard while waiting for the engine would deadlock
            // against the main loop, which publishes the snapshot while it still holds the engine.
            let published = ctx.rt.snapshot.lock().unwrap().clone();
            match published {
                Some(snap) => (snap, true),
                None => {
                    let eng = ctx.engine.lock().unwrap();
                    (Arc::new(take_snapshot(&eng)), false)
                }
            }
        }
        Err(std::sync::TryLockError::Poisoned(p)) => (Arc::new(take_snapshot(&p.into_inner())), false),
    }
}

/// An event we kept, with the time it was emitted.
struct Ev {
    at: SystemTime,
    kind: &'static str,
    tone: Tone,
    relation: String,
    identifier: String,
    text: String,
}

impl Ev {
    /// Keep the parts of an engine event the activity list shows.
    fn from(e: &Event) -> Ev {
        Ev { at: SystemTime::now(), kind: e.kind(), tone: e.tone(), relation: e.relation().to_owned(), identifier: e.identifier().to_owned(), text: e.text() }
    }
}

/// Serialize an event as one log line with the local time.
fn render_line(e: &Event) -> String {
    let stamp = DateTime::<Local>::from(SystemTime::now()).format("%Y-%m-%d %H:%M:%S");
    let ident = e.identifier();
    if ident.is_empty() {
        format!("{stamp} {} {}", e.relation(), e.text())
    } else {
        format!("{stamp} {} {ident} {}", e.relation(), e.text())
    }
}

/// Everything a dashboard request handler needs.
struct Dashboard {
    engine: Arc<Mutex<Engine>>,
    events: Arc<Mutex<VecDeque<Ev>>>,
    rt: Arc<Runtime>,
    config: DashboardConfig,
    gates: Arc<Gates>,
    archive: Archive,
}

/// Listen on the loopback interface and hand each connection to its own thread, so an idle or slow client never blocks the next request.
fn serve_dashboard(ctx: Arc<Dashboard>) {
    std::thread::spawn(move || {
        let listener = match TcpListener::bind(("127.0.0.1", ctx.config.port)) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("dashboard: cannot bind 127.0.0.1:{}: {e}", ctx.config.port);
                return;
            }
        };
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let ctx = ctx.clone();
            if let Err(e) = std::thread::Builder::new().spawn(move || handle_connection(stream, &ctx)) {
                eprintln!("dashboard: cannot start a connection thread: {e}");
            }
        }
    });
}

/// Parsed request line and headers.
struct Request {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
}

impl Request {
    /// Parse the head of an HTTP/1.x request. Header names are lowercased.
    fn parse(head: &str) -> Option<Request> {
        let mut lines = head.split("\r\n").flat_map(|l| l.split('\n'));
        let mut first = lines.next()?.split_whitespace();
        let method = first.next()?.to_owned();
        let target = first.next().unwrap_or("/");
        let (path, query) = match target.split_once('?') {
            Some((p, q)) => (p.to_owned(), q.to_owned()),
            None => (target.to_owned(), String::new()),
        };
        let headers = lines.take_while(|l| !l.is_empty()).filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            Some((k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        }).collect();
        Some(Request { method, path, query, headers })
    }

    /// First header with this (lowercase) name.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// Decoded query parameter.
    fn param(&self, key: &str) -> Option<String> {
        self.query.split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k == key).then(|| url_decode(v))
        })
    }
}

/// An HTTP response ready to write.
struct Response {
    status: &'static str,
    content_type: &'static str,
    extra: Vec<String>,
    body: Vec<u8>,
}

impl Response {
    fn new(status: &'static str, content_type: &'static str, body: impl Into<Vec<u8>>) -> Response {
        Response { status, content_type, extra: Vec::new(), body: body.into() }
    }

    fn html(body: String) -> Response {
        Response::new("200 OK", "text/html; charset=utf-8", body)
    }

    fn text(status: &'static str, body: impl Into<String>) -> Response {
        Response::new(status, "text/plain; charset=utf-8", body.into())
    }

    /// The 303 every control endpoint answers with, back to the dashboard.
    fn redirect_home() -> Response {
        let mut r = Response::new("303 See Other", "text/plain", "");
        r.extra.push("Location: /".into());
        r
    }

    fn not_found() -> Response {
        Response::text("404 Not Found", "not found")
    }

    fn method_not_allowed(allow: &str) -> Response {
        let mut r = Response::text("405 Method Not Allowed", format!("method not allowed; use {allow}"));
        r.extra.push(format!("Allow: {allow}"));
        r
    }

    fn write(&self, stream: &mut TcpStream) {
        let mut head = format!("HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n", self.status, self.content_type, self.body.len());
        for h in &self.extra {
            head.push_str(h);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(&self.body);
        let _ = stream.flush();
    }
}

/// Longest request head accepted.
const MAX_HEAD: usize = 16 * 1024;

/// Read one request head from the stream, under timeouts, and answer it.
fn handle_connection(mut stream: TcpStream, ctx: &Dashboard) {
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    // The whole head must arrive within five seconds, however slowly the client drips it.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 2048];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let _ = stream.set_read_timeout(Some(remaining));
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.windows(2).any(|w| w == b"\n\n") || buf.len() >= MAX_HEAD {
                    break;
                }
            }
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let Some(req) = Request::parse(&head) else {
        Response::text("400 Bad Request", "bad request").write(&mut stream);
        return;
    };
    let response = match route(&req, ctx) {
        Ok(r) => r,
        Err(Blob { path, name }) => {
            // Stream the blob instead of loading it, since a revision can be as large as the largest synced file.
            match fs::File::open(&path).and_then(|f| f.metadata().map(|m| (f, m.len()))) {
                Ok((mut f, len)) => {
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
                         Content-Disposition: attachment; filename=\"{name}\"\r\n\
                         Content-Length: {len}\r\nConnection: close\r\n\r\n"
                    );
                    let _ = stream.write_all(head.as_bytes());
                    let _ = std::io::copy(&mut f, &mut stream);
                }
                Err(e) => Response::text("404 Not Found", format!("not found: {e}")).write(&mut stream),
            }
            return;
        }
    };
    response.write(&mut stream);
}

/// A blob to stream, returned from routing instead of a buffered response.
struct Blob {
    path: PathBuf,
    name: String,
}

/// Loopback host names the dashboard answers to.
fn is_loopback_host(host: &str) -> bool {
    let host = host.trim();
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host)
    };
    matches!(name.to_ascii_lowercase().as_str(), "localhost" | "127.0.0.1" | "::1")
}

/// Whether a POST may act: the Host must be loopback (no DNS rebinding), any Origin must be this dashboard (no cross-site form posts),
/// and a browser's Sec-Fetch-Site must not say cross-site.
fn post_allowed(req: &Request, port: u16) -> std::result::Result<(), String> {
    match req.header("host") {
        Some(h) if is_loopback_host(h) => {}
        Some(h) => return Err(format!("POST refused: Host {h:?} is not loopback")),
        None => return Err("POST refused: no Host header".into()),
    }
    if let Some(origin) = req.header("origin") {
        let ok = origin.strip_prefix("http://").is_some_and(|rest| {
            is_loopback_host(rest) && rest.rsplit_once(':').is_some_and(|(_, p)| p == port.to_string())
        });
        if !ok {
            return Err(format!("POST refused: Origin {origin:?} is not this dashboard"));
        }
    }
    if let Some(site) = req.header("sec-fetch-site") {
        if !matches!(site, "same-origin" | "none") {
            return Err(format!("POST refused: Sec-Fetch-Site {site:?}"));
        }
    }
    Ok(())
}

/// Translate the gate `to` parameter: `open` or `closed`, nothing else.
fn gate_target(to: Option<&str>) -> Option<bool> {
    match to {
        Some("open") => Some(false),
        Some("closed") => Some(true),
        _ => None,
    }
}

/// Route one request to its handler.
fn route(req: &Request, ctx: &Dashboard) -> std::result::Result<Response, Blob> {
    let rt = &ctx.rt;
    let method = req.method.as_str();
    let path = req.path.as_str();
    // Every route answers loopback names alone, so a DNS-rebinding page cannot read the dashboard either.
    if let Some(h) = req.header("host") {
        if !is_loopback_host(h) {
            return Ok(Response::text("403 Forbidden", format!("refused: Host {h:?} is not loopback")));
        }
    }
    let is_post_route = matches!(path, "/gate" | "/pause" | "/resume" | "/sync" | "/restart" | "/halt");
    if is_post_route {
        if method != "POST" {
            return Ok(Response::method_not_allowed("POST"));
        }
        if let Err(reason) = post_allowed(req, ctx.config.port) {
            eprintln!("dashboard: {reason}");
            return Ok(Response::text("403 Forbidden", reason));
        }
    }
    Ok(match (method, path) {
        ("POST", "/gate") => {
            // Parse target state from parameter to. Update the gate for a specific relation if rel is present, falling back to the global gate.
            let Some(closed) = gate_target(req.param("to").as_deref()) else {
                return Ok(Response::text("400 Bad Request", "gate needs to=open or to=closed"));
            };
            let word = if closed { "closed" } else { "opened" };
            match req.param("rel") {
                Some(rel) => {
                    ctx.gates.set_relation(&rel, closed);
                    eprintln!("dashboard: gate for {rel} {word}");
                }
                None => {
                    ctx.gates.set_global(closed);
                    eprintln!("dashboard: global gate {word}");
                }
            }
            Response::redirect_home()
        }
        ("POST", "/pause") => {
            rt.paused.store(true, Ordering::Relaxed);
            let _ = rt.commands.send(Command::Pause);
            Response::redirect_home()
        }
        ("POST", "/resume") => {
            rt.paused.store(false, Ordering::Relaxed);
            let _ = rt.commands.send(Command::Resume);
            Response::redirect_home()
        }
        ("POST", "/sync") => {
            let _ = rt.commands.send(Command::Sync);
            Response::redirect_home()
        }
        ("POST", "/restart") => {
            // Validate the config exactly as startup would: a typo would otherwise crash-loop the daemon under launchd with the dashboard gone.
            match load_config(&rt.config_path).map(|_| ()) {
                Err(e) => Response::html(notice_page("restart refused", &format!("The config at {} does not load, so replica keeps running on the current one. Fix it, then restart again. {e:#}", tilde(&rt.config_path)), false)),
                Ok(()) => {
                    eprintln!("dashboard: restart requested; stopping after the current file operation, then exiting nonzero for launchd to relaunch");
                    EXIT_CODE.store(1, Ordering::SeqCst);
                    STOP.store(true, Ordering::SeqCst);
                    let _ = rt.commands.send(Command::Stop);
                    Response::html(notice_page("restarting", "Replica is stopping after the current file operation and will be relaunched. This page reloads in 5 seconds.", true))
                }
            }
        }
        ("POST", "/halt") => {
            eprintln!("dashboard: halt requested; stopping after the current file operation");
            EXIT_CODE.store(0, Ordering::SeqCst);
            STOP.store(true, Ordering::SeqCst);
            let _ = rt.commands.send(Command::Stop);
            Response::html(notice_page("halting", "Replica is stopping after the current file operation and exits cleanly. launchd will leave it stopped until the next login, or until you start it with launchctl kickstart.", false))
        }
        ("GET", "/history") => {
            let rel = req.param("rel").unwrap_or_default();
            let all = req.param("all").is_some();
            let entries = ctx.engine.lock().unwrap().store.archive_entries(&rel).map_err(|e| format!("{e:#}"));
            Response::html(render_history(&rel, entries, &ctx.archive, all))
        }
        ("GET", "/blob") => {
            // Fetch blob bytes from the archive using hash h. Return an attachment download with a sanitized filename from parameter as.
            let hex = req.param("h").unwrap_or_default();
            let name = req.param("as").unwrap_or_else(|| format!("{hex}.bin"));
            match ctx.archive.blob_path(&hex) {
                Ok(path) => return Err(Blob { path, name: name.replace(['"', '\r', '\n', '/', '\\'], "_") }),
                Err(e) => Response::text("404 Not Found", format!("not found: {e}")),
            }
        }
        ("GET", "/style.css") => Response::new("200 OK", "text/css; charset=utf-8", CSS),
        ("GET", "/favicon.svg") => Response::new("200 OK", "image/svg+xml", FAVICON),
        ("GET", "/status.json") => {
            let (snap, stale) = current_snapshot(ctx);
            Response::new("200 OK", "application/json", status_json(&snap.report, snap.at, stale, rt, &ctx.gates))
        }
        ("GET", "/") => {
            let (snap, stale) = current_snapshot(ctx);
            let recent: Vec<EvView> = ctx.events.lock().unwrap().iter().rev().map(EvView::from).collect();
            let show_all = req.param("all").and_then(|_| req.param("rel"));
            let view = PageData { snap: &snap, stale, recent: &recent, rt, dash: &ctx.config, archive: &ctx.archive, show_all };
            Response::html(render_page(&view))
        }
        (_, "/" | "/history" | "/blob" | "/style.css" | "/favicon.svg" | "/status.json") => Response::method_not_allowed("GET"),
        _ => Response::not_found(),
    })
}

/// Seconds since the Unix epoch.
fn epoch_secs(t: SystemTime) -> i64 {
    to_epoch_ms(t) / 1000
}

/// Sum of rows awaiting sync and rows held by a gate across relations.
fn totals(report: &Report) -> (usize, usize) {
    report.relations.iter().map(|r| r.counts(report.globally_gated)).fold((0, 0), |(a, h), (x, y)| (a + x, h + y))
}

/// Body of `/status.json`: process state, gate, counters, when the report was taken, and one state per relation.
fn status_json(report: &Report, report_at: SystemTime, stale: bool, rt: &Runtime, gates: &Gates) -> String {
    let now = SystemTime::now();
    let last = *rt.last_cycle.lock().unwrap();
    let next = *rt.next_cycle.lock().unwrap();
    let globally_gated = gates.globally_gated();
    let (awaiting, held) = totals(report);
    let relations: Vec<String> = report.relations.iter().map(|r| {
        let state = r.state(globally_gated);
        let (awaiting, held) = r.counts(globally_gated);
        format!(
            "{{\"name\":{},\"state\":{},\"awaiting\":{awaiting},\"held\":{held},\"gated\":{}}}",
            json_str(&r.name), json_str(&state_word(&state)), globally_gated || r.gated,
        )
    }).collect();
    format!(
        "{{\"state\":\"{}\",\"gate\":\"{}\",\"pid\":{},\"version\":\"{}\",\"uptime_secs\":{},\
         \"last_sync\":{},\"last_sync_took_ms\":{},\"next_sync\":{},\"report_at\":{},\"report_stale\":{stale},\
         \"cycles\":{},\"ops_applied\":{},\"errors\":{},\"awaiting_sync\":{awaiting},\"held\":{held},\"relations\":[{}]}}",
        if rt.paused.load(Ordering::Relaxed) { "paused" } else { "running" },
        if globally_gated { "closed" } else { "open" },
        rt.pid,
        rt.version,
        now.duration_since(rt.started).unwrap_or_default().as_secs(),
        last.map(|(at, _)| epoch_secs(at).to_string()).unwrap_or("null".into()),
        last.map(|(_, took)| took.as_millis().to_string()).unwrap_or("null".into()),
        next.map(|at| epoch_secs(at).to_string()).unwrap_or("null".into()),
        epoch_secs(report_at),
        rt.cycles.load(Ordering::Relaxed),
        rt.ops_applied.load(Ordering::Relaxed),
        rt.errors.load(Ordering::Relaxed),
        relations.join(","),
    )
}

/// One word for a relation state, for JSON consumers.
fn state_word(state: &RelationState) -> String {
    match state {
        RelationState::Error => "error",
        RelationState::Unavailable => "unavailable",
        RelationState::Awaiting(_) => "awaiting",
        RelationState::Held(_) => "held",
        RelationState::Degraded => "degraded",
        RelationState::FolderMissing => "folder missing",
        RelationState::InSync => "in sync",
    }.into()
}

/// Quote a string for JSON.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Rows of either kind shown per relation before the table is cut, unless `?rel=<name>&all=1` asks for everything.
const ROW_CAP: usize = 50;
/// Archive versions shown on the history page before it is cut.
const HISTORY_CAP: usize = 100;

/// Wrap body markup in the page skeleton.
fn page(head_extra: &str, body: &str) -> String {
    format!("{HEAD}{head_extra}</head><body>{body}</body></html>")
}

/// Assemble the HTML history view for a relation from recorded archive entries, displaying versions with download links for manual restoration when blobs remain on disk.
fn render_history(rel: &str, entries: std::result::Result<Vec<replica::store::ArchiveEntry>, String>, archive: &Archive, all: bool) -> String {
    let mut h = format!(
        "<header><a href=\"/\">&larr; back</a><span class=\"app-name\">Replica</span>\
         <span class=\"text-medium\">history of</span><b>{}</b></header>",
        esc(rel)
    );
    let entries = match entries {
        Ok(e) => e,
        Err(e) => {
            h.push_str(&format!("<section><div class=\"bad\">Could not read the archive index: {}</div></section>", esc(&e)));
            return page("", &h);
        }
    };
    if entries.is_empty() {
        h.push_str("<section><div class=\"text-light\">Nothing archived for this relation yet. Versions appear here when a sync overwrites or deletes a file.</div></section>");
        return page("", &h);
    }
    let shown = if all { entries.len() } else { entries.len().min(HISTORY_CAP) };
    h.push_str(&format!(
        "<section><div class=\"section-header\"><b>Saved versions</b>\
         <span class=\"text-medium\">{}, newest first. To restore one, download it and put it back where the relation expects it; a one-way relation reverts a file restored on its downstream side.</span></div>",
        plural(entries.len(), "version"),
    ));
    h.push_str("<table><tr><th>when</th><th>file</th><th>path</th><th>reason</th><th>content</th><th></th></tr>");
    for e in &entries[..shown] {
        let missing = !archive.dir.join(&e.hash).exists();
        h.push_str(&format!(
            "<tr><td title=\"{}\">{}</td><td>{}</td><td class=\"text-medium\">{}</td>\
             <td><span class=\"tag {}\">{}</span></td>\
             <td class=\"text-medium\" title=\"content hash {}\">{}, {}</td><td>{}</td></tr>",
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
                "<span class=\"text-light\">evicted</span>".to_string()
            } else {
                let name = e.path.rsplit('/').next().unwrap_or(&e.ident);
                format!(
                    "<a href=\"/blob?h={}&as={}\">download</a>",
                    esc(&e.hash),
                    url_encode(name),
                )
            },
        ));
    }
    h.push_str("</table>");
    if shown < entries.len() {
        h.push_str(&format!(
            "<div class=\"more-files\">and {} older; <a href=\"/history?rel={}&all=1\">show all</a></div>",
            entries.len() - shown, url_encode(rel)
        ));
    }
    h.push_str("</section>");
    page("", &h)
}

/// Generate a standalone notice screen displaying a status message, including a timed redirect toward root when reload is true.
fn notice_page(title: &str, text: &str, reload: bool) -> String {
    page(
        if reload { "<meta http-equiv=\"refresh\" content=\"5;url=/\">" } else { "" },
        &format!(
            "<header><a href=\"/\">&larr; back</a><span class=\"app-name\">Replica</span><span class=\"text-medium\">{}</span></header>\
             <section>{}</section>",
            esc(title),
            esc(text),
        ),
    )
}

/// Hold activity feed display fields for a single engine event, storing preformatted age and label text.
struct EvView {
    age: String,
    kind: &'static str,
    tone: Tone,
    relation: String,
    text: String,
}

impl EvView {
    /// Fill in the age, kind, relation, and text for the activity list.
    fn from(e: &Ev) -> EvView {
        let text = if e.identifier.is_empty() { e.text.clone() } else { format!("{}: {}", e.identifier, e.text) };
        EvView { age: fmt_ago(e.at), kind: e.kind, tone: e.tone, relation: e.relation.clone(), text }
    }
}

/// CSS class for a tone.
fn tone_class(t: Tone) -> &'static str {
    match t {
        Tone::Ok => "ok",
        Tone::Warn => "warn",
        Tone::Bad => "bad",
        Tone::Neutral => "",
    }
}

/// Escape `&`, `<`, `>`, and `"` for HTML text and attributes.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Decode `application/x-www-form-urlencoded` (query strings), working on bytes so a `%` before a multibyte character cannot split it.
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
                match hex {
                    Some(b) => { out.push(b); i += 2; }
                    None => out.push(b'%'),
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
fn tilde(p: &Path) -> String {
    tilde_str(&p.display().to_string())
}

/// Display a path string with `$HOME` replaced by `~`, matching whole path components so `/Users/home2` stays as it is.
fn tilde_str(s: &str) -> String {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() => match Path::new(s).strip_prefix(&h) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => s.into(),
        },
        _ => s.into(),
    }
}

/// `1 file`, `2 files`.
fn plural(n: usize, word: &str) -> String {
    if n == 1 { format!("1 {word}") } else { format!("{n} {word}s") }
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

const CSS: &str = include_str!("style.css");
const FAVICON: &str = include_str!("favicon.svg");

const HEAD: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>replica</title>\
    <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
    <link rel=\"icon\" type=\"image/svg+xml\" href=\"/favicon.svg\">\
    <link rel=\"stylesheet\" href=\"/style.css\">";

/// The writing / gated switch. Pass `None` for the global gate. The current side is a highlighted label and not clickable. The other side is the button.
fn gate_switch(action_rel: Option<&str>, gated: bool, disabled_note: Option<&str>) -> String {
    let target = |to: &str| match action_rel {
        Some(rel) => format!("/gate?rel={}&to={to}", url_encode(rel)),
        None => format!("/gate?to={to}"),
    };
    if let Some(note) = disabled_note {
        return format!("<span class=\"gate-switch disabled\" title=\"{}\"><span class=\"gate-state\">writing</span><span class=\"gate-state\">gated</span></span>", esc(note));
    }
    let (writing, gated_seg) = if gated {
        (
            format!("<form method=\"post\" action=\"{}\"><button class=\"gate-button\" title=\"Open the gate. The next sync will apply pending changes.\">writing</button></form>", target("open")),
            "<span class=\"gate-state current warn\">gated</span>".to_string(),
        )
    } else {
        (
            "<span class=\"gate-state current ok\">writing</span>".to_string(),
            format!("<form method=\"post\" action=\"{}\"><button class=\"gate-button\" title=\"Close the gate. Replica will keep planning and reporting, but will not write files.\">gated</button></form>", target("closed")),
        )
    };
    format!("<span class=\"gate-switch\">{writing}{gated_seg}</span>")
}

/// State tag markup for a relation state.
fn state_tag(state: &RelationState) -> String {
    match state {
        RelationState::Error => "<span class=\"tag bad\">failed</span>".into(),
        RelationState::Unavailable => "<span class=\"tag warn\">unavailable</span>".into(),
        RelationState::Awaiting(n) => format!("<span class=\"tag warn\">{n} awaiting sync</span>"),
        RelationState::Held(n) => format!("<span class=\"tag warn\">{n} held by gate</span>"),
        RelationState::Degraded => "<span class=\"tag warn\">partly unreadable</span>".into(),
        RelationState::FolderMissing => "<span class=\"tag warn\">folder missing</span>".into(),
        RelationState::InSync => "<span class=\"tag ok\">in sync</span>".into(),
    }
}

/// Everything the main page renders.
struct PageData<'a> {
    snap: &'a Snapshot,
    /// The snapshot was published by an earlier cycle because a cycle is running now.
    stale: bool,
    recent: &'a [EvView],
    rt: &'a Runtime,
    dash: &'a DashboardConfig,
    archive: &'a Archive,
    /// Relation whose rows are all shown, from `?rel=<name>&all=1`.
    show_all: Option<String>,
}

/// Build the HTML for the dashboard.
fn render_page(v: &PageData) -> String {
    let (report, rt, dash) = (&v.snap.report, v.rt, v.dash);
    let now = SystemTime::now();
    let mut h = String::new();

    let paused = rt.paused.load(Ordering::Relaxed);
    let globally_gated = report.globally_gated;
    let pause_button = if paused {
        "<form method=\"post\" action=\"/resume\"><button>resume</button></form>"
    } else {
        "<form method=\"post\" action=\"/pause\"><button>pause</button></form>"
    };
    h.push_str(&format!(
        "<header><span class=\"app-name\">Replica</span><span class=\"text-light\">{}</span>{}{}\
         <div class=\"daemon-controls\"><span class=\"gate-label\">all writes</span>{}\
         <form method=\"post\" action=\"/sync\"><button title=\"Run a sync now instead of waiting for the next scheduled one.\">sync now</button></form>\
         {pause_button}\
         <form method=\"post\" action=\"/restart\"><button>restart</button></form>\
         <form method=\"post\" action=\"/halt\" onsubmit=\"return confirm('Stop replica? launchd will leave it stopped until you start it again.')\"><button class=\"bad\">halt</button></form></div></header>",
        rt.version,
        if paused { " <span class=\"tag warn\">paused</span>" } else { "" },
        if globally_gated { " <span class=\"tag warn\" title=\"No files will be written until the gate is opened.\">all writes gated</span>" } else { "" },
        gate_switch(None, globally_gated, None),
    ));

    // Status strip
    let db_size = fs::metadata(&rt.db_path).map(|m| fmt_size(m.len())).unwrap_or_else(|_| "?".into());
    let uptime = now.duration_since(rt.started).unwrap_or_default();
    let ops = rt.ops_applied.load(Ordering::Relaxed);
    let errors = rt.errors.load(Ordering::Relaxed);
    let last = *rt.last_cycle.lock().unwrap();
    let next = *rt.next_cycle.lock().unwrap();
    let states: Vec<RelationState> = report.relations.iter().map(|r| r.state(globally_gated)).collect();
    let worst = states.iter().min().cloned().unwrap_or(RelationState::InSync);
    let (total_awaiting, total_held) = totals(report);

    fn stat(k: &str, v: String) -> String {
        format!("<div class=\"stat\"><div class=\"stat-label\">{k}</div><div class=\"stat-value\">{v}</div></div>")
    }
    h.push_str("<div class=\"status-strip\">");
    h.push_str(&stat("Process", format!("pid {}, up {}", rt.pid, fmt_dur(uptime))));
    h.push_str(&stat("Configuration", esc(&tilde(&rt.config_path))));
    let db_detail = match &v.snap.db_stats {
        Ok((rec_rows, int_rows)) => {
            let intents_note = if *int_rows > 0 { format!(", <span class=\"bad\">{}</span>", plural(*int_rows as usize, "unfinished intent")) } else { String::new() };
            format!("{db_size}, {}{intents_note}", plural(*rec_rows as usize, "record"))
        }
        Err(e) => format!("<span class=\"bad\">unreadable: {}</span>", esc(e)),
    };
    h.push_str(&stat("Database", format!("{}, {db_detail}", esc(&tilde(&rt.db_path)))));
    h.push_str(&stat("Last sync", match last {
        Some((at, took)) => format!("{}, took {}", fmt_clock(at), fmt_dur(took)),
        None => "not yet".into(),
    }));
    h.push_str(&stat("Next sync", match next {
        Some(at) if !paused => {
            let until = at.duration_since(now).map(fmt_dur).unwrap_or_else(|_| "0s".into());
            format!("{} <span class=\"text-medium\">(in {until})</span>", fmt_clock(at))
        }
        _ => if paused { "<span class=\"warn\">paused</span>".into() } else { "running now".into() },
    }));
    let error_note = if errors > 0 { format!(", <span class=\"bad\">{}</span>", plural(errors as usize, "error")) } else { String::new() };
    h.push_str(&stat("This run", format!("{}{error_note}", plural(ops as usize, "file operation"))));
    h.push_str(&stat("Archive", match &v.snap.arch_stats {
        Ok((arch_rows, arch_bytes)) => format!("{}, {} of {}", plural(*arch_rows as usize, "saved version"), fmt_size(*arch_bytes), fmt_size(v.archive.cap)),
        Err(e) => format!("<span class=\"bad\">unreadable: {}</span>", esc(e)),
    }));
    h.push_str(&stat("State", match worst {
        RelationState::Awaiting(_) | RelationState::Held(_) => {
            let mut parts = Vec::new();
            if total_awaiting > 0 {
                parts.push(format!("<span class=\"tag warn\">{total_awaiting} awaiting sync</span>"));
            }
            if total_held > 0 {
                parts.push(format!("<span class=\"tag warn\">{total_held} held by gate</span>"));
            }
            parts.join(" ")
        }
        RelationState::InSync => "<span class=\"tag ok\">everything in sync</span>".into(),
        other => state_tag(&other),
    }));
    h.push_str("</div>");
    if v.stale {
        h.push_str(&format!(
            "<div class=\"text-medium\" style=\"margin-top:6px\">A sync is running. This report was taken at {}, after the previous one.</div>",
            fmt_clock(v.snap.at)
        ));
    }

    // Unfinished intents are evidence of a crash mid-write: those cleared at startup stay listed for this run, and live ones mean an apply failed just now.
    let cleared = rt.cleared_intents.lock().unwrap();
    let live = v.snap.intents.as_ref().map(|i| i.as_slice()).unwrap_or(&[]);
    if !cleared.is_empty() || !live.is_empty() || v.snap.intents.is_err() {
        h.push_str("<section><div class=\"section-header\"><b class=\"bad\">Unfinished operations</b>\
                    <span class=\"text-medium\">A previous run stopped mid-write. The next sync re-plans these from the last recorded state.</span></div>");
        if let Err(e) = &v.snap.intents {
            h.push_str(&format!("<div class=\"bad\">intent table unreadable: {}</div>", esc(e)));
        }
        for (rel, ident, detail) in cleared.iter() {
            h.push_str(&format!("<div class=\"bad\">{} {}: {} <span class=\"text-light\">(found at startup)</span></div>", esc(rel), esc(&ident.join("/")), esc(detail)));
        }
        for (rel, ident, detail) in live {
            h.push_str(&format!("<div class=\"bad\">{} {}: {}</div>", esc(rel), esc(&ident.join("/")), esc(detail)));
        }
        h.push_str("</section>");
    }
    drop(cleared);

    for (rel, state) in report.relations.iter().zip(&states) {
        let effective_gated = globally_gated || rel.gated;
        let n_idents = rel.rows.len();
        let n_refs = rel.refs.len();
        let controls = format!(
            "<span class=\"section-controls\"><a href=\"/history?rel={}\" title=\"Overwritten and deleted versions kept in the archive.\">history</a>{}</span>",
            url_encode(&rel.name),
            gate_switch(
                Some(&rel.name),
                rel.gated,
                globally_gated.then_some("The global gate is closed, so this switch has no effect until the global gate opens."),
            ),
        );
        let tag = match state {
            RelationState::Awaiting(_) | RelationState::Held(_) => {
                let (awaiting, held) = rel.counts(globally_gated);
                let mut parts = Vec::new();
                if awaiting > 0 {
                    parts.push(format!("<span class=\"tag warn\">{awaiting} awaiting sync</span>"));
                }
                if held > 0 {
                    parts.push(format!("<span class=\"tag warn\">{held} held by gate</span>"));
                }
                parts.join(" ")
            }
            other => state_tag(other),
        };
        h.push_str(&format!(
            "<section><div class=\"section-header\"><b>{}</b>\
             <span class=\"text-medium\">{} across {}</span>{tag}{controls}</div>",
            esc(&rel.name),
            plural(n_idents, "file"),
            plural(n_refs, "location"),
        ));
        if let Some(e) = &rel.error {
            h.push_str(&format!("<div class=\"bad\">{}</div></section>", esc(e)));
            continue;
        }

        // Labels and their refs, flowing inline: "label path, path   label path"
        h.push_str("<div class=\"locations\">");
        for label in &rel.labels {
            let note = if label.notes.is_empty() { String::new() } else { format!(" <span class=\"text-light\">({})</span>", esc(&label.notes.join(", "))) };
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
                "<span class=\"location\"><b>{}</b>{note} <span class=\"path\">{}</span></span>",
                esc(&label.name),
                paths.join(", "),
            ));
        }
        h.push_str("</div>");
        if let Some(reason) = &rel.unavailable {
            h.push_str(&format!("<div class=\"warn\" style=\"margin-top:6px\">{} Nothing is planned until the folder is back.</div></section>", esc(reason)));
            continue;
        }
        for w in &rel.warnings {
            h.push_str(&format!("<div class=\"warn\" style=\"margin-top:6px\">{}</div>", esc(w)));
        }

        // A row per file and a column per location. Pending rows come first.
        if rel.rows.is_empty() {
            h.push_str("<div class=\"text-light\" style=\"margin-top:6px\">no files yet</div></section>");
            continue;
        }
        h.push_str("<table><tr><th>file</th>");
        for rf in &rel.refs {
            h.push_str(&format!("<th>{}</th>", esc(&rf.name)));
        }
        h.push_str("</tr>");

        let show_all = v.show_all.as_deref() == Some(rel.name.as_str());
        let cap = if show_all { usize::MAX } else { ROW_CAP };
        let (pending, in_sync): (Vec<&RowView>, Vec<&RowView>) = rel.rows.iter().partition(|r| r.needs_attention());
        let shown_pending = pending.len().min(cap);
        let shown_synced = in_sync.len().min(cap);
        for row in pending.iter().take(cap).chain(in_sync.iter().take(cap)) {
            // The exact plan the next ungated sync applies.
            let plan_html = if !row.plan.is_empty() {
                let verb = if effective_gated { "held" } else { "next sync" };
                format!(
                    "<div class=\"plan {}\"><span class=\"plan-label\">{verb}</span>{}</div>",
                    if effective_gated { "warn" } else { "" },
                    esc(&row.plan_text),
                )
            } else if let Some(note) = &row.note {
                format!("<div class=\"plan\"><span class=\"plan-label\">waiting</span>{}</div>", esc(note))
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
                             <div class=\"file-meta\">{}, {}</div></td>",
                            hash8(&snap.hash),
                            fmt_size(snap.size),
                            fmt_stamp(snap.mtime),
                        ));
                    }
                    Status::Missing => {
                        if cell.change == Change::Disappeared {
                            h.push_str("<td><span class=\"tag bad\">deleted</span></td>");
                        } else {
                            h.push_str("<td><span class=\"text-light\">absent</span></td>");
                        }
                    }
                    Status::Unreadable(e) => {
                        h.push_str(&format!("<td><span class=\"tag bad\">unreadable</span><div class=\"file-meta\">{}</div></td>", esc(e)));
                    }
                }
            }
            h.push_str("</tr>");
        }
        h.push_str("</table>");
        let hidden_pending = pending.len() - shown_pending;
        let hidden_synced = in_sync.len() - shown_synced;
        if hidden_pending + hidden_synced > 0 {
            let mut parts = Vec::new();
            if hidden_pending > 0 {
                parts.push(format!("{hidden_pending} more awaiting sync"));
            }
            if hidden_synced > 0 {
                parts.push(format!("{hidden_synced} more in sync"));
            }
            h.push_str(&format!(
                "<div class=\"more-files\">and {}; <a href=\"/?rel={}&all=1\">show all</a></div>",
                parts.join(" and "), url_encode(&rel.name)
            ));
        }
        h.push_str("</section>");
    }

    h.push_str("<section><div class=\"section-header\"><b>Activity</b></div>");
    if v.recent.is_empty() {
        h.push_str("<div class=\"text-light\" style=\"margin-top:4px\">nothing yet</div>");
    } else {
        for ev in v.recent {
            h.push_str(&format!(
                "<div class=\"event\"><span class=\"event-age\">{}</span><span class=\"event-kind tag {}\">{}</span>\
                 <span class=\"event-relation\">{}</span><span class=\"text-medium\">{}</span></div>",
                esc(&ev.age), tone_class(ev.tone), esc(ev.kind), esc(&ev.relation), esc(&ev.text)
            ));
        }
    }
    h.push_str("</section>");

    // Refresh by swapping the body in place: no page load, so no tab flash.
    // Hidden tabs and pages with a text selection are left alone; a failed fetch shows a banner instead of a stale green page.
    h.push_str(&format!(
        "<div id=\"unreachable\" class=\"banner bad\" hidden>replica is not answering; this page is stale</div>\
         <script>setInterval(async()=>{{\
         if(document.hidden)return;\
         const sel=window.getSelection();if(sel&&!sel.isCollapsed)return;\
         try{{\
         const r=await fetch(location.pathname+location.search,{{cache:'no-store'}});\
         if(!r.ok)throw new Error(r.status);\
         const d=new DOMParser().parseFromString(await r.text(),'text/html');\
         document.body.innerHTML=d.body.innerHTML;\
         }}catch(e){{const b=document.getElementById('unreachable');if(b)b.hidden=false;}}}},{});</script>",
        dash.refresh.max(1) * 1000
    ));
    page("", &h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_decode_works_on_bytes() {
        assert_eq!(url_decode("a+b%20c"), "a b c");
        assert_eq!(url_decode("%C3%A9"), "é");
        // A percent sign followed by a multibyte character must not panic or split the character.
        assert_eq!(url_decode("%é"), "%é");
        assert_eq!(url_decode("%zz"), "%zz");
        assert_eq!(url_decode("100%"), "100%");
    }

    #[test]
    fn tilde_matches_whole_components() {
        let home = std::env::var("HOME").unwrap();
        assert_eq!(tilde_str(&format!("{home}/x/y")), "~/x/y");
        assert_eq!(tilde_str(&home), "~");
        assert_eq!(tilde_str(&format!("{home}2/x")), format!("{home}2/x"));
        assert_eq!(tilde_str("/etc/nginx"), "/etc/nginx");
    }

    #[test]
    fn durations_and_sizes_format() {
        assert_eq!(fmt_dur(Duration::from_millis(800)), "800ms");
        assert_eq!(fmt_dur(Duration::from_secs(5)), "5s");
        assert_eq!(fmt_dur(Duration::from_secs(252)), "4m 12s");
        assert_eq!(fmt_dur(Duration::from_secs(3600)), "1h");
        assert_eq!(fmt_dur(Duration::from_secs(2 * 86400 + 3 * 3600)), "2d 3h");
        assert_eq!(fmt_size(512), "512 B");
        assert_eq!(fmt_size(1536), "1.5 KB");
        assert_eq!(fmt_size(3 << 20), "3.0 MB");
        assert_eq!(plural(1, "file"), "1 file");
        assert_eq!(plural(2, "file"), "2 files");
    }

    #[test]
    fn requests_parse_headers_and_query() {
        let req = Request::parse("POST /gate?rel=a%20b&to=closed HTTP/1.1\r\nHost: localhost:18516\r\nOrigin: http://localhost:18516\r\n\r\n").unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/gate");
        assert_eq!(req.param("rel").as_deref(), Some("a b"));
        assert_eq!(req.param("to").as_deref(), Some("closed"));
        assert_eq!(req.header("host"), Some("localhost:18516"));
        assert!(Request::parse("").is_none());
    }

    #[test]
    fn posts_need_a_loopback_host_and_a_matching_origin() {
        let parse = |head: &str| Request::parse(head).unwrap();
        assert!(post_allowed(&parse("POST /pause HTTP/1.1\r\nHost: localhost:18516\r\n\r\n"), 18516).is_ok());
        assert!(post_allowed(&parse("POST /pause HTTP/1.1\r\nHost: 127.0.0.1:18516\r\nOrigin: http://127.0.0.1:18516\r\n\r\n"), 18516).is_ok());
        assert!(post_allowed(&parse("POST /pause HTTP/1.1\r\nHost: [::1]:18516\r\nOrigin: http://[::1]:18516\r\nSec-Fetch-Site: same-origin\r\n\r\n"), 18516).is_ok());
        // A cross-site form post carries the attacker's origin.
        assert!(post_allowed(&parse("POST /pause HTTP/1.1\r\nHost: localhost:18516\r\nOrigin: https://evil.example\r\n\r\n"), 18516).is_err());
        assert!(post_allowed(&parse("POST /pause HTTP/1.1\r\nHost: localhost:18516\r\nOrigin: null\r\n\r\n"), 18516).is_err());
        assert!(post_allowed(&parse("POST /pause HTTP/1.1\r\nHost: localhost:18516\r\nOrigin: http://localhost:1\r\n\r\n"), 18516).is_err());
        assert!(post_allowed(&parse("POST /pause HTTP/1.1\r\nHost: localhost:18516\r\nSec-Fetch-Site: cross-site\r\n\r\n"), 18516).is_err());
        // DNS rebinding arrives with a foreign Host.
        assert!(post_allowed(&parse("POST /pause HTTP/1.1\r\nHost: evil.example:18516\r\n\r\n"), 18516).is_err());
        assert!(post_allowed(&parse("POST /pause HTTP/1.1\r\n\r\n"), 18516).is_err());
    }

    #[test]
    fn gate_target_matches_exactly() {
        assert_eq!(gate_target(Some("open")), Some(false));
        assert_eq!(gate_target(Some("closed")), Some(true));
        assert_eq!(gate_target(Some("close")), None);
        assert_eq!(gate_target(Some("")), None);
        assert_eq!(gate_target(None), None);
    }

    #[test]
    fn args_accept_equals_form_and_flags() {
        let a = parse_args(&["--config=~/c.yaml".into(), "--once".into(), "--check".into()]).unwrap().unwrap();
        assert!(a.once && a.check && !a.gate);
        assert_eq!(a.config, PathBuf::from(format!("{}/c.yaml", std::env::var("HOME").unwrap())));
        let a = parse_args(&["--config".into(), "/c.yaml".into(), "--gate".into()]).unwrap().unwrap();
        assert!(a.gate && a.config == PathBuf::from("/c.yaml"));
        assert!(parse_args(&["--help".into()]).unwrap().is_none());
        assert!(parse_args(&["--version".into()]).unwrap().is_none());
        let err = parse_args(&["--bogus".into()]).unwrap_err().to_string();
        assert!(err.contains("unknown argument") && err.contains("usage:"), "{err}");
        let err = parse_args(&[]).unwrap_err().to_string();
        assert!(err.contains("usage:"), "{err}");
    }

    #[test]
    fn json_strings_are_escaped() {
        assert_eq!(json_str("plain"), "\"plain\"");
        assert_eq!(json_str("a\"b\\c\n"), "\"a\\\"b\\\\c\\n\"");
    }
}
