//! Every trace in `traces/` replays to exactly its `traces/expected/<name>.txt`.
//! `UPDATE_GOLDEN=1 cargo test` rewrites them; review the diff before committing.

use std::path::Path;

use multimaxxing_engine::Config;
use replay::{Opts, run};

#[test]
fn traces_replay_to_their_golden_output() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../traces");
    let update = std::env::var_os("UPDATE_GOLDEN").is_some();
    let mut seen = 0;
    let mut entries: Vec<_> = std::fs::read_dir(&root).unwrap().map(|e| e.unwrap().path()).collect();
    entries.sort();
    for trace in entries.into_iter().filter(|p| p.extension().is_some_and(|e| e == "jsonl")) {
        let name = trace.file_stem().unwrap().to_string_lossy().into_owned();
        let got = run(&std::fs::read_to_string(&trace).unwrap(), Config::default(), &Opts::default()).unwrap();
        let want_path = root.join("expected").join(format!("{name}.txt"));
        if update {
            std::fs::write(&want_path, &got).unwrap();
        } else {
            let want = std::fs::read_to_string(&want_path).unwrap_or_else(|_| panic!("no golden for {name}; run with UPDATE_GOLDEN=1"));
            assert_eq!(got, want, "{name} drifted from its golden output");
        }
        seen += 1;
    }
    assert!(seen >= 5, "expected the traces, found {seen}");
}

#[test]
fn the_learned_rhythm_turns_an_interrupt_into_a_hold() {
    let trace = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../traces/synthetic-learned-rhythm.jsonl")).unwrap();
    let opts = Opts { events: false, rows: false, ..Opts::default() };
    let line = |out: &str| out.lines().find(|l| l.contains("billing") && l.contains("failed")).map(str::to_owned).unwrap();
    let learned = run(&trace, Config::default(), &opts).unwrap();
    let base = run(&trace, Config { learn: false, ..Config::default() }, &opts).unwrap();
    let (l, b) = (line(&learned), line(&base));
    assert!(l.contains("hold") && l.contains("you usually get to billing within 4m (cost ×2.0)"), "{l}");
    assert!(b.contains("NOTIFY") && !b.contains("usually"), "{b}");
    assert_eq!(run(&trace, Config::default(), &opts).unwrap(), learned, "the same trace, the same output");
}
