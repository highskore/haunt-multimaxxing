//! Whil every agent is working: how long until the first one needs you, and what fits in that gap.

use std::collections::BTreeMap;

use crate::model::{Phase, Tab, dur, plural};
use crate::predict::{self, Predictor};
use crate::score::{self, Config};

/// Looking at a failure without edits to review.
const FAILURE_MS: u64 = 2 * 60_000;
const NOTE_MS: u64 = 2 * 60_000;
const MAX_ITEMS: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub surface: String,
    pub title: String,
    pub est_ms: u64,
    pub fits: bool,
    pub reason: String,
}

/// The gap and what to do in it, or `None` when something needs you, nothing is running, the wait
/// is unknown (fewer than three turns seen) or too short to bother.
pub fn suggest(
    tabs: &BTreeMap<String, Tab>,
    focus: Option<&str>,
    notes: &BTreeMap<String, String>,
    p: &Predictor,
    c: &Config,
    now: u64,
) -> Option<(u64, Vec<Suggestion>)> {
    if tabs.values().any(|t| matches!(t.phase, Phase::Blocked(_))) {
        return None;
    }
    let mut soonest: Option<(u64, &str)> = None;
    for t in tabs.values().filter(|t| t.phase == Phase::Running) {
        p.estimate_turn(&t.name).filter(|e| e.n >= predict::CONFIDENT)?;
        let left = score::turn_left(t, now, p)?;
        if soonest.is_none_or(|(l, _)| left < l) {
            soonest = Some((left, &t.name));
        }
    }
    let (gap, first) = soonest?;
    if gap < c.filler_min_gap_ms {
        return None;
    }
    let mut items: Vec<(i64, Suggestion)> = Vec::new();
    for (s, t) in tabs {
        if focus == Some(s.as_str()) {
            continue;
        }
        let rank = score::parts(t, now, p, c).total();
        let item = match t.phase {
            Phase::Done { .. } if !t.edits.is_empty() => {
                let n = t.edits.len();
                Some((format!("review {}'s diff ({})", t.name, plural(n, "file")), n as u64 * c.review_ms_per_file))
            }
            Phase::Done { failed: true } if !t.acked => Some((format!("look at why {} failed", t.name), FAILURE_MS)),
            _ => None,
        };
        if let Some((title, est)) = item {
            items.push((rank, sug(s, title, est, gap, first)));
        }
        if let Some(n) = notes.get(s) {
            items.push((0, sug(s, format!("your note on {}: {n}", t.name), NOTE_MS, gap, first)));
        }
    }
    if items.is_empty() {
        return None;
    }
    items.sort_by(|(ra, a), (rb, b)| b.fits.cmp(&a.fits).then(rb.cmp(ra)).then_with(|| a.title.cmp(&b.title)));
    Some((gap, items.into_iter().take(MAX_ITEMS).map(|(_, s)| s).collect()))
}

fn sug(surface: &str, title: String, est: u64, gap: u64, first: &str) -> Suggestion {
    let fits = est <= gap;
    let reason = if fits {
        format!("~{} of ~{} before {first} is due", dur(est), dur(gap))
    } else {
        format!("~{}, longer than the ~{} before {first} is due", dur(est), dur(gap))
    };
    Suggestion { surface: surface.into(), title, est_ms: est, fits, reason }
}
