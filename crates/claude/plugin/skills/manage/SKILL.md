---
name: manage
description: Run a piece of work as a manager session of subagents, one per task (a light worker reused for a next task in code it has already read), several at once where their files do not overlap, and show it in Giverny's management panel as Running, Next up with ETAs, and Done. Invoke it whenever the user says "manage" — "manage this", "manage that task", "and manage it plz", "/manage" — before doing any of the work yourself; "manage" means this skill, not a project's own orchestrate or dispatch skill. Also use it when asked to fan work out to subagents or work through a list of tasks in parallel.
---

# Manage a session

You are the dispatcher. You split the work into tasks, give each task its own
subagent, and keep the manager session's state in Giverny's management panel with
`giverny-manage`. The pane reads what `giverny-manage` writes: every row you plan
shows under **Next up** with its estimate, a started row counts its time under
**Running**, and a landed row shows under **Done** against its estimate.

If the project has its own rules for picking or finishing work (a task list, a
CONTRIBUTING file, instructions from the user), those decide *what* to do;
this skill only says how to run it and keep the pane true.

## 1. Plan

Your name: when Giverny names every session (a setting, off by default),
you got one at the session's start; keep it.
`giverny-manage name` prints it (and gives you one if you have none yet). If
the project signs its tasks, sign your umbrella task with that full name, and
introduce yourself with it once, in your first message to the user. When
Giverny does not ask, do not name yourself.

Break the work into tasks a single subagent can finish on its own. For each,
choose:

- a **task name**: short, unique in this manager session, letters, digits and
  `-` (`auth-fix`, `docs-api`, `12`). It is the row's name in the pane, and the
  link between the row and its worker.
- a **title**: one line saying what the task is. If the project types its
  tasks, start the title with the type word and a colon (`BUG: …`,
  `FEATURE: …`): how far off your gut runs is told per type.
- an **estimate** in minutes, as two figures: your **gut** figure (`--gut`),
  the raw one, and the one you **settle on** (`--eta`) after allowing for how
  far off your gut usually runs. The settled figure is the one the pane shows.

Group the tasks into **lanes** by the files they will touch. Tasks in different
lanes may run at the same time; tasks sharing files run one after another: plan
each one after the task ahead of it in its lane (`--after <task>`).

Write each task's **brief** to a file: the prompt you will give its worker
(see [Start a task](#2-start-a-task)), or at least the task's own text. Its
**Next up** row opens to it, so the user can read what each worker will be
told. Keep the briefs in a folder inside the project that git ignores
(`.claude/briefs/`, say), not under `/tmp`, which a reboot wipes. If the
prompt changes before the spawn, pass the new file on `start` (`--brief FILE`).

Record every task, in the order you mean to run it:

```bash
giverny-manage plan auth-fix --eta 25 --gut 50 --title "Fix the token refresh race" --brief briefs/auth-fix.md
giverny-manage plan docs-api --eta 15 --gut 30 --title "Document the new endpoints" --brief briefs/docs-api.md
giverny-manage plan auth-tests --eta 10 --gut 20 --title "Test the refresh race" --after auth-fix --brief briefs/auth-tests.md
```

Each `plan` (and `start --eta`) prints the figure the pane counts down from
and how far off your gut on such tasks has run:

```
planned auth-fix: ~25m (gut 50m)
  your gut estimates on BUG tasks in myapp have usually run about 2.1× too high (last 9, median); give your gut figure and the one you settle on: `--eta <settled> --gut <gut>`
```

Give your gut figure as `--gut` every time, and settle `--eta` on the figure
you believe after allowing for that line; how much is your judgement. Nothing
is corrected for you. Pass `--repo <name>` when the task belongs to a project
other than the directory you run in. When the user asks whether estimates are
getting better, answer from `giverny-manage accuracy` (optionally
`--repo <name>`).

Tell the user the plan in a few lines (task, lane, estimate) before you start.

### A planned task never waits silently

Start every planned task whose lane is free and whose claim is granted, right
away. A planned task may wait on only three things:

- **a lease**: its `claim` was queued;
- **another task**: it was planned `--after` that task, until that one lands;
- **a hold you record**, with its reason, and tell the user about:
  `giverny-manage hold <task> --why "<reason>"`; `giverny-manage unhold <task>`
  ends it. Hold only for a reason that will end: a person to answer, a review,
  a fix to land elsewhere. "A quieter machine" is not one: claim and let it
  queue.

Giverny tells you on your next tool call when a Next up row has waited five
minutes with none of these: start it, or record what it waits for.

## 2. Start a task

First claim what its worker needs from the machine. Every Giverny session
shares one ledger of RAM, GPUs and exclusive slots, so a claim is how two
managers stay out of each other's way. You claim, because the lease is
yours; workers never do.

```bash
giverny-manage claim auth-fix --ram 3G
```

**Reserve RAM; CPU is shared and measured.** A lease holds memory, never CPU;
`claim` queues on its own while the CPU is busy with workers already building.
Size the RAM from the hint `claim` prints once there is history (`the last 4
BUG tasks in myapp peaked at 1.8G (median), 2.6G at most`), else take the
default lease (`giverny-manage resources` prints it). Add `--slot <name>` for
something only one task at a time may use (a shared build folder), and
`--priority` when the task is urgent.

Each answer is one line that says what to do:

- `granted`: go on.
- `queued`: something it needs is held, or the CPU is busy. Run the same claim
  with `--wait` in the background (Bash with `run_in_background`) and start
  another planned task from a free lane meanwhile:

  ```bash
  giverny-manage claim auth-fix --ram 3G --wait
  ```

  It prints the queued answer at once, re-claims every 30 seconds, and ends
  when the claim is granted or refused; its end wakes you: act on its last
  line. When the queued answer suggests `ask`, send
  `giverny-manage ask <holder's task> "<why, the priority, how long you need
  it>"`; the holder may release.
- `refused`: larger than this machine's limits. Ask for less, or tell the user.

Then, once granted, stamp the task and spawn its worker in the same breath:

```bash
giverny-manage start auth-fix
```

Then spawn **one subagent for this task** with the Agent tool. One task, one
agent is the default; to reuse an idle worker instead, see
[Giving a worker its next task](#giving-a-worker-its-next-task).

When you spawn one:

- Its `description` must contain the task name as a whole word, e.g.
  `auth-fix: fix the token refresh race`. The pane joins the row to its worker
  by this name.
- Run workers in the background when several run at once, and never start two
  workers that touch the same files. Once one is Running, keep a
  `giverny-manage watch` going in the background (see **A progress check**).
- Give the worker everything it needs in its prompt; it cannot see this
  conversation. Include these lines, with the task name filled in:

  > You are the worker for task `<task>`. Work only on this task and only in the
  > files it needs. Your first re-estimate is Giverny's ask, about five
  > minutes in, once you have read the code; do not give one before it. The
  > ask tells you how far off your gut figures on such tasks usually run:
  > answer it, even if the figure stands, with your gut figure and the one
  > you settle on after allowing for that, as you judge: `giverny-manage eta
  > <task> <settled> --gut <gut> --note "<why>"`. After it, re-estimate the
  > same way whenever the figure turns out wrong, allowing for the last line
  > you were shown. When you are waiting on
  > something that is not the work (a build slot, a lock, a person), say so
  > with `--why wait`; your next `eta` without it ends the wait. Do not run
  > `giverny-manage start` or `giverny-manage land`; the dispatcher does. Run heavy
  > commands (builds, test suites, anything that eats CPU or memory) as
  > `giverny-manage run <task> -- <command>`: it keeps them inside the memory
  > granted, shares the CPU fairly, and measures them. Never set CPU or memory
  > caps or build jobs by hand. If `run`
  > says the memory cap killed your command (OOM), do not retry it: report that
  > to the dispatcher, who claims more. Before your report, stop every
  > background shell you started (`TaskStop <shell id>`): a task with anything
  > still running is not done. End with a short report: what you did,
  > how you checked it, and anything left undone.

## 3. While it runs

- When you learn a task will take longer or shorter, re-estimate it:
  `giverny-manage eta <task> <minutes left>`.
- If a worker has to wait on something outside the work (a person, a quota, a
  build slot), stop its clock with `giverny-manage pause <task> --note "<why>"`
  and restart it with `giverny-manage resume <task>`. Paused spans are left out
  of the working time the history learns from.
- **`pause` stops only the clock.** The worker and its commands keep going.
  When the user asks you to pause a worker (or everyone), `pause` its task,
  then:
  - tell the worker to come to a safe point: start no new build or test, and
    let a running one finish or stop it;
  - have it commit and push its work in progress, write its next step where
    the project keeps it, and end its turn with a short report;
  - stop your own background commands for that task (a `claim --wait`, a
    build you ran);
  - tell the user it has paused only once it really has. `pause` says
    `nothing of <task> runs` when it has; any other answer (`paused <task>'s
    clock; its worker … is still working`, a `run` command or background
    shell still going, each with the command that stops it) means it has
    not, however the row reads. Giverny then tells you `<task> is now really
    paused` when the worker stops, and once every task you paused has. Giverny
    stops nothing itself, so stopping or keeping each is your call.
- **A progress check** reaches you, once per event, when a worker's figure
  runs out or it asks for much more time. It names what the worker is doing
  and its transcript. Read its recent work there, judge
  whether it makes sense, is all needed, or could be done faster, then tell
  the worker (SendMessage) to cut, change course or go on, or let it go on;
  re-estimate the row or split the task if that is what it needs. Giverny
  stops and re-estimates nothing.
  - **Keep one `giverny-manage watch` running in the background** (Bash with
    `run_in_background`) while you have Running rows: it sleeps until the
    next check falls due, prints it and exits, and its exit wakes you even
    while you wait idle for your workers. Read its output file — that is the
    check — act on it, then start a fresh `watch`. One that says nothing is
    Running needs no restart until a task runs again.
- **A row reads `killed`** when Giverny, WSL or your session went off with
  its worker (Giverny tells you which on your comeback). Its clock stopped
  there and stays stopped: revive it with a fresh worker (its description
  naming the task) or an idle one handed it with `start <task> --agent <id>`,
  then `giverny-manage resume <task>`; or `pause` or `drop` it.
- **`resume` restarts only the clock** too. It says whether the task's worker
  is working or has ended its turn; one that ended will not carry on by
  itself: wake it with SendMessage saying what to do next, or spawn a fresh
  worker with a brief. Tell the user it has resumed once someone works on it.
  One that ended its turn with a background shell of its own still running
  is **asleep**, not ended: Claude Code wakes it when that shell ends, so
  there is nothing to wake or land; stop the shell only if the work should
  not go on.
- **Lines the user typed in a worker's view.** In Giverny the user can open a
  row and type in its prompt box. Claude Code keeps no view of a worker whose
  turn ended, and a Next up task has no worker yet, so Giverny puts that line
  at your prompt, in one of two shapes. The text after the colon is the
  user's own words, typed by them. Act on it as you would on anything they
  ask you directly:
  - `Send this to <task>'s worker (agent <id>): <text>`: SendMessage that
    agent the text as written, adding nothing. That wakes it. If its task
    has landed, run `giverny-manage start <task> --agent <id>` first: it puts
    the row back in Running, and the time it sat landed is not counted. A
    task that is Running but finished needs no command. When the worker
    reports, check and land it as usual.
  - `Start <task> now, adding this to its brief: <text>`: start that Next up
    task now. `claim` it, `start` it and spawn its worker as in sections 1–2,
    with the user's text added to the brief as theirs (`The user adds: <text>`).
    If its lane is not free, or it waits `--after` another task, tell the
    user and ask before you go ahead anyway.

  Giverny takes the user's tab into the worker's view once it runs. Do not
  tell them that, or re-type their line back to them.
- A task you decide not to do comes out of the plan: `giverny-manage drop <task>`.
  Like `land`, `drop` refuses it while anything of it still runs (section 4).
- A worker that reports an OOM needs a bigger lease: `giverny-manage release
  <task>`, then `claim` it again with more `--ram`, then send the worker on
  (`run` takes the new grant). A held lease never grows through `claim`. Once,
  not in a loop: if the bigger claim fails too, tell the user.
- **Another manager may ask for your resources** (`Giverny: another manager
  on this machine asks for resources (message mXXXXXX, …)`). Weigh its
  priority and time left against your task's, then always answer with
  `giverny-manage reply <id> "<answer>"`, even with no. To give way,
  `giverny-manage release <task>` frees everything, slots included;
  `giverny-manage claim <task> --ram <less>` shrinks the lease and keeps its
  slots. When a reply to your own ask arrives, re-run your `claim` at once.
- `giverny-manage show` prints the manager session as it stands.

### Giving a worker its next task

Reuse a worker only when **both** hold: it is **light**, under 100k tokens of
context (`start` prints each idle worker's), and the next task is **in the
files it has already read, or follows on from what it just did**. Otherwise
spawn a fresh worker with a full brief: every turn of a reused worker re-reads
its whole context, so a heavy one makes each turn slow and expensive.

To hand it over, run this **before** you send the worker its next task with
SendMessage:

```bash
giverny-manage start <next task> --agent <worker's agent id>
```

It is the only hand-off: a message alone gives the worker nothing in the
panel, whatever it says. It lands the worker's earlier task with its own
measured time and starts the new one with its own clock; like `land`, it
refuses while anything of the earlier task still runs. It refuses a worker over the limit too: spawn a fresh
worker then. Pass `--heavy-ok` only when the task truly needs what that
worker has in its context and a brief cannot carry it.

Giverny tells you once, on your next tool call, when a Running row's worker
finished three minutes ago and you have not landed it (check its report and
land it), and when a Running row has had no worker for three minutes: spawn
one, hand it to an idle one with `start <task> --agent <id>`, or `pause` or
`drop` it.

## 4. Land a task

When a worker reports, check its work yourself before you call it done (read
the diff, run the tests). Then land it, with one of:

```bash
giverny-manage land auth-fix                                   # done
giverny-manage land auth-fix --outcome Blocked --note "needs the API key"
giverny-manage land ui-empty-state --review "<who> — <what to look at> — <where>"
```

A task with anything still running is not done, so `land` refuses it, for
every `--outcome` and with no override: its worker still mid-turn (wait for its
report), or asleep on background shells of its own; a `giverny-manage run`
command of the task still going; a background shell its worker started still
running. The refusal lists each with how to stop it (`TaskStop <shell id>`,
`kill <pid>`) and changes nothing; Giverny stops nothing itself. Once they are
stopped, land it again.

Use `--review` when the work is finished but a person has to look at it before
it counts: anything visual, a judgement call between two good options, an
irreversible change. The text is one line naming who, what exactly, and where
to see it; the pane shows it at the top of the row.

Landing also gives the task's lease back; there is nothing else to release.
Then start, at once, every planned task whose lane is now free (those planned
`--after` it among them), until none are left.

## 5. Finish

When every task has landed, tell the user what landed, what is blocked or
waiting for review, and what was left undone. Leave the rows: the pane keeps
them as the manager session's record. `giverny-manage clear` removes them
when the user asks.

## If `giverny-manage` is missing

The command comes with Giverny. If running it fails with "command not found",
carry on without it: the management works the same, the pane just shows the
workers without the plan and the estimates. If its usage has no `claim` (an
older Giverny), skip `claim`, `run`, `ask` and `reply`; workers run their
commands themselves.
