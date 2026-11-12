//! How long things take: the last durations per key (a whole turn, a tool, a command), read back as
//! a median and an 80th percentile.

use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};

/// Durations kept per key; the newest replace the oldest.
pub const KEEP: usize = 32;
/// Keys kept in all; the least used go first.
pub const MAX_KEYS: usize = 128;
/// Samples before an estimate is trusted enough to show as "~3m".
pub const CONFIDENT: usize = 3;

pub const TURN: &str = "turn";

/// One tab's turns, by its name, which outlives the surface id.
pub fn turn_key(name: &str) -> String {
    format!("turn@{name}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Estimate {
    pub p50: u64,
    pub p80: u64,
    pub n: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Predictor {
    keys: BTreeMap<String, VecDeque<u64>>,
}

impl Predictor {
    pub fn add(&mut self, key: &str, ms: u64) {
        let r = self.keys.entry(key.to_string()).or_default();
        if r.len() == KEEP {
            r.pop_front();
        }
        r.push_back(ms);
        if self.keys.len() > MAX_KEYS {
            let fewest = self.keys.iter().filter(|(k, _)| *k != TURN).min_by_key(|(k, v)| (v.len(), (*k).clone())).map(|(k, _)| k.clone());
            if let Some(k) = fewest {
                self.keys.remove(&k);
            }
        }
    }

    pub fn estimate(&self, key: &str) -> Option<Estimate> {
        let r = self.keys.get(key)?;
        let mut v: Vec<u64> = r.iter().copied().collect();
        v.sort_unstable();
        let at = |q: usize| v[((v.len() - 1) * q) / 100];
        Some(Estimate { p50: at(50), p80: at(80), n: v.len() })
    }

    /// This tab's turns once it has a few, else every tab's.
    pub fn estimate_turn(&self, name: &str) -> Option<Estimate> {
        self.estimate(&turn_key(name)).filter(|e| e.n >= CONFIDENT).or_else(|| self.estimate(TURN))
    }

    /// The command's own history, else its tool's.
    pub fn estimate_call(&self, key: &str) -> Option<Estimate> {
        self.estimate(key).or_else(|| key.split_once(':').and_then(|(tool, _)| self.estimate(tool)))
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn from_json(s: &str) -> Option<Self> {
        serde_json::from_str(s).ok()
    }
}

/// Time left for something `elapsed` into a run expected to take `e`: to the median while under it,
/// then to the 80th percentile; past that it's anyone's guess.
pub fn remaining(e: Estimate, elapsed: u64) -> Option<u64> {
    if elapsed < e.p50 {
        Some(e.p50 - elapsed)
    } else if elapsed < e.p80 {
        Some(e.p80 - elapsed)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_and_p80_of_the_last_runs() {
        let mut p = Predictor::default();
        for s in [10, 20, 30, 40, 100] {
            p.add("Bash:cargo test", s * 1000);
        }
        assert_eq!(p.estimate("Bash:cargo test"), Some(Estimate { p50: 30_000, p80: 40_000, n: 5 }));
        assert_eq!(p.estimate_call("Bash:npm test"), None);
        p.add("Bash", 5_000);
        assert_eq!(p.estimate_call("Bash:npm test").map(|e| e.p50), Some(5_000), "falls back to the tool");
    }

    #[test]
    fn keeps_the_newest_and_caps_keys() {
        let mut p = Predictor::default();
        for i in 0..(KEEP as u64 + 5) {
            p.add(TURN, i);
        }
        assert_eq!(p.estimate(TURN).unwrap().n, KEEP);
        for i in 0..(MAX_KEYS + 10) {
            p.add(&format!("k{i}"), 1);
        }
        assert!(p.keys.len() <= MAX_KEYS && p.estimate(TURN).is_some(), "the turn key survives");
        assert_eq!(Predictor::from_json(&p.to_json()), Some(p));
    }

    #[test]
    fn remaining_walks_median_then_p80() {
        let e = Estimate { p50: 60_000, p80: 90_000, n: 5 };
        assert_eq!([remaining(e, 0), remaining(e, 70_000), remaining(e, 95_000)], [Some(60_000), Some(20_000), None]);
    }
}
