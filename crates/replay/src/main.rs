use std::process::ExitCode;

use multimaxxing_engine::Config;
use replay::{Opts, import, run};

const USAGE: &str = "usage: replay [--decisions-only] [--no-rows] [--rank] [--no-learn] [--tick SECS] TRACE.jsonl...\n       replay --import WIRE.jsonl   (a `record` file as a scrubbed trace on stdout)";

fn main() -> ExitCode {
    let mut opts = Opts::default();
    let mut cfg = Config::default();
    let mut files = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--decisions-only" => opts.events = false,
            "--no-rows" => opts.rows = false,
            "--rank" => opts.rank = true,
            "--no-learn" => cfg.learn = false,
            "--import" => {
                let Some(f) = args.next() else {
                    eprintln!("{USAGE}");
                    return ExitCode::from(2);
                };
                return match std::fs::read_to_string(&f).map_err(|e| e.to_string()).and_then(|t| import(&t)) {
                    Ok(out) => {
                        print!("{out}");
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("{f}: {e}");
                        ExitCode::FAILURE
                    }
                };
            }
            "--tick" => match args.next().and_then(|s| s.parse::<u64>().ok()) {
                Some(s) => opts.tick_ms = s * 1000,
                None => {
                    eprintln!("{USAGE}");
                    return ExitCode::from(2);
                }
            },
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            _ => files.push(a),
        }
    }
    if files.is_empty() {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    }
    for f in files {
        let text = match std::fs::read_to_string(&f) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("{f}: {e}");
                return ExitCode::FAILURE;
            }
        };
        match run(&text, cfg.clone(), &opts) {
            Ok(out) => print!("{out}"),
            Err(e) => {
                eprintln!("{f}: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    ExitCode::SUCCESS
}
