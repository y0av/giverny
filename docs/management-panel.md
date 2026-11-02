# The management panel

The management panel is a table under a tab's terminal listing that tab's Claude Code subagents, **Running** and **Done**, one row each. It needs no setup: every row comes from data Claude Code already writes. It is off by default; turn it on with `claude.management_panel` (Settings → Management panel).

**The pane shows only when subagents are involved**: in a tab whose session has a real subagent, running or done this session. A tab with none shows nothing.

**Settings → Management panel** holds the pane's switch and one switch per column, in the order the pane draws them (`[management_panel.columns]`: `stage`, `id`, `title`, `elapsed`, `now`, `tokens`, all on by default). A column switched off takes no width; the rest close up and TASK takes the room. Every one applies as it is changed. Which **Done** rows show has no row there but is honoured when set by hand in `config.toml` (`management_panel.done_rows`: `"all"` by default, `"hide"`, or `"last"` for the newest `management_panel.done_last` of them).

## What a row shows

- **STAGE**: `Running` or `Done`, and the whole row is tinted by it.
- **TASK**: the agent's name and its description; an agent with no name shows its model (`opus`, `sonnet`, `haiku`) in place of the name. An agent another agent spawned hangs under that agent's row, indented a step per level, and goes where that row goes. A `▾` at the head of the spawner's title folds them away (`▸ … +3, 1 running`), kept per tab while Giverny runs.
- **ELAPSED**: a stopwatch while it runs, the time it took once Done. A span the worker stood stopped on an API error is off its clock. While the tab's account is out of a usage limit the clock stops, and starts again from where it stood once the window reopens.
- **NOW**: what it is doing, its last tool call (`Bash: cargo test`); `5h limit → 13:00` while the account is out; `stopped: no network` while it stands on an API error; how it ended once Done (`done`, `failed`, `killed`, `stopped`).
- **TOKENS**: Claude Code's own count, as its agents view shows it.

A row's hover has its whole title and why it is where it is. A drag selects the pane's text as the terminal does, and copies it when it ends. A look at a tab that wore the unseen-finish tick breathes the frames of the rows that landed since the last look.

Rows stay until the tab's conversation is cleared (`/clear`), a fresh `claude` starts in it, or the tab is closed. They are saved, so a Giverny restart keeps the Done rows; they show again once that session is back up.

## Where the rows come from

- **Running** rows come from Claude Code's live worker list, through the relay below.
- **Done** rows come from the workers' own transcripts (`projects/<cwd>/<session>/subagents/agent-<id>.jsonl`) and from the `<task-notification>` Claude Code queues on the parent's transcript when a worker stops. A worker messaged again after it finished is Running again until its next notification.

## The relay

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
