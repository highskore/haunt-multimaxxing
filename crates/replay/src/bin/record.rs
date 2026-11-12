//! Records a live session's hook events: listens where an agent's Haunt adapter sends them
//! (`HAUNT_SOCK`) and appends each line, with the time it arrived, to a wire file. Turn the wire file
//! into a scrubbed trace with `replay --import`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [sock, out] = args.as_slice() else {
        eprintln!("usage: record SOCKET WIRE.jsonl");
        return ExitCode::from(2);
    };
    let _ = std::fs::remove_file(sock);
    let listener = match UnixListener::bind(sock) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{sock}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let file = match std::fs::OpenOptions::new().create(true).append(true).open(out) {
        Ok(f) => Arc::new(Mutex::new(f)),
        Err(e) => {
            eprintln!("{out}: {e}");
            return ExitCode::FAILURE;
        }
    };
    for conn in listener.incoming().flatten() {
        let file = Arc::clone(&file);
        std::thread::spawn(move || {
            for line in BufReader::new(conn).lines().map_while(Result::ok) {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
                let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
                let rec = serde_json::json!({ "at": at, "line": v });
                let mut f = file.lock().unwrap_or_else(|e| e.into_inner());
                let _ = writeln!(f, "{rec}");
            }
        });
    }
    ExitCode::SUCCESS
}
