# MULTIMAXXING

An attention scheduler for Haunt. Haunt shows what each agent is doing; MULTIMAXXING decides which tab deserves you next, whether a ping may interrupt you now or should wait for a break, and what fits while every agent is busy.

With five agents running, you become the bottleneck: every finished turn and every approval pings you, whatever you were in the middle of. MULTIMAXXING scores each tab, weighs it against what switching would cost you right now, and holds what can wait. Held pings go out together at your next natural break: you send a prompt, switch tabs, or the tab you're watching stops. Nothing waits longer than a cap, and every decision comes with a one-line reason.

## Run it on Linux, no Haunt needed

All the logic is in `crates/engine`, a plain Rust library with no wasm, no Haunt dependency and no clock: time is passed in. `crates/replay` feeds a trace through it.

```sh
cargo test
cargo run -p replay -- --decisions-only --no-rows traces/synthetic-payoff.jsonl
```

```
# synthetic-payoff (hand-made): two approvals while you read docs. build has ~20 min of work behind
# its approval, so it interrupts at once; lint has ~30 s, so it waits until it has waited long enough.
00:02:00  NOTIFY  build  interrupt: build waits 0s on Bash (docker build) approval, ~18m of work behind it; you're 1m into docs (score 390 > cost 100 + 60)
00:03:40  hold    lint   held: lint waits 0s on Bash (npx eslint) approval; you're 3m into docs (score 120 <= cost 150 + 60)
00:06:45  NOTIFY  lint   escalate: lint waits 3m on Bash (npx eslint) approval; you're 6m into docs (score 305 > cost 242 + 60)
```

`replay` with no flags also prints each event, the row labels as they change, filler suggestions and resume briefs. `--rank` adds the ranking after every event; `--tick SECS` sets the timer (15 s, like the plugin); `--no-learn` turns off the learned rhythm.

## What works

- **Router** (`crates/engine/src/lib.rs`, `score.rs`): per-tab score, focus cost, interrupt-or-hold, batch release at breaks, escalation, the hold cap, drops for pings that stop mattering, row labels.
- **Duration predictor** (`predict.rs`): median and 80th percentile of the last 32 runs per tab turn, tool and command; persisted in the plugin's kv.
- **Filler** (`filler.rs`): when no tab waits on you and every running turn has a confident estimate, it lists what fits before the first one is due (review a diff, look at a failure, your note).
- **Resume** (`stash.rs`): per tab, what happened since you last looked, from counts only, plus your own note.
- **Haunt plugin** (`plugin/`): labels rows, sends or holds notifications, and adds the ⌘K modes `next`, `why`, `resume` and `note`.
- **Tests**: unit tests per module; golden replays of every trace (`crates/replay/tests/golden.rs`); property tests over thousands of random sessions (`crates/engine/tests/properties.rs`): no ping is held past the cap, the ranking is a stable total order, a repeated tick changes nothing, the tab you're on is never pinged, and no label uses Haunt's status words; an end-to-end test against a real Haunt core (`plugin/e2e/`).

## What's real and what's synthetic

- `traces/recorded-demo-repos.jsonl` is real. Three headless Claude Code sessions ran in parallel on copies of small demo repos, and `record` captured their hook events at the socket Haunt listens on. `replay --import` then scrubbed them: commands are cut to their key, files are numbered, and prompts and paths are dropped. Nobody was at the keyboard, so no tab is ever focused, and the headless billing session's approval went unanswered.
- The `synthetic-*` traces are hand-made, each to show one behaviour, and each says so in its first line.
- `learn` lines in a trace stand for durations the predictor would already know from earlier sessions, and `rhythm` lines for how soon you got to a tab before.
- `traces/synthetic-learned-rhythm.jsonl` is where learning changes a decision: billing's failure is held because you usually get to billing within 4m. `replay --no-learn` on it interrupts instead.
- The replay is the engine on a trace, not a live Haunt; the plugin runs the same engine on live events.

Record your own: `cargo run -p replay --bin record -- /tmp/mmx.sock wire.jsonl`, run agents with `HAUNT=1 HAUNT_SOCK=/tmp/mmx.sock HAUNT_SURFACE_ID=<tab name>` and Haunt's Claude adapter, then `cargo run -p replay -- --import wire.jsonl > trace.jsonl`.

## How it decides

The detail is in [DESIGN.md](DESIGN.md).

Each tab gets `score = blocked + stale + payoff`, in points that are roughly seconds of value:

- **blocked**: a tab waiting on an approval or a question: 120, plus a point a second it has waited, more with open tasks and live subagents behind it.
- **stale**: finished output you haven't looked at: 40 and growing slowly; a failed turn starts at 100. Each edited file adds 15.
- **payoff**: the predicted rest of a blocked tab's turn, a quarter point a second up to 20 minutes. Unblocking 20 minutes of work beats unblocking 30 seconds.

Switching costs nothing for 30 s after a break, 30 while you watch a tab work, and 60 plus half a point a second while you read or answer one, up to 600. A ping interrupts when `score - cost > 60`; otherwise it waits. Held pings re-check every tick, so a block that keeps waiting escalates. A held ping goes out after 10 minutes at the latest, and is dropped if it resolves first (answered in the terminal, the tab moved on, you opened it).

Row labels: `next` (orange, the one tab to go to), `later`, `held`, or `~3m` for a running turn's predicted time left. Haunt's own status words never appear, and on a row that needs you Haunt's status replaces the label.

## In Haunt

```sh
plugin/build.sh
haunt plugin install plugin
```

Capabilities: agent events with `event_details` (commands key the predictor; only the program and up to two plain lowercase words are kept, never flags, paths or values, and prompts are never read), `decorate`, `notifications`, `sidebar` (only to read which tab is selected and the tabs' names; it never arranges the sidebar), `timer` (15 s), `notify_hold` and `title` (the title-bar line; needs a Haunt with the title slot).

Notifications have two modes:

- `hold_haunt = true` (default): Haunt keeps notifying. While MULTIMAXXING holds a ping, it holds Haunt's own "turn finished" notes too, and they come back as Haunt's summary at your break. Haunt never holds an approval, so approvals still reach you at once. If Haunt refuses the hold, for example because another plugin holds and this Haunt allows one holder, MULTIMAXXING doesn't retry for those pings. It logs it once and sends those pings itself at the break. Until Haunt allows several holders, turn this off while using Focus block: while MULTIMAXXING holds, "Start a focus block" does nothing.
- `hold_haunt = false`: MULTIMAXXING sends every ping itself, approvals included. Turn off Haunt's `[notifications] on_done` and `on_permission` so you don't get two.

Settings, under `[plugins."dev.haunt.multimaxxing"]` in config.toml:

| key | default | |
|---|---|---|
| `notify` | true | decide when pings reach you |
| `hold_haunt` | true | see above |
| `labels` | true | `next`, `held`, `later`, `~3m` on rows |
| `reasons` | true | the short reason after the row's meta line |
| `bars` | false | a running turn's predicted progress as the row's bar |
| `filler` | true | suggestions in ⌘K `next` while you wait |
| `resume` | true | ⌘K `resume` |
| `title` | true | one line in the title bar: `next: web · finished 4m ago`, or `2 held · out at your break` |
| `threshold` | 60 | how far a ping must outweigh your focus to interrupt |
| `max_hold_min` | 10 | the longest a ping waits |
| `min_gap_min` | 3 | the shortest wait worth filling |
| `learn` | true | learn how soon you get to each tab, and interrupt less for ones you reach fast |
| `unverified` | false | also follow events without the tab's nonce |

**Go to next agent** (⌘⇧N, palette command `next_agent`) switches to the tab the title names: the engine's top pick, the first row of ⌘K `next`. It needs Haunt's `sidebar.focus` (api 0.5).

⌘K modes: `next` (tabs worth going to with their reasons, then filler; Enter jumps to the tab), `why` (the last 50 decisions and their reasons), `resume` (what each tab did since you last looked), `note` (type a note for the current tab; it shows in `resume` and as filler).

Notes and per-tab state last as long as the tab: Haunt gives tabs new ids each launch. Durations are kept by tab name, so they carry over.

## End to end

`plugin/e2e/multimaxxing.rs` runs against a real Haunt core: copy it into a Haunt checkout's `cli/tests/` and run `cargo test -p haunt-cli --features plugins --test multimaxxing`, with `HAUNT_PLUGINS_DIR` pointing at the folder holding this repo.

## Layout

```
crates/engine   the scheduler: model, predict, score, filler, stash
crates/replay   the trace CLI and the golden test
plugin/         the Haunt component: a thin wrapper around the engine
traces/         hand-made traces and their expected replays
```

MIT, see `LICENSE`.
