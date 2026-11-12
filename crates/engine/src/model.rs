//! Events in, per-tab state, and the small text helpers the reasons share.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// One input to the engine. Haunt's agent events keep their wire names (`tool_start`, `stop`, ...);
/// the user's own signals are `focus` (a tab was selected), `close` and `name`. This is also one line
/// of a trace file, with `t` in ms.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    #[serde(default)]
    pub t: u64,
    #[serde(rename = "k")]
    pub kind: String,
    #[serde(rename = "s", default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// A task's status on `task_updated`; a tab's name on `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl Event {
    pub fn new(t: u64, kind: &str, surface: &str) -> Self {
        Self { t, kind: kind.into(), surface: Some(surface.into()), ..Self::default() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Block {
    Approval,
    Question,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Seen, but no turn yet.
    Fresh,
    Running,
    Blocked(Block),
    Done {
        failed: bool,
    },
}

/// Why a tab would ping you. Haunt notifies at the same moments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ping {
    Approval(String),
    Question,
    Finished,
    Failed,
}

impl Ping {
    pub fn what(&self) -> String {
        match self {
            Ping::Approval(w) => format!("{w} approval"),
            Ping::Question => "a question".into(),
            Ping::Finished => "finished output".into(),
            Ping::Failed => "a failed turn".into(),
        }
    }

    /// Whether the tab is still in the state that raised this ping.
    pub fn live(&self, phase: Phase) -> bool {
        match self {
            Ping::Approval(_) => phase == Phase::Blocked(Block::Approval),
            Ping::Question => phase == Phase::Blocked(Block::Question),
            Ping::Finished => phase == Phase::Done { failed: false },
            Ping::Failed => phase == Phase::Done { failed: true },
        }
    }
}

/// What happened on a tab since you last looked at it: the stash's raw material.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Trail {
    pub since: u64,
    pub turns_done: u32,
    pub turns_failed: u32,
    pub tasks_done: u32,
    pub tool_failures: BTreeMap<String, u32>,
    pub approvals: u32,
    pub questions: u32,
}

#[derive(Debug, Clone)]
pub struct Tab {
    pub name: String,
    pub phase: Phase,
    pub phase_since: u64,
    pub turn_start: Option<u64>,
    /// Open tool calls: predictor key and start.
    pub open: Vec<(String, u64)>,
    pub tasks: BTreeSet<String>,
    pub subagents: u32,
    /// Files edited since you last looked; a path-less edit counts under its own key.
    pub edits: BTreeSet<String>,
    pub failures_in_turn: u32,
    /// What it waits on: "Bash (git push)".
    pub waiting_on: Option<String>,
    /// You looked at it since it reached this phase.
    pub acked: bool,
    pub trail: Trail,
}

impl Tab {
    pub fn new(name: &str, now: u64) -> Self {
        Self {
            name: name.into(),
            phase: Phase::Fresh,
            phase_since: now,
            turn_start: None,
            open: Vec::new(),
            tasks: BTreeSet::new(),
            subagents: 0,
            edits: BTreeSet::new(),
            failures_in_turn: 0,
            waiting_on: None,
            acked: false,
            trail: Trail { since: now, ..Trail::default() },
        }
    }

    /// Whether the phase changed.
    pub fn set_phase(&mut self, p: Phase, now: u64) -> bool {
        if self.phase == p {
            return false;
        }
        self.phase = p;
        self.phase_since = now;
        self.acked = false;
        true
    }
}

/// "1 file", "3 files".
pub fn plural(n: usize, one: &str) -> String {
    if n == 1 { format!("1 {one}") } else { format!("{n} {one}s") }
}

/// A short duration: "40s", "6m", "1h05m".
pub fn dur(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}

fn word(t: &str, max: usize) -> bool {
    let mut c = t.chars();
    c.next().is_some_and(|f| f.is_ascii_lowercase())
        && t.len() <= max
        && t.chars().all(|c| c.is_ascii_lowercase() || matches!(c, ':' | '_' | '-'))
}

/// The part of a command that predicts its duration: the program and up to two plain lowercase
/// words after it ("cargo test", "npm run build"). Never a flag, path, number or value, so a token
/// in an argument can't end up in kv or a label.
pub fn command_key(cmd: &str) -> Option<String> {
    let mut toks = cmd.split_whitespace().skip_while(|t| t.contains('='));
    let prog = toks.next()?.rsplit('/').next()?;
    let ok = prog.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && prog.len() <= 24
        && prog.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'));
    if !ok {
        return None;
    }
    let mut key = prog.to_string();
    for t in toks.take(2) {
        if !word(t, 16) {
            break;
        }
        key.push(' ');
        key.push_str(t);
    }
    Some(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_short() {
        assert_eq!([dur(40_000), dur(360_000), dur(3_900_000)], ["40s", "6m", "1h05m"]);
    }

    #[test]
    fn command_keys_keep_only_plain_words() {
        assert_eq!(command_key("cargo test -p engine").as_deref(), Some("cargo test"));
        assert_eq!(command_key("npm run build").as_deref(), Some("npm run build"));
        assert_eq!(command_key("TOKEN=abc /usr/bin/git push origin main").as_deref(), Some("git push origin"));
        assert_eq!(command_key("echo sk-ABC123").as_deref(), Some("echo"));
        assert_eq!(command_key("./run.sh"), Some("run.sh".into()));
        assert_eq!(command_key("$(curl x)"), None);
        assert_eq!(command_key(""), None);
    }
}
