//! A nudge toward the manage skill when a session's subagent looks like task
//! work.
//!
//! A session that hands a task to a general-purpose subagent — edit this,
//! build that, commit — gets none of what the manage skill gives it: no row
//! in the management panel, no ETA, no place in the build queue. So the
//! plugin's `PreToolUse` hook on the `Agent` tool (named `Task` in older
//! Claude Code) adds non-blocking context to such a call, once per session,
//! saying the skill is there ([`CONTEXT`]). `PreToolUse`, not `PostToolUse`:
//! Claude Code takes `additionalContext` there too (measured on 2.1.296), and
//! it reaches the model with the call it rides on, so the session can switch
//! before its next spawn.
//!
//! **When it fires** ([`nudge`]): on the session's own call (no `agent_id`)
//! starting a worker — a general-purpose or fork agent, or one with no type,
//! which Claude Code starts as general-purpose — that shows task work:
//! - its prompt asks it to edit, commit, build or test ([`asks_for_changes`]),
//!   or it runs in a worktree (`isolation: worktree`) or in the background;
//! - or it is the session's second worker, whatever its prompt (a fan-out).
//!
//! **When it stays silent**: for read-only helpers (Explore, Plan,
//! claude-code-guide, statusline-setup, and any other named type); for a
//! subagent's own spawn, which carries an `agent_id` — a workflow's agents
//! are such subagents, started by the workflow and never by a session's own
//! `Agent` call; for an agent started inside another skill or command, whose
//! turn ran a `Skill` call or began as a typed `/command` ([`in_skill`]); and
//! for a manager session ([`crate::managers`]), which runs the
//! skill already. A session is told at most once: `<feeds>/manage-hint/
//! <session>.json` beside the feeds records its workers seen and whether it
//! was told.
//!
//! It is registered only while the manage skill is in
//! (`management_panel.manage_skill`): with the skill off there is nothing to
//! point at ([`crate::plugin`]).

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// What the session is told.
pub const CONTEXT: &str = "Giverny: this looks like task work for a subagent. The manage skill \
(`giverny:manage`, or `/giverny:manage`) runs it with rows in the management panel, ETAs and the \
build queue; invoke it with the Skill tool before you spawn more. Quick lookups don't need it.";

/// The markers directory under the feed directory.
pub const DIR: &str = "manage-hint";

/// How much of a transcript's tail [`in_skill`] reads looking for the
/// turn's start: a turn longer than this is taken for no skill's.
const TAIL: u64 = 4 << 20;

/// A session's own `Agent` call, as its `PreToolUse` payload gives it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Spawn {
    pub session: String,
    /// The session's transcript: its conversation's root (the manager
    /// mark) and its current turn ([`in_skill`]).
    pub transcript: Option<PathBuf>,
    /// `subagent_type`; `None` when the call names none.
    pub kind: Option<String>,
    pub prompt: String,
    pub background: bool,
    /// `isolation` (`"worktree"`), when given.
    pub isolation: Option<String>,
}

/// A `PreToolUse` payload that is a session's own `Agent` (or `Task`) call;
/// `None` for any other payload, and for a subagent's spawn.
pub fn spawn_of(input: &str) -> Option<Spawn> {
    #[derive(serde::Deserialize)]
    struct In {
        subagent_type: Option<String>,
        prompt: Option<String>,
        run_in_background: Option<bool>,
        isolation: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct Pre {
        session_id: Option<String>,
        agent_id: Option<String>,
        transcript_path: Option<String>,
        hook_event_name: Option<String>,
        tool_name: Option<String>,
        tool_input: Option<In>,
    }
    if !input.contains("\"PreToolUse\"") {
        return None; // the other events carry a tool's whole response
    }
    let p: Pre = serde_json::from_str(input).ok()?;
    if p.hook_event_name.as_deref() != Some("PreToolUse")
        || !matches!(p.tool_name.as_deref(), Some("Agent" | "Task"))
        || p.agent_id.is_some_and(|a| !a.trim().is_empty())
    {
        return None;
    }
    let i = p.tool_input?;
    let nonempty = |s: Option<String>| s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    Some(Spawn {
        session: nonempty(p.session_id)?,
        transcript: nonempty(p.transcript_path).map(PathBuf::from),
        kind: nonempty(i.subagent_type),
        prompt: i.prompt.unwrap_or_default(),
        background: i.run_in_background == Some(true),
        isolation: nonempty(i.isolation),
    })
}

/// Whether the spawn starts a worker — a general-purpose or fork agent, or
/// one with no type — rather than a read-only or specialised helper.
fn is_worker(spawn: &Spawn) -> bool {
    matches!(
        spawn.kind.as_deref(),
        None | Some("general-purpose" | "fork")
    )
}

/// Whether the spawn shows task work on its own: run in a worktree or in
/// the background, or a prompt asking for changes.
fn task_shaped(spawn: &Spawn) -> bool {
    spawn.isolation.as_deref() == Some("worktree")
        || spawn.background
        || asks_for_changes(&spawn.prompt)
}

/// Words a prompt asking a subagent to edit, commit, build or test uses.
const CHANGE_WORDS: &[&str] = &[
    "edit",
    "edits",
    "editing",
    "commit",
    "commits",
    "committing",
    "build",
    "builds",
    "building",
    "rebuild",
    "test",
    "tests",
    "testing",
    "fix",
    "fixing",
    "implement",
    "implementing",
    "refactor",
    "refactoring",
    "modify",
    "modifying",
];

/// Phrases that make a prompt read-only whatever else it says: a helper
/// told "do not edit anything" is a lookup.
const READ_ONLY: &[&str] = &[
    "read-only",
    "read only",
    "do not edit",
    "don't edit",
    "dont edit",
    "no edits",
    "without editing",
    "do not modify",
    "don't modify",
    "without modifying",
    "make no changes",
    "do not change any",
    "don't change any",
];

/// Whether `prompt` asks its subagent to edit, commit, build or test.
pub fn asks_for_changes(prompt: &str) -> bool {
    let p = prompt.to_lowercase().replace('’', "'");
    if READ_ONLY.iter().any(|r| p.contains(r)) {
        return false;
    }
    p.split(|c: char| !c.is_alphanumeric())
        .any(|w| CHANGE_WORDS.contains(&w))
}

/// Whether the session's current turn runs inside a skill or command: it
/// began as a typed `/command`, or the session called the `Skill` tool in
/// it. Read back from the transcript's tail to the turn's prompt — the
/// last user line that is no tool result and no injected text (`isMeta`:
/// a skill's own text, a subagent's hand-back).
pub fn in_skill(transcript: &Path) -> bool {
    let Some(tail) = tail(transcript) else {
        return false;
    };
    for line in tail.lines().rev() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let content = &v["message"]["content"];
        match v.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                let skill = content.as_array().is_some_and(|blocks| {
                    blocks
                        .iter()
                        .any(|b| b["type"] == "tool_use" && b["name"].as_str() == Some("Skill"))
                });
                if skill {
                    return true;
                }
            }
            Some("user") => {
                if v.get("isMeta").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                let text = match content {
                    Value::String(s) => s.clone(),
                    Value::Array(blocks) => {
                        if blocks.iter().any(|b| b["type"] == "tool_result") {
                            continue;
                        }
                        blocks
                            .iter()
                            .filter_map(|b| b["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("\n")
                    }
                    _ => continue,
                };
                return text.contains("<command-name>");
            }
            _ => {}
        }
    }
    false
}

/// The last [`TAIL`] bytes of `path`, from its first whole line.
fn tail(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    let from = if start == 0 {
        0
    } else {
        buf.iter()
            .position(|&c| c == b'\n')
            .map_or(buf.len(), |i| i + 1)
    };
    Some(String::from_utf8_lossy(&buf[from..]).into_owned())
}

/// `<feeds>/manage-hint/<session>.json`; `None` for an id unfit for a file
/// name.
fn marker(feeds: &Path, session: &str) -> Option<PathBuf> {
    let safe = !session.is_empty() && !session.contains(['/', '\\']) && !session.starts_with('.');
    safe.then(|| feeds.join(DIR).join(format!("{session}.json")))
}

/// The hook's answer to a session's `Agent` call: the nudge as a
/// `PreToolUse` reply, or nothing (see the module doc for when). Each
/// worker it counts is recorded in the session's marker, and the nudge
/// once given is never given again.
pub fn nudge(spawn: &Spawn, feeds: &Path) -> Option<String> {
    if !is_worker(spawn) {
        return None;
    }
    let file = marker(feeds, &spawn.session)?;
    let mut seen: Value = std::fs::read(&file)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| json!({"session": spawn.session, "workers": 0}));
    if seen["told"].as_bool() == Some(true) {
        return None;
    }
    if crate::managers::is_manager(feeds, &spawn.session, spawn.transcript.as_deref()) {
        return None;
    }
    if spawn.transcript.as_deref().is_some_and(in_skill) {
        return None; // the skill or command running decides how it spawns
    }
    let workers = seen["workers"].as_u64().unwrap_or(0) + 1;
    let tell = task_shaped(spawn) || workers >= 2;
    seen["workers"] = json!(workers);
    if tell {
        seen["told"] = json!(true);
    }
    if let Err(e) = write(&file, &seen) {
        tracing::debug!("manage hint marker for {} not written: {e}", spawn.session);
        return None; // unrecorded, it could be told again: say nothing
    }
    tell.then(reply)
}

fn write(file: &Path, doc: &Value) -> std::io::Result<()> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = file.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, format!("{doc}\n"))?;
    std::fs::rename(&tmp, file)
}

/// The `PreToolUse` reply carrying [`CONTEXT`]: context only, no
/// permission decision, so the call runs as it would have.
fn reply() -> String {
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "additionalContext": CONTEXT
        }
    })
    .to_string()
}
