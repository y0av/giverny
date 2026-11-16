# The management panel

The management panel is a table under a tab's terminal listing that tab's Claude Code subagents, **Running** and **Done**, one row each. It needs no setup: every row comes from data Claude Code already writes. It is off by default; turn it on with `claude.management_panel` (Settings → Management panel).

**The pane shows only when subagents are involved**: in a tab whose session has a real subagent, running or done this session. A tab with none shows nothing.

**Settings → Management panel** holds the pane's switch and one switch per column, in the order the pane draws them (`[management_panel.columns]`: `stage`, `id`, `title`, `elapsed`, `now`, `tokens`, all on by default). A column switched off takes no width; the rest close up and TASK takes the room. Every one applies as it is changed. Which **Done** rows show has no row there but is honoured when set by hand in `config.toml` (`management_panel.done_rows`: `"all"` by default, `"hide"`, or `"last"` for the newest `management_panel.done_last` of them).

## What a row shows

- **STAGE**: `Running` or `Done`, and the whole row is tinted by it.
- **TASK**: the agent's name and its description; an agent with no name shows its model (`opus`, `sonnet`, `haiku`) in place of the name. An agent another agent spawned hangs under that agent's row, indented a step per level, and goes where that row goes. A `▾` at the head of the spawner's title folds them away (`▸ … +3, 1 running`), kept per tab while Giverny runs.
- **ELAPSED**: a stopwatch while it runs, the time it took once Done. A span the worker stood stopped on an API error is off its clock. While the tab's account is out of a usage limit the clock stops, and starts again from where it stood once the window reopens.
- **NOW**: what it is doing, its last tool call (`Bash: cargo test`); `5h limit → 13:00` while the account is out; `stopped: no network` while it stands on an API error; how it ended once Done (`done`, `failed`, `killed`, `stopped`).
- **TOKENS**: the context of its last turn, read from its transcript the way the status line counts it. Claude Code's own figure in its agents view adds the run's output so far, a few hundred tokens more for a short worker.

A row's hover has its whole title and why it is where it is. A drag selects the pane's text as the terminal does, and copies it when it ends. A look at a tab that wore the unseen-finish tick breathes the frames of the rows that landed since the last look.

Rows stay until the tab's conversation is cleared (`/clear`), a fresh `claude` starts in it, or the tab is closed. They are saved, so a Giverny restart keeps the Done rows; they show again once that session is back up.

## Where the rows come from

Giverny reads a tab's workers off the disk once a second. It writes nothing to Claude Code's settings for the pane: the rows come from files Claude Code writes anyway and from the hooks Giverny already installs.

- **A tab gets a table** when its session is seen to have a worker: a `PostToolUse` hook from one (its payload carries `agent_id`) or the session's own `Agent` launch, each relayed with the tab it ran in (`GIVERNY_TAB_ID`). A `SessionStart` that resumes a conversation gives it one too, so its finished workers show again. A fresh `claude` with no worker costs nothing.
- **Each worker is its transcript**, `<config>/projects/<cwd>/<session>/subagents/agent-<id>.jsonl`, with `agent-<id>.meta.json` beside it: `description`, `agentType`, `model`, `name` when the agent was given one, and for an agent another agent spawned `parentAgentId` and `spawnDepth`; `finished: true` once its run has ended, and `revivedAt` when a message woke it again. A second's look is the folder listed, a `stat` per worker, and only the bytes a running worker appended since.
- **Running** while it has not landed since its last turn and has written since the claude now running the session started (that claude's `startedAt` in Claude Code's session registry, `<config>/sessions/<pid>.json`). A worker of an earlier claude of the conversation was killed with it and lands nowhere, so it is Done (`stopped`) at its last write instead.
- **Done** once landed: the parent transcript's `<task-notification>` for its id is at least as new as its last turn, its `<status>` the outcome (`completed`, `failed`, `killed`, `stopped`), or, in the seconds before the notification and where none comes, its meta's `finished` (the meta's mtime is when). A worker messaged after that is Running again from the message, until its next landing.
- **Its cells.** The clock starts at the transcript's first line, or at `revivedAt` for a worker woken again. NOW is its last tool call (`Bash: cargo test`).
- **Only the tab's own session.** A `claude` started inside the tab's claude, or under a `tmux` the tab launched, inherits `GIVERNY_TAB_ID` too. The tab also exports the app's process id (`GIVERNY_PID`), and the hook relay walks up from itself: the tab's own claude reaches the app with no other `claude` on the way. A nested one passes a second `claude`, and a multiplexer's sessions never reach the app at all, so their hooks move neither the tab's rows nor its state or resume target. Where the walk sees no `claude` at all (a relay run on the Windows side for a session inside WSL), it trusts `GIVERNY_TAB_ID`.
- **A conversation's earlier ids.** A resume or a compaction can give the conversation a new session id; the table keeps its rows across it and looks for workers under every id the conversation had. Two ids are the same conversation when their transcripts start from the same record (the first one with no parent, which Claude Code copies forward on a re-id). `/clear`, a fresh `claude` and a `/resume` into another conversation start the table over, and an earlier id found to hold another conversation is dropped, with its rows, when the session next starts.

## Claude Code's own agent list

Claude Code draws its own list of a session's agents under the prompt: a blank row, `● main`, and a row per worker. Since 2.1.323 nothing inside Claude Code can hide it. While the pane is on, it stands in for that list, so Giverny keeps the list out of the tab's picture itself. This is part of `claude.management_panel` and has no setting of its own; with the pane off the list is drawn as Claude Code draws it.

- **Only the tab's own claude**, the one in front of its shell: a `claude` inside `tmux` or a nested shell keeps its list.
- **In fullscreen** (`/tui fullscreen`, the alternate screen), Claude Code is told the terminal is taller by the list's height, and Giverny draws only down to the tab's bottom. Everything above lays out exactly as with no list, so the prompt does not move when workers start and end. Each change is a resize Claude Code answers with a redraw, and the last frame stays on screen until it has.
- **Outside fullscreen** nothing is resized, since a resize makes Claude Code reprint its scrollback; the list's rows are painted blank. The prompt still rises when the list appears, as Claude Code scrolls its own screen.
- **While the list is in use it shows.** `↓` from the prompt walks into the list (a `❯` on `main` or a worker's row), and Enter opens a worker's own view (`◯ main`). Then the list is drawn as Claude Code draws it, so it can be used as ever, and it goes again once the keyboard is back at the prompt of the main view.
- The footer's `← N agents` is not part of the list and stays. The grid still holds the hidden rows; only the picture leaves them out.
