//! The resume brief: what a tab did since you last looked, in one line, from counts alone.

use crate::model::{Block, Phase, Tab, dur, plural};

fn times(n: u32) -> String {
    match n {
        1 => "once".into(),
        2 => "twice".into(),
        n => format!("{n} times"),
    }
}

/// "while away 12m: 2 turns finished, 3 tasks done, cargo test failed twice; now waits on Bash
/// approval · note: check the migration". `None` when there's nothing to say.
pub fn brief(t: &Tab, note: Option<&str>, now: u64) -> Option<String> {
    let tr = &t.trail;
    let mut did = Vec::new();
    if tr.turns_done > 0 {
        did.push(format!("{} finished", plural(tr.turns_done as usize, "turn")));
    }
    if tr.turns_failed > 0 {
        did.push(format!("{} failed", plural(tr.turns_failed as usize, "turn")));
    }
    if tr.tasks_done > 0 {
        did.push(format!("{} done", plural(tr.tasks_done as usize, "task")));
    }
    for (k, n) in &tr.tool_failures {
        let what = k.split_once(':').map_or(k.as_str(), |(_, c)| c);
        did.push(format!("{what} failed {}", times(*n)));
    }
    if tr.approvals > 0 {
        did.push(format!("asked for {}", plural(tr.approvals as usize, "approval")));
    }
    if !t.edits.is_empty() {
        did.push(format!("{} edited", plural(t.edits.len(), "file")));
    }
    let state = match t.phase {
        Phase::Blocked(Block::Approval) => Some(format!("now waits on {} approval", t.waiting_on.as_deref().unwrap_or("tool"))),
        Phase::Blocked(Block::Question) => Some("now waits on your answer".into()),
        Phase::Running => Some("still working".into()),
        Phase::Done { .. } | Phase::Fresh => None,
    };
    if did.is_empty() && note.is_none() && !matches!(t.phase, Phase::Blocked(_)) {
        return None;
    }
    let mut s = format!("while away {}", dur(now.saturating_sub(tr.since)));
    if !did.is_empty() {
        s += &format!(": {}", did.join(", "));
    }
    if let Some(st) = state {
        s += &format!("; {st}");
    }
    if let Some(n) = note {
        s += &format!(" · note: {n}");
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_brief_from_counts() {
        let mut t = Tab::new("api", 0);
        assert_eq!(brief(&t, None, 60_000), None);
        t.trail.turns_done = 2;
        t.trail.tasks_done = 3;
        t.trail.tool_failures.insert("Bash:cargo test".into(), 2);
        t.waiting_on = Some("Bash (git push)".into());
        t.set_phase(Phase::Blocked(Block::Approval), 0);
        assert_eq!(
            brief(&t, Some("check the migration"), 720_000).as_deref(),
            Some(
                "while away 12m: 2 turns finished, 3 tasks done, cargo test failed twice; now waits on Bash (git push) approval · note: check the migration"
            )
        );
    }
}
