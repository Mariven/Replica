//! The dashboard, a small HTTP server on loopback for the status pages, `status.json`, history, blob downloads, and the control posts.
//!
//! If the engine is idle, a page shows its current state. While a cycle is running, the page shows the report from the end of the previous cycle and says so.

pub mod http;
pub mod render;
pub mod status;

use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Local};

use crate::blobs::Blobs;
use crate::config::DashboardConfig;
use crate::engine::{Engine, Gates, Report, STOP};
use crate::event::Stamped;
use crate::store::Intent;

use http::{Request, Response, post_allowed, is_loopback_host};

/// Requests from the dashboard to the main loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Sync,
    Pause,
    Resume,
    Stop,
    /// Save a gate change to the database. The dashboard has already set the shared flag.
    Gate { relation: Option<String>, closed: bool },
}

/// Process state and counters, shared by the main loop and the dashboard.
pub struct Runtime {
    pub pid: u32,
    pub version: &'static str,
    pub started: SystemTime,
    pub config_path: PathBuf,
    pub state_dir: PathBuf,
    pub next_cycle: Mutex<Option<SystemTime>>,
    pub last_cycle: Mutex<Option<(SystemTime, Duration)>>,
    pub cycles: AtomicU64,
    pub ops_applied: AtomicU64,
    pub errors: AtomicU64,
    pub paused: AtomicBool,
    pub watching: AtomicBool,
    pub watched_roots: Mutex<Vec<PathBuf>>,
    /// Relations with a change reported by the watcher. After the debounce period, the main loop empties this set and runs a cycle over those relations ahead of schedule.
    pub dirty: Mutex<BTreeSet<String>>,
    /// Intent rows found and cleared at startup. The dashboard shows them until the process exits.
    pub cleared_intents: Mutex<Vec<Intent>>,
    /// The snapshot taken at the end of the last cycle. Pages show it while a cycle is running.
    pub snapshot: Mutex<Option<Arc<Snapshot>>>,
    pub commands: Sender<Command>,
    /// Exit status once STOP is set: 0 for halt, 1 for restart so launchd relaunches.
    pub exit_code: std::sync::atomic::AtomicI32,
}

/// The engine state that a page shows, read at a single moment.
pub struct Snapshot {
    pub at: SystemTime,
    pub report: Report,
    pub db_stats: Result<(u64, u64), String>,
    pub intents: Result<Vec<Intent>, String>,
    pub blob_stats: Result<(u64, u64), String>,
}

/// Read the report and the database and archive statistics from the engine.
pub fn take_snapshot(eng: &Engine) -> Snapshot {
    Snapshot {
        at: SystemTime::now(),
        report: eng.report(),
        db_stats: eng.store.stats().map_err(|e| format!("{e:#}")),
        intents: eng.store.stale_intents().map_err(|e| format!("{e:#}")),
        blob_stats: eng.store.archive_stats().map_err(|e| format!("{e:#}")),
    }
}

/// The shared state that request handlers use.
pub struct Dashboard {
    pub engine: Arc<Mutex<Engine>>,
    /// The engine's gate flags. They are shared so that a gate request takes effect even while a cycle is running.
    pub gates: Arc<Gates>,
    pub events: Arc<Mutex<VecDeque<Stamped>>>,
    pub rt: Arc<Runtime>,
    pub config: DashboardConfig,
    pub blobs: Blobs,
    /// Check that the configuration still loads before a restart. If it fails to load, the dashboard refuses the restart and the daemon keeps running.
    pub revalidate: Box<dyn Fn() -> Result<(), String> + Send + Sync>,
}

/// Take a fresh snapshot if the engine is idle. While a cycle is running, return the snapshot from the end of the last cycle instead and mark it stale.
fn current_snapshot(ctx: &Dashboard) -> (Arc<Snapshot>, bool) {
    match ctx.engine.try_lock() {
        Ok(eng) => {
            let snap = Arc::new(take_snapshot(&eng));
            *ctx.rt.snapshot.lock().unwrap() = Some(snap.clone());
            (snap, false)
        }
        Err(std::sync::TryLockError::WouldBlock) => {
            // The main loop locks the engine and then the snapshot. To avoid a deadlock, this thread releases the snapshot lock before it waits for the engine.
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

/// Listen on the loopback interface and answer each connection on its own thread.
pub fn serve(ctx: Arc<Dashboard>) {
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
            if let Err(e) = std::thread::Builder::new().spawn(move || handle(stream, &ctx)) {
                eprintln!("dashboard: cannot start a connection thread: {e}");
            }
        }
    });
}

/// A blob download, streamed from disk instead of buffered like other responses.
struct Blob {
    path: PathBuf,
    name: String,
}

/// Read a request from the connection and write the response.
fn handle(mut stream: TcpStream, ctx: &Dashboard) {
    let Some(req) = Request::read(&mut stream) else {
        Response::text("400 Bad Request", "bad request").write(&mut stream);
        return;
    };
    match route(&req, ctx) {
        Ok(r) => r.write(&mut stream),
        Err(Blob { path, name }) => stream_blob(&mut stream, &path, &name),
    }
}

/// Send a blob as a download. It is streamed because an archived revision can be as large as any synced file.
fn stream_blob(stream: &mut TcpStream, path: &Path, name: &str) {
    match fs::File::open(path).and_then(|f| f.metadata().map(|m| (f, m.len()))) {
        Ok((mut f, len)) => {
            let head = format!("HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"{name}\"\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n");
            let _ = stream.write_all(head.as_bytes());
            let _ = std::io::copy(&mut f, stream);
        }
        Err(e) => Response::text("404 Not Found", format!("not found: {e}")).write(stream),
    }
}

/// Convert the `to` parameter of `/gate` to a closed flag: true for `closed`, false for `open`, and None for anything else.
fn gate_target(to: Option<&str>) -> Option<bool> {
    match to {
        Some("open") => Some(false),
        Some("closed") => Some(true),
        _ => None,
    }
}

/// Build the response to a request. A blob download comes back as `Err` so the caller can stream it.
fn route(req: &Request, ctx: &Dashboard) -> Result<Response, Blob> {
    let rt = &ctx.rt;
    let method = req.method.as_str();
    let path = req.path.as_str();
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
            let Some(closed) = gate_target(req.param("to").as_deref()) else {
                return Ok(Response::text("400 Bad Request", "gate needs to=open or to=closed"));
            };
            let word = if closed { "closed" } else { "opened" };
            // The shared flag changes at once, so a running cycle obeys it from its next relation on. The main loop saves it to the database when it next holds the engine.
            let relation = req.param("rel");
            match &relation {
                Some(rel) => {
                    ctx.gates.set_relation(rel, closed);
                    eprintln!("dashboard: gate for {rel} {word}");
                }
                None => {
                    ctx.gates.set_global(closed);
                    eprintln!("dashboard: global gate {word}");
                }
            }
            let _ = rt.commands.send(Command::Gate { relation, closed });
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
        ("POST", "/restart") => match (ctx.revalidate)() {
            Err(e) => Response::html(render::notice_page("restart refused", &format!("The config at {} does not load, so replica keeps running on the current one. Fix it, then restart again. {e}", tilde(&rt.config_path)), false)),
            Ok(()) => {
                eprintln!("dashboard: restart requested; stopping after the current file operation, then exiting nonzero for launchd to relaunch");
                rt.exit_code.store(1, Ordering::SeqCst);
                STOP.store(true, Ordering::SeqCst);
                let _ = rt.commands.send(Command::Stop);
                Response::html(render::notice_page("restarting", "Replica is stopping after the current file operation and will be relaunched. This page reloads in 5 seconds.", true))
            }
        },
        ("POST", "/halt") => {
            eprintln!("dashboard: halt requested; stopping after the current file operation");
            rt.exit_code.store(0, Ordering::SeqCst);
            STOP.store(true, Ordering::SeqCst);
            let _ = rt.commands.send(Command::Stop);
            Response::html(render::notice_page("halting", "Replica is stopping after the current file operation and exits cleanly. launchd leaves it stopped until the next login, or until you start it with launchctl kickstart.", false))
        }
        ("GET", "/history") => {
            let rel = req.param("rel").unwrap_or_default();
            let all = req.param("all").is_some();
            let entries = ctx.engine.lock().unwrap().store.archive_entries(&rel).map_err(|e| format!("{e:#}"));
            if req.param("json").is_some() {
                Response::json(render::history_json(&rel, entries, &ctx.blobs))
            } else {
                Response::html(render::history_page(&rel, entries, &ctx.blobs, all))
            }
        }
        ("GET", "/blob") => {
            let hex = req.param("h").unwrap_or_default();
            let name = req.param("as").unwrap_or_else(|| format!("{hex}.bin"));
            match ctx.blobs.path_for_hex(&hex) {
                Ok(path) => return Err(Blob { path, name: name.replace(['"', '\r', '\n', '/', '\\'], "_") }),
                Err(e) => Response::text("404 Not Found", format!("not found: {e}")),
            }
        }
        ("GET", "/style.css") => Response::new("200 OK", "text/css; charset=utf-8", render::CSS),
        ("GET", "/favicon.svg") => Response::new("200 OK", "image/svg+xml", render::FAVICON),
        ("GET", "/status.json") => {
            let (snap, stale) = current_snapshot(ctx);
            Response::json(status::status_json(&snap, stale, rt))
        }
        ("GET", "/") => {
            let (snap, stale) = current_snapshot(ctx);
            let recent: Vec<Stamped> = ctx.events.lock().unwrap().iter().rev().cloned().collect();
            let show_all = req.param("all").and_then(|_| req.param("rel"));
            Response::html(render::main_page(&render::PageData { snap: &snap, stale, recent: &recent, rt, dash: &ctx.config, blobs: &ctx.blobs, show_all }))
        }
        (_, "/" | "/history" | "/blob" | "/style.css" | "/favicon.svg" | "/status.json") => Response::method_not_allowed("GET"),
        _ => Response::not_found(),
    })
}

/// Display a path with `$HOME` replaced by `~`.
pub fn tilde(p: &Path) -> String {
    tilde_str(&p.display().to_string())
}

/// Display a path string with `$HOME` replaced by `~`. Only whole components match, so `/Users/home2` is left alone.
pub fn tilde_str(s: &str) -> String {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() => match Path::new(s).strip_prefix(&h) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => s.into(),
        },
        _ => s.into(),
    }
}

/// Format a count with its word, as in `1 file` or `2 files`.
pub fn plural(n: usize, word: &str) -> String {
    if n == 1 { format!("1 {word}") } else { format!("{n} {word}s") }
}

/// Local clock time, `HH:MM:SS`.
pub fn fmt_clock(t: SystemTime) -> String {
    DateTime::<Local>::from(t).format("%H:%M:%S").to_string()
}

/// Local date and time, `Mon D, HH:MM`.
pub fn fmt_stamp(t: SystemTime) -> String {
    DateTime::<Local>::from(t).format("%b %-d, %H:%M").to_string()
}

/// Format a duration in short form, as in `2d 3h`, `4m 12s`, or `800ms`.
pub fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs();
    let two = |big: u64, big_u: &str, small: u64, small_u: &str| if small > 0 { format!("{big}{big_u} {small}{small_u}") } else { format!("{big}{big_u}") };
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

/// How long ago `t` was, or `now` for a future time.
pub fn fmt_ago(t: SystemTime) -> String {
    match SystemTime::now().duration_since(t) {
        Ok(d) => format!("{} ago", fmt_dur(d)),
        Err(_) => "now".into(),
    }
}

/// Format a byte count in binary units, as in `512 B` or `1.5 KB`.
pub fn fmt_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(gate_target(Some("open")), Some(false));
        assert_eq!(gate_target(Some("close")), None);
    }
}
