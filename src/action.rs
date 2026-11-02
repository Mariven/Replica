//! Shell commands run by the daemon. Actions run in response to events, and transforms rewrite content on its way from one site to another. Each command can use `${name}` placeholders and has a timeout. Its output goes into the event that reports the run.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::event::Event;

/// The time a command may run before it is killed, if the configuration gives no timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// A command as configured: its text with `${name}` placeholders and its timeout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shell {
    pub text: String,
    pub timeout: Duration,
}

/// The result of a command run: its exit status, whether it timed out, and its output.
#[derive(Debug)]
pub struct Outcome {
    pub status: Option<i32>,
    pub timed_out: bool,
    /// Standard output followed by standard error, each in the order it arrived.
    pub output: String,
}

impl Outcome {
    /// True if the command exited with status zero before the timeout.
    pub fn ok(&self) -> bool {
        self.status == Some(0) && !self.timed_out
    }

    /// A one-line description for an event: the exit status or the timeout, then the last line of output.
    pub fn summary(&self) -> String {
        let how = if self.timed_out { "timed out".to_owned() } else { format!("exit {}", self.status.map(|s| s.to_string()).unwrap_or_else(|| "signal".into())) };
        let tail = self.output.trim();
        if tail.is_empty() { how } else { format!("{how}: {}", tail.lines().last().unwrap_or("")) }
    }
}

/// Replace each `${name}` with its value from the map. A placeholder whose name is not in the map stays as written.
pub fn expand(template: &str, vars: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                let name = &after[..end];
                match vars.get(name) {
                    Some(v) => out.push_str(v),
                    None => out.push_str(&rest[start..start + 3 + end]),
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Run a command through `sh -c` in `cwd`, with `env` added to its environment. The command is killed if it runs past its timeout.
pub fn run(shell: &Shell, cwd: &Path, env: &BTreeMap<String, String>) -> Result<Outcome> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&shell.text)
        .current_dir(cwd)
        .envs(env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting `{}`", shell.text))?;
    // Read both pipes on separate threads, so a command with a lot of output cannot fill a pipe and block while the loop below waits for it to exit.
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let out = std::thread::spawn(move || { let mut s = String::new(); let _ = stdout.read_to_string(&mut s); s });
    let err = std::thread::spawn(move || { let mut s = String::new(); let _ = stderr.read_to_string(&mut s); s });
    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait().context("waiting for the command")? {
            break status.code();
        }
        if started.elapsed() >= shell.timeout {
            let _ = child.kill();
            let _ = child.wait();
            timed_out = true;
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut output = out.join().unwrap_or_default();
    output.push_str(&err.join().unwrap_or_default());
    Ok(Outcome { status, timed_out, output })
}

/// The events a relation can run an action on, named by the keys under `on`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Trigger {
    /// Once per cycle in which the relation applied at least one operation.
    Changed,
    /// Once per applied operation.
    Resolved,
    /// When a closed gate holds a key's plan, and again if the plan changes.
    Held,
    /// When replicas of a key are held because of a conflict or an unreadable file, and again if the reasons change.
    Conflict,
    /// When deciding or applying a key fails, and again if the error changes.
    Error,
}

impl Trigger {
    /// The trigger for a key under `on`, or None if the key is not a trigger name.
    pub fn parse(word: &str) -> Option<Trigger> {
        match word {
            "changed" => Some(Trigger::Changed),
            "resolved" => Some(Trigger::Resolved),
            "held" => Some(Trigger::Held),
            "conflict" => Some(Trigger::Conflict),
            "error" => Some(Trigger::Error),
            _ => None,
        }
    }

    /// The trigger's key under `on`, which the command also gets as `REPLICA_EVENT`.
    pub fn word(self) -> &'static str {
        match self {
            Trigger::Changed => "changed",
            Trigger::Resolved => "resolved",
            Trigger::Held => "held",
            Trigger::Conflict => "conflict",
            Trigger::Error => "error",
        }
    }

    /// The trigger for an event, if any. No single event corresponds to `Changed`, so the engine fires it on its own once per cycle.
    pub fn of(event: &Event) -> Option<Trigger> {
        match event {
            Event::Resolved { ops, .. } if *ops > 0 => Some(Trigger::Resolved),
            Event::Held { .. } => Some(Trigger::Held),
            Event::Skipped { .. } => Some(Trigger::Conflict),
            Event::Errored { .. } | Event::StaleIntent { .. } => Some(Trigger::Error),
            _ => None,
        }
    }
}

/// A relation's actions, from its `on` block.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Actions {
    pub on: BTreeMap<Trigger, Shell>,
}

impl Actions {
    /// True if the relation has no actions.
    pub fn is_empty(&self) -> bool {
        self.on.is_empty()
    }

    /// Run the action for a trigger and return an event describing the run, or None if the trigger has no action. The variables fill the command's placeholders and also go into its environment as `REPLICA_<NAME>`.
    pub fn fire(&self, trigger: Trigger, relation: &str, vars: &BTreeMap<String, String>, cwd: &Path) -> Option<Event> {
        let shell = self.on.get(&trigger)?;
        let text = expand(&shell.text, vars);
        let mut env: BTreeMap<String, String> = vars.iter().map(|(k, v)| (format!("REPLICA_{}", k.to_ascii_uppercase()), v.clone())).collect();
        env.insert("REPLICA_EVENT".into(), trigger.word().into());
        let name = trigger.word().to_owned();
        Some(match run(&Shell { text: text.clone(), timeout: shell.timeout }, cwd, &env) {
            Ok(outcome) => Event::Action { relation: relation.to_owned(), name, ok: outcome.ok(), detail: format!("`{text}` {}", outcome.summary()) },
            Err(e) => Event::Action { relation: relation.to_owned(), name, ok: false, detail: format!("`{text}`: {e:#}") },
        })
    }
}

/// Run a transform command on `input`, writing to `output`. The command gets the two paths as `${in}` and `${out}`, and as `REPLICA_IN` and `REPLICA_OUT`. The transform succeeds if the command exits zero before its timeout and the output file exists.
pub fn transform(shell: &Shell, input: &Path, output: &Path, cwd: &Path) -> Result<()> {
    let vars: BTreeMap<String, String> = [("in".to_owned(), input.display().to_string()), ("out".to_owned(), output.display().to_string())].into_iter().collect();
    let text = expand(&shell.text, &vars);
    let env: BTreeMap<String, String> = [("REPLICA_IN".to_owned(), input.display().to_string()), ("REPLICA_OUT".to_owned(), output.display().to_string())].into_iter().collect();
    let outcome = run(&Shell { text: text.clone(), timeout: shell.timeout }, cwd, &env)?;
    if !outcome.ok() {
        bail!("transform `{text}` {}", outcome.summary());
    }
    if !output.is_file() {
        bail!("transform `{text}` wrote nothing to ${{out}}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Build a variable map from string pairs.
    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn expansion_replaces_known_placeholders_and_keeps_unknown_ones() {
        let v = vars(&[("path", "/a/b"), ("key", "f.txt")]);
        assert_eq!(expand("scp ${path} host:${key}", &v), "scp /a/b host:f.txt");
        assert_eq!(expand("echo ${nope} {hole}", &v), "echo ${nope} {hole}");
        assert_eq!(expand("unclosed ${path", &v), "unclosed ${path");
    }

    #[test]
    fn commands_run_with_output_status_and_timeout() {
        let tmp = TempDir::new().unwrap();
        let ok = run(&Shell { text: "echo hi; echo err >&2; exit 0".into(), timeout: DEFAULT_TIMEOUT }, tmp.path(), &BTreeMap::new()).unwrap();
        assert!(ok.ok());
        assert!(ok.output.contains("hi") && ok.output.contains("err"));
        let bad = run(&Shell { text: "echo nope; exit 3".into(), timeout: DEFAULT_TIMEOUT }, tmp.path(), &BTreeMap::new()).unwrap();
        assert!(!bad.ok());
        assert_eq!(bad.summary(), "exit 3: nope");
        let slow = run(&Shell { text: "sleep 5".into(), timeout: Duration::from_millis(100) }, tmp.path(), &BTreeMap::new()).unwrap();
        assert!(slow.timed_out && !slow.ok());
        assert_eq!(slow.summary(), "timed out");
    }

    #[test]
    fn actions_fire_with_event_variables_in_the_environment() {
        let tmp = TempDir::new().unwrap();
        let marker = tmp.path().join("ran");
        let actions = Actions { on: [(Trigger::Resolved, Shell { text: format!("echo \"$REPLICA_KEY ${{path}}\" > {}", marker.display()), timeout: DEFAULT_TIMEOUT })].into_iter().collect() };
        let ev = actions.fire(Trigger::Resolved, "r", &vars(&[("key", "f.txt"), ("path", "/x/f.txt")]), tmp.path()).unwrap();
        assert!(matches!(ev, Event::Action { ok: true, .. }), "{ev:?}");
        assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "f.txt /x/f.txt");
        assert!(actions.fire(Trigger::Held, "r", &BTreeMap::new(), tmp.path()).is_none());
    }

    #[test]
    fn transforms_must_write_their_output() {
        let tmp = TempDir::new().unwrap();
        let input = tmp.path().join("in.txt");
        std::fs::write(&input, "abc").unwrap();
        let output = tmp.path().join("out.txt");
        transform(&Shell { text: "tr a-z A-Z < ${in} > ${out}".into(), timeout: DEFAULT_TIMEOUT }, &input, &output, tmp.path()).unwrap();
        assert_eq!(std::fs::read_to_string(&output).unwrap(), "ABC");
        let err = transform(&Shell { text: "true".into(), timeout: DEFAULT_TIMEOUT }, &input, &tmp.path().join("missing"), tmp.path()).unwrap_err().to_string();
        assert!(err.contains("wrote nothing"), "{err}");
        let err = transform(&Shell { text: "exit 2".into(), timeout: DEFAULT_TIMEOUT }, &input, &output, tmp.path()).unwrap_err().to_string();
        assert!(err.contains("exit 2"), "{err}");
    }
}
