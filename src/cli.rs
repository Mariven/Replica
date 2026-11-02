//! The command line: subcommands, their arguments, and the ones that run without a daemon (check, explain, plan, forget) or talk to one over its dashboard port.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::blobs::Blobs;
use crate::config::{self, Loaded};
use crate::decide::describe_plan;
use crate::engine::{self, Engine, RelationState};
use crate::flow::Kind;
use crate::medium::Local;
use crate::site::display_names;
use crate::store::Store;

pub const USAGE: &str = "usage: replica <command> [options]

  run      --config PATH [--once] [--gate]      run cycles at the configured interval (--once: one cycle, then exit)
  check    --config PATH                         load the config, expand replicas, report overlaps, exit nonzero on problems
  explain  --config PATH [RELATION]              print how the config resolves: profiles, variables, patterns, replicas, flow
  plan     --config PATH [RELATION]              print what the next cycle would do, without writing anything
  status   (--port N | --config PATH) [--json]   ask the running daemon for its status
  sync | pause | resume | restart | halt (--port N | --config PATH)
  gate     open|closed [--relation NAME] (--port N | --config PATH)
  history  RELATION (--port N | --config PATH) [--json]
  forget   --config PATH RELATION REPLICA        drop the records of one replica, for a location that was replaced
  --help, --version";

/// A parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Run { config: PathBuf, once: bool, gate: bool },
    Check { config: PathBuf },
    Explain { config: PathBuf, relation: Option<String> },
    Plan { config: PathBuf, relation: Option<String> },
    Status { port: Port, json: bool },
    Control { port: Port, action: &'static str },
    Gate { port: Port, closed: bool, relation: Option<String> },
    History { port: Port, relation: String, json: bool },
    Forget { config: PathBuf, relation: String, replica: String },
    Help,
    Version,
}

/// Where to find the daemon: a port, or a config file whose dashboard setting has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Port {
    Number(u16),
    FromConfig(PathBuf),
}

impl Port {
    /// Resolve to a port number.
    pub fn resolve(&self) -> Result<u16> {
        match self {
            Port::Number(n) => Ok(*n),
            Port::FromConfig(path) => {
                let loaded = config::load_file(path)?;
                loaded.settings.dashboard.map(|d| d.port).with_context(|| format!("{} sets no dashboard port; pass --port", path.display()))
            }
        }
    }
}

/// Expand `~` in a command-line path.
fn path_arg(s: &str) -> PathBuf {
    PathBuf::from(shellexpand::tilde(s).into_owned())
}

/// Parse the command line after the program name.
pub fn parse(args: &[String]) -> Result<Command> {
    let Some(first) = args.first() else { bail!("{USAGE}") };
    let mut config: Option<PathBuf> = None;
    let mut port: Option<u16> = None;
    let mut once = false;
    let mut gate = false;
    let mut json = false;
    let mut relation: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut i = 1;
    while i < args.len() {
        let arg = args[i].as_str();
        match arg {
            "--config" => {
                i += 1;
                config = Some(path_arg(args.get(i).with_context(|| format!("--config needs a path\n{USAGE}"))?));
            }
            _ if arg.starts_with("--config=") => config = Some(path_arg(&arg["--config=".len()..])),
            "--port" => {
                i += 1;
                port = Some(args.get(i).with_context(|| format!("--port needs a number\n{USAGE}"))?.parse().context("--port must be a number")?);
            }
            _ if arg.starts_with("--port=") => port = Some(arg["--port=".len()..].parse().context("--port must be a number")?),
            "--relation" => {
                i += 1;
                relation = Some(args.get(i).with_context(|| format!("--relation needs a name\n{USAGE}"))?.clone());
            }
            "--once" => once = true,
            "--gate" => gate = true,
            "--json" => json = true,
            _ if arg.starts_with("--") => bail!("unknown option {arg:?}\n{USAGE}"),
            _ => positional.push(arg.to_owned()),
        }
        i += 1;
    }
    let need_config = |what: &str| config.clone().with_context(|| format!("{what} needs --config PATH\n{USAGE}"));
    let port_of = || -> Result<Port> {
        match (port, &config) {
            (Some(n), _) => Ok(Port::Number(n)),
            (None, Some(c)) => Ok(Port::FromConfig(c.clone())),
            (None, None) => bail!("{first} needs --port N or --config PATH\n{USAGE}"),
        }
    };
    let extra = |n: usize| -> Result<()> {
        if positional.len() > n {
            bail!("unexpected argument {:?}\n{USAGE}", positional[n]);
        }
        Ok(())
    };
    Ok(match first.as_str() {
        "--help" | "-h" | "help" => Command::Help,
        "--version" | "-V" | "version" => Command::Version,
        "run" => { extra(0)?; Command::Run { config: need_config("run")?, once, gate } }
        "check" => { extra(0)?; Command::Check { config: need_config("check")? } }
        "explain" => { extra(1)?; Command::Explain { config: need_config("explain")?, relation: positional.first().cloned() } }
        "plan" => { extra(1)?; Command::Plan { config: need_config("plan")?, relation: positional.first().cloned() } }
        "status" => { extra(0)?; Command::Status { port: port_of()?, json } }
        "sync" => { extra(0)?; Command::Control { port: port_of()?, action: "sync" } }
        "pause" => { extra(0)?; Command::Control { port: port_of()?, action: "pause" } }
        "resume" => { extra(0)?; Command::Control { port: port_of()?, action: "resume" } }
        "restart" => { extra(0)?; Command::Control { port: port_of()?, action: "restart" } }
        "halt" => { extra(0)?; Command::Control { port: port_of()?, action: "halt" } }
        "gate" => {
            extra(1)?;
            let closed = match positional.first().map(String::as_str) {
                Some("open") => false,
                Some("closed") | Some("close") => true,
                _ => bail!("gate needs open or closed\n{USAGE}"),
            };
            Command::Gate { port: port_of()?, closed, relation }
        }
        "history" => {
            extra(1)?;
            let relation = positional.first().cloned().with_context(|| format!("history needs a relation name\n{USAGE}"))?;
            Command::History { port: port_of()?, relation, json }
        }
        "forget" => {
            extra(2)?;
            let [relation, replica] = positional.as_slice() else { bail!("forget needs RELATION and REPLICA\n{USAGE}") };
            Command::Forget { config: need_config("forget")?, relation: relation.clone(), replica: replica.clone() }
        }
        other => bail!("unknown command {other:?}\n{USAGE}"),
    })
}

/// Load the configuration and run the static checks, as startup does. Reserved-path problems are fatal; others are returned.
pub fn load_checked(path: &Path) -> Result<(Loaded, Vec<String>)> {
    let loaded = config::load_file(path)?;
    engine::ensure_relations(&loaded.relations)?;
    let reserved = config::reserved_paths(&loaded.settings, &loaded.relations);
    let (fatal, problems): (Vec<_>, Vec<_>) = engine::check(&loaded.relations, &reserved, &Local).into_iter().partition(|p| p.reserved);
    if !fatal.is_empty() {
        bail!("{}", fatal.into_iter().map(|p| p.message).collect::<Vec<_>>().join("\n"));
    }
    Ok((loaded, problems.into_iter().map(|p| p.message).collect()))
}

/// The check subcommand. Returns the exit status.
pub fn check(path: &Path) -> Result<i32> {
    let (loaded, problems) = load_checked(path)?;
    for p in &problems {
        eprintln!("{p}");
    }
    if problems.is_empty() {
        println!("ok: {} from {}", crate::dashboard::plural(loaded.relations.len(), "relation"), path.display());
        return Ok(0);
    }
    Ok(1)
}

/// Pick the relations a subcommand works on.
fn select<'a>(loaded: &'a Loaded, relation: Option<&str>) -> Result<Vec<&'a engine::Relation>> {
    match relation {
        None => Ok(loaded.relations.iter().collect()),
        Some(name) => {
            let found: Vec<&engine::Relation> = loaded.relations.iter().filter(|r| r.name == name).collect();
            if found.is_empty() {
                bail!("no relation named {name:?} (configured: {})", loaded.relations.iter().map(|r| r.name.as_str()).collect::<Vec<_>>().join(", "));
            }
            Ok(found)
        }
    }
}

/// The explain subcommand: print how each relation resolves.
pub fn explain(path: &Path, relation: Option<&str>) -> Result<String> {
    let loaded = config::load_file(path)?;
    let mut out = String::new();
    let s = &loaded.settings;
    out.push_str(&format!("config: {}\n", path.display()));
    out.push_str(&format!("state: {}\n", s.state.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "(none; run needs one)".into())));
    out.push_str(&format!("interval: {:?}, watch: {}, debounce: {:?}, gate at start: {}\n", s.interval, s.watch, s.debounce, s.gate));
    if !s.ignore.is_empty() {
        out.push_str(&format!("ignore: {}\n", s.ignore.join(", ")));
    }
    for rel in select(&loaded, relation)? {
        out.push('\n');
        out.push_str(&format!("relation {}\n", rel.name));
        if let Some(prov) = loaded.provenance.get(&rel.name) {
            if !prov.profiles.is_empty() {
                out.push_str(&format!("    profiles: {}\n", prov.profiles.join(", ")));
            }
            for (k, v) in &prov.vars {
                out.push_str(&format!("    ${{{k}}} = {v}\n"));
            }
        }
        let identity: Vec<&str> = rel.identity.iter().map(String::as_str).collect();
        out.push_str(&format!("    key: {}\n", if identity.is_empty() { "(one file)".to_string() } else { format!("{{{}}}", identity.join("}, {")) }));
        if !rel.fanout.is_empty() {
            out.push_str(&format!("    fan-out: {}\n", rel.fanout.iter().cloned().collect::<Vec<_>>().join(", ")));
        }
        for site in &rel.sites {
            for loc in &site.locations {
                let depth = loc.pattern.depth().map(|d| d.to_string()).unwrap_or_else(|| "unbounded".into());
                out.push_str(&format!("    site {}: {}  (root {}, depth {depth})\n", site.name, loc.pattern, loc.pattern.root().display()));
                for v in loc.pattern.vars() {
                    for c in &v.constraints {
                        out.push_str(&format!("        {{{}}} must match {}\n", v.name, c.text()));
                    }
                }
            }
        }
        match engine::expand(rel, &Local) {
            Ok(replicas) => {
                let names = display_names(&replicas);
                for (r, name) in replicas.iter().zip(&names) {
                    let exists = if Local::default().is_dir_path(r.root()) { "" } else { "  (folder missing)" };
                    out.push_str(&format!("    replica {} [{}]: {}{exists}\n", name, r.id(), r.pattern));
                }
            }
            Err(e) => out.push_str(&format!("    replicas: cannot expand: {e:#}\n")),
        }
        for kind in Kind::ALL {
            let closure = rel.policy.flow.closure(kind);
            if closure.is_empty() {
                continue;
            }
            let parts: Vec<String> = closure.iter().filter(|(_, to)| !to.is_empty()).map(|(from, to)| format!("{from} -> {}", to.iter().cloned().collect::<Vec<_>>().join(", "))).collect();
            out.push_str(&format!("    {} travels: {}\n", kind.word(), parts.join("; ")));
        }
        if rel.policy.resolve.is_empty() {
            out.push_str("    resolve: (none; conflicts wait)\n");
        } else {
            out.push_str(&format!("    resolve: {}\n", rel.policy.resolve.iter().map(|r| r.to_string()).collect::<Vec<_>>().join("; ")));
        }
        if !rel.ignore_text.is_empty() {
            out.push_str(&format!("    ignore: {}\n", rel.ignore_text.join(", ")));
        }
        for ((a, b), shell) in &rel.transforms {
            out.push_str(&format!("    transform {a} -> {b}: {}\n", shell.text));
        }
        for (trigger, shell) in &rel.actions.on {
            out.push_str(&format!("    on {}: {}\n", trigger.word(), shell.text));
        }
        if let Some(log) = &rel.log {
            out.push_str(&format!("    log: {}\n", log.display()));
        }
        out.push_str(&format!("    sync_times: {}, watch: {}, gate at start: {}\n", rel.sync_times, rel.watch, rel.gate_default));
    }
    Ok(out)
}

/// Open the engine over the configured state directory without a lock, for read-only subcommands.
fn open_engine(loaded: Loaded) -> Result<Engine> {
    let state = loaded.settings.state.clone().context("the config needs a top-level state directory")?;
    let store = Store::open(&state.join("state.db"))?;
    let blobs = Blobs { dir: state.join("blobs"), cap: loaded.settings.archive_cap };
    let mut eng = Engine::new(store, blobs, Box::new(Local), loaded.relations, Vec::new(), state.clone());
    eng.reserved = config::reserved_paths(&loaded.settings, &eng.relations);
    Ok(eng)
}

/// The plan subcommand: what the next cycle would do, from the report.
pub fn plan(path: &Path, relation: Option<&str>) -> Result<String> {
    let loaded = config::load_file(path)?;
    let only: Option<BTreeSet<String>> = relation.map(|r| BTreeSet::from([r.to_owned()]));
    if let Some(name) = relation {
        select(&loaded, Some(name))?;
    }
    let eng = open_engine(loaded)?;
    let report = eng.report();
    let mut out = String::new();
    for rel in report.relations.iter().filter(|r| only.as_ref().is_none_or(|o| o.contains(&r.name))) {
        let state = rel.state(report.globally_gated);
        out.push_str(&format!("{}: {}\n", rel.name, state.word()));
        if let Some(e) = &rel.error {
            out.push_str(&format!("    error: {e}\n"));
        }
        if let Some(u) = &rel.unavailable {
            out.push_str(&format!("    {u}\n"));
        }
        for w in &rel.warnings {
            out.push_str(&format!("    warning: {w}\n"));
        }
        let gated = report.globally_gated || rel.gated;
        for row in rel.rows.iter().filter(|r| r.needs_attention()) {
            if !row.plan.is_empty() {
                let names: std::collections::BTreeMap<&str, &str> = rel.replicas.iter().map(|r| (r.id.as_str(), r.name.as_str())).collect();
                out.push_str(&format!("    {}: {} {}\n", row.key, if gated { "held:" } else { "next:" }, describe_plan(&names, &row.plan)));
            }
            if let Some(note) = &row.note {
                out.push_str(&format!("    {}: waiting: {note}\n", row.key));
            }
        }
        if matches!(state, RelationState::InSync) {
            out.push_str(&format!("    {} in sync\n", crate::dashboard::plural(rel.rows.len(), "file")));
        }
    }
    Ok(out)
}

/// The forget subcommand: drop the records of one replica.
pub fn forget(path: &Path, relation: &str, replica: &str) -> Result<String> {
    let loaded = config::load_file(path)?;
    select(&loaded, Some(relation))?;
    let eng = open_engine(loaded)?;
    let known = eng.store.replicas_with_records(relation)?;
    if !known.contains(replica) {
        bail!("relation {relation:?} has no records for replica {replica:?} (replicas with records: {})", known.into_iter().collect::<Vec<_>>().join(", "));
    }
    let n = eng.store.forget_replica(relation, replica)?;
    Ok(format!("forgot {} of {relation}/{replica}", crate::dashboard::plural(n, "record")))
}

/// Send one request to the daemon and return the body.
fn request(port: u16, method: &str, path: &str) -> Result<(u16, String)> {
    let mut stream = TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_secs(3)).with_context(|| format!("no replica answering on port {port}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    write!(stream, "{method} {path} HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n")?;
    let mut text = String::new();
    stream.read_to_string(&mut text)?;
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status: u16 = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    Ok((status, body.to_owned()))
}

/// Post a control action.
pub fn control(port: &Port, path: &str) -> Result<String> {
    let port = port.resolve()?;
    let (status, body) = request(port, "POST", path)?;
    if status >= 400 {
        bail!("replica refused {path}: {status} {}", body.trim());
    }
    Ok(format!("ok: {path}"))
}

/// Fetch status.json, or render it in words.
pub fn status(port: &Port, json: bool) -> Result<String> {
    let port = port.resolve()?;
    let (status, body) = request(port, "GET", "/status.json")?;
    if status != 200 {
        bail!("status {status}: {body}");
    }
    if json {
        return Ok(body);
    }
    // A small reading of the JSON for the terminal, without a JSON library: the fields are flat and known.
    let field = |name: &str| -> String {
        let needle = format!("\"{name}\":");
        body.find(&needle).map(|i| {
            let rest = &body[i + needle.len()..];
            let end = rest.find([',', '}']).unwrap_or(rest.len());
            rest[..end].trim_matches('"').to_owned()
        }).unwrap_or_default()
    };
    let mut out = format!("replica {} pid {}: {}, gate {}, {} awaiting sync, {} held\n", field("version"), field("pid"), field("state"), field("gate"), field("awaiting_sync"), field("held"));
    out.push_str(&format!("cycles {}, operations {}, errors {}\n", field("cycles"), field("ops_applied"), field("errors")));
    if let Some(start) = body.find("\"relations\":[") {
        for item in body[start..].split("{\"name\":").skip(1) {
            let name = item.split('"').nth(1).unwrap_or("?");
            let state = item.split("\"state\":\"").nth(1).and_then(|s| s.split('"').next()).unwrap_or("?");
            out.push_str(&format!("    {name}: {state}\n"));
        }
    }
    Ok(out)
}

/// Fetch the history of a relation.
pub fn history(port: &Port, relation: &str, json: bool) -> Result<String> {
    let port = port.resolve()?;
    let path = format!("/history?rel={}&json=1", crate::dashboard::http::url_encode(relation));
    let (status, body) = request(port, "GET", &path)?;
    if status != 200 {
        bail!("status {status}: {body}");
    }
    if json {
        return Ok(body);
    }
    let mut out = String::new();
    for item in body.split("{\"at\":").skip(1) {
        let key = item.split("\"key\":\"").nth(1).and_then(|s| s.split('"').next()).unwrap_or("?");
        let replica = item.split("\"replica\":\"").nth(1).and_then(|s| s.split('"').next()).unwrap_or("?");
        let reason = item.split("\"reason\":\"").nth(1).and_then(|s| s.split('"').next()).unwrap_or("?");
        let hash = item.split("\"hash\":\"").nth(1).and_then(|s| s.split('"').next()).unwrap_or("?");
        let available = item.contains("\"available\":true");
        out.push_str(&format!("{key} at {replica}: {reason}, {}{}\n", &hash[..hash.len().min(8)], if available { "" } else { " (evicted)" }));
    }
    if out.is_empty() {
        out.push_str("nothing archived\n");
    }
    Ok(out)
}

/// A helper on the local medium for explain output.
trait DirCheck {
    fn is_dir_path(&self, p: &Path) -> bool;
}

impl DirCheck for Local {
    fn is_dir_path(&self, p: &Path) -> bool {
        crate::medium::Medium::is_dir(self, p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn commands_parse_with_their_options() {
        assert_eq!(parse(&args(&["run", "--config", "/c.yaml", "--once", "--gate"])).unwrap(), Command::Run { config: "/c.yaml".into(), once: true, gate: true });
        assert_eq!(parse(&args(&["check", "--config=/c.yaml"])).unwrap(), Command::Check { config: "/c.yaml".into() });
        assert_eq!(parse(&args(&["explain", "--config", "/c.yaml", "lib"])).unwrap(), Command::Explain { config: "/c.yaml".into(), relation: Some("lib".into()) });
        assert_eq!(parse(&args(&["status", "--port", "18516", "--json"])).unwrap(), Command::Status { port: Port::Number(18516), json: true });
        assert_eq!(parse(&args(&["gate", "closed", "--relation", "lib", "--config", "/c.yaml"])).unwrap(), Command::Gate { port: Port::FromConfig("/c.yaml".into()), closed: true, relation: Some("lib".into()) });
        assert_eq!(parse(&args(&["history", "lib", "--port=1"])).unwrap(), Command::History { port: Port::Number(1), relation: "lib".into(), json: false });
        assert_eq!(parse(&args(&["forget", "--config", "/c.yaml", "lib", "dest"])).unwrap(), Command::Forget { config: "/c.yaml".into(), relation: "lib".into(), replica: "dest".into() });
        assert_eq!(parse(&args(&["halt", "--port", "2"])).unwrap(), Command::Control { port: Port::Number(2), action: "halt" });
        assert_eq!(parse(&args(&["--help"])).unwrap(), Command::Help);
        assert_eq!(parse(&args(&["version"])).unwrap(), Command::Version);
        assert!(parse(&args(&["run"])).unwrap_err().to_string().contains("needs --config"));
        assert!(parse(&args(&["status"])).unwrap_err().to_string().contains("--port"));
        assert!(parse(&args(&["gate", "sideways", "--port", "1"])).unwrap_err().to_string().contains("open or closed"));
        assert!(parse(&args(&["bogus"])).unwrap_err().to_string().contains("unknown command"));
        assert!(parse(&args(&[])).unwrap_err().to_string().contains("usage:"));
        assert!(parse(&args(&["check", "--config", "/c", "extra"])).unwrap_err().to_string().contains("unexpected argument"));
    }
}
