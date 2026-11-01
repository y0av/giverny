//! `giverny hook`: the plugin's `PostToolUse` hook, which runs on every tool
//! call of every session the plugin is loaded in — the dispatcher's and its
//! workers' alike — and hands each part of its work to the side that owns it.
//! The same command is the plugin's `PreToolUse` Bash hook, which lets a
//! command that would leave its tab's resource cap run and reminds the
//! session, once per kind, to keep heavy work in the cap ([`guard`],
//! [`crate::bash_guard`]), and its `PreToolUse` hook on a
//! subagent's report (`SubagentHandback`), which holds a task worker's report
//! back once while background shells of its own still run ([`handback`]),
//! and, with the manage skill in, its `PreToolUse`
//! Agent hook, which nudges a session that is no manager toward the skill
//! when a subagent it starts looks like task work ([`crate::manage_hint`]),
//! and the plugin's
//! `UserPromptSubmit` hook, which sends a prompt that asks to manage to the
//! `giverny:manage` skill ([`crate::manage_trigger`]), and the `SessionStart`
//! hook that, with names on, gives a tab's session its name
//! ([`crate::names`]), and puts a tab's claude under its cap before its
//! first command ([`crate::tab_cap::on_session_start`])
//! — or, for a session whose start ran no such name hook, its
//! next prompt does — and tells a manager's compacted
//! session it is one ([`crate::managers::start_context`]).
//! And it is the `SubagentStop` hook, which
//! holds a subagent's finish once to ask whether a background command of its
//! own should keep running after it reports ([`crate::leftovers`]),
//! and the `Stop` hook, which holds a session's turn's end
//! once per background command of its own that its timeout moved there or
//! that went quiet ([`crate::strays`]).
//!
//! - **Estimates.** Whoever spawns a worker gives its ETA. Right after a
//!   dispatcher's `Agent` call that started a worker in the background, if no
//!   estimate for it exists (no manager session task holds it, no agent
//!   ETA names it), the dispatcher is asked, once, to give one
//!   ([`agent_eta::dispatcher_ask`]). Five minutes into its work a worker is
//!   asked, once, to re-estimate, and again as each figure runs out (five
//!   minutes left, then past it): on its manager task's row when it
//!   holds one ([`manage_nudge::check`]), else on its agent ETA
//!   ([`agent_eta::worker_ask`], [`agent_eta::deadline_ask`]). Each ask
//!   tells a task worker how far off its gut on its kind's re-estimates
//!   usually runs; nothing is said between asks. A worker
//!   spawned in the foreground blocks its dispatcher until it is done, so
//!   only the worker's asks reach it.
//! - **A manager session's upkeep.** Its leases are renewed
//!   ([`manage_nudge::beat`]) and, on the dispatcher's own
//!   calls, its unread `ask`/`reply` messages delivered
//!   ([`manage_inbox`]), and its rows' chores — a finished worker's task to
//!   land, a Next up task idle with nothing to wait on — told once each
//!   ([`manage_chores`]), as is a task paused ten minutes with a `run`
//!   command of it still going, with how to stop it
//!   ([`manage_nudge::paused_runs`]), and a worker that ended
//!   its turn with background shells of its own still running
//!   ([`crate::bg_shells`]: the shells a task's worker leaves
//!   in the background are recorded on its `PostToolUse`, checked on its
//!   `SubagentStop`), and a worker that ran out of its figure or asked for
//!   much more time, with what it has done so far, for the manager to
//!   judge ([`crate::manage_progress`]).
//! - **What a subagent leaves running.** A subagent finishing with a
//!   background shell or monitor of its own still running is asked, once,
//!   whether it should keep running ([`crate::leftovers::subagent_stop`]);
//!   if it does, its parent session hears of it once, on its next tool call
//!   or prompt ([`crate::leftovers::deliver`]). Every subagent in a Giverny
//!   tab, managed or not.
//! - **What a session leaves running itself.** A shell of its own still
//!   running at its turn's end, moved to the background by its timeout or
//!   quiet for ten minutes, holds the stop once with a question
//!   ([`crate::strays::stop`]); one that goes quiet later is told once on
//!   its next tool call or prompt ([`crate::strays::between`]).
//! - **What left the cap afterwards.** A process of the session's that the
//!   app's sampler found outside its tab's slice is told once, on its next
//!   tool call or prompt ([`crate::escapes::deliver`]). And
//!   every hook call of a tab's session leaves its hook mark
//!   ([`crate::escapes::mark_hooked`]), whose absence the management panel
//!   marks.
//! - **The manager mark.** The session's own `Skill` call running the
//!   manage skill marks it a manager ([`crate::managers`]), as
//!   does the person typing `/giverny:manage` (the prompt hook): what lets
//!   its pane open before any worker, and draw its Next up.
//! - **What happened while it was away.** A session's next call after a
//!   reboot, a restart of its `claude`, a sleep or a long absence tells it
//!   once, in one short paragraph, what that took: its processes, `/tmp`,
//!   its connections and clocks, or only the person's patience to trust
//!   the last picture — and, for a manager, the Running rows whose workers
//!   went ([`crate::away`]). On its start, its prompt or its
//!   own tool call, every session, in a tab or not; a subagent's call only
//!   keeps the record and passes what it saw to its parent.
//!
//! **Only where something tracks the worker.** Every ask is for the pane:
//! it goes out only when the session runs in a Giverny tab
//! (`$GIVERNY_TAB_ID`, whose pane shows the worker) or has a manager
//! session (its file is this writer's). Anywhere else — a plain `claude` in
//! another terminal — the hook says nothing, where a `giverny-eta` nobody
//! reads would raise a permission prompt for it.
//!
//! **Cheap when idle.** A call that is not a spawn and not a worker's costs a
//! few `stat`s, the clocks, the session's small away record and a read of
//! the small `run` records directory; a worker's
//! call before its five minutes, a `stat` of its spawn metadata, and after
//! them a read of its small agent-ETA file. A `SubagentStop` with no
//! background command in flight reads no file either. Whatever happens the hook exits 0, so it
//! never fails the tool call it rides on.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::{Map, Value, json};

use crate::manage::{self, Lock};
use crate::manage_nudge::{beat, check, holds_row, row_key, stamp_agent};
use crate::{agent_eta, away, manage_chores, manage_history, manage_inbox, resources};

/// What the hook payload says about who is calling.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Caller {
    pub session: String,
    pub agent_id: String,
    /// The dispatcher's transcript, whose `<stem>/subagents/` holds the
    /// workers' spawn metadata.
    pub transcript: Option<PathBuf>,
}

impl Caller {
    /// `None` for a call that is not a subagent's.
    pub fn of(payload: &Value) -> Option<Caller> {
        Some(Caller {
            agent_id: text(payload, "agent_id")?,
            session: text(payload, "session_id")?,
            transcript: text(payload, "transcript_path").map(PathBuf::from),
        })
    }

    /// Where the worker's `agent-<id>.meta.json` is.
    fn meta(&self) -> Option<PathBuf> {
        let t = self.transcript.as_ref()?;
        Some(
            t.with_extension("")
                .join("subagents")
                .join(format!("agent-{}.meta.json", self.agent_id)),
        )
    }

    /// The worker's spawn description, from its `agent-<id>.meta.json`.
    fn description(&self) -> Option<String> {
        let t = self.transcript.as_ref()?;
        let dir = t.with_extension("").join("subagents");
        crate::subagents::read_meta(&dir, &self.agent_id).description
    }

    /// When the worker was spawned: its metadata file, written once then.
    fn spawned_ms(&self) -> Option<u64> {
        let t = std::fs::metadata(self.meta()?).ok()?.modified().ok()?;
        Some(t.duration_since(SystemTime::UNIX_EPOCH).ok()?.as_millis() as u64)
    }
}

/// A payload field as trimmed, non-empty text.
fn text(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(String::from)
}

/// The few fields of a hook payload the hook reads, as a small JSON object:
/// a `PostToolUse` payload carries the whole tool response, which is not
/// built into a tree only to be dropped. Only a dispatcher's `Agent` call
/// is read twice, for the worker it started (`spawned_agent`, its
/// `spawned_description`, and `spawned_background` when it runs on).
pub fn payload_of(input: &str) -> Option<Value> {
    #[derive(serde::Deserialize)]
    struct Hook {
        session_id: Option<String>,
        agent_id: Option<String>,
        transcript_path: Option<String>,
        hook_event_name: Option<String>,
        tool_name: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct Spawn {
        tool_input: Option<Value>,
        tool_response: Option<Value>,
    }
    let h: Hook = serde_json::from_str(input).ok()?;
    let mut m = Map::new();
    let spawn = matches!(h.tool_name.as_deref(), Some("Agent" | "Task")) && h.agent_id.is_none();
    let skill = h.tool_name.as_deref() == Some("Skill") && h.agent_id.is_none();
    let bash = h.tool_name.as_deref() == Some("Bash") && h.agent_id.is_some();
    let own_bash = h.tool_name.as_deref() == Some("Bash") && h.agent_id.is_none();
    for (k, v) in [
        ("session_id", h.session_id),
        ("agent_id", h.agent_id),
        ("transcript_path", h.transcript_path),
        ("hook_event_name", h.hook_event_name),
        ("tool_name", h.tool_name),
    ] {
        if let Some(v) = v {
            m.insert(k.into(), Value::String(v));
        }
    }
    if spawn && let Ok(s) = serde_json::from_str::<Spawn>(input) {
        let r = s.tool_response.unwrap_or_default();
        let field = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(String::from);
        if let Some(id) = field(&r, "agentId") {
            m.insert("spawned_agent".into(), json!(id));
        }
        let description = field(&r, "description")
            .or_else(|| s.tool_input.as_ref().and_then(|i| field(i, "description")));
        if let Some(d) = description {
            m.insert("spawned_description".into(), json!(d));
        }
        let background = r.get("status").and_then(Value::as_str) == Some("async_launched")
            || r.get("isAsync").and_then(Value::as_bool) == Some(true);
        m.insert("spawned_background".into(), json!(background));
    }
    // A worker's Bash call that left a shell in the background (its
    // response names it, Claude Code 2.1.303): its id and command, to
    // record against its task. Only then is it read whole;
    // such a response carries no output.
    if bash
        && input.contains("\"backgroundTaskId\"")
        && let Ok(s) = serde_json::from_str::<Spawn>(input)
    {
        let id = s
            .tool_response
            .as_ref()
            .and_then(|r| r.get("backgroundTaskId"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|i| !i.is_empty());
        let cmd = s
            .tool_input
            .as_ref()
            .and_then(|i| i.get("command"))
            .and_then(Value::as_str);
        if let (Some(id), Some(cmd)) = (id, cmd) {
            m.insert("bg_shell".into(), json!({"id": id, "cmd": cmd}));
        }
    }
    // The dispatcher's own `eta` on a task: its decision, which drops that
    // task's pending more-time check. Read whole only when
    // the call looks like one.
    if own_bash
        && input.contains("manage eta ")
        && let Ok(s) = serde_json::from_str::<Spawn>(input)
        && let Some(task) = s
            .tool_input
            .as_ref()
            .and_then(|i| i.get("command"))
            .and_then(Value::as_str)
            .and_then(crate::manage_progress::own_eta)
    {
        m.insert("own_eta".into(), json!(task));
    }
    // The session's own `Skill` call: which skill, for the manager mark
    // ([`crate::managers`]). Its response is the skill's text, never read.
    if skill && let Ok(s) = serde_json::from_str::<Spawn>(input) {
        let name = s
            .tool_input
            .as_ref()
            .and_then(crate::managers::skill_of)
            .map(String::from);
        if let Some(name) = name {
            m.insert("skill".into(), json!(name));
        }
    }
    Some(Value::Object(m))
}

/// Whether this process runs for a session a Giverny pane shows: a tab's
/// own — `$GIVERNY_TAB_ID`, which the app sets in a tab's shell and a hook
/// inherits, and no other claude between it and the tab's
/// ([`crate::lineage`]), as a claude nested in the tab is not the tab's —
/// or a background job's own session, which the tab showing the job shows.
/// A claude nested in a job is neither.
pub fn in_giverny_tab() -> bool {
    if crate::lineage::in_bg_job() {
        return crate::lineage::bg_job().is_some();
    }
    crate::lineage::giverny_var("GIVERNY_TAB_ID").is_some()
        && crate::lineage::of_this_process().is_tabs()
}

/// Whether this process runs for a Giverny tab's own session: not a
/// background job's, not a claude nested in the tab ([`in_giverny_tab`]).
pub fn tabs_own() -> bool {
    !crate::lineage::in_bg_job()
        && crate::lineage::giverny_var("GIVERNY_TAB_ID").is_some()
        && crate::lineage::of_this_process().is_tabs()
}

/// The session's manager session, when it has one of this writer's:
/// its file and document. `Err` for a file another writer owns.
fn manager_doc(dir: &Path, session: &str) -> Result<Option<(PathBuf, Value)>, ()> {
    let file = manage::file_for(dir, session);
    let Some(bytes) = std::fs::read(&file).ok() else {
        return Ok(None);
    };
    let Ok(doc) = serde_json::from_slice::<Value>(&bytes) else {
        return Err(());
    };
    if manage::writer_of(&doc) != Some(manage::WRITER) {
        return Err(());
    }
    Ok(Some((file, doc)))
}

/// The hook's whole run: `payload` is its stdin ([`payload_of`]), `dir` the
/// feed directory, `in_tab` whether the session runs in a Giverny tab
/// ([`in_giverny_tab`]). Returns what to print (the hook reply), or nothing.
pub fn run(payload: &Value, dir: &Path, now: u64, in_tab: bool) -> Option<String> {
    run_with(payload, dir, now, in_tab, None)
}

/// [`run`], with what happened while the session was away ([`crate::away`])
/// told first on the dispatcher's own call.
pub fn run_with(
    payload: &Value,
    dir: &Path,
    now: u64,
    in_tab: bool,
    away: Option<String>,
) -> Option<String> {
    let session = payload
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.contains(['/', '\\']) && !s.starts_with('.'))?;
    let ledger = resources::ledger_path(dir);
    beat(&ledger, Some(dir), session, now);
    // The manage skill ran: the session is a manager from now on,
    // whatever it has written yet.
    if crate::managers::skill_call(payload) {
        crate::managers::mark_from(dir, payload);
    }
    if let Some(caller) = Caller::of(payload) {
        // A subagent's call hears of its session's escapes too: it may be
        // the one that ran it.
        let asks: Vec<String> = [
            worker(&caller, payload, dir, now, in_tab),
            crate::escapes::deliver(dir, session),
        ]
        .into_iter()
        .flatten()
        .collect();
        return (!asks.is_empty()).then(|| reply(&asks.join("\n\n")));
    }
    // The dispatcher's own call: its messages, its rows' chores, the
    // worker it just started, the commands a finished subagent left
    // running, and its own that went quiet.
    let asks: Vec<String> = [
        away,
        manage_inbox::deliver(dir, &ledger, session, now),
        resources::news(&ledger, session, now),
        manage_chores::deliver(dir, session, now),
        crate::manage_nudge::paused_runs(dir, session, now),
        crate::manage_progress::deliver(
            dir,
            session,
            payload,
            now,
            &crate::manage_progress::System {
                feed_dir: dir,
                session,
                transcript: payload
                    .get("transcript_path")
                    .and_then(Value::as_str)
                    .map(Path::new),
            },
        ),
        crate::bg_shells::deliver(dir, session, now, &crate::bg_shells::System),
        spawned(payload, dir, session, in_tab),
        crate::leftovers::deliver(dir, session),
        crate::strays::between(dir, session, now, &crate::strays::System),
        crate::escapes::deliver(dir, session),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!asks.is_empty()).then(|| reply(&asks.join("\n\n")))
}

/// A dispatcher's `Agent` call that started a worker in the background with
/// no estimate anywhere: ask the dispatcher for one.
fn spawned(payload: &Value, dir: &Path, session: &str, in_tab: bool) -> Option<String> {
    let agent = text(payload, "spawned_agent")?;
    if payload.get("spawned_background").and_then(Value::as_bool) != Some(true) {
        return None; // it has run to its end already
    }
    let description = text(payload, "spawned_description");
    match manager_doc(dir, session) {
        Err(()) => return None, // another writer's feed: its rows, its estimates
        Ok(Some((_, doc))) => {
            if holds_row(&doc, &agent, description.as_deref()) {
                return None; // a manager session's task: its row is its estimate
            }
        }
        Ok(None) if !in_tab => return None, // nothing would show this worker
        Ok(None) => {}
    }
    agent_eta::dispatcher_ask(dir, session, &agent, description.as_deref())
}

/// A worker's call: its re-estimate ask, when something tracks it — a
/// Giverny tab, or a manager session.
fn worker(caller: &Caller, payload: &Value, dir: &Path, now: u64, in_tab: bool) -> Option<String> {
    let description = caller.description();
    let desc = description.as_deref();
    match manager_doc(dir, &caller.session) {
        Err(()) => return None,
        Ok(Some((file, d))) if holds_row(&d, &caller.agent_id, desc) => {
            // A shell it left in the background is its task's.
            if let Some(bg) = payload.get("bg_shell")
                && let Some(task) = row_key(&d, &caller.agent_id, desc)
            {
                let field = |k: &str| bg.get(k).and_then(Value::as_str).unwrap_or("").to_string();
                crate::bg_shells::record(
                    dir,
                    &caller.session,
                    crate::bg_shells::Shell {
                        id: field("id"),
                        agent: caller.agent_id.clone(),
                        task,
                        cmd: field("cmd"),
                        started_ms: now,
                    },
                    now,
                );
            }
            // Under the lock, re-read: the dispatcher may be writing it.
            let _lock = Lock::take(&file).ok()?;
            let mut d: Value = serde_json::from_slice(&std::fs::read(&file).ok()?).ok()?;
            let stamped = stamp_agent(&mut d, &caller.agent_id, desc);
            let ask = check(
                &mut d,
                &caller.agent_id,
                desc,
                now,
                manage_history::path(dir).as_deref(),
            );
            // Run out of its figure: its manager hears of it too.
            let late = crate::manage_nudge::overdue(&mut d, &caller.agent_id, desc, now);
            if stamped || ask.is_some() || late {
                manage::write(&file, &d).ok()?;
            }
            if late {
                crate::manage_progress::mark(dir, &caller.session, &d);
            }
            return ask;
        }
        Ok(Some(_)) => {}
        Ok(None) if !in_tab => return None, // nothing would show this worker
        Ok(None) => {}
    }
    let spawned = caller.spawned_ms()?;
    agent_eta::worker_ask(dir, &caller.session, &caller.agent_id, spawned, now)
        .or_else(|| agent_eta::deadline_ask(dir, &caller.session, &caller.agent_id, spawned, now))
}

/// At a task worker's `SubagentStop`: the background shells of its own
/// still in flight that were never recorded, recorded now against its task.
/// Its transcript says which are its own and when each was
/// started or moved to the background ([`crate::leftovers::owned`]); the
/// payload's registry says they are in flight. Poly's 170 lost its
/// `wrangler deploy` (`bohhzzia9`) so: the call's timeout moved it to the
/// background at 16:45:28Z, 52 s before the hook that records shells was
/// installed, and only its turn's end came after. Cheap: nothing is read
/// unless a command is in flight that is not recorded, and the worker holds
/// a row.
fn backfill(stop: &crate::leftovers::Stop, dir: &Path, now: u64) {
    let missing: Vec<crate::leftovers::Task> = stop
        .commands
        .iter()
        .filter(|c| !crate::bg_shells::is_recorded(dir, &stop.session, &c.id))
        .cloned()
        .collect();
    if missing.is_empty() {
        return;
    }
    let Ok(Some((_, doc))) = manager_doc(dir, &stop.session) else {
        return;
    };
    let desc = stop.description.as_deref();
    if !holds_row(&doc, &stop.agent_id, desc) {
        return;
    }
    let (Some(task), Some(transcript)) = (
        row_key(&doc, &stop.agent_id, desc),
        stop.transcript.as_deref(),
    ) else {
        return;
    };
    for l in crate::leftovers::owned(transcript, &missing) {
        crate::bg_shells::record(
            dir,
            &stop.session,
            crate::bg_shells::Shell {
                id: l.task.id,
                agent: stop.agent_id.clone(),
                task: task.clone(),
                cmd: l.task.what,
                started_ms: l.started_ms.unwrap_or(now),
            },
            now,
        );
    }
}

/// The tool a subagent sends its final report with (measured on Claude
/// Code 2.1.308, in auto mode: its `PreToolUse` payload carries
/// `agent_id`, `session_id`, the parent's `transcript_path` and
/// `tool_input.message`, but no `background_tasks`).
pub const HANDBACK_TOOL: &str = "SubagentHandback";

/// A task worker's final report (`SubagentHandback`'s `PreToolUse`) while
/// background shells of its own still run: denied once per task, with the
/// shells and how to stop each; the next try goes through
/// ([`crate::bg_shells::handback`]). Its turn's end, asleep on
/// its shells, is the `SubagentStop` hook's and untouched.
/// Cheap: a `stat` when the session has no shell recorded.
pub fn handback(
    payload: &Value,
    dir: &Path,
    now: u64,
    probe: &dyn crate::bg_shells::Probe,
) -> Option<String> {
    if payload.get("tool_name").and_then(Value::as_str) != Some(HANDBACK_TOOL) {
        return None;
    }
    let caller = Caller::of(payload)?;
    std::fs::metadata(crate::bg_shells::path(dir, &caller.session)).ok()?;
    let task = match manager_doc(dir, &caller.session) {
        Ok(Some((_, doc))) => row_key(&doc, &caller.agent_id, caller.description().as_deref()),
        _ => None,
    };
    let why = crate::bg_shells::handback(
        dir,
        &caller.session,
        &caller.agent_id,
        task.as_deref(),
        now,
        probe,
    )?;
    Some(deny(&why))
}

/// The `PostToolUse` reply carrying `text` into the caller's context.
pub fn reply(text: &str) -> String {
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PostToolUse",
            "additionalContext": text
        }
    })
    .to_string()
}

/// A `PreToolUse` payload's Bash command; `None` for any other payload
/// (read whole only when it names that event: the others carry a tool's
/// whole response).
pub fn pre_bash(input: &str) -> Option<String> {
    let p = pre_tool(input)?;
    if p.tool != "Bash" {
        return Some(String::new());
    }
    Some(p.command.unwrap_or_default())
}

/// What a `PreToolUse` payload says: the tool, its Bash command or the file
/// it edits, who calls and where.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PreTool {
    pub tool: String,
    pub command: Option<String>,
    /// Edit/Write/MultiEdit's `file_path`, NotebookEdit's `notebook_path`.
    pub file: Option<String>,
    pub session: Option<String>,
    pub cwd: Option<String>,
}

/// The tools that write a file, refused in another session's worktree
/// ([`crate::reach_guard`]).
pub const EDIT_TOOLS: &[&str] = &["Edit", "Write", "MultiEdit", "NotebookEdit"];

/// A `PreToolUse` payload, or `None` for any other.
pub fn pre_tool(input: &str) -> Option<PreTool> {
    #[derive(serde::Deserialize)]
    struct Input {
        command: Option<String>,
        file_path: Option<String>,
        notebook_path: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct Pre {
        hook_event_name: Option<String>,
        tool_name: Option<String>,
        tool_input: Option<Input>,
        session_id: Option<String>,
        cwd: Option<String>,
    }
    if !input.contains("\"PreToolUse\"") {
        return None;
    }
    let p: Pre = serde_json::from_str(input).ok()?;
    if p.hook_event_name.as_deref() != Some("PreToolUse") {
        return None;
    }
    let (command, file) = match p.tool_input {
        Some(i) => (i.command, i.file_path.or(i.notebook_path)),
        None => (None, None),
    };
    Some(PreTool {
        tool: p.tool_name.unwrap_or_default(),
        command,
        file,
        session: p.session_id.filter(|s| !s.trim().is_empty()),
        cwd: p.cwd,
    })
}

/// The `PreToolUse` reply refusing a tool call another session's work
/// would suffer ([`crate::reach_guard`]): a Bash command
/// killing its processes, stopping its scopes, mutating git in its
/// worktree or speaking as it, or an edit in its worktree. Anywhere the
/// plugin runs, in a tab or not; with no session known, nothing. The
/// caller (`me`) is looked up only for a call that reaches for something.
pub fn reach(
    pre: &PreTool,
    w: &dyn crate::reach_guard::World,
    me: impl FnOnce() -> crate::reach_guard::Me,
) -> Option<String> {
    use crate::reach_guard::{edit_verdict, judge, linked_root, reaches};
    let why = if pre.tool == "Bash" {
        let cmd = pre.command.as_deref()?;
        let cwd = pre
            .cwd
            .as_deref()
            .map(PathBuf::from)
            .or_else(|| std::env::current_dir().ok())?;
        let found = reaches(cmd, &cwd);
        if found.is_empty() {
            return None;
        }
        let me = me();
        found.iter().find_map(|r| judge(r, &me, w))?
    } else if EDIT_TOOLS.contains(&pre.tool.as_str()) {
        let file = PathBuf::from(pre.file.as_deref()?);
        let file = match (file.is_absolute(), pre.cwd.as_deref()) {
            (false, Some(cwd)) => Path::new(cwd).join(file),
            _ => file,
        };
        linked_root(file.parent().unwrap_or(&file))?;
        edit_verdict(&file, &me(), w)?
    } else {
        return None;
    };
    Some(deny(&why))
}

/// The `PreToolUse` reply denying the call, `why` shown to Claude.
fn deny(why: &str) -> String {
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": why
        }
    })
    .to_string()
}

/// The `PreToolUse` reply reminding the session (`session`) that a Bash
/// `command` would leave its tab's resource cap ([`crate::bash_guard`]),
/// once per kind of escape ([`crate::escapes::remind`]), or
/// nothing. The command runs either way: it is never refused. Only in a
/// Giverny tab (`in_tab`, asked only for a command that escapes), whose
/// claude runs capped: a claude elsewhere has no cap to leave.
pub fn guard(
    command: &str,
    session: Option<&str>,
    feeds: &Path,
    in_tab: impl FnOnce() -> bool,
) -> Option<String> {
    let found = crate::bash_guard::escapes(command);
    if found.is_empty() || !in_tab() {
        return None;
    }
    let said = crate::escapes::remind(feeds, session, &found)?;
    Some(pre_context(&said))
}

/// The `PreToolUse` reply letting the call run with `text` added to the
/// session's context.
fn pre_context(text: &str) -> String {
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "additionalContext": text
        }
    })
    .to_string()
}

/// Leave the session's hook mark ([`crate::escapes::mark_hooked`]) when it
/// runs in a Giverny tab: the pane marks a session with none.
fn mark_hooked(feeds: &Path, session: Option<&str>) {
    if let Some(s) = session
        && crate::lineage::giverny_var("GIVERNY_TAB_ID").is_some()
    {
        crate::escapes::mark_hooked(feeds, s);
    }
}

/// A prompt payload's `session_id`, read alone.
fn session_of(input: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Prompt {
        session_id: Option<String>,
    }
    serde_json::from_str::<Prompt>(input).ok()?.session_id
}

/// The `UserPromptSubmit` reply carrying every context given, one paragraph
/// each: a prompt can both ask to manage ([`crate::manage_trigger`]) and be
/// the first its session's name ask rides on ([`crate::names::first_prompt`]),
/// and bring what a finished subagent left running ([`crate::leftovers`])
/// and what happened while the session was away ([`crate::away`]).
fn prompt_reply(contexts: &[Option<&str>]) -> Option<String> {
    context_reply("UserPromptSubmit", contexts)
}

/// `event`'s reply carrying every context given, one paragraph each, or
/// nothing when none is.
fn context_reply(event: &str, contexts: &[Option<&str>]) -> Option<String> {
    let all: Vec<&str> = contexts.iter().flatten().copied().collect();
    (!all.is_empty()).then(|| {
        json!({
            "hookSpecificOutput": {
                "hookEventName": event,
                "additionalContext": all.join("\n\n")
            }
        })
        .to_string()
    })
}

/// `giverny hook`: the payload on stdin, the reply (if any) on stdout, exit 0
/// whatever happens.
pub fn main() -> i32 {
    let mut input = String::new();
    let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut input);
    // The session's turn's end: held once per background command of its
    // own that its timeout moved there or that went quiet.
    if let Some(stop) = crate::strays::stop_payload(&input) {
        let feeds = crate::feed::feed_dir();
        // A named session's `/resume` row follows its text.
        if crate::names::is_on(&feeds)
            && let Ok(payload) = serde_json::from_str::<Value>(&input)
        {
            crate::names::keep_title(&feeds, &payload);
        }
        let probe = crate::strays::System;
        if let Some(out) =
            crate::strays::stop(&stop, &feeds, manage::now_ms(), &probe, in_giverny_tab)
        {
            println!("{out}");
        }
        return 0;
    }
    // A subagent's finish: held once when a background command of its own
    // still runs.
    if let Some(stop) = crate::leftovers::stop_payload(&input) {
        let feeds = crate::feed::feed_dir();
        let now = manage::now_ms();
        // A task worker's shell its PostToolUse never recorded: recorded
        // now, so the manager hears of it with its task.
        backfill(&stop, &feeds, now);
        if let Some(out) = crate::leftovers::subagent_stop(&stop, &feeds, now, in_giverny_tab) {
            println!("{out}");
            return 0;
        }
        // Let through: the worker's turn ended. Its background shells
        // still running are its manager's to hear of.
        let in_flight: Vec<&str> = stop.in_flight.iter().map(String::as_str).collect();
        crate::bg_shells::turn_ended(
            &feeds,
            &stop.session,
            &stop.agent_id,
            &in_flight,
            now,
            &crate::bg_shells::System,
        );
        return 0;
    }
    if let Some(prompt) = crate::manage_trigger::prompt_of(&input) {
        let feeds = crate::feed::feed_dir();
        // The person typed the skill's command: Claude Code expands it with
        // no `Skill` call, so the mark is made here.
        if crate::managers::typed_skill(&prompt)
            && let Ok(v) = serde_json::from_str::<Value>(&input)
        {
            crate::managers::mark_from(&feeds, &v);
        }
        let on = crate::names::is_on(&feeds);
        let name = on
            .then(|| serde_json::from_str::<Value>(&input).ok())
            .flatten()
            .and_then(|p| crate::names::first_prompt(&p, &feeds, on, tabs_own));
        let manage = crate::manage_trigger::asks_to_manage(&prompt)
            .then_some(crate::manage_trigger::CONTEXT);
        if manage.is_some() {
            // The skill about to be read is this binary's.
            crate::plugin::refresh_own_skill();
        }
        // What a finished subagent left running, if its parent has not
        // heard yet.
        let session = session_of(&input);
        let left = session
            .as_deref()
            .and_then(|s| crate::leftovers::deliver(&feeds, s));
        // Its own background commands that went quiet since its last turn.
        let strays = session.as_deref().and_then(|s| {
            crate::strays::between(&feeds, s, manage::now_ms(), &crate::strays::System)
        });
        // What the sampler found outside its cap.
        mark_hooked(&feeds, session.as_deref());
        let escaped = session
            .as_deref()
            .and_then(|s| crate::escapes::deliver(&feeds, s));
        // What happened since its last turn, first.
        let away = session
            .as_deref()
            .and_then(|s| away::on_call(&feeds, s, manage::now_ms(), away::Call::Prompt));
        let contexts = [
            away.as_deref(),
            manage,
            name.as_deref(),
            left.as_deref(),
            strays.as_deref(),
            escaped.as_deref(),
        ];
        if let Some(out) = prompt_reply(&contexts) {
            println!("{out}");
        }
        return 0;
    }
    // A session's end: its `/resume` row, should what came after its last
    // turn have pushed its title out of what the picker reads.
    if let Some(end) = crate::names::end_payload(&input) {
        let feeds = crate::feed::feed_dir();
        if crate::names::is_on(&feeds) {
            crate::names::keep_title(&feeds, &end);
        }
        return 0;
    }
    if let Some(start) = crate::names::start_payload(&input) {
        // A tab's claude is put under its cap before its first command,
        // not on the sampler's next pass.
        if start.get("agent_id").is_none() {
            let env_file = std::env::var_os("CLAUDE_ENV_FILE").map(PathBuf::from);
            crate::tab_cap::on_session_start(env_file.as_deref());
        }
        let feeds = crate::feed::feed_dir();
        if start.get("agent_id").is_none() {
            // The skill this session reads matches the CLI it runs, though
            // the app that writes the plugin has not restarted since the
            // binary was rebuilt.
            crate::plugin::refresh_own_skill();
            crate::escapes::prune_hooked(&feeds);
            mark_hooked(&feeds, text(&start, "session_id").as_deref());
        }
        let on = crate::names::is_on(&feeds);
        let name = crate::names::start_context(&start, &feeds, on, on && tabs_own());
        if on {
            // A resumed one's row, should its text have moved.
            crate::names::keep_title(&feeds, &start);
        }
        // A resume after a reboot or a relaunch hears of it here.
        let away = text(&start, "session_id")
            .filter(|_| start.get("agent_id").is_none())
            .and_then(|s| away::on_call(&feeds, &s, manage::now_ms(), away::Call::Start));
        // A manager's compacted session is told it is one.
        let manager = crate::managers::start_context(&feeds, &start);
        if let Some(out) =
            context_reply("SessionStart", &[away.as_deref(), manager, name.as_deref()])
        {
            println!("{out}");
        }
        return 0;
    }
    // A session's own `Agent` call: maybe the nudge toward the manage
    // skill. A subagent's spawn goes on to the Bash check,
    // which has nothing to say about it.
    if let Some(spawn) = crate::manage_hint::spawn_of(&input) {
        if let Some(out) = crate::manage_hint::nudge(&spawn, &crate::feed::feed_dir()) {
            println!("{out}");
        }
        return 0;
    }
    if let Some(pre) = pre_tool(&input) {
        let feeds = crate::feed::feed_dir();
        mark_hooked(&feeds, pre.session.as_deref());
        // A task worker's report while shells of its own still run: held
        // back once.
        if pre.tool == HANDBACK_TOOL {
            if let Ok(payload) = serde_json::from_str::<Value>(&input)
                && let Some(out) = handback(
                    &payload,
                    &feeds,
                    manage::now_ms(),
                    &crate::bg_shells::System,
                )
            {
                println!("{out}");
            }
            return 0;
        }
        // Another session's work is refused; leaving the cap
        // only reminded of, and only for a call that runs.
        let refused = pre.session.as_deref().and_then(|session| {
            let w = crate::reach_guard::System::new(feeds.clone());
            reach(&pre, &w, || w.me(session))
        });
        let out = refused.or_else(|| {
            (pre.tool == "Bash")
                .then(|| {
                    guard(
                        pre.command.as_deref().unwrap_or_default(),
                        pre.session.as_deref(),
                        &feeds,
                        in_giverny_tab,
                    )
                })
                .flatten()
        });
        if let Some(out) = out {
            println!("{out}");
        }
        return 0;
    }
    if let Some(payload) = payload_of(&input) {
        let feeds = crate::feed::feed_dir();
        let now = manage::now_ms();
        // A subagent's call keeps its session's record and queues what it
        // saw for the parent; the parent's is told.
        let call = if payload.get("agent_id").is_some() {
            away::Call::Subagent
        } else {
            away::Call::Tool
        };
        let away = text(&payload, "session_id").and_then(|s| away::on_call(&feeds, &s, now, call));
        mark_hooked(&feeds, text(&payload, "session_id").as_deref());
        if let Some(out) = run_with(&payload, &feeds, now, in_giverny_tab(), away) {
            println!("{out}");
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prompt_reply_carries_every_context_given() {
        assert_eq!(prompt_reply(&[None, None]), None);
        let one: Value = serde_json::from_str(&prompt_reply(&[None, Some("b")]).unwrap()).unwrap();
        assert_eq!(
            one["hookSpecificOutput"]["hookEventName"],
            "UserPromptSubmit"
        );
        assert_eq!(one["hookSpecificOutput"]["additionalContext"], "b");
        let both: Value =
            serde_json::from_str(&prompt_reply(&[Some("a"), Some("b")]).unwrap()).unwrap();
        assert_eq!(both["hookSpecificOutput"]["additionalContext"], "a\n\nb");
    }

    /// What Claude Code sends after a dispatcher's `Agent` call.
    fn spawn_input(session: &str, agent: &str, desc: &str, background: bool) -> String {
        let status = if background {
            "async_launched"
        } else {
            "completed"
        };
        json!({"session_id": session, "hook_event_name": "PostToolUse", "tool_name": "Agent",
               "tool_input": {"description": desc, "prompt": "…"},
               "tool_response": {"isAsync": background, "status": status, "agentId": agent,
                                 "description": desc, "prompt": "…"}})
        .to_string()
    }

    #[test]
    fn a_spawn_is_read_from_the_agent_call_and_nothing_else() {
        let p = payload_of(&spawn_input("s1", "a1", "Classify chunk 0", true)).unwrap();
        assert_eq!(p["spawned_agent"], "a1");
        assert_eq!(p["spawned_description"], "Classify chunk 0");
        assert_eq!(p["spawned_background"], true);
        assert!(p.get("tool_response").is_none(), "the response is not kept");
        let p =
            payload_of(r#"{"session_id":"s1","tool_name":"Bash","tool_response":{"agentId":"x"}}"#)
                .unwrap();
        assert!(
            p.get("spawned_agent").is_none(),
            "only an Agent call spawns"
        );
        // A worker's own Agent call is a worker's call.
        let p = payload_of(
            r#"{"session_id":"s1","agent_id":"w","tool_name":"Agent","tool_response":{"agentId":"x"}}"#,
        )
        .unwrap();
        assert!(p.get("spawned_agent").is_none());
        // A response that is no object costs only the spawn.
        let p = payload_of(r#"{"session_id":"s1","tool_name":"Agent","tool_response":"done"}"#)
            .unwrap();
        assert_eq!(p["session_id"], "s1");
        assert!(p.get("spawned_agent").is_none());
    }
}
