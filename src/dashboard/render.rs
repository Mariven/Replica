//! HTML for the dashboard pages, assembled region by region: header, status strip, unfinished operations, one section per relation, activity.

use std::fs;
use std::sync::atomic::Ordering;
use std::time::SystemTime;

use crate::blobs::Blobs;
use crate::config::DashboardConfig;
use crate::engine::{RelationReport, RelationState, Report, RowView};
use crate::event::{Stamped, Tone};
use crate::observe::{Observation, Transition, hex, short_hash};
use crate::store::ArchiveEntry;

use super::http::{esc, json_str, url_encode};
use super::status::totals;
use super::{Runtime, Snapshot, fmt_ago, fmt_clock, fmt_dur, fmt_size, fmt_stamp, plural, tilde, tilde_str};

pub const CSS: &str = include_str!("style.css");
pub const FAVICON: &str = include_str!("favicon.svg");

const HEAD: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>replica</title>\
    <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
    <link rel=\"icon\" type=\"image/svg+xml\" href=\"/favicon.svg\">\
    <link rel=\"stylesheet\" href=\"/style.css\">";

/// Rows of either kind shown per relation before the table is cut.
const ROW_CAP: usize = 50;
/// Archive versions shown on the history page before it is cut.
const HISTORY_CAP: usize = 100;

/// Everything the main page renders.
pub struct PageData<'a> {
    pub snap: &'a Snapshot,
    /// The snapshot comes from an earlier cycle because a cycle is running now.
    pub stale: bool,
    pub recent: &'a [Stamped],
    pub rt: &'a Runtime,
    pub dash: &'a DashboardConfig,
    pub blobs: &'a Blobs,
    /// Relation whose rows are all shown, from `?rel=<name>&all=1`.
    pub show_all: Option<String>,
}

/// Wrap body markup in the page skeleton.
fn page(head_extra: &str, body: &str) -> String {
    format!("{HEAD}{head_extra}</head><body>{body}</body></html>")
}

/// A colored tag.
fn tag(class: &str, text: &str) -> String {
    format!("<span class=\"tag {class}\">{}</span>", esc(text))
}

/// A form with one button posting to an action.
fn button(action: &str, label: &str, class: &str, title: &str, confirm: Option<&str>) -> String {
    let onsubmit = confirm.map(|c| format!(" onsubmit=\"return confirm('{}')\"", esc(c))).unwrap_or_default();
    let class_attr = if class.is_empty() { String::new() } else { format!(" class=\"{class}\"") };
    format!("<form method=\"post\" action=\"{action}\"{onsubmit}><button{class_attr} title=\"{}\">{label}</button></form>", esc(title))
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

/// The writing / gated switch. The current side is a highlighted label; the other side is the button.
fn gate_switch(relation: Option<&str>, gated: bool, disabled_note: Option<&str>) -> String {
    let target = |to: &str| match relation {
        Some(rel) => format!("/gate?rel={}&to={to}", url_encode(rel)),
        None => format!("/gate?to={to}"),
    };
    if let Some(note) = disabled_note {
        return format!("<span class=\"gate-switch disabled\" title=\"{}\"><span class=\"gate-state\">writing</span><span class=\"gate-state\">gated</span></span>", esc(note));
    }
    let (writing, gated_seg) = if gated {
        (
            format!("<form method=\"post\" action=\"{}\"><button class=\"gate-button\" title=\"Open the gate. The next sync applies pending changes.\">writing</button></form>", target("open")),
            "<span class=\"gate-state current warn\">gated</span>".to_string(),
        )
    } else {
        (
            "<span class=\"gate-state current ok\">writing</span>".to_string(),
            format!("<form method=\"post\" action=\"{}\"><button class=\"gate-button\" title=\"Close the gate. Replica keeps planning and reporting without writing files.\">gated</button></form>", target("closed")),
        )
    };
    format!("<span class=\"gate-switch\">{writing}{gated_seg}</span>")
}

/// Tag markup for a relation state.
fn state_tag(state: &RelationState) -> String {
    match state {
        RelationState::Error => tag("bad", "failed"),
        RelationState::Unavailable => tag("warn", "unavailable"),
        RelationState::Awaiting(n) => tag("warn", &format!("{n} awaiting sync")),
        RelationState::Held(n) => tag("warn", &format!("{n} held by gate")),
        RelationState::Degraded => tag("warn", "partly unreadable"),
        RelationState::FolderMissing => tag("warn", "folder missing"),
        RelationState::InSync => tag("ok", "in sync"),
    }
}

/// Tags for counts of awaiting and held rows, or the state tag when neither applies.
fn count_tags(awaiting: usize, held: usize, fallback: &RelationState) -> String {
    let mut parts = Vec::new();
    if awaiting > 0 {
        parts.push(tag("warn", &format!("{awaiting} awaiting sync")));
    }
    if held > 0 {
        parts.push(tag("warn", &format!("{held} held by gate")));
    }
    if parts.is_empty() { state_tag(fallback) } else { parts.join(" ") }
}

/// The header with the process controls.
fn header(rt: &Runtime, globally_gated: bool) -> String {
    let paused = rt.paused.load(Ordering::Relaxed);
    let pause = if paused { button("/resume", "resume", "", "Resume the schedule, starting with a sync.", None) } else { button("/pause", "pause", "", "Hold the schedule; the dashboard keeps answering.", None) };
    format!(
        "<header><span class=\"app-name\">Replica</span><span class=\"text-light\">{}</span>{}{}\
         <div class=\"daemon-controls\"><span class=\"gate-label\">all writes</span>{}{}{pause}{}{}</div></header>",
        rt.version,
        if paused { format!(" {}", tag("warn", "paused")) } else { String::new() },
        if globally_gated { " <span class=\"tag warn\" title=\"No files are written until the gate opens.\">all writes gated</span>".to_string() } else { String::new() },
        gate_switch(None, globally_gated, None),
        button("/sync", "sync now", "", "Run a sync now instead of waiting for the next scheduled one.", None),
        button("/restart", "restart", "", "Reload the configuration by restarting under launchd.", None),
        button("/halt", "halt", "bad", "Stop replica.", Some("Stop replica? launchd leaves it stopped until you start it again.")),
    )
}

/// One cell of the status strip.
fn stat(label: &str, value: String) -> String {
    format!("<div class=\"stat\"><div class=\"stat-label\">{label}</div><div class=\"stat-value\">{value}</div></div>")
}

/// The status strip: process, configuration, database, timing, counters, blob store, overall state.
fn status_strip(v: &PageData, states: &[RelationState]) -> String {
    let (rt, report) = (v.rt, &v.snap.report);
    let now = SystemTime::now();
    let db_path = rt.state_dir.join("state.db");
    let db_size = fs::metadata(&db_path).map(|m| fmt_size(m.len())).unwrap_or_else(|_| "?".into());
    let uptime = now.duration_since(rt.started).unwrap_or_default();
    let ops = rt.ops_applied.load(Ordering::Relaxed);
    let errors = rt.errors.load(Ordering::Relaxed);
    let last = *rt.last_cycle.lock().unwrap();
    let next = *rt.next_cycle.lock().unwrap();
    let paused = rt.paused.load(Ordering::Relaxed);
    let worst = states.iter().min().cloned().unwrap_or(RelationState::InSync);
    let (total_awaiting, total_held) = totals(report);
    let mut h = String::from("<div class=\"status-strip\">");
    h.push_str(&stat("Process", format!("pid {}, up {}", rt.pid, fmt_dur(uptime))));
    h.push_str(&stat("Configuration", esc(&tilde(&rt.config_path))));
    let db_detail = match &v.snap.db_stats {
        Ok((records, intents)) => {
            let note = if *intents > 0 { format!(", <span class=\"bad\">{}</span>", plural(*intents as usize, "unfinished intent")) } else { String::new() };
            format!("{db_size}, {}{note}", plural(*records as usize, "record"))
        }
        Err(e) => format!("<span class=\"bad\">unreadable: {}</span>", esc(e)),
    };
    h.push_str(&stat("Database", format!("{}, {db_detail}", esc(&tilde(&db_path)))));
    h.push_str(&stat("Last sync", match last {
        Some((at, took)) => format!("{}, took {}", fmt_clock(at), fmt_dur(took)),
        None => "not yet".into(),
    }));
    h.push_str(&stat("Next sync", match next {
        Some(at) if !paused => format!("{} <span class=\"text-medium\">(in {})</span>", fmt_clock(at), at.duration_since(now).map(fmt_dur).unwrap_or_else(|_| "0s".into())),
        _ => if paused { "<span class=\"warn\">paused</span>".into() } else { "running now".into() },
    }));
    let watch = if rt.watching.load(Ordering::Relaxed) {
        let dirty = rt.dirty.lock().unwrap();
        if dirty.is_empty() { "watching, quiet".to_string() } else { format!("watching, changes in {}", dirty.iter().cloned().collect::<Vec<_>>().join(", ")) }
    } else {
        "schedule alone".to_string()
    };
    h.push_str(&stat("Watch", esc(&watch)));
    let error_note = if errors > 0 { format!(", <span class=\"bad\">{}</span>", plural(errors as usize, "error")) } else { String::new() };
    h.push_str(&stat("This run", format!("{}{error_note}", plural(ops as usize, "file operation"))));
    h.push_str(&stat("Archive", match &v.snap.blob_stats {
        Ok((rows, bytes)) => format!("{}, {} of {}", plural(*rows as usize, "saved version"), fmt_size(*bytes), fmt_size(v.blobs.cap)),
        Err(e) => format!("<span class=\"bad\">unreadable: {}</span>", esc(e)),
    }));
    h.push_str(&stat("State", match worst {
        RelationState::Awaiting(_) | RelationState::Held(_) => count_tags(total_awaiting, total_held, &worst),
        RelationState::InSync => tag("ok", "everything in sync"),
        other => state_tag(&other),
    }));
    h.push_str("</div>");
    if v.stale {
        h.push_str(&format!("<div class=\"text-medium\" style=\"margin-top:6px\">A sync is running. This report was taken at {}, after the previous one.</div>", fmt_clock(v.snap.at)));
    }
    h
}

/// Unfinished operations: journal rows cleared at startup and any live ones, which mean a plan just failed.
fn unfinished(v: &PageData) -> String {
    let cleared = v.rt.cleared_intents.lock().unwrap();
    let live = v.snap.intents.as_ref().map(|i| i.as_slice()).unwrap_or(&[]);
    if cleared.is_empty() && live.is_empty() && v.snap.intents.is_ok() {
        return String::new();
    }
    let mut h = String::from("<section><div class=\"section-header\"><b class=\"bad\">Unfinished operations</b><span class=\"text-medium\">A previous run stopped mid-write. The next sync plans these again from the last recorded state.</span></div>");
    if let Err(e) = &v.snap.intents {
        h.push_str(&format!("<div class=\"bad\">intent table unreadable: {}</div>", esc(e)));
    }
    for i in cleared.iter() {
        h.push_str(&format!("<div class=\"bad\">{} {}: {} <span class=\"text-light\">(found at startup)</span></div>", esc(&i.relation), esc(&i.key.to_string()), esc(&i.detail)));
    }
    for i in live {
        h.push_str(&format!("<div class=\"bad\">{} {}: {}</div>", esc(&i.relation), esc(&i.key.to_string()), esc(&i.detail)));
    }
    h.push_str("</section>");
    h
}

/// The section header of a relation: name, counts, state, history link, gate switch.
fn relation_header(rel: &RelationReport, state: &RelationState, globally_gated: bool) -> String {
    let (awaiting, held) = rel.counts(globally_gated);
    let tags = match state {
        RelationState::Awaiting(_) | RelationState::Held(_) => count_tags(awaiting, held, state),
        other => state_tag(other),
    };
    let controls = format!(
        "<span class=\"section-controls\"><a href=\"/history?rel={}\" title=\"Overwritten and deleted versions kept in the archive.\">history</a>{}</span>",
        url_encode(&rel.name),
        gate_switch(Some(&rel.name), rel.gated, globally_gated.then_some("The global gate is closed, so this switch has no effect until it opens.")),
    );
    format!(
        "<div class=\"section-header\"><b>{}</b><span class=\"text-medium\">{} across {}</span>{tags}{controls}</div>",
        esc(&rel.name), plural(rel.rows.len(), "file"), plural(rel.replicas.len(), "location"),
    )
}

/// The sites of a relation with their replicas' paths, flowing inline.
fn locations(rel: &RelationReport) -> String {
    let mut h = String::from("<div class=\"locations\">");
    for site in &rel.sites {
        let note = if site.notes.is_empty() { String::new() } else { format!(" <span class=\"text-light\">({})</span>", esc(&site.notes.join(", "))) };
        let paths: Vec<String> = rel.replicas.iter().filter(|r| r.site == site.name).map(|r| {
            format!("{}{}", esc(&tilde_str(&r.pattern)), if r.root_exists { "" } else { " <span class=\"warn\">(folder does not exist)</span>" })
        }).collect();
        h.push_str(&format!("<span class=\"location\"><b>{}</b>{note} <span class=\"path\">{}</span></span>", esc(&site.name), paths.join(", ")));
    }
    h.push_str("</div>");
    h
}

/// One cell of the file table.
fn cell(observation: &Observation, transition: Transition) -> String {
    match observation {
        Observation::Present(snap) => {
            let (word, cls) = match transition {
                Transition::Unchanged => ("in sync", "ok"),
                Transition::Edited => ("edited", "warn"),
                Transition::Appeared => ("new", "warn"),
                Transition::Recreated => ("recreated", "warn"),
                _ => ("changed", "warn"),
            };
            format!("<td title=\"content hash {}\">{}<div class=\"file-meta\">{}, {}</div></td>", short_hash(&snap.hash), tag(cls, word), fmt_size(snap.size), fmt_stamp(snap.mtime))
        }
        Observation::Missing if transition == Transition::Disappeared => format!("<td>{}</td>", tag("bad", "deleted")),
        Observation::Missing => "<td><span class=\"text-light\">absent</span></td>".to_string(),
        Observation::Unreadable(e) => format!("<td>{}<div class=\"file-meta\">{}</div></td>", tag("bad", "unreadable"), esc(e)),
    }
}

/// One row of the file table: the key with its plan or note, then a cell per replica.
fn row(r: &RowView, gated: bool) -> String {
    let plan = if !r.plan.is_empty() {
        let verb = if gated { "held" } else { "next sync" };
        format!("<div class=\"plan {}\"><span class=\"plan-label\">{verb}</span>{}</div>", if gated { "warn" } else { "" }, esc(&r.plan_text))
    } else if let Some(note) = &r.note {
        format!("<div class=\"plan\"><span class=\"plan-label\">waiting</span>{}</div>", esc(note))
    } else {
        String::new()
    };
    let mut h = format!("<tr><td>{}{plan}</td>", esc(&r.key));
    for c in &r.cells {
        h.push_str(&cell(&c.observation, c.transition));
    }
    h.push_str("</tr>");
    h
}

/// The file table of a relation, pending rows first, cut at ROW_CAP unless the page asks for all.
fn file_table(rel: &RelationReport, gated: bool, show_all: bool) -> String {
    if rel.rows.is_empty() {
        return "<div class=\"text-light\" style=\"margin-top:6px\">no files yet</div>".to_string();
    }
    let mut h = String::from("<table><tr><th>file</th>");
    for r in &rel.replicas {
        h.push_str(&format!("<th>{}</th>", esc(&r.name)));
    }
    h.push_str("</tr>");
    let cap = if show_all { usize::MAX } else { ROW_CAP };
    let (pending, in_sync): (Vec<&RowView>, Vec<&RowView>) = rel.rows.iter().partition(|r| r.needs_attention());
    for r in pending.iter().take(cap).chain(in_sync.iter().take(cap)) {
        h.push_str(&row(r, gated));
    }
    h.push_str("</table>");
    let hidden_pending = pending.len().saturating_sub(cap);
    let hidden_synced = in_sync.len().saturating_sub(cap);
    if hidden_pending + hidden_synced > 0 {
        let mut parts = Vec::new();
        if hidden_pending > 0 {
            parts.push(format!("{hidden_pending} more awaiting sync"));
        }
        if hidden_synced > 0 {
            parts.push(format!("{hidden_synced} more in sync"));
        }
        h.push_str(&format!("<div class=\"more-files\">and {}; <a href=\"/?rel={}&all=1\">show all</a></div>", parts.join(" and "), url_encode(&rel.name)));
    }
    h
}

/// One relation section.
fn relation_section(rel: &RelationReport, state: &RelationState, globally_gated: bool, show_all: bool) -> String {
    let mut h = format!("<section>{}", relation_header(rel, state, globally_gated));
    if let Some(e) = &rel.error {
        h.push_str(&format!("<div class=\"bad\">{}</div></section>", esc(e)));
        return h;
    }
    h.push_str(&locations(rel));
    if let Some(reason) = &rel.unavailable {
        h.push_str(&format!("<div class=\"warn\" style=\"margin-top:6px\">{} Nothing is planned until the folder is back.</div></section>", esc(reason)));
        return h;
    }
    for w in &rel.warnings {
        h.push_str(&format!("<div class=\"warn\" style=\"margin-top:6px\">{}</div>", esc(w)));
    }
    h.push_str(&file_table(rel, globally_gated || rel.gated, show_all));
    h.push_str("</section>");
    h
}

/// The activity list of recent events.
fn activity(recent: &[Stamped]) -> String {
    let mut h = String::from("<section><div class=\"section-header\"><b>Activity</b></div>");
    if recent.is_empty() {
        h.push_str("<div class=\"text-light\" style=\"margin-top:4px\">nothing yet</div>");
    }
    for s in recent {
        let e = &s.event;
        let text = if e.key().is_empty() { e.text() } else { format!("{}: {}", e.key(), e.text()) };
        h.push_str(&format!(
            "<div class=\"event\"><span class=\"event-age\">{}</span><span class=\"event-kind tag {}\">{}</span><span class=\"event-relation\">{}</span><span class=\"text-medium\">{}</span></div>",
            esc(&fmt_ago(s.at)), tone_class(e.tone()), esc(e.kind()), esc(e.relation()), esc(&text)
        ));
    }
    h.push_str("</section>");
    h
}

/// The refresh script: swap the body in place so the tab never flashes, leave hidden tabs and text selections alone, show a banner when the daemon stops answering.
fn refresh_script(seconds: u64) -> String {
    format!(
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
        seconds.max(1) * 1000
    )
}

/// The main page.
pub fn main_page(v: &PageData) -> String {
    let report: &Report = &v.snap.report;
    let states: Vec<RelationState> = report.relations.iter().map(|r| r.state(report.globally_gated)).collect();
    let mut h = header(v.rt, report.globally_gated);
    h.push_str(&status_strip(v, &states));
    h.push_str(&unfinished(v));
    for (rel, state) in report.relations.iter().zip(&states) {
        let show_all = v.show_all.as_deref() == Some(rel.name.as_str());
        h.push_str(&relation_section(rel, state, report.globally_gated, show_all));
    }
    h.push_str(&activity(v.recent));
    h.push_str(&refresh_script(v.dash.refresh));
    page("", &h)
}

/// A standalone notice, with a timed return to the main page when `reload` is set.
pub fn notice_page(title: &str, text: &str, reload: bool) -> String {
    page(
        if reload { "<meta http-equiv=\"refresh\" content=\"5;url=/\">" } else { "" },
        &format!("<header><a href=\"/\">&larr; back</a><span class=\"app-name\">Replica</span><span class=\"text-medium\">{}</span></header><section>{}</section>", esc(title), esc(text)),
    )
}

/// The history page of a relation: archived versions, newest first, with downloads while the blob is still on disk.
pub fn history_page(rel: &str, entries: Result<Vec<ArchiveEntry>, String>, blobs: &Blobs, all: bool) -> String {
    let mut h = format!("<header><a href=\"/\">&larr; back</a><span class=\"app-name\">Replica</span><span class=\"text-medium\">history of</span><b>{}</b></header>", esc(rel));
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
        "<section><div class=\"section-header\"><b>Saved versions</b><span class=\"text-medium\">{}, newest first. To restore one, download it and put it back where the relation expects it; a one-way relation reverts a file restored on its downstream side.</span></div>",
        plural(entries.len(), "version"),
    ));
    h.push_str("<table><tr><th>when</th><th>file</th><th>replica</th><th>path</th><th>reason</th><th>content</th><th></th></tr>");
    for e in &entries[..shown] {
        let digest = hex(&e.hash);
        let download = if blobs.has(&e.hash) {
            let name = e.path.rsplit('/').next().unwrap_or(&e.key);
            format!("<a href=\"/blob?h={digest}&as={}\">download</a>", url_encode(name))
        } else {
            "<span class=\"text-light\">evicted</span>".to_string()
        };
        h.push_str(&format!(
            "<tr><td title=\"{}\">{}</td><td>{}</td><td>{}</td><td class=\"text-medium\">{}</td><td>{}</td><td class=\"text-medium\" title=\"content hash {digest}\">{}, {}</td><td>{download}</td></tr>",
            fmt_ago(e.at), fmt_stamp(e.at), esc(&e.key), esc(&e.replica), esc(&tilde_str(&e.path)),
            tag(if e.reason == "deleted" { "bad" } else { "warn" }, &e.reason), fmt_size(e.size), &digest[..8],
        ));
    }
    h.push_str("</table>");
    if shown < entries.len() {
        h.push_str(&format!("<div class=\"more-files\">and {} older; <a href=\"/history?rel={}&all=1\">show all</a></div>", entries.len() - shown, url_encode(rel)));
    }
    h.push_str("</section>");
    page("", &h)
}

/// The history of a relation as JSON, for the command line.
pub fn history_json(rel: &str, entries: Result<Vec<ArchiveEntry>, String>, blobs: &Blobs) -> String {
    match entries {
        Err(e) => format!("{{\"relation\":{},\"error\":{}}}", json_str(rel), json_str(&e)),
        Ok(entries) => {
            let items: Vec<String> = entries.iter().map(|e| format!(
                "{{\"at\":{},\"key\":{},\"replica\":{},\"path\":{},\"reason\":{},\"size\":{},\"hash\":{},\"available\":{}}}",
                super::status::epoch_secs(e.at), json_str(&e.key), json_str(&e.replica), json_str(&e.path), json_str(&e.reason), e.size, json_str(&hex(&e.hash)), blobs.has(&e.hash),
            )).collect();
            format!("{{\"relation\":{},\"versions\":[{}]}}", json_str(rel), items.join(","))
        }
    }
}
