# How MULTIMAXXING decides

The engine (`crates/engine`) is a pure function of the events it has seen and the time it is given. It never reads a clock, so the same trace always gives the same decisions, on any machine. Scores are integers. Maps are ordered.

## State per tab

Each tab has a phase: *fresh* (no turn yet), *running*, *blocked* (an approval or a question), or *done* (finished or failed). It also tracks when that phase began, when the turn began, its open tool calls, open tasks, live subagents, files edited since you last looked, and whether you've looked at it since it finished. The focused tab comes from Haunt's selected row. A prompt also counts as focus, since you can only type into the tab in front of you.

## Score

`score = blocked + stale + payoff`, in points that are roughly seconds of value (`score.rs`):

| part | when | value |
|---|---|---|
| blocked | waiting on you | `120 + waited_s × (2 + open_tasks + subagents) / 2` |
| stale | finished, unread | `40 + waited_s / 10`; failed: `100 + waited_s / 5`; plus 15 per edited file |
| payoff | waiting on you | `min(predicted rest of turn, 1200 s) / 4` |

A running tab scores 0. A finished tab you've looked at scores 0. Waiting grows a block's cost by a point a second, and faster when tasks or subagents are stuck behind it. Payoff is where the predictor matters: an approval that lets 20 minutes of work run outranks one that frees 30 seconds.

## Switch cost

Interrupting you costs depending on what you're doing (`Focus` in `score.rs`):

- **At a break** (within 30 s of sending a prompt, switching tabs, or the tab you watch stopping): 0.
- **Watching** a tab work: 30.
- **Reading or answering** a tab (it's done, blocked or fresh): `60 + seconds_since_break / 2`, capped at 600. Six minutes in, that's 240.
- **No tab focused**: 0.

With `learn` on, a non-zero cost is scaled by how soon you usually get to the tab the ping is for (see Rhythm): `cost * 8m / usual`, clamped to 0.5–2×. The reason line says so: `you usually get to billing within 4m (cost ×2.0)`.

## Interruption policy

When a tab raises a ping (it blocks, finishes or fails while you're elsewhere):

1. If `score - cost > threshold` (60), notify now. Otherwise hold it.
2. On every tick (15 s) and every event, each held ping is checked in this order:
   - **Dropped** if it no longer applies: the approval was answered, the tab moved on, the tab was closed, or you opened it.
   - **Released alone** once it has been held for the cap (10 min). This is the starvation bound, and the property tests check it.
   - **Escalated** (sent now) once `score - cost` crosses the threshold. A block's score grows twice as fast as your reading cost, so a long-waiting approval gets through.
3. At a break, everything still held goes out as one notification, best first.

Every decision carries its numbers: `held: lint waits 0s on Bash (npx eslint) approval; you're 3m into docs (score 120 <= cost 150 + 60)`.

## Labels

The best tab to go to, not the one in focus and not held, is `next` (the one orange label). Other tabs that score above 0 are `later`. Held pings are `held`. A running tab with a confident estimate shows `~Nm` left. Labels are recomputed after each event and tick, and only changes are sent to Haunt.

## Predictor

`predict.rs` keeps the last 32 durations per key and reads them back as the median and the 80th percentile (nearest rank). Keys:

- `turn@<tab name>`: from prompt to stop for that tab. Names outlive Haunt's per-launch tab ids.
- `turn`: every tab's turns, the fallback until a tab has 3 of its own.
- `<tool>` and `<tool>:<command key>`: tool calls from start to done (`Bash:cargo test`). The command key is the program plus at most two plain lowercase words, never a flag, path, number or value.

The time left for something `e` long that is `elapsed` in: `p50 - elapsed` while under the median, then `p80 - elapsed`. Past the 80th percentile it returns nothing. Labels and the filler need at least 3 samples; payoff uses any estimate. At most 128 keys are kept, and the least-sampled go first. The plugin stores the predictor in kv after each finished call or turn.

## Rhythm

`rhythm.rs` learns how long you take to get to each tab after it needs you, keyed by tab name (usually the repo). A sample is the time from a tab finishing or blocking to you opening it, or to you answering its approval or question from elsewhere. It's an EWMA (`(old * 3 + new) / 4`, first sample as is), each sample capped at an hour. It counts after 3 samples. At most 128 keys are kept, and the least-sampled go first.

A tab you usually reach within 4 minutes doubles the cost of interrupting you for it: you'll be there soon anyway, so the ping waits for your break. One you leave for 16 minutes halves it. There's no model and no network, and the same events always give the same rhythm. The plugin keeps it in kv (`rhythm`) whenever it changes. `learn = false` stops learning and scaling, and keeps what was learned.

## Filler

When no tab is blocked and every running tab has a confident time left, the gap is the soonest of them. If the gap is at least 3 minutes, the filler offers a review of a finished tab's diff (45 s per file), a look at an unread failure (2 min) or your note on a tab (2 min). Items that fit the gap come first, then items by score.

## Resume

Per tab, counts since you last left it: turns finished and failed, tasks completed, tool failures by command key, approvals asked, files edited, the current wait and your note. The resume brief is built from these counts alone, with no text from the agent.

## Haunt's notification hold

With `hold_haunt` on, the plugin starts Haunt's hold while it holds any ping, and releases it when none are left. Haunt then shows the "turn finished" notes it held as one summary. If Haunt refuses the hold, the plugin doesn't retry for the pings held at that moment, logs it once, and sends those pings itself, as with `hold_haunt` off. On a host with several holders, a release drops only this plugin's hold.

## Title bar

`headline` gives one line of at most 48 characters. When there is a top pick (`top_pick`, the first of `next`: the tab "Go to next agent" opens), it always starts with it, `next: <tab> · <reason>` (`next: web · finished 4m ago`), or `next: <tab> · <n> held` while pings wait for your break; a long tab name is cut before the rest. So the line names the pick even when it's a needs-you row whose label Haunt's status hides. Without a pick: how many pings wait (`2 held · out at your break`), else the gap and the first thing that fits in it. The plugin sets it when it changes. If Haunt doesn't take it (another plugin owns the slot, or the rate limit), it tries again on the next event or tick.
