//! Replays a trace through the engine and renders every decision as one line with its reason.
//!
//! A trace is JSONL: one engine `Event` per line (`{"t":ms,"k":"tool_start","s":"api","tool":"Bash",
//! "cmd":"cargo test"}`), `#` comment lines, which are printed as headers, and `{"k":"learn","key":
//! "turn","ms":[...]}` lines, durations the predictor already knew, and `{"k":"rhythm","key":"billing",
//! "ms":[...]}` lines, waits the rhythm already knew (the plugin keeps both in kv across launches).

use std::fmt::Write as _;

use multimaxxing_engine::model::plural;
use multimaxxing_engine::{Config, Decision, Engine, Event, Row};

#[derive(Debug, Clone)]
pub struct Opts {
    /// Ticks between events, as the plugin's timer would send them; 0 for none.
    pub tick_ms: u64,
    /// Print each event, not only decisions.
    pub events: bool,
    /// Print row decorations.
    pub rows: bool,
    /// Print the ranking after each event.
    pub rank: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Self { tick_ms: 15_000, events: true, rows: true, rank: false }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Tally {
    pub pings: u32,
    pub interrupts: u32,
    pub held: u32,
    pub batches: u32,
    pub dropped: u32,
}

fn clock(ms: u64) -> String {
    let s = ms / 1000;
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

fn row_text(r: &Option<Row>) -> String {
    let Some(r) = r else { return "cleared".into() };
    let mut parts = Vec::new();
    if let Some(l) = &r.label {
        parts.push(if r.accent { format!("[{l}]") } else { l.clone() });
    }
    if let Some(s) = &r.suffix {
        parts.push(format!("\"{s}\""));
    }
    if let Some(b) = r.bar {
        parts.push(format!("bar {}%", b / 10));
    }
    parts.join(" ")
}

fn render(d: &Decision, t: &mut Tally, opts: &Opts) -> Option<String> {
    Some(match d {
        Decision::Notify { surface, reason, .. } => {
            t.pings += 1;
            t.interrupts += 1;
            format!("NOTIFY   {surface:<8} {reason}")
        }
        Decision::Hold { surface, reason } => {
            t.pings += 1;
            t.held += 1;
            format!("hold     {surface:<8} {reason}")
        }
        Decision::Release { pings, reason, .. } => {
            t.batches += 1;
            let s: Vec<&str> = pings.iter().map(|p| p.surface.as_str()).collect();
            format!("RELEASE  {:<8} {reason}", s.join(","))
        }
        Decision::Drop { surface, reason } => {
            t.dropped += 1;
            format!("drop     {surface:<8} {reason}")
        }
        Decision::Row { surface, row } => {
            if !opts.rows {
                return None;
            }
            format!("row      {surface:<8} {}", row_text(row))
        }
        Decision::Title(t) => {
            if !opts.rows {
                return None;
            }
            format!("title             {}", t.as_deref().map_or("cleared".into(), |t| format!("\"{t}\"")))
        }
        Decision::Fill { gap_ms, items } => {
            let list = items
                .iter()
                .map(|s| format!("{} ({}{})", s.title, s.reason, if s.fits { "" } else { ", too long" }))
                .collect::<Vec<_>>()
                .join("; ");
            format!("fill     ~{:<7} {list}", multimaxxing_engine::model::dur(*gap_ms))
        }
    })
}

fn describe(e: &Event) -> String {
    let mut s = format!("{:<8} {}", e.surface.as_deref().unwrap_or("-"), e.kind);
    for v in [&e.tool, &e.cmd, &e.path, &e.task, &e.text].into_iter().flatten() {
        s += &format!(" {v}");
    }
    s
}

/// The replay of `trace`, one line per event and decision, then a tally.
pub fn run(trace: &str, cfg: Config, opts: &Opts) -> Result<String, String> {
    let mut eng = Engine::new(cfg);
    let mut out = String::new();
    let mut tally = Tally::default();
    let mut start: Option<u64> = None;
    let mut last: Option<u64> = None;
    for (i, line) in trace.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(c) = line.strip_prefix('#') {
            writeln!(out, "#{c}").ok();
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(line).map_err(|e| format!("line {}: {e}", i + 1))?;
        if v["k"] == "learn" {
            let key = v["key"].as_str().ok_or(format!("line {}: learn needs a key", i + 1))?;
            let mut p = eng.predictor().clone();
            for ms in v["ms"].as_array().into_iter().flatten().filter_map(|m| m.as_u64()) {
                p.add(key, ms);
            }
            eng.set_predictor(p);
            continue;
        }
        if v["k"] == "rhythm" {
            let key = v["key"].as_str().ok_or(format!("line {}: rhythm needs a key", i + 1))?;
            let mut r = eng.rhythm().clone();
            for ms in v["ms"].as_array().into_iter().flatten().filter_map(|m| m.as_u64()) {
                r.add(key, ms);
            }
            eng.set_rhythm(r);
            continue;
        }
        let e: Event = serde_json::from_value(v).map_err(|e| format!("line {}: {e}", i + 1))?;
        let t0 = *start.get_or_insert(e.t);
        if last.is_some_and(|l| e.t < l) {
            return Err(format!("line {}: time goes backwards", i + 1));
        }
        if opts.tick_ms > 0
            && let Some(last) = last
        {
            let mut t = last - (last - t0) % opts.tick_ms + opts.tick_ms;
            while t < e.t {
                for d in eng.tick(t) {
                    if let Some(l) = render(&d, &mut tally, opts) {
                        writeln!(out, "{}            {l}", clock(t - t0)).ok();
                    }
                }
                t += opts.tick_ms;
            }
        }
        last = Some(e.t);
        if opts.events {
            writeln!(out, "{}  > {}", clock(e.t - t0), describe(&e)).ok();
        }
        // Coming back to a tab: what the resume brief would say.
        if let Some(s) = e.surface.as_deref()
            && e.kind == "focus"
            && eng.focused() != Some(s)
            && let Some(b) = eng.brief(s, e.t)
        {
            writeln!(out, "{}            resume   {s:<8} {b}", clock(e.t - t0)).ok();
        }
        for d in eng.ingest(&e, e.t) {
            if let Some(l) = render(&d, &mut tally, opts) {
                writeln!(out, "{}            {l}", clock(e.t - t0)).ok();
            }
        }
        if opts.rank {
            let r: Vec<String> =
                eng.rank(e.t).iter().map(|r| format!("{}={}{}", r.surface, r.score, if r.focused { "*" } else { "" })).collect();
            writeln!(out, "                       rank {}", r.join(" ")).ok();
        }
    }
    writeln!(
        out,
        "-- {}: {} interrupted at once, {} held; {} at a break or the cap; {} dropped before they reached you",
        plural(tally.pings as usize, "ping"),
        tally.interrupts,
        tally.held,
        plural(tally.batches as usize, "release"),
        tally.dropped
    )
    .ok();
    Ok(out)
}

fn plain(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// A recorded wire file (`record`) as a trace, keeping only what the engine reads: the event kind,
/// the tab (by the id the recording gave it), the tool's name, a command's key (`cargo test`), files
/// as `file1`, `file2` per tab, task ids and statuses. Prompts, paths, cwds, session ids, arguments
/// and messages never leave.
pub fn import(wire: &str) -> Result<String, String> {
    use multimaxxing_engine::model::command_key;
    use std::collections::BTreeMap;
    let mut files: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut tasks: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut out = String::new();
    for (i, line) in wire.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
        let r: serde_json::Value = serde_json::from_str(line).map_err(|e| format!("line {}: {e}", i + 1))?;
        let w = &r["line"];
        let (Some(at), Some(kind), Some(s)) = (r["at"].as_u64(), w["event"].as_str(), w["surface_id"].as_str()) else { continue };
        if !plain(kind, 32) || !plain(s, 32) {
            continue;
        }
        let mut e = Event::new(at, kind, s);
        e.tool = w["tool_name"].as_str().map(|t| {
            if t.starts_with("mcp__") {
                "mcp".into()
            } else if plain(t, 32) {
                t.into()
            } else {
                "tool".into()
            }
        });
        e.cmd = w["tool_input"]["command"].as_str().and_then(command_key);
        if let Some(p) = w["tool_input"]["file_path"].as_str() {
            let n = files.len();
            let k = *files.entry((s.into(), p.into())).or_insert(n + 1);
            e.path = Some(format!("file{k}"));
        }
        if let Some(id) = w["task_id"].as_str() {
            let n = tasks.len();
            let k = *tasks.entry((s.into(), id.into())).or_insert(n + 1);
            e.task = Some(k.to_string());
        }
        e.text = w["task_status"].as_str().filter(|st| matches!(*st, "pending" | "in_progress" | "completed" | "deleted")).map(Into::into);
        writeln!(out, "{}", serde_json::to_string(&e).map_err(|e| e.to_string())?).ok();
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_keeps_only_what_the_engine_reads() {
        let wire = r#"{"at":1000,"line":{"v":1,"event":"prompt_submit","surface_id":"web","query":"my secret plan","cwd":"/Users/me/x","session_id":"abc"}}
{"at":2000,"line":{"event":"tool_start","surface_id":"web","tool_name":"Bash","tool_input":{"command":"TOKEN=sk-123 curl -H x https://h"}}}
{"at":3000,"line":{"event":"file_edited","surface_id":"web","tool_name":"Edit","tool_input":{"file_path":"/Users/me/x/.env"}}}
{"at":4000,"line":{"event":"task_updated","surface_id":"web","task_id":"t-9f","task_status":"completed"}}
{"at":5000,"line":{"event":"stop","surface_id":"/Users/me"}}"#;
        let out = import(wire).unwrap();
        assert_eq!(
            out,
            r#"{"t":1000,"k":"prompt_submit","s":"web"}
{"t":2000,"k":"tool_start","s":"web","tool":"Bash","cmd":"curl"}
{"t":3000,"k":"file_edited","s":"web","tool":"Edit","path":"file1"}
{"t":4000,"k":"task_updated","s":"web","task":"1","text":"completed"}
"#
        );
    }
}
