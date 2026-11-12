//! Properties over random traces: thousands of sessions from a seeded generator, so a failure
//! names the seed that reproduces it.

use std::collections::BTreeMap;

use multimaxxing_engine::{Config, Decision, Engine, Event};

const TICK: u64 = 15_000;
const TABS: [&str; 5] = ["api", "web", "db", "ops", "docs"];
const KINDS: [&str; 17] = [
    "close",
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
    "focus",
];
/// Words Haunt's own status uses; a plugin label must never read like it.
const STATUS_WORDS: [&str; 7] = ["needs you", "waiting", "idle", "running", "done", "failed", "error"];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[(self.next() % xs.len() as u64) as usize]
    }
}

fn random_event(r: &mut Rng, t: u64) -> Event {
    let mut e = Event::new(t, r.pick(&KINDS), r.pick(&TABS));
    e.tool = Some(r.pick(&["Bash", "Edit", "Read"]).into());
    e.cmd = Some(r.pick(&["cargo test", "npm run build", "git push origin main"]).into());
    e.task = Some(r.pick(&["1", "2", "3"]).into());
    e.text = Some(r.pick(&["completed", "in_progress", "deleted"]).into());
    e.path = Some(r.pick(&["a.rs", "b.rs"]).into());
    e
}

/// Feeds `steps` random events, ticking every 15 s between them as the plugin does; calls `check`
/// with each batch of decisions.
fn session(seed: u64, steps: usize, mut check: impl FnMut(&Engine, u64, &[Decision])) -> (Engine, u64) {
    let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut eng = Engine::new(Config::default());
    let mut t = 0;
    for _ in 0..steps {
        let gap = r.next() % 120_000;
        let end = t + gap;
        let mut tick = t - t % TICK + TICK;
        while tick < end {
            let d = eng.tick(tick);
            check(&eng, tick, &d);
            tick += TICK;
        }
        t = end;
        let e = random_event(&mut r, t);
        let d = eng.ingest(&e, t);
        check(&eng, t, &d);
    }
    (eng, t)
}

#[test]
fn no_ping_is_held_past_the_cap() {
    let cap = Config::default().max_hold_ms;
    for seed in 0..400 {
        let mut since: BTreeMap<String, u64> = BTreeMap::new();
        let (mut eng, t) = session(seed, 200, |_, now, ds| {
            for d in ds {
                match d {
                    Decision::Hold { surface, .. } => {
                        since.entry(surface.clone()).or_insert(now);
                    }
                    Decision::Notify { surface, .. } | Decision::Drop { surface, .. } => {
                        since.remove(surface);
                    }
                    Decision::Release { pings, .. } => {
                        for p in pings {
                            since.remove(&p.surface);
                        }
                    }
                    _ => {}
                }
            }
            for (s, at) in &since {
                assert!(now - at <= cap + TICK, "seed {seed}: {s} held {} ms", now - at);
            }
        });
        // Left alone, everything still held goes out by the cap.
        let mut now = t;
        while now <= t + cap + TICK {
            now += TICK;
            for d in eng.tick(now) {
                if let Decision::Release { pings, .. } = &d {
                    for p in pings {
                        since.remove(&p.surface);
                    }
                } else if let Decision::Notify { surface, .. } | Decision::Drop { surface, .. } = &d {
                    since.remove(surface);
                }
            }
        }
        assert!(since.is_empty(), "seed {seed}: still held {since:?}");
    }
}

#[test]
fn ranking_is_stable_and_ordered() {
    for seed in 0..300 {
        session(seed, 150, |eng, now, _| {
            let a = eng.rank(now);
            assert_eq!(a, eng.rank(now), "seed {seed}: same state, same ranking");
            for w in a.windows(2) {
                let ok = w[0].score > w[1].score
                    || (w[0].score == w[1].score
                        && (w[0].name.as_str(), w[0].surface.as_str()) < (w[1].name.as_str(), w[1].surface.as_str()));
                assert!(ok, "seed {seed}: {} before {}", w[0].surface, w[1].surface);
            }
        });
    }
}

#[test]
fn a_second_tick_at_the_same_moment_changes_nothing() {
    for seed in 0..300 {
        let (mut eng, t) = session(seed, 100, |_, _, _| {});
        eng.tick(t + TICK);
        assert_eq!(eng.tick(t + TICK), Vec::new(), "seed {seed}");
    }
}

#[test]
fn never_pings_the_tab_you_are_on_and_labels_stay_out_of_haunts_words() {
    for seed in 0..300 {
        session(seed, 150, |eng, now, ds| {
            for d in ds {
                if let Decision::Notify { surface, .. } | Decision::Hold { surface, .. } = d {
                    assert_ne!(Some(surface.as_str()), eng.focused(), "seed {seed}");
                }
            }
            let rows = eng.rows(now);
            let next = rows.values().flatten().filter(|r| r.label.as_deref() == Some("next")).count();
            assert!(next <= 1, "seed {seed}: {next} tabs say next");
            for r in rows.values().flatten() {
                let l = r.label.as_deref().unwrap_or("").to_lowercase();
                assert!(l.chars().count() <= 24 && !STATUS_WORDS.iter().any(|w| l.contains(w)), "seed {seed}: label {l:?}");
                assert!(r.suffix.as_deref().is_none_or(|s| s.chars().count() <= 64), "seed {seed}: suffix too long");
            }
            if let Some(h) = eng.headline(now) {
                assert!(h.chars().count() <= 48, "seed {seed}: title {h:?}");
            }
        });
    }
}
