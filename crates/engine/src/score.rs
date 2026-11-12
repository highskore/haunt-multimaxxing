//! How much each tab deserves your attention now, and how much it costs to pull you off the one
//! you're on. Points are roughly seconds of value, kept in integers so every platform agrees.

use crate::model::{Block, Phase, Tab, dur};
use crate::predict::{self, Predictor};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Interrupt when a tab's score beats the cost of switching by this much.
    pub threshold: i64,
    /// No ping is held longer than this.
    pub max_hold_ms: u64,
    /// Switching is free this long after a break (a prompt sent, a tab switched, the tab you watch stopping).
    pub grace_ms: u64,
    /// The cost of pulling you off a tab you're only watching work.
    pub watch_cost: i64,
    /// The cost of pulling you off a tab you're reading or answering, growing half a point a second.
    pub focus_base: i64,
    pub focus_cap: i64,
    pub blocked_base: i64,
    pub done_base: i64,
    pub failed_base: i64,
    pub per_edit: i64,
    /// Work behind a block counts up to this many seconds.
    pub payoff_cap_s: i64,
    /// Filler: the shortest wait worth filling, and how long a file takes to review.
    pub filler_min_gap_ms: u64,
    pub review_ms_per_file: u64,
    pub router: bool,
    /// Route approvals and questions too. Off when Haunt shows those itself.
    pub route_blocks: bool,
    pub labels: bool,
    pub suffixes: bool,
    pub bars: bool,
    pub filler: bool,
    pub stash: bool,
    /// One line for the title bar: the top pick, what waits for your break, or the gap.
    pub title: bool,
    /// Learn how quickly you get to each repo, and scale its interruption cost by `rhythm_ref_ms /
    /// usual`, clamped to 0.5–2.
    pub learn: bool,
    /// The usual wait at which a repo's interruption cost is the base.
    pub rhythm_ref_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            threshold: 60,
            max_hold_ms: 10 * 60_000,
            grace_ms: 30_000,
            watch_cost: 30,
            focus_base: 60,
            focus_cap: 600,
            blocked_base: 120,
            done_base: 40,
            failed_base: 100,
            per_edit: 15,
            payoff_cap_s: 1200,
            filler_min_gap_ms: 3 * 60_000,
            review_ms_per_file: 45_000,
            router: true,
            route_blocks: true,
            labels: true,
            suffixes: true,
            bars: false,
            filler: true,
            stash: true,
            title: true,
            learn: true,
            rhythm_ref_ms: 8 * 60_000,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parts {
    /// It waits on you, weighted by how long and by the tasks and subagents behind it.
    pub blocked: i64,
    /// Finished output loses value unread; a failure more so.
    pub stale: i64,
    /// Unblocking it lets this much predicted work run.
    pub payoff: i64,
    /// The predicted work behind a block, for the reason.
    pub behind_ms: Option<u64>,
}

impl Parts {
    pub fn total(&self) -> i64 {
        self.blocked + self.stale + self.payoff
    }
}

/// The rest of the current turn, or a typical turn when it hasn't started.
pub fn turn_left(t: &Tab, now: u64, p: &Predictor) -> Option<u64> {
    let e = p.estimate_turn(&t.name)?;
    let elapsed = t.turn_start.map_or(0, |s| now.saturating_sub(s));
    predict::remaining(e, elapsed)
}

pub fn parts(t: &Tab, now: u64, p: &Predictor, c: &Config) -> Parts {
    let waited = (now.saturating_sub(t.phase_since) / 1000) as i64;
    match t.phase {
        Phase::Blocked(_) => {
            let weight = 2 + t.tasks.len() as i64 + i64::from(t.subagents);
            let behind = turn_left(t, now, p).map(|ms| ms.max(30_000));
            let payoff = behind.map_or(0, |ms| ((ms / 1000) as i64).min(c.payoff_cap_s) / 4);
            Parts { blocked: c.blocked_base + waited * weight / 2, stale: 0, payoff, behind_ms: behind }
        }
        Phase::Done { .. } if t.acked => Parts::default(),
        Phase::Done { failed } => {
            let edits = t.edits.len() as i64 * c.per_edit;
            let stale = if failed { c.failed_base + waited / 5 } else { c.done_base + waited / 10 };
            Parts { stale: stale + edits, ..Parts::default() }
        }
        Phase::Fresh | Phase::Running => Parts::default(),
    }
}

/// What you're doing on the focused tab, for the cost and its reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Focus {
    Nowhere,
    /// Right after a break: free to switch.
    Break,
    Watching(String),
    Reading {
        name: String,
        for_ms: u64,
    },
}

impl Focus {
    pub fn cost(&self, c: &Config) -> i64 {
        match self {
            Focus::Nowhere | Focus::Break => 0,
            Focus::Watching(_) => c.watch_cost,
            Focus::Reading { for_ms, .. } => (c.focus_base + (*for_ms / 2000) as i64).min(c.focus_cap),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Focus::Nowhere => "no tab in focus".into(),
            Focus::Break => "you're at a break".into(),
            Focus::Watching(n) => format!("you're watching {n} work"),
            Focus::Reading { name, for_ms } => format!("you're {} into {name}", dur(*for_ms)),
        }
    }
}

pub fn blocked_word(b: Block) -> &'static str {
    match b {
        Block::Approval => "approval",
        Block::Question => "question",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::predict::TURN;

    #[test]
    fn blocks_grow_with_the_wait_and_the_work_behind() {
        let c = Config::default();
        let mut p = Predictor::default();
        let mut t = Tab::new("api", 0);
        t.set_phase(Phase::Blocked(Block::Approval), 0);
        assert_eq!(parts(&t, 60_000, &p, &c).total(), 120 + 60);
        t.tasks.insert("1".into());
        t.subagents = 1;
        assert_eq!(parts(&t, 60_000, &p, &c).total(), 120 + 120);
        for _ in 0..3 {
            p.add(TURN, 20 * 60_000);
        }
        t.turn_start = Some(0);
        let q = parts(&t, 60_000, &p, &c);
        assert_eq!((q.payoff, q.behind_ms), (285, Some(19 * 60_000)), "19 min of turn left, a quarter point a second");
    }

    #[test]
    fn failures_outrank_successes_and_edits_add() {
        let c = Config::default();
        let p = Predictor::default();
        let mut ok = Tab::new("a", 0);
        ok.set_phase(Phase::Done { failed: false }, 0);
        let mut bad = Tab::new("b", 0);
        bad.set_phase(Phase::Done { failed: true }, 0);
        assert!(parts(&bad, 0, &p, &c).total() > parts(&ok, 0, &p, &c).total());
        ok.edits.insert("x.rs".into());
        assert_eq!(parts(&ok, 0, &p, &c).total(), 40 + 15);
    }

    #[test]
    fn focus_cost_ramps_and_caps() {
        let c = Config::default();
        let r = |ms| Focus::Reading { name: "api".into(), for_ms: ms }.cost(&c);
        assert_eq!([Focus::Break.cost(&c), Focus::Watching("x".into()).cost(&c), r(0), r(360_000), r(3_600_000)], [0, 30, 60, 240, 600]);
    }
}
