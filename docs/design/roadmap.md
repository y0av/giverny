# What to build next

Written 2026-07-30, after a fortnight of daily use. Ordered by what the app is
*for*, not by what terminals usually have.

Giverny is meant to be **a real terminal** that happens to be built around
Claude Code — not a Claude dashboard that can run a shell. Both halves count:
an item that makes daily terminal work better earns its place even when it has
nothing to do with Claude.

An earlier version of this document ranked general-terminal features below
Claude-aware ones on principle. That was the wrong test, and the images entry
below was marked down because of it.

## 1. Background agents are invisible — the biggest gap  ✅ built

Claude Code runs work with no tab attached: `/fork` copies a conversation into a
background session, the Bash tool runs commands with `run_in_background`, and
agents keep going after the session that started them moves on.

Each one has a state file. Verified on this machine, `~/.claude/jobs/<id>/state.json`:

```json
{ "state": "working", "detail": "board drained; resume ACTIVE; next poll ~5 min",
  "tempo": "idle", "inFlight": { "tasks": 2, "queued": 0,
                                 "kinds": ["local_bash", "session_cron"] },
  "fan": [ { "id": "b0xpa7pas", "kind": "shell", "label": "until grep -qE …" } ],
  "name": …, "sessionId": …, "resumeSessionId": …, "cwd": …, "tokens": …,
  "children": …, "createdAt": …, "updatedAt": … }
```

Alongside it: `jobs/<id>/timeline.jsonl`, `jobs/pins.json`, `daemon/roster.json`,
`daemon.status.json`.

There was an agent in exactly this state while this document was being written,
and the app showed nothing. That is the premise failing: a Claude that needs
attention is invisible precisely because it has no tab.

**Build:** a *background* section in the rail, above the categories. One row per
job — name, state, in-flight count, age. The same spinner and attention grammar
as tabs, since it is the same question. Clicking opens a tab attached to it:
`resumeSessionId` and `cwd` are both in the file, which is everything the
existing resume path needs. Pinned jobs (`jobs/pins.json`) sort first.

**Why first:** it is the only item here that no other terminal could do, it uses
machinery that already exists (file watching, the state machine, resume), and it
closes the hole in the thing the app claims to be.

## 2. Prompt marks are parsed and unused

`tee.rs` already extracts OSC 133 prompt marks. Nothing consumes them.

**Build:** jump to previous/next prompt; select the output of the last command;
"scroll to where this command started". In a Claude session that has printed
four thousand lines, "take me back to where I asked" is a daily need, and the
data is already flowing through the parser.

Cheap, self-contained, immediately useful.

## 3. Hints: act on what is on screen without the mouse  ✅ built

kitty's `hints` kitten labels every URL/path on screen and you pick one with a
keystroke — open it, insert it at the prompt, or copy it.

We already have the detector (`search::target_at`) and now OSC 8 metadata as
well. Hints are a second front-end over the same function: overlay labels,
read a keystroke, act. The natural follow-on is *insert into the prompt*, which
is how you hand Claude a path from `git status` output without touching the
mouse.

## 4. Tokens and cost per tab

Transcripts record `message.usage` per turn. Deduplicated on
`(message.id, requestId)`, that gives real token counts per session.

With three accounts and a weekly cap, the question "which tab is eating the
quota" currently has no answer — the usage panel says *an account* is at 90%,
never *which conversation* took it there. This turns the meters from a warning
into something you can act on.

## 5. Cross-tab search

Search works inside one tab. With thirty tabs the more common question is
"which tab was that in". Same index, wider scope, one extra column in the
results.

## 6. Images (kitty graphics protocol)  ✅ built

The one real capability gap against kitty, Ghostty and WezTerm. Programs that
draw images print nothing here.

It is the largest item here and the clearest gap for a terminal that means to
stand on its own: image previews in file managers, plots from a REPL, `timg`,
charts from a build. Worth doing on its own merits.

## 7. Broadcast to a category

Send the same prompt to every Claude in a category. "Run the tests and report"
across six repositories, in one keystroke, with the rail already showing which
of them came back.

Powerful, and easy to fire into a session that was mid-thought — it needs a
confirmation showing exactly which tabs will receive it, and it must refuse tabs
where Claude is waiting on a permission prompt.

## 8. Plans and file history

`~/.claude/plans/*.md` holds plan-mode plans; `file-history/<uuid>/` holds the
snapshots behind `/rewind`. Both are readable, both are per-session.

Surfacing "this tab has a plan" and "Claude has touched 12 files in this
session" is cheap. Do not reimplement rewind — that is Claude's, and racing it
would corrupt state. Visibility only.

## Known gaps, already documented

- Selecting or clicking inside an RTL run maps to the wrong cell (the forward
  mapping exists; it needs inverting).
- The palette matches tabs only, and shows no chords despite the keymap table
  existing to feed it.
- Themes apply on click rather than previewing on hover; no theme files.
- No key rebinding.
- Split panes, SSH tabs, plugins — deferred since the beginning, still deferred.

## Ordering

1. ~~Background agents~~ — built.
2. ~~Hints~~ — built. **Prompt marks** need shell integration installed first
   (nothing emits OSC 133 here), and do not exist inside a full-screen program
   like Claude, so they rank lower than they first appeared.
3. **Tokens per tab** — makes the meters actionable.

See `terminal.md` for the general-terminal side.

Then, depending on what Giverny is for: **images** if it should stand as a
general terminal, **broadcast** if it should lean further into orchestration.
