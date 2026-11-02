//! The daemon: load the configuration, open the state, serve the dashboard, run cycles on the schedule and sooner when the watcher reports a change, stop cleanly on signals and dashboard commands.

use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local as LocalTime};

use replica::blobs::Blobs;
use replica::cli::{self, Command};
use replica::config;
use replica::dashboard::{self, Dashboard, Runtime, take_snapshot};
use replica::engine::{Engine, STOP};
use replica::event::{Event, Hook, Stamped};
use replica::medium::Local;
use replica::store::Store;
use replica::watch::{Notice, Watch};

/// Signal handler: an atomic store alone, which is safe inside a handler. The engine checks the flag between keys.
extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

/// Stop cleanly on SIGTERM and SIGINT instead of dying mid-write.
fn install_signal_handlers() {
    let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
    // SAFETY: the handler stores an atomic alone, and signal() with a plain function pointer is the documented use.
    unsafe {
        libc::signal(libc::SIGTERM, handler);
        libc::signal(libc::SIGINT, handler);
    }
}

/// One line of the log for an event, with the local time.
fn render_line(e: &Event) -> String {
    let stamp = DateTime::<LocalTime>::from(SystemTime::now()).format("%Y-%m-%d %H:%M:%S");
    let key = e.key();
    let rel = e.relation();
    match (rel.is_empty(), key.is_empty()) {
        (true, _) => format!("{stamp} {}", e.text()),
        (false, true) => format!("{stamp} {rel} {}", e.text()),
        (false, false) => format!("{stamp} {rel} {key} {}", e.text()),
    }
}

/// Something the main loop waits for.
enum Msg {
    Command(dashboard::Command),
    Notice(Notice),
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let code = match run(&argv) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("replica: {e:#}");
            2
        }
    };
    std::process::exit(code);
}

/// Dispatch the command line. Returns the exit status.
fn run(argv: &[String]) -> Result<i32> {
    match cli::parse(argv)? {
        Command::Help => { println!("{}", cli::USAGE); Ok(0) }
        Command::Version => { println!("replica {}", env!("CARGO_PKG_VERSION")); Ok(0) }
        Command::Check { config } => cli::check(&config),
        Command::Explain { config, relation } => { print!("{}", cli::explain(&config, relation.as_deref())?); Ok(0) }
        Command::Plan { config, relation } => { print!("{}", cli::plan(&config, relation.as_deref())?); Ok(0) }
        Command::Forget { config, relation, replica } => { println!("{}", cli::forget(&config, &relation, &replica)?); Ok(0) }
        Command::Status { port, json } => { print!("{}", cli::status(&port, json)?); Ok(0) }
        Command::History { port, relation, json } => { print!("{}", cli::history(&port, &relation, json)?); Ok(0) }
        Command::Control { port, action } => { println!("{}", cli::control(&port, &format!("/{action}"))?); Ok(0) }
        Command::Gate { port, closed, relation } => {
            let mut path = format!("/gate?to={}", if closed { "closed" } else { "open" });
            if let Some(rel) = relation {
                path.push_str(&format!("&rel={}", replica::dashboard::http::url_encode(&rel)));
            }
            println!("{}", cli::control(&port, &path)?);
            Ok(0)
        }
        Command::Run { config, once, gate } => run_daemon(&config, once, gate),
    }
}

/// Hooks: standard error, counters, per-relation log files, the dashboard's activity list.
fn build_hooks(loaded: &config::Loaded, runtime: &Arc<Runtime>, events: &Arc<Mutex<VecDeque<Stamped>>>, cap: usize) -> Result<Vec<Hook>> {
    let mut hooks: Vec<Hook> = vec![Box::new(|e: &Event| {
        // A quiet cycle is routine; everything else is worth a line.
        if !matches!(e, Event::Cycle { ops: 0, errors: 0, .. }) {
            eprintln!("{}", render_line(e));
        }
    })];
    let rt = runtime.clone();
    hooks.push(Box::new(move |e: &Event| match e {
        Event::Resolved { ops, .. } => { rt.ops_applied.fetch_add(*ops as u64, Ordering::Relaxed); }
        Event::Errored { .. } | Event::StaleIntent { .. } => { rt.errors.fetch_add(1, Ordering::Relaxed); }
        _ => {}
    }));
    for rel in &loaded.relations {
        let Some(path) = &rel.log else { continue };
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let file = Mutex::new(fs::OpenOptions::new().create(true).append(true).open(path).with_context(|| format!("opening log {}", path.display()))?);
        let name = rel.name.clone();
        hooks.push(Box::new(move |e: &Event| {
            if e.relation() == name {
                let _ = writeln!(file.lock().unwrap(), "{}", render_line(e));
            }
        }));
    }
    let sink = events.clone();
    hooks.push(Box::new(move |e: &Event| {
        if matches!(e, Event::Cycle { .. }) {
            return;
        }
        let mut q = sink.lock().unwrap();
        if q.len() == cap.max(1) {
            q.pop_front();
        }
        q.push_back(Stamped::now(e));
    }));
    Ok(hooks)
}

/// The run subcommand.
fn run_daemon(config_path: &Path, once: bool, gate_flag: bool) -> Result<i32> {
    let (loaded, problems) = cli::load_checked(config_path)?;
    let state = loaded.settings.state.clone().context("the config needs a top-level state directory to run")?;
    fs::create_dir_all(&state).with_context(|| format!("creating {}", state.display()))?;
    install_signal_handlers();

    // One daemon per state directory, so two never race on the same journal.
    let lock_path = state.join("replica.lock");
    let lock = fs::File::create(&lock_path).with_context(|| format!("creating {}", lock_path.display()))?;
    if let Err(e) = lock.try_lock() {
        bail!("another replica is running against {} (lock {}): {e}", state.display(), lock_path.display());
    }
    let _lock = lock;

    let (tx, rx) = mpsc::channel::<Msg>();
    let (cmd_tx, cmd_rx) = mpsc::channel::<dashboard::Command>();
    let runtime = Arc::new(Runtime {
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION"),
        started: SystemTime::now(),
        config_path: config_path.to_path_buf(),
        state_dir: state.clone(),
        next_cycle: Mutex::new(None),
        last_cycle: Mutex::new(None),
        cycles: AtomicU64::new(0),
        ops_applied: AtomicU64::new(0),
        errors: AtomicU64::new(0),
        paused: AtomicBool::new(false),
        watching: AtomicBool::new(false),
        watched_roots: Mutex::new(Vec::new()),
        dirty: Mutex::new(BTreeSet::new()),
        cleared_intents: Mutex::new(Vec::new()),
        snapshot: Mutex::new(None),
        commands: cmd_tx,
        exit_code: AtomicI32::new(0),
    });
    // Dashboard commands arrive on their own channel and are forwarded, so the dashboard never learns about the watcher.
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            for cmd in cmd_rx {
                if tx.send(Msg::Command(cmd)).is_err() {
                    return;
                }
            }
        });
    }
    let events: Arc<Mutex<VecDeque<Stamped>>> = Arc::default();
    let hooks = build_hooks(&loaded, &runtime, &events, loaded.settings.dashboard.as_ref().map(|d| d.recent_events).unwrap_or(200))?;

    let settings = loaded.settings.clone();
    let store = Store::open(&state.join("state.db"))?;
    let blobs = Blobs { dir: state.join("blobs"), cap: settings.archive_cap };
    let mut eng = Engine::new(store, blobs.clone(), Box::new(Local), loaded.relations, hooks, state.clone());
    eng.reserved = config::reserved_paths(&settings, &eng.relations);
    if settings.gate || gate_flag {
        eng.set_global_gate(true);
    }
    let n_relations = eng.relations.len();
    let gate_open = !eng.gates.globally_gated();
    // The watcher lives as long as this binding; dropping it stops the notifications.
    let _watch = if settings.watch {
        let (ntx, nrx) = mpsc::channel::<Notice>();
        let tx = tx.clone();
        std::thread::spawn(move || {
            for n in nrx {
                if tx.send(Msg::Notice(n)).is_err() {
                    return;
                }
            }
        });
        match Watch::start(&eng.relations, &Local, ntx) {
            Ok(w) => {
                runtime.watching.store(true, Ordering::Relaxed);
                *runtime.watched_roots.lock().unwrap() = w.roots();
                Some(w)
            }
            Err(e) => {
                eprintln!("watch: disabled: {e:#}");
                None
            }
        }
    } else {
        None
    };
    let engine = Arc::new(Mutex::new(eng));
    eprintln!(
        "replica {} pid {}: {} from {}, state in {}, gate {}, watch {}{}",
        runtime.version, runtime.pid, dashboard::plural(n_relations, "relation"), dashboard::tilde(config_path), dashboard::tilde(&state),
        if gate_open { "open" } else { "closed" },
        if _watch.is_some() { "on" } else { "off" },
        match &settings.dashboard {
            Some(d) => format!(", dashboard at http://localhost:{}", d.port),
            None => String::new(),
        },
    );
    for p in &problems {
        eprintln!("startup: {p}");
    }
    if let Some(dash) = &settings.dashboard {
        let revalidate_path = config_path.to_path_buf();
        dashboard::serve(Arc::new(Dashboard {
            engine: engine.clone(),
            events,
            rt: runtime.clone(),
            config: dash.clone(),
            blobs,
            revalidate: Box::new(move || cli::load_checked(&revalidate_path).map(|_| ()).map_err(|e| format!("{e:#}"))),
        }));
    }
    let cleared = engine.lock().unwrap().recover()?;
    *runtime.cleared_intents.lock().unwrap() = cleared;

    let interval = settings.interval;
    let debounce = settings.debounce;
    let has_dashboard = settings.dashboard.is_some();
    let exit = |rt: &Runtime| -> i32 {
        let code = rt.exit_code.load(Ordering::SeqCst);
        eprintln!("replica pid {}: stopping with status {code}", rt.pid);
        code
    };
    // Run one cycle over all relations or the dirty ones, publish a snapshot for the dashboard, log a summary when anything happened.
    let run_cycle = |only: Option<&BTreeSet<String>>, paused: bool| -> Result<u64> {
        let begun = SystemTime::now();
        let timer = Instant::now();
        let (ops_before, errors_before) = (runtime.ops_applied.load(Ordering::Relaxed), runtime.errors.load(Ordering::Relaxed));
        let eng = engine.lock().unwrap();
        eng.cycle(only)?;
        let n = runtime.cycles.fetch_add(1, Ordering::Relaxed) + 1;
        let took = timer.elapsed();
        *runtime.last_cycle.lock().unwrap() = Some((begun, took));
        if only.is_none() {
            *runtime.next_cycle.lock().unwrap() = if paused || once { None } else { Some(SystemTime::now() + interval) };
        }
        if has_dashboard && !once {
            *runtime.snapshot.lock().unwrap() = Some(Arc::new(take_snapshot(&eng)));
        }
        drop(eng);
        let ops = runtime.ops_applied.load(Ordering::Relaxed) - ops_before;
        let errors = runtime.errors.load(Ordering::Relaxed) - errors_before;
        if ops > 0 || errors > 0 {
            eprintln!("cycle {n} done in {}: {}, {}", dashboard::fmt_dur(took), dashboard::plural(ops as usize, "file operation"), dashboard::plural(errors as usize, "error"));
        }
        Ok(errors)
    };

    let mut paused = false;
    let mut deadline = Instant::now();
    let mut dirty_deadline: Option<Instant> = None;
    loop {
        if STOP.load(Ordering::SeqCst) {
            return Ok(exit(&runtime));
        }
        let now = Instant::now();
        // A full cycle when the schedule is due; an early cycle over dirty relations when the quiet period has passed.
        if !paused && now >= deadline {
            let errors = run_cycle(None, false)?;
            runtime.dirty.lock().unwrap().clear();
            dirty_deadline = None;
            if once {
                if errors > 0 {
                    eprintln!("replica: {} in the only cycle", dashboard::plural(errors as usize, "error"));
                    return Ok(1);
                }
                return Ok(0);
            }
            deadline = Instant::now() + interval;
            continue;
        }
        if !paused && dirty_deadline.is_some_and(|d| now >= d) {
            let dirty: BTreeSet<String> = std::mem::take(&mut *runtime.dirty.lock().unwrap());
            dirty_deadline = None;
            if !dirty.is_empty() {
                run_cycle(Some(&dirty), false)?;
            }
            continue;
        }
        // Wait for the nearest deadline, a message, or a stop signal; signals set STOP alone, so wake at least once a second.
        let mut wait = Duration::from_secs(1);
        if !paused {
            wait = wait.min(deadline.saturating_duration_since(now));
            if let Some(d) = dirty_deadline {
                wait = wait.min(d.saturating_duration_since(now));
            }
        }
        match rx.recv_timeout(wait) {
            Ok(Msg::Command(dashboard::Command::Sync)) => {
                eprintln!("dashboard: sync requested");
                if paused {
                    run_cycle(None, true)?;
                } else {
                    deadline = Instant::now();
                }
            }
            Ok(Msg::Command(dashboard::Command::Pause)) => {
                paused = true;
                runtime.paused.store(true, Ordering::Relaxed);
                *runtime.next_cycle.lock().unwrap() = None;
                eprintln!("dashboard: paused");
            }
            Ok(Msg::Command(dashboard::Command::Resume)) => {
                paused = false;
                runtime.paused.store(false, Ordering::Relaxed);
                deadline = Instant::now();
                eprintln!("dashboard: resumed");
            }
            Ok(Msg::Command(dashboard::Command::Stop)) | Err(RecvTimeoutError::Disconnected) => return Ok(exit(&runtime)),
            Ok(Msg::Notice(Notice::Changed { relations, .. })) => {
                runtime.dirty.lock().unwrap().extend(relations);
                dirty_deadline = Some(Instant::now() + debounce);
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}
