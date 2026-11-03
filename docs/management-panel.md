# The management panel

The management panel is a table under a tab's terminal listing that tab's Claude Code subagents, **Running**, **Next up** and **Done**, one row each. Running and Done need no setup: every such row comes from data Claude Code already writes. A **manager** that knows more (which task each worker holds, what is queued next, when each is due to land, who reviews it) adds that by writing a **feed**: one JSON file per Claude session.

This document is the contract between Giverny and anything that writes rows. Giverny ships no manager. A manager is any program that writes the files below; a Claude Code plugin with a skill and a command of its own is one example. Giverny only reads them, and writes nothing in the feed directory. The reader is `crates/claude/src/feed.rs`.

The pane is off by default; turn it on with `claude.management_panel` (Settings → Management panel).

**The pane shows only when subagents are involved**: in a tab whose session has a real subagent, running or done this session, or whose session is a manager (see **The manager mark**), so its Next up shows before its first spawn. A feed never opens it by itself, and every row drawn is a real agent, save a manager's Next up rows (see **Merge with the live rows**).

**Settings → Management panel** holds the pane's switch and one switch per column, in the order the pane draws them (`[management_panel.columns]`: `stage`, `id`, `title`, `elapsed`, `eta`, `now`, `tokens`, all on by default). A column switched off takes no width; the rest close up and TASK takes the room. ETA also takes no width while no row has an estimate, as in a session with no feed. Every one applies as it is changed. Which **Done** rows show has no row there but is honoured when set by hand in `config.toml` (`management_panel.done_rows`: `"all"` by default, `"hide"`, or `"last"` for the newest `management_panel.done_last` of them).

## What a row shows

- **STAGE**: `Running`, `NextUp` or `Done`, and the whole row is tinted by it.
- **TASK**: a feed row's key and title. A worker no feed row names shows its name and description; one with no name shows its model (`opus`, `sonnet`, `haiku`) in place of the name. An agent another agent spawned hangs under that agent's row, indented a step per level, and goes where that row goes. A `▾` at the head of the spawner's title folds them away (`▸ … +3, 1 running`), kept per tab while Giverny runs.
- **ELAPSED**: a stopwatch while it runs, the time it took once Done. Empty on a Next up row.
- **ETA**: a Running row's time left against its estimate (`~20m`, `-~12m` once over it); a Next up row's estimate (`~1h3m`); a Done row's landing against its estimate (`(+5m)`). A Running feed row with no estimate reads a dim `no ETA`.
- **NOW**: what it is doing, its last tool call (`Bash: cargo test`); `5h limit → 13:00` while the account is out of a usage limit; `stopped: no network` while it stands on an API error; `paused since 12:58` for a row its manager paused; what a Next up row waits for; a Done row's `landing`, else how it ended (`done`, `failed`, `killed`, `stopped`).
- **TOKENS**: Claude Code's own count, as its agents view shows it.

A row whose worker is the same as the row directly above it draws its repeated cells (ELAPSED, ETA, NOW, TOKENS) as `"`. A row's hover has its whole title and why it is where it is. A drag selects the pane's text as the terminal does, and copies it when it ends. A look at a tab that wore the unseen-finish tick breathes the frames of the rows that landed since the last look. A feed's `footer` is drawn dim under the rows.

Rows stay until the tab's conversation is cleared (`/clear`), a fresh `claude` starts in it, a manager clears them (see **Clearing Done rows**), or the tab is closed. They are saved, so a Giverny restart keeps the Done rows; they show again once that session is back up.

## What a click on a row opens

A click on a row opens Giverny's **worker page** in the tab, in the terminal's place. Claude Code cannot open a worker's view from outside, and keeps none of a worker whose turn has ended, so Giverny draws the worker as Claude Code's own subagent view does, from its transcript (`agent-<id>.jsonl`):

- It fills the terminal's area, in the terminal's own grid, font and colours, on a background tinted toward the theme's magenta. The worker's prompt is on its grey band, and each tool call and reply under it: `●` green once its result is back, red on an error, grey while it runs, and a tool's output folded to its first three lines.
- Claude Code's prompt box follows, its rules in the worker's colour and the task's name on the top one. A line typed there goes to the tab's manager, which passes it on (see **Lines from the worker page**). On a row with no worker to send to, or with `management_panel.prompt_box` off, the box is dimmed and `read-only`. Nothing typed on the page reaches the shell under it.
- A header over the page names the task (the feed's key, else the worker's description up to its first `:`) and carries the row's facts: elapsed against the estimate, NOW and tokens. A Done row with a `review` adds its **Review line** (see below). Its `×` hides the header for that page.
- A **Running** worker's page follows its transcript as it grows. A **Done** one's opens at its end, on the worker's report, and so does a Running row's whose worker has finished. A worker with no transcript yet gets a page saying so.
- A **Next up** row's page shows its `brief`, drawn as the prompt its worker will be given, from the top. With no brief it shows the row's key, title and note.
- A Running row with no worker and an `open` command runs that command in a new tab beside this one. A Done row whose `open` resumes (`claude --resume <id>`) a conversation running in a Giverny tab goes to that tab instead of its page. A command that resumes a conversation already running outside Giverny is not run: two claudes on one transcript would interleave it.
- The row on the page is lit in the pane.
- The "back" button under the prompt box, Esc, or a second click on the row goes back to the terminal.
- The wheel, ↑/↓ and PgUp/PgDn scroll it, and a drag selects and copies. Space and Home/End scroll too while the box takes no line; while it does, they edit it.

## Where the live rows come from

- **Running** rows come from Claude Code's live worker list, through the relay below.
- **Done** rows come from the workers' own transcripts (`projects/<cwd>/<session>/subagents/agent-<id>.jsonl`) and from the `<task-notification>` Claude Code queues on the parent's transcript when a worker stops. A worker messaged again after it finished is Running again until its next notification.

### The relay

With `claude.management_panel` on, Giverny installs one key into each account's `settings.json`:

```json
"subagentStatusLine": { "type": "command", "command": "<giverny> relay --subagent-line" }
```

Claude Code runs that command at least every five seconds while a session has live workers. It passes the worker list on stdin: `session_id` and `tasks[]`. The relay forwards that list to the app, tagged with the tab it ran in (`GIVERNY_TAB_ID`, which reaches this command).

**The pane stands in for Claude Code's own subagent panel** (the strip under its prompt) while it is on. Claude Code reads each line the command prints as one row's decoration, `{"id":…,"content":…}`, and drops a row whose decoration is `""`; with every row dropped it draws no strip at all. So with the setting on, the relay prints an empty decoration for every worker, and the workers show once, in the pane. With it off, the relay prints nothing and Claude Code draws its strip as it would without the command. The way into a worker's own Claude Code view, which goes through that strip, comes back in a later change.

- **Outside a Giverny tab** (no `GIVERNY_TAB_ID`), the relay forwards nothing and prints nothing, so Claude Code's strip is drawn as usual.
- **In a session that is not the tab's own**, the relay does the same. A `claude` started inside the tab's claude, or under a `tmux` the tab launched, inherits `GIVERNY_TAB_ID` too. The tab also exports the app's process id (`GIVERNY_PID`), and the relay walks up from itself: the tab's own claude reaches the app with no other `claude` on the way. A nested one passes a second `claude`, and a multiplexer's sessions never reach the app at all, so their workers stay out of the tab's pane. The plain hook relay applies the same check, so a nested session does not move the tab's state or its resume target either. Where the walk sees no `claude` at all (a relay run on the Windows side for a session inside WSL), it trusts `GIVERNY_TAB_ID` as before.
- **Only in an account that opted in.** Giverny writes the key only into an account whose `settings.json` already holds Giverny's hooks: installing them is the consent, as it is for the live-usage statusline. Installing the hooks brings the key with them. An account without them is never written to.
- **With the setting off**, Giverny removes the key again.
- **Uninstalling the hooks** removes the key too, since it is the same relay.
- **An account whose `subagentStatusLine` is someone else's** is left alone: the installer never replaces a command it did not write. On that account the pane gets no live rows.
- **Project settings override user settings.** A project that sets its own `subagentStatusLine` shadows Giverny's. To keep both, that command must pass through: inside a Giverny tab (`GIVERNY_TAB_ID` set), pipe the stdin it received, byte for byte, into `giverny relay --subagent-line`.

This is the only key Giverny writes into Claude Code's settings for the pane.

----

## The feed

### Where the file goes

```
$GIVERNY_FEED_DIR/<claude session id>.json
```

- **`GIVERNY_FEED_DIR`** is exported by Giverny into every tab (and listed in `WSLENV` with `/p`, so a tab inside WSL gets it as a WSL path). A writer running inside a Giverny tab, or in a hook or subagent of a Claude session running there, just reads it. When it is unset or empty, the writer uses the default: `<config dir>/giverny/feeds`, where `<config dir>` is the platform's config directory: `~/.config` on Linux (`$XDG_CONFIG_HOME` if set), `~/Library/Application Support` on macOS, `%APPDATA%` on Windows. The writer creates the directory if it does not exist.
- **`<claude session id>`** is the session id of the *parent* Claude Code session whose subagents the rows describe: the `session_id` Claude Code puts on every hook payload and on the `subagentStatusLine` input. It is a file name, so use it verbatim.
- One file per session. Giverny never writes, renames or deletes anything in this directory; stale files are the writer's to clean up (or leave: an unmatched file costs nothing).

### Writing it: atomically

Write the whole file to a temporary name **in the same directory**, then `rename` it over `<session>.json`. The temporary name must **not** end in `.json` (use `<session>.json.tmp` or `.<session>.<pid>.tmp`), because Giverny scans `*.json` when it searches by alias.

```python
tmp = os.path.join(d, f"{sid}.json.tmp")
with open(tmp, "w") as f:
    json.dump(feed, f)
os.replace(tmp, os.path.join(d, f"{sid}.json"))
```

Giverny checks the file's mtime and size about once a second and re-reads it only when either changed. A file that fails to parse (a torn write from a writer that skipped the rename) is ignored and the last good read stays on screen, so the worst a non-atomic writer gets is a flicker of staleness. But do the rename.

Rewrite the file whenever anything in it changes. Do **not** rewrite it just to move the clocks: Giverny ticks ELAPSED and ETA itself every second.

### Schema (version 1)

```json
{
  "version": 1,
  "session": "5c1e…",
  "aliases": ["0a9f…", "77d2…"],
  "rows": [
    {
      "key": "acme#158",
      "stage": "running",
      "title": "FEATURE: management panel",
      "agent_id": "a93f0c1d2e",
      "started": "2026-09-23T10:00:00Z",
      "eta_s": 2400,
      "landing": "Review — alex",
      "brief": "/home/me/briefs/acme-158.md",
      "note": "waiting on the relay"
    },
    { "key": "acme#159", "stage": "planned", "title": "BUG: header", "eta_s": 900, "after": "acme#158" },
    { "agent_id": "b71e44c0aa", "eta_s": 600 }
  ],
  "footer": { "text": "session 3 · total: 1.2M" }
}
```

#### Top level

| Field | Type | Required | Meaning |
|---|---|---|---|
| `version` | integer | no (absent = `1`) | The format version; see **Versioning**. |
| `session` | string | recommended | The Claude session id this feed is for: normally the file's own name without `.json`. |
| `aliases` | array of strings | no | Earlier session ids of the same conversation. Claude Code gives a conversation a new id on some resumes and restarts; a tab still holding an old id finds the feed through this list. List every id the conversation has had. |
| `rows` | array of row objects | no (absent = no rows) | The table, in the order the writer wants it drawn within each stage. |
| `footer` | object `{"text": string}`, or a bare string | no | One line drawn under the table, verbatim (a session count, a total). |

#### Row

| Field | Type | Required | Meaning |
|---|---|---|---|
| `key` | string | yes, unless `agent_id` is given | What the row *is*: a task id (`acme#158`), a lane name, anything short. Drawn in TASK, and what the row is known by from one frame to the next. A row with no `key` names no task: it is drawn as its worker would be with no feed (its name or model, its description), with what else the row gives. |
| `stage` | string | yes, unless `agent_id` is given | `running`, `planned` or `done` (case-insensitive; `live`, `working`, `in progress`, `queued`, `next`, `finished`, `completed`, `landed` are accepted as synonyms). Decides the section. A row whose stage Giverny does not recognise is dropped. A row with an `agent_id` and no `stage` follows its worker: Running while it runs, Done once it has finished, and no row while Claude Code lists no such worker. That is how a writer gives any worker an estimate: `{"agent_id": "<id>", "eta_s": 600}`. |
| `title` | string | no | The row's text: the task's title. Without it the pane shows the worker's own description. |
| `agent_id` | string | no | The Claude Code subagent id holding this row: **the join key** with the live rows (see **Merge**). It is Claude Code's id for the worker: `tasks[].id` in the `subagentStatusLine` input, and the `<id>` in `<config>/projects/<cwd>/<session>/subagents/agent-<id>.jsonl`. Several rows may carry the same `agent_id` (one worker holding several tasks). |
| `started` | timestamp | no | When this row's work started. RFC 3339 string (`2026-09-23T10:00:00Z`, any offset) or integer epoch **milliseconds**. |
| `ended` | timestamp | no | When a Done row landed. Same formats. |
| `eta_s` | non-negative integer | no | The row's estimated total duration, in seconds, from `started`. On Running and Planned rows it fills ETA; on Done rows it is what the landing is measured against (see **The Done row's ETA cell**). Giverny holds it through a usage limit itself. |
| `left_s`, `left_at` | non-negative integer, timestamp | no | Running rows: the latest figure for the time left, in seconds, and when it was given. ETA is then `left_s` less the work since `left_at` (its wall time less usage-limit holds and API-error spans), in place of `eta_s` less the work since the start. A writer that re-estimates sends these and leaves `eta_s` as it was. |
| `eta_delta_s` | integer (may be negative) | no | Done rows only: how late (positive) or early (negative) the row landed against its estimate, in seconds. Send it only when you know better than Giverny's own arithmetic. |
| `spawned` | timestamp | no | The row's true start, when `started` has been moved on by the spans it was paused. |
| `paused_s` | non-negative integer | no | Seconds the row has spent paused, an open pause counted up to when the file was written. `started` is already moved on by them. |
| `paused_since` | timestamp | no | Running rows only: the row is paused now, since this instant. Its ELAPSED and ETA hold at the values they had when the file was written (its mtime), `started` being taken as true as of then, and NOW reads `paused since 12:58`. |
| `landing` | string | no | Where the row lands or landed (`Review — alex`, `Done`, `Blocked`). A Done row's NOW, verbatim. |
| `tokens` | non-negative integer | no | Tokens spent. A live worker's own count wins; this is the fallback (a Done worker Claude Code has forgotten). A Planned row has none. |
| `brief` | string (absolute path) | no | Planned rows: a file shown read-only when the row is clicked, as the prompt on its worker page. |
| `open` | string (shell command) | no | Run in a new tab when a Running row with no worker is clicked, e.g. `claude --resume <id>`. On a Done row whose command resumes a conversation running in a Giverny tab, a click goes to that tab. See **What a click on a row opens**. |
| `note` | string | no | Free text: in the row's hover, and on the page of a Planned row with no `brief`. |
| `review` | string | no | Done rows: the one line a person has to read before the row counts. See **The Review line**. |
| `after` | string | no | Planned rows: the key of the task this one waits for. NOW reads `after <key>` until that row is Done. |
| `held` | string | no | Planned rows: why the manager holds it. NOW reads `held: <why>`, in full in the page's header. |

Timestamps and numbers are forgiving: a number sent as a numeric string (`"2400"`) is read, a fractional number is truncated, a negative `eta_s` or `tokens` reads as absent. `null` is the same as leaving the field out. Fields Giverny does not know are ignored, so a writer may keep its own state in the rows.

### Merge with the live rows

Claude Code reports each live subagent itself (id, name, status, start time, tokens, what it is doing now), and Giverny keeps the ones that finished until the rows are cleared (see above). Those are the **live rows**. The feed's rows are joined to them on **`agent_id` = the live row's subagent id**:

1. **Every feed row a worker stands behind is drawn**, in feed order, in the stage the feed gave it. The feed's stage wins over the live one: a worker that holds two tasks may have one Done and one still Running. **A feed row no live row carries is not drawn**: a feed row annotates a worker, and alone it would be blank where the worker's cells go. The one exception is a **manager** session (see **The manager mark**): its Next up rows are drawn with no worker, including Running rows with none (rule 7). What a Next up row waits for (rule 6) is still read off every row, drawn or not.
2. **A feed row whose `agent_id` matches a live row carries both.** The feed supplies `key`, `title`, `eta_s`, `landing`, `brief`, `open`; the live row supplies the clock's start, tokens and the current activity, the first two exactly as Claude Code's own agents view has them, so the two agree. The clock counts from the worker's `startTime`, not from `started`, which a writer stamps a little before the spawn. `started` counts only for a row that began more than five minutes after its worker did (a later task given to the same worker), or more than five minutes before (a fresh worker taking a task over). A row's pauses move the worker's start on by as much as they moved `started` on (`started − spawned`, else `paused_s`).
3. **A feed row with no `agent_id`, or one Claude Code no longer lists, is joined by its `key` instead**: to the live row whose spawn `description` names the key as a whole word (`Work demo#82 open direct` names `demo#82`, never `demo#820`). A description naming several keys is one worker holding each of them. A Running row takes a worker still running where there is one, any other row a finished one. A Running row whose named worker has finished takes a worker still running whose description names its key, spawned after it, if there is one: the task was handed to a fresh worker. So a writer that could not know the worker's id when it wrote the row still gets one row with the worker's tokens and activity. An agent another agent spawned is a row's worker only by `agent_id`: its description may well name its spawner's task.
4. **A live row no feed row carries is drawn on its own**, unless it is still running and its description names a feed row's key, in which case it is that row's worker and is never drawn a second time. It is drawn as Running or Done by its own state, after that stage's feed rows. So a feed that describes only some workers leaves the rest visible. A finished one that names a key no row took it for was replaced on that task: the row moved to a fresh worker (by `agent_id`, or by rule 3). It is a Done row of its own, with its own clock and tokens, so the task's row counts only the worker on it now and the first worker's time and tokens stay in sight.
5. **Sections are drawn Running, then Next up, then Done.** Order within Running is feed order, then unmatched live rows in Claude Code's order. Done is newest landed first, a worker's rows kept together, undated rows last. **Next up reads in the order its rows will start, the next on top**: first a Running row with no worker yet (it waits only for one); then rows with nothing recorded to wait for; then rows `after` another task, below that task when it is in Next up too, chained; held and paused rows last, with whatever waits on them. Rows that wait alike keep feed order.
6. **A Next up row says what it waits for**, the first that applies: `held: <why>` (its `held`); `after <key>` (its `after` row, while that is not Done; a key the feed lacks is no wait), or the task its worker is busy with (a Planned row whose `agent_id` names a worker on another Running row). A row with none of these waits on nothing, and its NOW is empty.
7. **A task nobody works on is Next up, not Running.** A Running row with no worker at all (a manager stamps `started` a moment before it spawns), or one whose worker has finished and that its manager `held` or paused (`paused_since`), is drawn under Next up with a Next up row's cells, its NOW saying why: `no worker yet`, `held: <why>`, `paused since 10:42`. The file still says Running and its clock keeps the pause rules; a worker picking it up draws it Running again. A Running row whose worker has finished and that nobody has landed, held or paused stays Running, its NOW saying how and when the worker's turn ended (`turn ended 11:41`, `failed 11:41`). Its clock runs on: landing is still time on the task.
8. **An agent a worker spawned hangs under that worker's row.** Claude Code lists every agent of the session flat; only the agent's `agent-<id>.meta.json` names its spawner (`parentAgentId`). Such an agent is drawn right under its spawner's row, its title indented a step per level (`└ Triage group A`), with its own stage, clock and tokens, and goes wherever that row goes. A worker holding several rows takes it under the one whose task was under way when it spawned it. `done_rows`' `last N` counts only Done rows of their own; what hung under a Done row it drops goes too, but an agent still running there is drawn on its own.

`aliases` are not part of the row merge; they only decide which file belongs to a tab. Giverny looks for `<current session id>.json` first, and if there is none, for the most recently modified `*.json` whose `session` or `aliases` contains the current id. If none does, it tries the tab's earlier ids the same way, newest first, so a session Claude Code just re-id'd keeps its rows. A writer that keeps one conversation in one file across re-ids names the new id in `session` and moves the old one to `aliases`.

### Usage limits, pauses and stopped workers

While the tab's account is out of a usage limit (Giverny's own meters show a window at 100% with a reset still to come, or the tab stopped on a limit message), nothing runs, so the pane holds every **Running** row's clock: ELAPSED stops, ETA stays where it was instead of counting down past zero, and NOW reads `5h limit → 13:00` (`7d limit → Mon 08:00` for a reset on another day, `limit` when nothing says when). Once the limit clears both clocks carry on from where they stopped: the span is taken off for as long as the pane remembers it (while Giverny runs). Planned ETAs are durations and do not move. A Done row is measured history and is left as it landed. A writer does not need to do anything for this. A writer that pauses rows itself (moving `started` on, with `paused_s`/`paused_since`) may: a row carrying either field is taken to have its stops in `started` already, and the pane's own hold is not applied to it again.

A worker that stops on an API error (no network, a usage limit, an expired login, a timeout) writes Claude Code's `<synthetic>` `isApiErrorMessage` line as its last turn. From that line on its **Running** row stands still: ELAPSED and ETA hold, and NOW says why (`stopped: no network`, `stopped: usage limit`, `stopped: login expired`, `stopped: timed out`, `stopped: API error`). When the worker is continued (Claude Code lists it running again, or it writes its next real turn), the clocks carry on from where they stood, with the stopped span taken off for good.

### The Done row's ETA cell

A Done row has nothing left to estimate, so its ETA cell says how the landing compared with the estimate: **`(+5m)`** landed five minutes late, **`(-1h20m)`** an hour twenty early, **`(±0m)`** on the minute. It is blank when there is no estimate.

- If the row has **`eta_delta_s`**, that is the value.
- Otherwise it is **`work − eta_s`**, where the work is the row's clock (the worker's start, else `started`, to `ended`, else the worker's end) less the spans its worker stood on an API error. If the end, the estimate or both start times are missing, the cell is blank.
- The header over the row's page says which figure it was: `(+15m) vs estimate 20m`.

Durations in the pane are written the way the ETA column writes them: whole minutes (rounded), units that are zero left out (`5m`, `1h3m`, `1h`, `1d3h12m`), with `~` in front of an estimate and none on a measured span.

### The Review line

When a **Done** row is clicked, the header over its worker page can carry one line saying what a person has to look at before the work counts: the row's `review` field, shown as written, after `Review`. A good line says who, what and where (`alex — the pane's colours — branch pane-2, cargo run`). A row without one opens with no Review line. Giverny does not look the line up anywhere else, such as an issue tracker.

## The manager mark

A session is a **manager** when its manager marks it. Writing rows does not make one: any agent can write a feed row. The mark decides two things: the pane opens for a manager before its first spawn, so its Next up shows, and a manager's Next up rows are drawn with no worker behind them (rule 1 of **Merge**). A session that is no manager gets a pane only from real subagents.

The mark is a file beside the feeds:

```
$GIVERNY_FEED_DIR/managers/<claude session id>.json
$GIVERNY_FEED_DIR/managers/roots/<conversation root>.json
```

- Giverny looks only at whether a file exists; what it holds is the writer's. A manager writes `{"session": "<id>", "root": "<root>"}` in both.
- **The session file** is looked for under the tab's current session id, then under its earlier ids.
- **The root file** carries the mark across ids. Claude Code gives a conversation a new session id on a resume or a compaction, and copies its records forward, so the `uuid` of the transcript's first record stays the same where the id does not. That uuid is the conversation's root. When no id is marked, Giverny reads the root from the session's transcript once and looks for `managers/roots/<root>.json`. `/clear` starts a new conversation, with a new root: it is no manager until it is marked again.
- **When to write it.** When the manager starts managing in a session: before it writes its first row, so the pane opens with its Next up. Marking a subagent's session is marking its parent: a subagent's hooks carry the parent's `session_id`.
- **To unmark**, delete the files.

## Clearing Done rows

A manager can clear the pane's Done rows in its tab, keeping what runs, by piping one message into the relay from inside the tab (a hook, or a command in the session's Bash tool):

```sh
echo '{"hook_event_name":"GivernyClearDone","session_id":"<id>","at_ms":1790000000000}' | <giverny> relay
```

- `<giverny>` is the binary the account's hooks run (`giverny` on `PATH` in a Giverny install).
- The relay forwards it with the tab's id, as it forwards any hook, and only from the tab's own session (see **The relay**). Outside a Giverny tab it does nothing.
- `at_ms` is when the clear was asked for, in epoch milliseconds; absent, it is when the app reads the message. The tab's finished workers leave the pane and do not come back from the disk, unless they run again. The feed's Done rows that landed by `at_ms` (or say no `ended`) are hidden too. The feed file is not touched.
- A closed app reads the message at its next start, from the spool.

## Session names (the manager's, not read by Giverny)

A manager may name the sessions it runs. Its names live beside the feeds, in files of the manager's own, and nothing in Giverny reads them yet:

- `$GIVERNY_FEED_DIR/names/<claude session id>.json` and `$GIVERNY_FEED_DIR/names/roots/<conversation root>.json`, each `{"session", "name", "room", "root"}`, found across ids by the root as the manager mark is.
- `$GIVERNY_FEED_DIR/names/on` exists while naming is on.

They are listed here so that a Giverny reader (a name beside the model on the status line, a tab titled after its session) can be added later against a file whose shape is settled.

## Lines from the worker page

Claude Code has no way to type to a subagent from outside: the only way in is its parent's `SendMessage`. So a line typed in the worker page's prompt box goes to the tab's own Claude Code, the manager, as one plain line naming the worker, and the manager passes it on. These are the lines, for a manager to understand:

- `Send this to <key>'s worker (agent <agent id>): <text>`, from a Running or Done row's page. A running worker takes the message at its next tool call; one whose turn has ended is woken with it. With no key, `Send this to the worker (agent <agent id>): <text>`.
- `Start <key> now, adding this to its brief: <text>`, from a Next up row's page.

`<text>` is what the person typed, line breaks kept (Shift+Enter makes one), and it goes only on their Enter. The box is on by default wherever the pane is; `management_panel.prompt_box = false` makes it read-only.

**How it is sent.** The line is pasted at the manager's prompt and submitted, so the manager reads it as the person's own typing, not as another session's message. Nothing is read off the screen.

- **When.** Only while the hooks say Claude Code runs in the tab and the tab's terminal has bracketed paste on. The session's `sessions/<pid>.json` is read at send time: while its `status` is `waiting` (a permission, a question or a plan approval is up), the Enter would answer it, so the box says "Not sent: the manager is waiting on a question in its tab" and keeps the text. With no hooks reporting on the session, it refuses too.
- **What.** Control characters are stripped: ESC, the C0 controls but tab and line break, DEL and C1. A tab becomes four spaces. The text is split into bracketed pastes of at most 800 UTF-16 units and 2 line breaks each, all in one write, then Enter. Claude Code folds a longer paste into `[Pasted text #N]`, which reaches the model as pasted material rather than typing.
- **Confirm.** The line counts as delivered once a `UserPromptSubmit` from that session carries it (compared word for word, paste tags off). Until then the box says "sending…"; with none within five seconds it says "not sent" and keeps the text. While the session compacts (`PreCompact` seen, the compaction's `SessionStart` not yet), Claude Code holds the line until it ends, so the box says "queued behind compaction" and waits.

The box writes nothing into Claude Code's settings; it uses the hooks the relay already installs.

## Versioning

- `version` is bumped **only for an incompatible change**: a field whose meaning or type changes, or a new field a reader must understand to draw the table correctly.
- **Adding** a field is not a bump. Readers ignore fields they do not know, so a writer may send more than this page lists.
- A feed whose `version` is **greater** than the reader's is ignored entirely (the pane shows only the live rows) rather than half-understood. A missing `version` is read as `1`.
- The only things that make Giverny ignore a whole file are: it is not JSON, it is not a JSON object, or its `version` is too new. Anything smaller costs that field (a wrong type reads as absent) or that row (an unknown `stage`, neither `key` nor `agent_id`, or neither `stage` nor `agent_id`).

## Checklist for a writer

- [ ] Path: `${GIVERNY_FEED_DIR:-<config>/giverny/feeds}/<parent session id>.json`, directory created if missing.
- [ ] Write to a non-`.json` temp name in the same directory, then rename.
- [ ] `"version": 1`, `"session"`, and every earlier id in `"aliases"`.
- [ ] Each row has `stage` and `key`; `agent_id` wherever a worker holds it, or the task's key in the worker's spawn description.
- [ ] Timestamps RFC 3339 or epoch ms; `eta_s` in seconds from `started`.
- [ ] Done rows: `ended` (and `eta_delta_s` only when you know better).
- [ ] Rewrite on change, not on every tick.
- [ ] A manager: mark the session (`managers/<session>.json`, and `managers/roots/<root>.json`) before its first row.
