//! The manager mark: which sessions are manager sessions.
//!
//! A session is a manager once the manage skill ran in it — never because it
//! wrote rows. Any agent can leave itself a note to run `giverny manage
//! start`, and a feed row alone once opened the management panel with no
//! worker behind it; so the mark comes from the skill being run, which the
//! plugin's hooks see without the skill's text having to say anything:
//!
//! - the `PostToolUse` hook (matcher `*`) sees the model call the `Skill`
//!   tool with `giverny:manage` ([`skill_call`]);
//! - the `UserPromptSubmit` hook sees the person type `/giverny:manage`,
//!   which Claude Code expands into the skill's text with no `Skill` call
//!   ([`typed_skill`]).
//!
//! A subagent that runs the skill marks nobody: its session id is its
//! dispatcher's.
//!
//! **What the mark decides.** The pane opens for a manager before its first
//! spawn, so its Next up shows, and draws a manager's own rows no worker
//! stands behind (Next up, its build rows); a session that is no manager
//! gets a pane only from real subagents ([`crate::feed::Drawn`]). Ask
//! [`is_manager`] (or [`is_marked`], given the ids and root already).
//!
//! **Where it lives.** Beside the feeds, never in one, as names do
//! ([`crate::names`]): `<feeds>/managers/<session>.json` holds `{session,
//! root}`, and `<feeds>/managers/roots/<root>.json` the same, keyed by the
//! conversation's root ([`crate::subagents::conversation_root`]). A resumed
//! or compacted session has a new id but the same root, so it is found
//! there and the mark copied under the new id. `/clear` starts a new root:
//! a cleared session is no manager until the skill runs again.
//!
//! **Told after a compaction.** A compaction's summary may drop that the
//! session runs the manage skill, and the skill's text is gone with it. So
//! the `SessionStart` hook of a manager's own compacted session carries
//! [`START_CONTEXT`] ([`start_context`]). A resume hears
//! nothing: its transcript already holds that line from its last
//! compaction.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// The marks directory under the feed directory.
pub const DIR: &str = "managers";

/// The skill names that are the manage skill: the plugin's, and its short
/// form.
const SKILL_NAMES: &[&str] = &["giverny:manage", "manage"];

/// `<feeds>/managers`.
pub fn dir(feeds: &Path) -> PathBuf {
    feeds.join(DIR)
}

/// An id usable as a file name.
fn safe(id: &str) -> bool {
    !id.is_empty() && !id.contains(['/', '\\']) && !id.starts_with('.')
}

fn session_file(feeds: &Path, session: &str) -> Option<PathBuf> {
    safe(session).then(|| dir(feeds).join(format!("{session}.json")))
}

fn root_file(feeds: &Path, root: &str) -> Option<PathBuf> {
    safe(root).then(|| dir(feeds).join("roots").join(format!("{root}.json")))
}

fn write_doc(file: &Path, doc: &Value) -> std::io::Result<()> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = file.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, format!("{doc}\n"))?;
    std::fs::rename(&tmp, file)
}

/// Mark `session` a manager, under its id and, when known, its
/// conversation's `root`.
pub fn mark(feeds: &Path, session: &str, root: Option<&str>) -> std::io::Result<()> {
    let Some(file) = session_file(feeds, session) else {
        return Ok(());
    };
    let mut doc = json!({"session": session});
    if let Some(r) = root {
        doc["root"] = json!(r);
    }
    write_doc(&file, &doc)?;
    if let Some(rf) = root.and_then(|r| root_file(feeds, r)) {
        write_doc(&rf, &doc)?;
    }
    Ok(())
}

/// Is `session` a manager: marked under its own id, else under one of its
/// earlier ids (`aliases`, a tab's tracker keeps them) or its conversation's
/// root, which `root` gives — asked only when the ids are not marked, as
/// it reads the transcript's head. Found by an alias or the root, the mark
/// is copied under `session`, so the next look is one `stat` again.
pub fn is_marked(
    feeds: &Path,
    session: &str,
    aliases: &[&str],
    root: impl FnOnce() -> Option<String>,
) -> bool {
    let Some(own) = session_file(feeds, session) else {
        return false;
    };
    if own.is_file() {
        return true;
    }
    let by_alias = aliases
        .iter()
        .filter_map(|a| session_file(feeds, a))
        .any(|f| f.is_file());
    let root = if by_alias {
        None
    } else {
        match root() {
            Some(r) if root_file(feeds, &r).is_some_and(|f| f.is_file()) => Some(r),
            _ => return false,
        }
    };
    let mut doc = json!({"session": session});
    if let Some(r) = &root {
        doc["root"] = json!(r);
    }
    let _ = write_doc(&own, &doc);
    true
}

/// Is `session` a manager ([`is_marked`]): its own mark, else the one its
/// conversation — the root its `transcript` holds — was marked under.
pub fn is_manager(feeds: &Path, session: &str, transcript: Option<&Path>) -> bool {
    is_marked(feeds, session, &[], || {
        crate::subagents::conversation_root(transcript?)
    })
}

/// What a manager session hears when it starts again from a compaction
/// ([`start_context`]).
pub const START_CONTEXT: &str = "Giverny: this is a manager session (giverny:manage), \
continued from a compaction, and the manage skill's rules are no longer in \
context. Before acting, call the Skill tool with `giverny:manage` to reload them; then \
check `giverny-manage` rows and the workers really running against what the summary says.";

/// The context a `SessionStart` payload's session hears for being a
/// manager: [`START_CONTEXT`] when its `source` is `compact`, it is the
/// session's own start (no `agent_id`), and the session is a manager
/// ([`is_manager`], which finds a new id by its conversation's root and
/// copies the mark under it). Never for `startup`, `clear` or `resume` (a
/// resumed transcript already holds the line from its last compaction).
pub fn start_context(feeds: &Path, payload: &Value) -> Option<&'static str> {
    let source = payload.get("source").and_then(Value::as_str)?;
    if source != "compact" || payload.get("agent_id").is_some() {
        return None;
    }
    let session = payload
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let transcript = payload
        .get("transcript_path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(Path::new);
    is_manager(feeds, session, transcript).then_some(START_CONTEXT)
}

/// The skill a `Skill` tool call ran, from its `tool_input`.
pub fn skill_of(tool_input: &Value) -> Option<&str> {
    tool_input
        .get("skill")
        .and_then(Value::as_str)
        .map(str::trim)
}

/// Is `skill` (a `Skill` call's, or a typed command's) the manage skill?
pub fn is_manage_skill(skill: &str) -> bool {
    let s = skill.trim().trim_start_matches('/');
    SKILL_NAMES.contains(&s)
}

/// A hook payload ([`crate::plugin_hook::payload_of`]) that is the session's
/// own `Skill` call running the manage skill: the payload carries the
/// skill as `skill`, and no `agent_id` (a subagent's call marks nobody).
pub fn skill_call(payload: &Value) -> bool {
    payload.get("agent_id").is_none()
        && payload.get("tool_name").and_then(Value::as_str) == Some("Skill")
        && payload
            .get("skill")
            .and_then(Value::as_str)
            .is_some_and(is_manage_skill)
}

/// A prompt the person typed that runs the manage skill itself:
/// `/giverny:manage …`. (`/manage` is no command of its own: the prompt hook
/// sends it to the skill, whose `Skill` call marks the session.)
pub fn typed_skill(prompt: &str) -> bool {
    prompt.split_whitespace().next() == Some("/giverny:manage")
}

/// Mark the session a hook payload is for (`session_id`), with the root of
/// its `transcript_path` when it has one.
pub fn mark_from(feeds: &Path, payload: &Value) {
    let text = |k: &str| {
        payload
            .get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let Some(session) = text("session_id").filter(|s| safe(s)) else {
        return;
    };
    let root =
        text("transcript_path").and_then(|t| crate::subagents::conversation_root(Path::new(t)));
    if let Err(e) = mark(feeds, session, root.as_deref()) {
        tracing::debug!("manager mark for {session} not written: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_sessions_own_manage_skill_call_marks_it() {
        let call = |skill: &str, agent: Option<&str>| {
            let mut p = json!({"session_id": "s1", "tool_name": "Skill", "skill": skill});
            if let Some(a) = agent {
                p["agent_id"] = json!(a);
            }
            p
        };
        assert!(skill_call(&call("giverny:manage", None)));
        assert!(skill_call(&call("manage", None)));
        assert!(
            !skill_call(&call("giverny:manage", Some("a1"))),
            "a subagent's"
        );
        assert!(!skill_call(&call("simplify", None)));
        assert!(!skill_call(
            &json!({"session_id": "s1", "tool_name": "Bash", "skill": "giverny:manage"})
        ));
        assert!(typed_skill("/giverny:manage task 12"));
        assert!(typed_skill("/giverny:manage"));
        assert!(!typed_skill("please /giverny:manage"));
        assert!(
            !typed_skill("/manage this"),
            "sent to the skill, whose call marks it"
        );
    }
}
