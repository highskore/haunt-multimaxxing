//! MULTIMAXXING: schedules your attention across many agents. It ranks tabs by how much they
//! deserve you now, decides whether a ping interrupts you or waits for a break, suggests what fits
//! while you wait, and keeps what each tab did while you looked elsewhere.
//!
//! Pure and deterministic: time comes in as `now` (Unix ms), nothing reads a clock or does I/O, and
//! every decision carries a one-line reason.

pub mod filler;
pub mod model;
pub mod predict;
pub mod rhythm;
pub mod score;
pub mod stash;

use std::collections::BTreeMap;

pub use filler::Suggestion;
pub use model::{Event, Phase, Ping, Tab};
pub use predict::Predictor;
pub use rhythm::Rhythm;
pub use score::Config;

use model::{Block, command_key, dur, plural};
use score::{Focus, Parts};

/// What a tab's row shows: a label ("next", "~3m", "held", "later"), a reason after the meta line
/// and a bar. `accent` is the one orange label, for the tab to go to next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub label: Option<String>,
    pub accent: bool,
    pub suffix: Option<String>,
    /// 0 to 1000.
    pub bar: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Worth breaking your focus for now.
    Notify { surface: String, title: String, body: String, reason: String, ping: Ping },
    /// Waits for a break.
    Hold { surface: String, reason: String },
    /// Held pings, sent as one, best first.
    Release { pings: Vec<Released>, title: String, body: String, reason: String },
    /// A held ping that no longer matters.
    Drop { surface: String, reason: String },
    /// Set (or with `None`, clear) the row's decoration.
    Row { surface: String, row: Option<Row> },
    /// While you wait: things that fit before the next agent needs you.
    Fill { gap_ms: u64, items: Vec<Suggestion> },
    /// Set (or with `None`, clear) the title-bar line.
    Title(Option<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Released {
    pub surface: String,
    pub name: String,
    pub ping: Ping,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ranked {
    pub surface: String,
    pub name: String,
    pub score: i64,
    pub parts: Parts,
    pub focused: bool,
    pub held: bool,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Held {
    since: u64,
    ping: Ping,
}

#[derive(Debug, Clone, Default)]
pub struct Engine {
    pub cfg: Config,
    tabs: BTreeMap<String, Tab>,
    pred: Predictor,
    focus: Option<String>,
    focus_since: u64,
    last_break: Option<u64>,
    held: BTreeMap<String, Held>,
    rows: BTreeMap<String, Row>,
    fill: Vec<String>,
    title: Option<String>,
    notes: BTreeMap<String, String>,
    rhythm: Rhythm,
}

impl Engine {
    pub fn new(cfg: Config) -> Self {
        Self { cfg, ..Self::default() }
    }

    pub fn predictor(&self) -> &Predictor {
        &self.pred
    }

    pub fn set_predictor(&mut self, p: Predictor) {
        self.pred = p;
    }

    /// How quickly you get to each tab name, learned while `cfg.learn` is on.
    pub fn rhythm(&self) -> &Rhythm {
        &self.rhythm
    }

    pub fn set_rhythm(&mut self, r: Rhythm) {
        self.rhythm = r;
    }

    /// You got to tab `t` `waited` ms after it began waiting on you.
    fn learn(rhythm: &mut Rhythm, cfg: &Config, t: &Tab, now: u64) {
        let waiting = matches!(t.phase, Phase::Blocked(_)) || matches!(t.phase, Phase::Done { .. }) && !t.acked;
        if cfg.learn && waiting {
            rhythm.add(&t.name, now.saturating_sub(t.phase_since));
        }
    }

    pub fn tab(&self, surface: &str) -> Option<&Tab> {
        self.tabs.get(surface)
    }

    pub fn surfaces(&self) -> impl Iterator<Item = &str> {
        self.tabs.keys().map(String::as_str)
    }

    /// Pings waiting for a break.
    pub fn held(&self) -> usize {
        self.held.len()
    }

    pub fn focused(&self) -> Option<&str> {
        self.focus.as_deref()
    }

    pub fn note(&self, surface: &str) -> Option<&str> {
        self.notes.get(surface).map(String::as_str)
    }

    /// An empty note removes it.
    pub fn set_note(&mut self, surface: &str, text: &str) {
        let t = text.trim();
        if t.is_empty() {
            self.notes.remove(surface);
        } else {
            self.notes.insert(surface.to_string(), t.chars().take(200).collect());
        }
    }

    /// One event at `now`; returns what to do about it.
    pub fn ingest(&mut self, e: &Event, now: u64) -> Vec<Decision> {
        if e.kind == "tick" {
            return self.tick(now);
        }
        let mut out = Vec::new();
        let (ping, brk) = match e.surface.as_deref() {
            Some(s) => self.apply(s, e, now, &mut out),
            None => (None, None),
        };
        if let Some(why) = brk {
            out.extend(self.sweep(now, true));
            out.extend(self.release_all(now, &why));
        }
        if let (Some(p), Some(s)) = (ping, e.surface.as_deref()) {
            out.extend(self.route(s, p, now));
        }
        out.extend(self.settle(now));
        out
    }

    /// Time passing with no event: held pings may escalate or hit the cap, labels age.
    pub fn tick(&mut self, now: u64) -> Vec<Decision> {
        self.settle(now)
    }

    fn settle(&mut self, now: u64) -> Vec<Decision> {
        let mut out = self.sweep(now, false);
        out.extend(self.rows_diff(now));
        out.extend(self.fill_diff(now));
        out.extend(self.title_diff(now));
        out
    }

    fn tab_mut(&mut self, s: &str, now: u64) -> &mut Tab {
        self.tabs.entry(s.to_string()).or_insert_with(|| Tab::new(s, now))
    }

    fn name(&self, s: &str) -> String {
        self.tabs.get(s).map_or_else(|| s.to_string(), |t| t.name.clone())
    }

    /// Updates the tab; returns the ping it raises and whether this is a break.
    fn apply(&mut self, s: &str, e: &Event, now: u64, out: &mut Vec<Decision>) -> (Option<Ping>, Option<String>) {
        let focused = self.focus.as_deref() == Some(s);
        match e.kind.as_str() {
            "close" => {
                let name = self.tabs.remove(s).map_or_else(|| s.to_string(), |t| t.name);
                if self.held.remove(s).is_some() {
                    out.push(Decision::Drop { surface: s.into(), reason: format!("dropped: {name} closed") });
                }
                self.rows.remove(s);
                self.notes.remove(s);
                if focused {
                    self.focus = None;
                }
                return (None, None);
            }
            "focus" => return (None, self.focus_on(s, now, out)),
            "name" => {
                if let Some(n) = e.text.as_deref().filter(|n| !n.trim().is_empty()) {
                    self.tab_mut(s, now).name = n.trim().to_string();
                }
                return (None, None);
            }
            _ => {}
        }
        let mut brk = None;
        let t = self.tabs.entry(s.to_string()).or_insert_with(|| Tab::new(s, now));
        let call_key = |e: &Event| {
            let tool = e.tool.clone().unwrap_or_else(|| "tool".into());
            match e.cmd.as_deref().and_then(command_key) {
                Some(c) => format!("{tool}:{c}"),
                None => tool,
            }
        };
        let mut ping = None;
        match e.kind.as_str() {
            "prompt_submit" => {
                t.set_phase(Phase::Running, now);
                t.turn_start = Some(now);
                t.failures_in_turn = 0;
                t.open.clear();
                if focused {
                    // Agents also prompt themselves (a background subagent finishing); only yours is a break.
                    brk = Some(format!("you sent a prompt on {}", t.name));
                }
            }
            "tool_start" => {
                t.open.push((call_key(e), now));
                t.set_phase(Phase::Running, now);
                t.turn_start.get_or_insert(now);
            }
            "tool_done" | "tool_failed" | "tool_ran" => {
                let tool = e.tool.clone().unwrap_or_default();
                let key = call_key(e);
                let at = t
                    .open
                    .iter()
                    .rposition(|(k, _)| *k == key)
                    .or_else(|| t.open.iter().rposition(|(k, _)| k.split(':').next() == Some(tool.as_str())));
                if let Some(i) = at {
                    let (k, start) = t.open.remove(i);
                    self.pred.add(&k, now.saturating_sub(start));
                }
                if e.kind == "tool_failed" {
                    t.failures_in_turn += 1;
                    *t.trail.tool_failures.entry(key).or_default() += 1;
                }
                if matches!(t.phase, Phase::Blocked(Block::Approval)) {
                    // Answered without opening the tab (Allow on the notification): that's getting to it too.
                    if !focused {
                        Self::learn(&mut self.rhythm, &self.cfg, t, now);
                    }
                    t.set_phase(Phase::Running, now);
                }
            }
            "permission_seen" => {
                let what = match e.cmd.as_deref().and_then(command_key) {
                    Some(c) => format!("{} ({c})", e.tool.as_deref().unwrap_or("tool")),
                    None => e.tool.clone().unwrap_or_else(|| "tool".into()),
                };
                t.waiting_on = Some(what.clone());
                t.trail.approvals += 1;
                if t.set_phase(Phase::Blocked(Block::Approval), now) {
                    ping = Some(Ping::Approval(what));
                }
            }
            "question_asked" => {
                t.trail.questions += 1;
                if t.set_phase(Phase::Blocked(Block::Question), now) {
                    ping = Some(Ping::Question);
                }
            }
            "question_answered" => {
                if !focused && matches!(t.phase, Phase::Blocked(Block::Question)) {
                    Self::learn(&mut self.rhythm, &self.cfg, t, now);
                }
                t.set_phase(Phase::Running, now);
            }
            "task_created" => {
                if let Some(id) = &e.task {
                    t.tasks.insert(id.clone());
                }
            }
            "task_updated" => {
                if let Some(id) = &e.task {
                    match e.text.as_deref() {
                        Some("completed") => {
                            if t.tasks.remove(id) {
                                t.trail.tasks_done += 1;
                            }
                        }
                        Some("deleted") => {
                            t.tasks.remove(id);
                        }
                        _ => {
                            t.tasks.insert(id.clone());
                        }
                    }
                }
            }
            "subagent_start" => t.subagents += 1,
            "subagent_stop" => t.subagents = t.subagents.saturating_sub(1),
            "file_edited" if !focused => {
                let key = e.path.clone().unwrap_or_else(|| format!("#{}", t.edits.len()));
                t.edits.insert(key);
            }
            "stop" | "stop_failure" => {
                let failed = e.kind == "stop_failure";
                if let Some(start) = t.turn_start.take() {
                    self.pred.add(predict::TURN, now.saturating_sub(start));
                    self.pred.add(&predict::turn_key(&t.name), now.saturating_sub(start));
                }
                t.open.clear();
                t.subagents = 0;
                t.waiting_on = None;
                if failed {
                    t.trail.turns_failed += 1;
                } else {
                    t.trail.turns_done += 1;
                }
                let changed = t.set_phase(Phase::Done { failed }, now);
                if focused {
                    t.acked = true;
                    brk = Some(format!("{} stopped", t.name));
                } else if changed {
                    ping = Some(if failed { Ping::Failed } else { Ping::Finished });
                }
            }
            _ => {}
        }
        if let Some(t) = self.tabs.get_mut(s)
            && !matches!(t.phase, Phase::Blocked(_))
        {
            t.waiting_on = None;
        }
        (ping, brk)
    }

    /// You're on tab `s` now. A switch is a break.
    fn focus_on(&mut self, s: &str, now: u64, out: &mut Vec<Decision>) -> Option<String> {
        if self.focus.as_deref() == Some(s) {
            return None;
        }
        if let Some(prev) = self.focus.take().and_then(|p| self.tabs.get_mut(&p)) {
            prev.trail = model::Trail { since: now, ..model::Trail::default() };
        }
        if let Some(t) = self.tabs.get(s) {
            Self::learn(&mut self.rhythm, &self.cfg, t, now);
        }
        self.focus = Some(s.to_string());
        self.focus_since = now;
        let t = self.tab_mut(s, now);
        t.edits.clear();
        t.trail = model::Trail { since: now, ..model::Trail::default() };
        if matches!(t.phase, Phase::Done { .. }) {
            t.acked = true;
        }
        let name = t.name.clone();
        if self.held.remove(s).is_some() {
            out.push(Decision::Drop { surface: s.into(), reason: format!("dropped: you opened {name}") });
        }
        Some(format!("you switched to {name}"))
    }

    fn focus_state(&self, now: u64) -> Focus {
        if self.last_break.is_some_and(|b| now.saturating_sub(b) < self.cfg.grace_ms) {
            return Focus::Break;
        }
        let Some(t) = self.focus.as_deref().and_then(|f| self.tabs.get(f)) else { return Focus::Nowhere };
        match t.phase {
            Phase::Running => Focus::Watching(t.name.clone()),
            _ => {
                let from = self.focus_since.max(self.last_break.unwrap_or(0));
                Focus::Reading { name: t.name.clone(), for_ms: now.saturating_sub(from) }
            }
        }
    }

    /// "api waits 2m on Bash approval, ~18m of work behind it"
    fn describe(&self, t: &Tab, p: &Parts, now: u64) -> String {
        let ago = dur(now.saturating_sub(t.phase_since));
        match t.phase {
            Phase::Blocked(b) => {
                let on = match b {
                    Block::Approval => format!("{} approval", t.waiting_on.as_deref().unwrap_or("tool")),
                    Block::Question => "a question".into(),
                };
                let mut s = format!("{} waits {ago} on {on}", t.name);
                let mut behind = Vec::new();
                if let Some(ms) = p.behind_ms {
                    behind.push(format!("~{} of work", dur(ms)));
                }
                if !t.tasks.is_empty() {
                    behind.push(format!("{} open tasks", t.tasks.len()));
                }
                if t.subagents > 0 {
                    behind.push(format!("{} subagents", t.subagents));
                }
                if !behind.is_empty() {
                    s += &format!(", {} behind it", behind.join(", "));
                }
                s
            }
            Phase::Done { failed } => {
                let mut s = format!("{} {} {ago} ago", t.name, if failed { "failed" } else { "finished" });
                if !t.edits.is_empty() {
                    s += &format!(", {} edited", plural(t.edits.len(), "file"));
                }
                s
            }
            Phase::Running => format!("{} is working", t.name),
            Phase::Fresh => format!("{} has no turn yet", t.name),
        }
    }

    /// Whether `s` is worth interrupting you for now, and why.
    fn judge(&self, s: &str, now: u64) -> (bool, String) {
        let Some(t) = self.tabs.get(s) else { return (false, String::new()) };
        let p = score::parts(t, now, &self.pred, &self.cfg);
        let f = self.focus_state(now);
        let (score, mut cost, thr) = (p.total(), f.cost(&self.cfg), self.cfg.threshold);
        let what = self.describe(t, &p, now);
        let mut fd = f.describe();
        if let Some((pm, usual)) = self.pace(&t.name)
            && cost > 0
        {
            cost = cost * pm as i64 / 1000;
            fd += &format!("; you usually get to {} within {} (cost ×{})", t.name, dur(usual), rhythm::times(pm));
        }
        if score - cost > thr {
            (true, format!("{what}; {fd} (score {score} > cost {cost} + {thr})"))
        } else {
            (false, format!("{what}; {fd} (score {score} <= cost {cost} + {thr})"))
        }
    }

    fn route(&mut self, s: &str, ping: Ping, now: u64) -> Vec<Decision> {
        let block = matches!(ping, Ping::Approval(_) | Ping::Question);
        if !self.cfg.router || (block && !self.cfg.route_blocks) || self.focus.as_deref() == Some(s) {
            return Vec::new();
        }
        let (now_worth, reason) = self.judge(s, now);
        if now_worth {
            self.held.remove(s);
            let name = self.name(s);
            return vec![Decision::Notify {
                surface: s.into(),
                title: name,
                body: ping.what(),
                reason: format!("interrupt: {reason}"),
                ping,
            }];
        }
        let since = self.held.get(s).map_or(now, |h| h.since);
        self.held.insert(s.to_string(), Held { since, ping });
        vec![Decision::Hold { surface: s.into(), reason: format!("held: {reason}") }]
    }

    /// Held pings that resolved, grew urgent, or hit the cap. At a break only the resolved go: the
    /// rest leave together in the batch.
    fn sweep(&mut self, now: u64, at_break: bool) -> Vec<Decision> {
        let mut out = Vec::new();
        let ids: Vec<String> = self.held.keys().cloned().collect();
        for s in ids {
            let h = self.held[&s].clone();
            let Some(t) = self.tabs.get(&s) else {
                self.held.remove(&s);
                continue;
            };
            if !h.ping.live(t.phase) {
                let why = match h.ping {
                    Ping::Approval(_) | Ping::Question => "answered while held",
                    Ping::Finished | Ping::Failed => "moved on while held",
                };
                out.push(Decision::Drop { surface: s.clone(), reason: format!("dropped: {} {why}", t.name) });
                self.held.remove(&s);
            } else if at_break {
            } else if now.saturating_sub(h.since) >= self.cfg.max_hold_ms {
                let name = t.name.clone();
                out.push(Decision::Release {
                    pings: vec![Released { surface: s.clone(), name: name.clone(), ping: h.ping.clone() }],
                    title: name.clone(),
                    body: h.ping.what(),
                    reason: format!("released: {name} held {}, the cap", dur(now - h.since)),
                });
                self.held.remove(&s);
            } else {
                let (now_worth, reason) = self.judge(&s, now);
                if now_worth {
                    let name = t.name.clone();
                    out.push(Decision::Notify {
                        surface: s.clone(),
                        title: name,
                        body: h.ping.what(),
                        reason: format!("escalate: {reason}"),
                        ping: h.ping.clone(),
                    });
                    self.held.remove(&s);
                }
            }
        }
        out
    }

    fn release_all(&mut self, now: u64, why: &str) -> Vec<Decision> {
        self.last_break = Some(now);
        if self.held.is_empty() {
            return Vec::new();
        }
        let pings: Vec<Released> = self
            .rank(now)
            .into_iter()
            .filter(|r| r.held)
            .map(|r| Released { ping: self.held[&r.surface].ping.clone(), surface: r.surface, name: r.name })
            .collect();
        let body = pings.iter().map(|p| format!("{}: {}", p.name, p.ping.what())).collect::<Vec<_>>().join(" · ");
        let n = pings.len();
        self.held.clear();
        vec![Decision::Release {
            pings,
            title: if n == 1 { "1 held ping".into() } else { format!("{n} held pings") },
            body,
            reason: format!("released {n} at a break: {why}"),
        }]
    }

    /// Every tab, best first: score, then name, then id, so equal tabs never swap.
    pub fn rank(&self, now: u64) -> Vec<Ranked> {
        let mut v: Vec<Ranked> = self
            .tabs
            .iter()
            .map(|(s, t)| {
                let parts = score::parts(t, now, &self.pred, &self.cfg);
                let reason = self.describe(t, &parts, now);
                Ranked {
                    surface: s.clone(),
                    name: t.name.clone(),
                    score: parts.total(),
                    parts,
                    focused: self.focus.as_deref() == Some(s),
                    held: self.held.contains_key(s),
                    reason,
                }
            })
            .collect();
        v.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.name.cmp(&b.name)).then_with(|| a.surface.cmp(&b.surface)));
        v
    }

    /// Tabs worth going to, best first, without the one you're on.
    pub fn next(&self, now: u64) -> Vec<Ranked> {
        self.rank(now).into_iter().filter(|r| !r.focused && r.score > 0).collect()
    }

    /// The tab "Go to next agent" opens and the title names: the first of `next`.
    pub fn top_pick(&self, now: u64) -> Option<Ranked> {
        self.next(now).into_iter().next()
    }

    /// The learned cost factor (thousandths) and usual wait for tab name `name`, when learning is on
    /// and it has a rhythm yet.
    fn pace(&self, name: &str) -> Option<(u64, u64)> {
        if !self.cfg.learn {
            return None;
        }
        Some((self.rhythm.permille(name, self.cfg.rhythm_ref_ms)?, self.rhythm.usual(name)?))
    }

    pub fn rows(&self, now: u64) -> BTreeMap<String, Option<Row>> {
        let next = self.next(now);
        let top = next.iter().find(|r| !r.held).map(|r| r.surface.clone());
        let mut out = BTreeMap::new();
        for (s, t) in &self.tabs {
            let mut row = Row { label: None, accent: false, suffix: None, bar: None };
            if self.focus.as_deref() == Some(s) {
                // You can see it.
            } else if let Some(h) = self.held.get(s) {
                row.label = Some("held".into());
                row.suffix = Some(format!("held {} · {}", mins(now.saturating_sub(h.since)), h.ping.what()));
            } else if next.iter().any(|r| r.surface == *s) {
                let is_top = top.as_deref() == Some(s);
                row.label = Some(if is_top { "next" } else { "later" }.into());
                row.accent = is_top;
                row.suffix = Some(short_reason(t, now));
            } else if t.phase == Phase::Running
                && let Some(e) = self.pred.estimate_turn(&t.name).filter(|e| e.n >= predict::CONFIDENT)
            {
                let elapsed = t.turn_start.map_or(0, |st| now.saturating_sub(st));
                if let Some(left) = predict::remaining(e, elapsed) {
                    row.label = Some(format!("~{}m", left.div_ceil(60_000)));
                    row.suffix = Some(format!("turns take ~{}", dur(e.p50)));
                    let whole = if elapsed < e.p50 { e.p50 } else { e.p80 };
                    row.bar = Some(((elapsed * 20 / whole.max(1)) * 50) as u16);
                }
            }
            if !self.cfg.labels {
                row.label = None;
                row.accent = false;
            }
            if !self.cfg.suffixes {
                row.suffix = None;
            }
            if !self.cfg.bars {
                row.bar = None;
            }
            let empty = row.label.is_none() && row.suffix.is_none() && row.bar.is_none();
            out.insert(s.clone(), if empty { None } else { Some(row) });
        }
        out
    }

    fn rows_diff(&mut self, now: u64) -> Vec<Decision> {
        let mut out = Vec::new();
        for (s, row) in self.rows(now) {
            match row {
                Some(r) if self.rows.get(&s) != Some(&r) => {
                    self.rows.insert(s.clone(), r.clone());
                    out.push(Decision::Row { surface: s, row: Some(r) });
                }
                None if self.rows.remove(&s).is_some() => out.push(Decision::Row { surface: s, row: None }),
                _ => {}
            }
        }
        out
    }

    pub fn filler(&self, now: u64) -> Option<(u64, Vec<Suggestion>)> {
        if !self.cfg.filler {
            return None;
        }
        filler::suggest(&self.tabs, self.focus.as_deref(), &self.notes, &self.pred, &self.cfg, now)
    }

    fn fill_diff(&mut self, now: u64) -> Vec<Decision> {
        let f = self.filler(now);
        let key: Vec<String> =
            f.as_ref().map(|(_, v)| v.iter().map(|s| format!("{}:{}", s.surface, s.title)).collect()).unwrap_or_default();
        if key == self.fill {
            return Vec::new();
        }
        self.fill = key;
        match f {
            Some((gap_ms, items)) => vec![Decision::Fill { gap_ms, items }],
            None => Vec::new(),
        }
    }

    /// The title-bar line, at most 48 characters: what waits for your break, else the tab to go to
    /// next, else the gap and what fits in it.
    pub fn headline(&self, now: u64) -> Option<String> {
        if !self.cfg.title {
            return None;
        }
        let line = if let Some(r) = self.top_pick(now) {
            // Always names the top pick first, so it reads even when Haunt's status hides the row's label;
            // the tab name gives way before the rest.
            let t = &self.tabs[&r.surface];
            let rest = match self.held.len() {
                0 => short_reason(t, now),
                n => format!("{n} held"),
            };
            let tail = if rest.is_empty() { String::new() } else { format!(" · {rest}") };
            let room = HEADLINE_MAX.saturating_sub("next: ".len() + tail.chars().count()).max(1);
            format!("next: {}{tail}", t.name.chars().take(room).collect::<String>())
        } else if !self.held.is_empty() {
            format!("{} held · out at your break", self.held.len())
        } else if let Some((gap, items)) = self.filler(now).filter(|(_, v)| v[0].fits) {
            format!("~{} gap: {}", dur(gap), items[0].title)
        } else {
            return None;
        };
        Some(line.chars().take(HEADLINE_MAX).collect())
    }

    fn title_diff(&mut self, now: u64) -> Vec<Decision> {
        let h = self.headline(now);
        if h == self.title {
            return Vec::new();
        }
        self.title = h.clone();
        vec![Decision::Title(h)]
    }

    /// What tab `s` did since you last looked, and your note on it.
    pub fn brief(&self, s: &str, now: u64) -> Option<String> {
        if !self.cfg.stash {
            return None;
        }
        stash::brief(self.tabs.get(s)?, self.notes.get(s).map(String::as_str), now)
    }
}

/// The title bar line's length.
pub const HEADLINE_MAX: usize = 48;

/// Minutes, for rows, which shouldn't change every tick.
fn mins(ms: u64) -> String {
    if ms < 60_000 { "<1m".into() } else { dur(ms - ms % 60_000) }
}

fn short_reason(t: &Tab, now: u64) -> String {
    let ago = mins(now.saturating_sub(t.phase_since));
    match t.phase {
        Phase::Blocked(b) => format!("{} for {ago}", score::blocked_word(b)),
        Phase::Done { failed } => {
            let mut s = format!("{} {ago} ago", if failed { "failed" } else { "finished" });
            if !t.edits.is_empty() {
                s += &format!(" · {}", plural(t.edits.len(), "file"));
            }
            s
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(eng: &mut Engine, t: u64, kind: &str, s: &str) -> Vec<Decision> {
        eng.ingest(&Event::new(t, kind, s), t)
    }

    #[test]
    fn the_title_names_the_top_pick_first_then_what_waits() {
        let mut eng = Engine::new(Config::default());
        at(&mut eng, 0, "focus", "web");
        at(&mut eng, 0, "prompt_submit", "web");
        at(&mut eng, 1_000, "focus", "api");
        at(&mut eng, 1_000, "prompt_submit", "api");
        at(&mut eng, 2_000, "stop", "api");
        let d = at(&mut eng, 5_000, "stop", "web");
        assert!(d.contains(&Decision::Title(Some("next: web · 1 held".into()))), "{d:?}");
        let d = at(&mut eng, 70_000, "prompt_submit", "api");
        assert!(d.contains(&Decision::Title(Some("next: web · finished 1m ago".into()))), "{d:?}");
        assert_eq!(eng.tick(70_000), Vec::new(), "no change, no decision");
        let d = at(&mut eng, 80_000, "focus", "web");
        assert!(d.contains(&Decision::Title(None)), "{d:?}");
        eng.cfg.title = false;
        at(&mut eng, 90_000, "focus", "api");
        assert_eq!(eng.headline(90_000), None);
    }

    #[test]
    fn the_title_always_starts_with_the_tab_go_to_next_opens() {
        let mut eng = Engine::new(Config::default());
        let long = "a-very-long-repository-name-that-goes-on-and-on";
        at(&mut eng, 0, "focus", "web");
        eng.ingest(&Event { kind: "name".into(), surface: Some("b".into()), text: Some(long.into()), ..Event::default() }, 0);
        at(&mut eng, 0, "prompt_submit", "b");
        at(&mut eng, 30_000, "permission_seen", "b");
        let top = eng.top_pick(90_000).unwrap();
        assert_eq!(top.surface, "b", "a needs-you tab is the top pick too");
        let h = eng.headline(90_000).unwrap();
        assert!(h.starts_with("next: a-very-long") && h.ends_with(" · 1 held"), "{h}");
        assert_eq!(h.chars().count(), HEADLINE_MAX, "the name gives way, not the reason: {h}");
        at(&mut eng, 95_000, "focus", "b");
        assert_eq!(eng.top_pick(95_000), None, "the tab you're on is never next");
    }

    /// `web` fails a minute in while you watch `api` work: score 112 against a watching cost of 30, so
    /// it interrupts by 82 over a threshold of 60, unless the cost doubles.
    fn failed_while_watching(eng: &mut Engine) -> Vec<Decision> {
        at(eng, 0, "focus", "api");
        at(eng, 0, "prompt_submit", "api");
        at(eng, 0, "prompt_submit", "web");
        at(eng, 60_000, "stop_failure", "web")
    }

    #[test]
    fn a_repo_you_reach_fast_costs_more_to_interrupt_for() {
        let mut learned = Engine::new(Config::default());
        let mut r = Rhythm::default();
        for _ in 0..3 {
            r.add("web", 60_000);
        }
        learned.set_rhythm(r.clone());
        let d = failed_while_watching(&mut learned);
        let hold = d.iter().find_map(|d| match d {
            Decision::Hold { reason, .. } => Some(reason.clone()),
            _ => None,
        });
        let hold = hold.unwrap_or_else(|| panic!("{d:?}"));
        assert!(hold.contains("you usually get to web within 1m (cost ×2.0)"), "{hold}");

        let mut off = Engine::new(Config { learn: false, ..Config::default() });
        off.set_rhythm(r);
        let d = failed_while_watching(&mut off);
        assert!(d.iter().any(|d| matches!(d, Decision::Notify { .. })), "without learning the same failure interrupts: {d:?}");
    }

    #[test]
    fn getting_to_a_tab_teaches_its_pace_only_while_learning() {
        for learn in [true, false] {
            let mut eng = Engine::new(Config { learn, ..Config::default() });
            at(&mut eng, 0, "focus", "api");
            for i in 0..3u64 {
                let t = i * 1_000_000;
                at(&mut eng, t, "prompt_submit", "web");
                at(&mut eng, t + 10_000, "stop", "web");
                at(&mut eng, t + 250_000, "focus", "web");
                at(&mut eng, t + 260_000, "focus", "api");
            }
            let want = learn.then_some(240_000);
            assert_eq!(eng.rhythm().usual("web"), want, "learn = {learn}");
        }
        let mut eng = Engine::new(Config::default());
        at(&mut eng, 0, "focus", "api");
        at(&mut eng, 0, "prompt_submit", "web");
        at(&mut eng, 1_000, "permission_seen", "web");
        at(&mut eng, 61_000, "tool_done", "web");
        assert_eq!(eng.rhythm().pace("web").map(|p| (p.ewma_ms, p.n)), Some((60_000, 1)), "Allow from the notification counts");
        at(&mut eng, 62_000, "focus", "web");
        assert_eq!(eng.rhythm().pace("web").unwrap().n, 1, "opening a tab that no longer waits teaches nothing");
    }

    #[test]
    fn the_same_trace_gives_the_same_rhythm() {
        let run = || {
            let mut eng = Engine::new(Config::default());
            at(&mut eng, 0, "focus", "api");
            for i in 0..5u64 {
                at(&mut eng, i * 100_000, "prompt_submit", "web");
                at(&mut eng, i * 100_000 + 1_000, "stop", "web");
                at(&mut eng, i * 100_000 + 1_000 + i * 7_000, "focus", "web");
                at(&mut eng, i * 100_000 + 50_000, "focus", "api");
            }
            eng.rhythm().to_json()
        };
        assert_eq!(run(), run());
    }
}
