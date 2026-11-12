//! End to end against a real haunt core. Copy into haunt's `cli/tests/` and run
//! `cargo test -p haunt-cli --features plugins --test multimaxxing` (see README).
#![cfg(feature = "plugins")]

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use haunt_core::decorate::{Decoration, Tone};
use haunt_core::plugin_settings::PluginConfig;
use haunt_core::plugin_store::{PluginSet, Store, attach};
use haunt_core::plugins::{HostRequest, PluginHost};
use haunt_core::{Core, SurfaceEnv};

const ID: &str = "dev.haunt.multimaxxing";

fn plugin_dir() -> PathBuf {
    std::env::var_os("HAUNT_PLUGINS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../haunt-plugins"))
        .join("haunt-multimaxxing/plugin")
}

fn until(what: &str, f: impl Fn() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(t.elapsed() < Duration::from_secs(5), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn send(sock: &mut UnixStream, env: &SurfaceEnv, rest: &str) {
    let line = format!(r#"{{"v":1,"agent":"claude","surface_id":"{}","nonce":"{}","session_id":"s-{}","cwd":"/tmp",{rest}}}"#, env.surface_id, env.nonce, env.surface_id);
    sock.write_all(format!("{line}\n").as_bytes()).unwrap();
}

struct Rig {
    home: PathBuf,
    core: Core,
    set: Arc<Mutex<PluginSet>>,
    api: SurfaceEnv,
    web: SurfaceEnv,
    sock: UnixStream,
}

impl Rig {
    fn new(tag: &str, settings: &str) -> Option<Self> {
        let dir = plugin_dir();
        if !dir.join("haunt-plugin.toml").is_file() {
            eprintln!("skipped: haunt-multimaxxing not found; set HAUNT_PLUGINS_DIR");
            return None;
        }
        let home = PathBuf::from(format!("/tmp/haunt-mmx-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        // Builds outside the repo; build.sh copies only the wasm to the manifest's entry.
        let target = std::env::var_os("MMX_PLUGIN_TARGET").map(PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join("mmx-plugin-target"));
        let status = Command::new("sh").arg(dir.join("build.sh")).env("CARGO_TARGET_DIR", target).status().unwrap();
        assert!(status.success());
        let o = Command::new(env!("CARGO_BIN_EXE_haunt")).args(["plugin", "install", dir.to_str().unwrap(), "--yes"]).env("HAUNT_HOME", &home).output().unwrap();
        assert!(o.status.success(), "{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
        let core = Core::new(&home);
        core.start().unwrap();
        let (api, web) = (core.register_surface("/tmp").unwrap(), core.register_surface("/tmp").unwrap());
        let mut set = Store::open(&home).unwrap().load_all(&PluginHost::new().unwrap(), Some(&core.handle())).unwrap();
        assert!(set.errors().is_empty(), "{:?}", set.errors());
        set.set_config(&PluginConfig::parse(&format!("[plugins.\"{ID}\"]\n{settings}\n")));
        let set = Arc::new(Mutex::new(set));
        attach(&core, &set);
        let sock = UnixStream::connect(&api.socket_path).unwrap();
        Some(Self { home, core, set, api, web, sock })
    }

    fn decoration(&self, s: &str) -> Option<Decoration> {
        self.core.snapshot().groups.iter().flat_map(|g| &g.rows).find(|r| r.surface_id == s).and_then(|r| r.decoration.clone())
    }

    fn label(&self, s: &str) -> Option<String> {
        self.decoration(s).and_then(|d| d.label)
    }

    fn requests(&self) -> Vec<HostRequest> {
        self.set.lock().unwrap().take_requests()
    }

    /// web and api both start a turn; you end on api, which finishes while you watch.
    fn two_turns_then_read_api(&mut self) {
        let (api, web) = (self.api.clone(), self.web.clone());
        self.core.select_surface(&web.surface_id);
        send(&mut self.sock, &web, r#""event":"session_start""#);
        send(&mut self.sock, &web, r#""event":"prompt_submit","query":"go""#);
        std::thread::sleep(Duration::from_millis(300));
        self.core.select_surface(&api.surface_id);
        send(&mut self.sock, &api, r#""event":"session_start""#);
        send(&mut self.sock, &api, r#""event":"prompt_submit","query":"go""#);
        send(&mut self.sock, &api, r#""event":"stop""#);
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

#[test]
fn a_finished_turn_waits_for_your_next_prompt_then_goes_out_on_its_own() {
    let Some(mut r) = Rig::new("own", "hold_haunt = false") else { return };
    r.two_turns_then_read_api();
    let (api, web) = (r.api.clone(), r.web.clone());

    send(&mut r.sock, &web, r#""event":"stop""#);
    until("web held", || r.label(&web.surface_id).as_deref() == Some("held"));
    let q = r.requests();
    assert!(!q.iter().any(|q| matches!(q, HostRequest::Notify { .. })), "nothing interrupts you yet");
    let title = |q: &[HostRequest]| q.iter().rev().find_map(|q| if let HostRequest::TitleSet { text, .. } = q { Some(text.clone()) } else { None });
    assert_eq!(title(&q), Some(Some("next: tmp · 1 held".into())), "the title names the top pick first: {q:?}");

    send(&mut r.sock, &api, r#""event":"prompt_submit","query":"next step""#);
    until("web up next", || r.label(&web.surface_id).as_deref() == Some("next"));
    let d = r.decoration(&web.surface_id).unwrap();
    assert_eq!(d.label_tone, Tone::Warn, "next is the one orange label");
    assert!(d.suffix.as_deref().unwrap().starts_with("finished"), "{:?}", d.suffix);
    let notes: Vec<_> = r.requests().into_iter().filter_map(|q| if let HostRequest::Notify { body, binding, .. } = q { Some((body, binding)) } else { None }).collect();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].0.ends_with(": finished output"), "{notes:?}");
    assert_eq!(notes[0].1.as_deref(), Some(web.surface_id.as_str()), "the notification brings web up");

    let next = r.set.lock().unwrap().palette_items(Some("next"), "");
    assert_eq!(next[0].surface_id.as_deref(), Some(web.surface_id.as_str()), "{next:?}");
    let why = r.set.lock().unwrap().palette_items(Some("why"), "");
    assert!(why.iter().any(|i| i.title.starts_with("released 1 at a break")), "{why:?}");

    r.requests();
    r.set.lock().unwrap().invoke(ID, "next_agent", true).unwrap();
    let focused: Vec<_> = r.requests().into_iter().filter_map(|q| if let HostRequest::FocusTab { surface, .. } = q { Some(surface) } else { None }).collect();
    assert_eq!(focused, [web.surface_id.clone()], "Go to next agent opens the tab ⌘K next lists first");
}

#[test]
fn with_haunt_holding_its_own_notes_the_plugin_only_starts_and_ends_the_hold() {
    let Some(mut r) = Rig::new("hold", "hold_haunt = true") else { return };
    r.two_turns_then_read_api();
    let (api, web) = (r.api.clone(), r.web.clone());
    r.requests();

    send(&mut r.sock, &web, r#""event":"stop""#);
    until("web held", || r.label(&web.surface_id).as_deref() == Some("held"));
    let q = r.requests();
    assert!(q.iter().any(|q| matches!(q, HostRequest::HoldNotifications { minutes: 10, .. })), "{q:?}");

    send(&mut r.sock, &api, r#""event":"prompt_submit","query":"next step""#);
    until("web up next", || r.label(&web.surface_id).as_deref() == Some("next"));
    let q = r.requests();
    assert!(q.iter().any(|q| matches!(q, HostRequest::ReleaseNotifications { .. })), "{q:?}");
    assert!(!q.iter().any(|q| matches!(q, HostRequest::Notify { .. })), "Haunt's summary is the one notification: {q:?}");
}
