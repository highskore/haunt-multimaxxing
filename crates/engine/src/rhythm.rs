//! How quickly you get to each repo on your own: an EWMA of the time from a tab blocking or
//! finishing to you answering it or opening its output, kept per tab name. A repo you reach fast
//! costs more to interrupt for (you'll be there soon anyway); one you leave waiting costs less.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Keys kept in all; the least sampled go first.
pub const MAX_KEYS: usize = 128;
/// Samples before the learned pace changes anything.
pub const CONFIDENT: u32 = 3;
/// A single wait counts as at most this long, so one forgotten tab doesn't swamp the average.
pub const SAMPLE_CAP_MS: u64 = 60 * 60_000;
/// The cost factor's bounds, in thousandths: half to twice the base cost.
pub const MIN_PERMILLE: u64 = 500;
pub const MAX_PERMILLE: u64 = 2000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pace {
    /// The moving average, in ms: each sample moves it a quarter of the way.
    pub ewma_ms: u64,
    pub n: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rhythm {
    keys: BTreeMap<String, Pace>,
}

impl Rhythm {
    /// You got to `key` `ms` after it started waiting on you.
    pub fn add(&mut self, key: &str, ms: u64) {
        let ms = ms.min(SAMPLE_CAP_MS);
        let p = self.keys.entry(key.to_string()).or_default();
        p.ewma_ms = if p.n == 0 { ms } else { (p.ewma_ms * 3 + ms) / 4 };
        p.n = p.n.saturating_add(1);
        if self.keys.len() > MAX_KEYS {
            let fewest = self.keys.iter().min_by_key(|(k, p)| (p.n, (*k).clone())).map(|(k, _)| k.clone());
            if let Some(k) = fewest {
                self.keys.remove(&k);
            }
        }
    }

    pub fn pace(&self, key: &str) -> Option<Pace> {
        self.keys.get(key).copied()
    }

    /// How long you usually take to get to `key`, once there are enough samples.
    pub fn usual(&self, key: &str) -> Option<u64> {
        self.pace(key).filter(|p| p.n >= CONFIDENT).map(|p| p.ewma_ms)
    }

    /// The factor on the cost of interrupting you for `key`, in thousandths: `reference / usual`,
    /// clamped to 0.5–2. None until `usual` is known.
    pub fn permille(&self, key: &str, reference_ms: u64) -> Option<u64> {
        let usual = self.usual(key)?.max(1);
        Some((reference_ms.saturating_mul(1000) / usual).clamp(MIN_PERMILLE, MAX_PERMILLE))
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn from_json(s: &str) -> Option<Self> {
        let r: Self = serde_json::from_str(s).ok()?;
        (r.keys.len() <= MAX_KEYS).then_some(r)
    }
}

/// `1500` as "1.5", for reasons.
pub fn times(permille: u64) -> String {
    format!("{}.{}", permille / 1000, permille % 1000 / 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_average_moves_a_quarter_per_sample_and_waits_for_three() {
        let mut r = Rhythm::default();
        r.add("billing", 240_000);
        assert_eq!(r.usual("billing"), None, "one sample is not a rhythm");
        r.add("billing", 80_000);
        r.add("billing", 80_000);
        // 240 → (720+80)/4 = 200 → (600+80)/4 = 170 s
        assert_eq!(r.pace("billing"), Some(Pace { ewma_ms: 170_000, n: 3 }));
        assert_eq!(r.usual("billing"), Some(170_000));
    }

    #[test]
    fn the_factor_is_reference_over_usual_clamped_to_half_and_double() {
        let mut r = Rhythm::default();
        for (k, ms) in [("fast", 60_000), ("even", 480_000), ("slow", 3_600_000), ("mid", 320_000)] {
            for _ in 0..3 {
                r.add(k, ms);
            }
        }
        let f = |k| r.permille(k, 480_000);
        assert_eq!((f("fast"), f("even"), f("slow"), f("mid"), f("none")), (Some(2000), Some(1000), Some(500), Some(1500), None));
        assert_eq!(times(1500), "1.5");
    }

    #[test]
    fn one_long_wait_is_capped_and_keys_are_bounded() {
        let mut r = Rhythm::default();
        r.add("a", u64::MAX);
        assert_eq!(r.pace("a").unwrap().ewma_ms, SAMPLE_CAP_MS);
        for i in 0..=MAX_KEYS {
            r.add(&format!("k{i:03}"), 1_000);
        }
        assert_eq!(r.keys.len(), MAX_KEYS);
        assert_eq!(Rhythm::from_json(&r.to_json()), Some(r));
    }
}
