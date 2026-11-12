//! MULTIMAXXING in Haunt: a thin wrapper that feeds agent events, the selected tab and the timer
//! into `multimaxxing-engine`, and turns its decisions into notifications, row labels and the ⌘K
//! `next`, `why`, `resume` and `note` modes.
// wit-bindgen's flattened `notify` import has 14 params.
#![allow(clippy::too_many_arguments)]

wit_bindgen::generate!({ path: "wit/haunt-plugin.wit", world: "plugin" });

use std::collections::VecDeque;
use std::sync::Mutex;

use haunt::plugin::log::log;
use haunt::plugin::rows::{self, Decoration, Tone};
use haunt::plugin::types::LogLevel;
use haunt::plugin::{hold, kv, notify, palette, sidebar, timer, title};
use multimaxxing_engine::model::dur;
use multimaxxing_engine::{Config, Decision, Engine, Event, Ping, Predictor, Released, Rhythm, Row};
use serde::Deserialize;

const TICK_SECS: u32 = 15;
const PREDICTOR: &str = "predictor";
const RHYTHM: &str = "rhythm";
const WHY_KEEP: usize = 50;
const NEXT_AGENT: &str = "next_agent";

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub notify: bool,
    pub hold_haunt: bool,
    pub labels: bool,
    pub reasons: bool,
    pub bars: bool,
    pub filler: bool,
    pub resume: bool,
    pub title: bool,
    pub threshold: i64,
    pub max_hold_min: i64,
    pub min_gap_min: i64,
    pub unverified: bool,
    pub learn: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            notify: true,
            hold_haunt: true,
            labels: true,
            reasons: true,
            bars: false,
            filler: true,
            resume: true,
            title: true,
            threshold: 60,
            max_hold_min: 10,
            min_gap_min: 3,
            unverified: false,
            learn: true,
        }
    }
}

impl Settings {
    pub fn config(&self) -> Config {
        Config {
            router: self.notify,
            route_blocks: !self.hold_haunt,
            labels: self.labels,
            suffixes: self.reasons,
            bars: self.bars,
            filler: self.filler,
            stash: self.resume,
            title: self.title,
            threshold: self.threshold.clamp(0, 10_000),
            max_hold_ms: self.max_hold_min.clamp(1, 120) as u64 * 60_000,
            filler_min_gap_ms: self.min_gap_min.clamp(1, 120) as u64 * 60_000,
            learn: self.learn,
            ..Config::default()
        }
    }
}

#[derive(Default)]
struct Live {
    settings: Settings,
    eng: Engine,
    why: VecDeque<(u64, String)>,
    /// What the ⌘K `note` mode would save: the tab and the text typed so far.
    draft: Option<(String, String)>,
    /// We own Haunt's notification hold.
    holding: bool,
    /// Haunt refused our hold for the pings held now; tried again for the next ones.
    hold_refused: bool,
    hold_logged: bool,
    /// What the title bar shows, as far as Haunt took it.
    shown_title: Option<String>,
    /// The rhythm as last written to kv.
    saved_rhythm: Rhythm,
}

static LIVE: Mutex<Option<Live>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut Live) -> R) -> R {
    f(LIVE.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Live::default))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// The agent event kinds the engine reads. `focus`, `close`, `name` and `tick` are the plugin's own
/// signals, from the sidebar and the timer; an agent event can't carry them.
pub const AGENT_KINDS: &[&str] = &[
    "prompt_submit",
    "tool_start",
    "tool_done",
    "tool_failed",
    "tool_ran",
    "permission_seen",
    "question_asked",
    "question_answered",
    "task_created",
    "task_updated",
    "subagent_start",
    "subagent_stop",
    "file_edited",
    "stop",
    "stop_failure",
];

/// The engine's view of a Haunt event, or `None` for a kind it doesn't read. Only a task's status
/// rides along as text.
pub fn to_event(e: &AgentEvent, now: u64) -> Option<Event> {
    if !AGENT_KINDS.contains(&e.kind.as_str()) {
        return None;
    }
    Some(Event {
        t: now,
        kind: e.kind.clone(),
        surface: Some(e.surface.clone()?),
        tool: e.tool.clone(),
        cmd: e.command.clone(),
        path: e.file_path.clone(),
        task: e.task_id.clone(),
        text: if e.kind == "task_updated" { e.summary.clone() } else { None },
    })
}

/// Orange for the one tab to go to next; everything else dim.
pub fn decoration(r: &Row) -> Decoration {
    Decoration {
        label: r.label.clone(),
        label_tone: if r.accent { Tone::Warn } else { Tone::Dim },
        suffix: r.suffix.clone(),
        suffix_tone: Tone::Dim,
        scroll: false,
        bar: r.bar.map(|b| f32::from(b) / 1000.0),
        spark: None,
    }
}

fn note_why(l: &mut Live, now: u64, line: String) {
    if l.why.len() == WHY_KEEP {
        l.why.pop_back();
    }
    l.why.push_front((now, line));
}

/// Which of these pings this plugin posts itself. With `hold_haunt` Haunt keeps notifying: it shows
/// approvals and questions at once (it never holds those), and while we hold, it holds its own
/// "turn finished" notes and sums them up when we release. So we post only a finished or failed
/// turn that leaves the hold early (a cap or an escalation while others still wait).
pub fn ours(hold_haunt: bool, hold_continues: bool, ping: &Ping) -> bool {
    !hold_haunt || (hold_continues && matches!(ping, Ping::Finished | Ping::Failed))
}

fn post(title: &str, body: &str, binding: Option<&str>) {
    if let Err(e) = notify::notify(title, body, &[], binding) {
        log(LogLevel::Warn, &format!("multimaxxing: notify: {e}"));
    }
}

fn act(l: &mut Live, now: u64, ds: Vec<Decision>) {
    save_rhythm(l);
    // A refused hold falls back to sending our own pings, as with hold_haunt off.
    let hold_haunt = l.settings.hold_haunt && !l.hold_refused;
    let continues = l.holding && l.eng.held() > 0;
    for d in ds {
        match d {
            Decision::Notify { surface, title, body, reason, ping } => {
                note_why(l, now, reason);
                if ours(hold_haunt, continues, &ping) {
                    post(&title, &body, Some(&surface));
                }
            }
            Decision::Release { pings, reason, .. } => {
                note_why(l, now, reason);
                let mine: Vec<&Released> = pings.iter().filter(|p| ours(hold_haunt, continues, &p.ping)).collect();
                if let Some(first) = mine.first() {
                    let title = if mine.len() == 1 { first.name.clone() } else { format!("{} held pings", mine.len()) };
                    let body = mine.iter().map(|p| format!("{}: {}", p.name, p.ping.what())).collect::<Vec<_>>().join(" · ");
                    post(&title, &body, Some(&first.surface));
                }
            }
            Decision::Hold { reason, .. } | Decision::Drop { reason, .. } => note_why(l, now, reason),
            Decision::Row { surface, row } => {
                if let Err(e) = rows::decorate(&surface, row.as_ref().map(decoration).as_ref()) {
                    log(LogLevel::Debug, &format!("multimaxxing: {surface}: {e}"));
                }
            }
            Decision::Fill { .. } => {}
            // The title follows `headline`, retried until Haunt takes it (it rate-limits sets).
            Decision::Title(_) => {}
        }
    }
    hold_haunt_notes(l);
    show_title(l, now);
}

fn show_title(l: &mut Live, now: u64) {
    let want = l.eng.headline(now);
    if want == l.shown_title {
        return;
    }
    match title::set(want.as_deref()) {
        Ok(()) => l.shown_title = want,
        // Another plugin owns the slot, or too many sets this second: try again next time.
        Err(e) if passes(&e) => log(LogLevel::Debug, &format!("multimaxxing: title: {e}")),
        // Anything else may be the text, such as a tab named "done" or "requests": the line without names.
        Err(_) => match title::set(nameless(want.as_deref(), l.eng.held()).as_deref()) {
            Ok(()) => l.shown_title = want,
            Err(e) => log(LogLevel::Debug, &format!("multimaxxing: title: {e}")),
        },
    }
}

/// A title.set error that clears on its own, whatever the line says.
fn passes(e: &str) -> bool {
    ["times a second", "slot is taken", "too many pending"].iter().any(|p| e.contains(p))
}

/// `headline` with no tab names or item titles in it.
fn nameless(line: Option<&str>, held: usize) -> Option<String> {
    match (line?.starts_with("next: "), held) {
        (true, 0) => Some("next agent".into()),
        (true, n) => Some(format!("next agent · {n} held")),
        (false, 0) => None,
        (false, n) => Some(format!("{n} held")),
    }
}

/// While we hold a ping, Haunt holds its own "turn finished" notes too, so a held ping is quiet
/// everywhere; releasing shows Haunt's summary.
fn hold_haunt_notes(l: &mut Live) {
    let want = l.settings.hold_haunt && l.settings.notify && l.eng.held() > 0;
    if want && !l.holding && !l.hold_refused {
        let mins = (l.eng.cfg.max_hold_ms / 60_000).clamp(1, 240) as u32;
        // Refused (another plugin holds, on a host that allows one holder): don't fight for it for these
        // pings; Haunt's notes follow the other hold.
        match hold::start(mins) {
            Ok(()) => l.holding = true,
            Err(e) => {
                l.hold_refused = true;
                if !l.hold_logged {
                    l.hold_logged = true;
                    log(LogLevel::Info, &format!("multimaxxing: Haunt's notes not held: {e}"));
                }
            }
        }
    } else if !want {
        l.hold_refused = false;
        if l.holding {
            // Drops only our hold; Haunt shows its summary when the last holder lets go.
            hold::release();
            l.holding = false;
        }
    }
}

/// Brings the engine up to date with the sidebar: names, closed tabs and the selected one.
fn sync(l: &mut Live, now: u64) {
    let rows = sidebar::rows();
    let mut ds = Vec::new();
    for r in &rows {
        let known = l.eng.tab(&r.surface).map(|t| t.name != r.name);
        if known.unwrap_or(r.kind != "shell") {
            ds.extend(l.eng.ingest(
                &Event { kind: "name".into(), surface: Some(r.surface.clone()), text: Some(r.name.clone()), ..Event::default() },
                now,
            ));
        }
    }
    let gone: Vec<String> = l.eng.surfaces().filter(|s| !rows.iter().any(|r| r.surface == *s)).map(String::from).collect();
    for s in gone {
        ds.extend(l.eng.ingest(&Event::new(now, "close", &s), now));
    }
    if let Some(sel) = rows.iter().find(|r| r.selected)
        && l.eng.focused() != Some(sel.surface.as_str())
    {
        ds.extend(l.eng.ingest(&Event::new(now, "focus", &sel.surface), now));
    }
    act(l, now, ds);
}

fn save_predictor(p: &Predictor) {
    if let Err(e) = kv::set(PREDICTOR, &p.to_json()) {
        log(LogLevel::Warn, &format!("multimaxxing: durations not saved: {e}"));
    }
}

fn save_rhythm(l: &mut Live) {
    if *l.eng.rhythm() == l.saved_rhythm {
        return;
    }
    l.saved_rhythm = l.eng.rhythm().clone();
    if let Err(e) = kv::set(RHYTHM, &l.saved_rhythm.to_json()) {
        log(LogLevel::Warn, &format!("multimaxxing: rhythm not saved: {e}"));
    }
}

fn apply_settings(l: &mut Live, json: &str) {
    l.settings = serde_json::from_str(json).unwrap_or_default();
    l.eng.cfg = l.settings.config();
}

fn matches(q: &str, text: &str) -> bool {
    q.is_empty() || text.to_lowercase().contains(&q.to_lowercase())
}

fn item(id: String, title: String, subtitle: Option<String>, surface: Option<String>, at: Option<u64>) -> PaletteItem {
    PaletteItem { id, title, subtitle, at_ms: at, mark: None, surface }
}

/// Registered again after every settings change, which drops a plugin's commands.
fn register_commands() {
    palette::command(NEXT_AGENT, "Go to next agent", Some("cmd+shift+n"));
}

/// Focuses the tab the engine ranks first, the one the title names; returns it, or none when no tab waits.
pub fn go_next(eng: &Engine, now: u64, focus: impl FnOnce(&str) -> Result<(), String>) -> Result<Option<String>, String> {
    let Some(pick) = eng.top_pick(now) else { return Ok(None) };
    focus(&pick.surface).map_err(|e| format!("could not go to {}: {e}", pick.name))?;
    Ok(Some(pick.surface))
}

pub fn next_items(eng: &Engine, now: u64, query: &str) -> Vec<PaletteItem> {
    let mut out: Vec<PaletteItem> = eng
        .next(now)
        .into_iter()
        .map(|r| {
            item(
                format!("go:{}", r.surface),
                r.name.clone(),
                Some(r.reason.clone() + if r.held { " · held" } else { "" }),
                Some(r.surface),
                None,
            )
        })
        .collect();
    if let Some((gap, fill)) = eng.filler(now) {
        for s in fill {
            let fits = if s.fits { "" } else { " (longer than the wait)" };
            out.push(item(
                format!("fill:{}", s.surface),
                format!("You have ~{}: {}{fits}", dur(gap), s.title),
                Some(s.reason),
                Some(s.surface),
                None,
            ));
        }
    }
    if out.is_empty() {
        out.push(item("none".into(), "Nothing to go to".into(), Some("no tab is waiting on you or has unread output".into()), None, None));
    }
    out.retain(|i| matches(query, &i.title) || matches(query, i.subtitle.as_deref().unwrap_or("")));
    out
}

struct Plugin;

impl Guest for Plugin {
    fn init(json: String) {
        with(|l| {
            apply_settings(l, &json);
            if let Some(p) = kv::get(PREDICTOR).and_then(|j| Predictor::from_json(&j)) {
                l.eng.set_predictor(p);
            }
            if let Some(r) = kv::get(RHYTHM).and_then(|j| Rhythm::from_json(&j)) {
                l.saved_rhythm = r.clone();
                l.eng.set_rhythm(r);
            }
        });
        palette::mode("next", "Next: where your attention pays most");
        palette::mode("why", "Why: each ping MULTIMAXXING held or sent");
        palette::mode("resume", "Resume: what each tab did since you looked");
        palette::mode("note", "Note: leave yourself a note on this tab");
        register_commands();
        if let Err(e) = timer::every(TICK_SECS) {
            log(LogLevel::Warn, &format!("multimaxxing: no timer, so held pings only go out on events: {e}"));
        }
    }

    fn on_settings_changed(json: String) {
        register_commands();
        with(|l| {
            apply_settings(l, &json);
            let now = now_ms();
            let ds = l.eng.tick(now);
            act(l, now, ds);
        });
    }

    fn on_event(event: AgentEvent) {
        with(|l| {
            if !event.verified && !l.settings.unverified {
                return;
            }
            let now = now_ms();
            let Some(e) = to_event(&event, now) else { return };
            sync(l, now);
            let ds = l.eng.ingest(&e, now);
            act(l, now, ds);
            if matches!(e.kind.as_str(), "stop" | "stop_failure" | "tool_done" | "tool_failed" | "tool_ran") {
                save_predictor(l.eng.predictor());
            }
        });
    }

    fn palette_items(mode: String, query: String) -> Vec<PaletteItem> {
        with(|l| {
            let now = now_ms();
            sync(l, now);
            match mode.as_str() {
                "next" => next_items(&l.eng, now, &query),
                "why" => l
                    .why
                    .iter()
                    .filter(|(_, w)| matches(&query, w))
                    .enumerate()
                    .map(|(i, (at, w))| {
                        let (head, rest) = w.split_once(": ").unwrap_or((w.as_str(), ""));
                        item(
                            format!("why:{i}"),
                            format!("{head}: {}", rest.split("; ").next().unwrap_or(rest)),
                            Some(w.clone()),
                            None,
                            Some(*at),
                        )
                    })
                    .collect(),
                "resume" => {
                    let names: Vec<(String, String)> = l
                        .eng
                        .surfaces()
                        .filter(|s| l.eng.focused() != Some(*s))
                        .map(|s| (s.to_string(), l.eng.tab(s).map(|t| t.name.clone()).unwrap_or_default()))
                        .collect();
                    names
                        .into_iter()
                        .filter_map(|(s, name)| l.eng.brief(&s, now).map(|b| item(format!("resume:{s}"), name, Some(b), Some(s), None)))
                        .filter(|i| matches(&query, &i.title) || matches(&query, i.subtitle.as_deref().unwrap_or("")))
                        .collect()
                }
                "note" => {
                    let Some(s) = l.eng.focused().map(String::from) else {
                        return vec![item("none".into(), "Open a tab first".into(), None, None, None)];
                    };
                    let name = l.eng.tab(&s).map(|t| t.name.clone()).unwrap_or_default();
                    let mut v = Vec::new();
                    let text = query.trim().to_string();
                    if text.is_empty() {
                        v.push(item(
                            "none".into(),
                            format!("Type a note for {name}"),
                            Some("it shows in ⌘K resume and next".into()),
                            None,
                            None,
                        ));
                    } else {
                        v.push(item("note".into(), format!("Save note on {name}: {text}"), None, None, None));
                    }
                    if let Some(n) = l.eng.note(&s) {
                        v.push(item("note-clear".into(), format!("Clear note: {n}"), None, None, None));
                    }
                    l.draft = Some((s, text));
                    v
                }
                _ => Vec::new(),
            }
        })
    }

    fn on_action(action_id: String, payload: String) {
        with(|l| match action_id.as_str() {
            NEXT_AGENT if matches!(payload.as_str(), "" | "key-down") => {
                let now = now_ms();
                sync(l, now);
                if let Err(e) = go_next(&l.eng, now, sidebar::focus) {
                    log(LogLevel::Warn, &format!("multimaxxing: {e}"));
                }
            }
            "note" => {
                if let Some((s, text)) = l.draft.take() {
                    l.eng.set_note(&s, &text);
                }
            }
            "note-clear" => {
                if let Some((s, _)) = l.draft.take() {
                    l.eng.set_note(&s, "");
                }
            }
            _ => {}
        });
    }

    fn row_menu(_surface: String) -> Vec<MenuItem> {
        Vec::new()
    }

    fn on_row_moved(_surface: String, _group: String, _before: Option<String>) {}

    fn on_theme_changed() {}

    fn on_tick(now_ms: u64) {
        with(|l| {
            sync(l, now_ms);
            let ds = l.eng.tick(now_ms);
            act(l, now_ms, ds);
        });
    }

    fn on_exec(_job: u64, _outcome: Result<haunt::plugin::exec::Output, String>) {}

    fn on_away_changed(_away: bool, _away_ms: u64) {}

    fn on_hover_hold(_held: bool, _surface: String) {}

    fn on_fs_tree(_job: u64, _outcome: Result<haunt::plugin::fs_list::TreeResult, String>) {}
}

export!(Plugin);

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: &str, summary: Option<&str>) -> AgentEvent {
        AgentEvent {
            kind: kind.into(),
            session_id: None,
            agent_id: None,
            cwd: None,
            summary: summary.map(Into::into),
            verified: true,
            surface: Some("s1".into()),
            tool: Some("Bash".into()),
            command: Some("cargo test".into()),
            file_path: None,
            task_id: Some("7".into()),
            request_key: None,
        }
    }

    #[test]
    fn events_map_without_prompts() {
        let e = to_event(&ev("prompt_submit", Some("my secret prompt")), 5).unwrap();
        assert_eq!((e.kind.as_str(), e.surface.as_deref(), e.text), ("prompt_submit", Some("s1"), None));
        let e = to_event(&ev("task_updated", Some("completed")), 5).unwrap();
        assert_eq!((e.task.as_deref(), e.text.as_deref()), (Some("7"), Some("completed")));
        let mut no_tab = ev("stop", None);
        no_tab.surface = None;
        assert!(to_event(&no_tab, 5).is_none());
    }

    #[test]
    fn an_agent_event_cannot_pose_as_focus_close_name_or_tick() {
        let mut eng = Engine::new(Config::default());
        eng.ingest(&Event::new(0, "focus", "s2"), 0);
        eng.ingest(&Event::new(0, "prompt_submit", "s2"), 0);
        eng.ingest(&Event::new(0, "prompt_submit", "s1"), 0);
        eng.ingest(&Event::new(1_000, "stop", "s1"), 1_000);
        let before = (eng.focused().map(String::from), eng.surfaces().map(String::from).collect::<Vec<_>>(), eng.rank(2_000), eng.held());
        for kind in ["focus", "close", "name", "tick", "session_start", "made_up"] {
            let mut e = ev(kind, Some("evil"));
            e.summary = Some("evil".into());
            let mapped = to_event(&e, 2_000);
            assert_eq!(mapped, None, "{kind} must not reach the engine");
            // The same path on_event takes: nothing to ingest, so nothing changes.
            if let Some(m) = mapped {
                eng.ingest(&m, 2_000);
            }
            let after =
                (eng.focused().map(String::from), eng.surfaces().map(String::from).collect::<Vec<_>>(), eng.rank(2_000), eng.held());
            assert_eq!(after, before, "{kind}");
        }
        assert_eq!(eng.tab("s1").map(|t| t.name.as_str()), Some("s1"), "a name event can't rename a tab");
    }

    #[test]
    fn only_next_is_orange() {
        let row = Row { label: Some("next".into()), accent: true, suffix: Some("finished 2m ago".into()), bar: Some(500) };
        let d = decoration(&row);
        assert!(matches!((d.label_tone, d.suffix_tone), (Tone::Warn, Tone::Dim)));
        assert_eq!(d.bar, Some(0.5));
        assert!(matches!(decoration(&Row { accent: false, ..row }).label_tone, Tone::Dim));
    }

    #[test]
    fn with_haunt_holding_we_post_only_turns_that_leave_early() {
        assert!(ours(false, false, &Ping::Question), "on our own we send everything");
        assert!(!ours(true, true, &Ping::Approval("Bash".into())), "Haunt already showed it");
        assert!(ours(true, true, &Ping::Finished));
        assert!(!ours(true, false, &Ping::Finished), "Haunt's summary covers it");
    }

    #[test]
    fn settings_clamp() {
        let s: Settings = serde_json::from_str(r#"{"max_hold_min": 0, "notify": false, "min_gap_min": 500}"#).unwrap();
        let c = s.config();
        assert_eq!((c.max_hold_ms, c.router, c.filler_min_gap_ms, c.labels), (60_000, false, 120 * 60_000, true));
        assert!(c.learn, "learning is on by default");
        assert!(!serde_json::from_str::<Settings>(r#"{"learn": false}"#).unwrap().config().learn);
    }

    #[test]
    fn go_to_next_agent_focuses_the_engines_top_pick() {
        let mut eng = Engine::new(Config::default());
        for (t, k, s) in [(0, "focus", "a"), (0, "prompt_submit", "a"), (0, "focus", "b"), (0, "prompt_submit", "b")] {
            eng.ingest(&Event::new(t, k, s), t);
        }
        eng.ingest(&Event::new(500, "focus", "c"), 500);
        eng.ingest(&Event::new(1_000, "stop", "a"), 1_000);
        eng.ingest(&Event::new(2_000, "permission_seen", "b"), 2_000);
        let now = 30_000;
        let top = eng.top_pick(now).unwrap().surface;
        let mut focused = Vec::new();
        assert_eq!(
            go_next(&eng, now, |s| {
                focused.push(s.to_owned());
                Ok(())
            }),
            Ok(Some(top.clone()))
        );
        assert_eq!(focused, std::slice::from_ref(&top), "exactly the top pick, once");
        assert_eq!(top, "b", "the approval outranks the finished turn");
        assert!(go_next(&eng, now, |_| Err("no".into())).unwrap_err().contains("could not go to b"));
        assert_eq!(go_next(&Engine::new(Config::default()), now, |_| panic!("nothing to go to")), Ok(None));
    }

    #[test]
    fn next_lists_the_waiting_tab_with_its_reason() {
        let mut eng = Engine::new(Config::default());
        eng.ingest(&Event::new(0, "focus", "a"), 0);
        eng.ingest(&Event::new(0, "prompt_submit", "a"), 0);
        eng.ingest(&Event::new(1_000, "focus", "b"), 1_000);
        eng.ingest(&Event::new(2_000, "stop", "a"), 2_000);
        let items = next_items(&eng, 62_000, "");
        assert_eq!(items[0].surface.as_deref(), Some("a"));
        assert!(items[0].subtitle.as_deref().unwrap().starts_with("a finished 1m ago"), "{:?}", items[0].subtitle);
        assert!(next_items(&eng, 62_000, "zzz").is_empty());
    }

    #[test]
    fn a_refused_title_falls_back_to_one_without_names() {
        assert_eq!(nameless(Some("next: requests · 2 held"), 2).as_deref(), Some("next agent · 2 held"));
        assert_eq!(nameless(Some("next: done · idle 3m"), 0).as_deref(), Some("next agent"));
        assert_eq!(nameless(Some("~5m gap: Approve the PR"), 0), None);
        assert_eq!(nameless(Some("2 held · out at your break"), 2).as_deref(), Some("2 held"));
        assert_eq!(nameless(None, 3), None);
        assert!(passes("set at most 5 times a second") && passes("the title-bar slot is taken: dev.x") && passes("too many pending requests"));
        assert!(!passes("the title line can't use Haunt's status or permission words") && !passes("some new refusal"), "unknown errors fall back");
    }
}
