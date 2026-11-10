//! One conversation across session ids.
//!
//! Claude Code gives a conversation a new session id mid-run — a resume,
//! the agents view's switch, the move into a background host — and copies
//! its records forward, so the transcript's root record
//! ([`crate::subagents::conversation_root`]) stays the same where the id
//! does not. The files Giverny keys by session id (a manager
//! session's feed, the agent-ETA store) find their conversation again by
//! that root: each records its `root`, and a session with no file of its
//! own adopts the one whose root is its own, the id it had kept in
//! `aliases` ([`adopt`]). `/clear` starts a new root, and so a new file.
//!
//! The documents share three fields: `session` (the id it is for now),
//! `aliases` (the ids it was for before) and `root`.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::{Value, json};

use crate::subagents::{conversation_root, session_transcript};

/// The Claude config dir this process's session runs under:
/// `$CLAUDE_CONFIG_DIR`, else a background job's own account, else
/// `$GIVERNY_PROFILE_DIR` (not inside a job, where it is the daemon's), else
/// `~/.claude`.
pub fn config_dir() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(crate::lineage::job_account)
        .or_else(|| crate::lineage::giverny_var("GIVERNY_PROFILE_DIR").map(PathBuf::from))
        .or_else(|| dirs::home_dir().map(|h| h.join(".claude")))
}

/// The conversation `session` holds: its transcript's root, under `cfg`
/// (else [`config_dir`]). `None` when the transcript cannot be found or has
/// no turn yet.
pub fn root_of(cfg: Option<&Path>, session: &str) -> Option<String> {
    let cfg = cfg.map(Path::to_path_buf).or_else(config_dir)?;
    conversation_root(&session_transcript(&cfg, session)?)
}

/// Every id `doc` is for: its session, then its aliases.
pub fn ids(doc: &Value) -> Vec<String> {
    let session = doc.get("session").and_then(Value::as_str);
    let aliases = doc
        .get("aliases")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str);
    session
        .into_iter()
        .chain(aliases)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// Is `doc` for `session`, by its own id or by an alias?
pub fn names(doc: &Value, session: &str) -> bool {
    ids(doc).iter().any(|s| s == session)
}

/// The conversation `doc` is for: the root it records, else the first of
/// its ids whose transcript has one (a file written before roots were).
fn root_of_doc(doc: &Value, cfg: Option<&Path>) -> Option<String> {
    if let Some(r) = doc.get("root").and_then(Value::as_str) {
        return Some(r.to_string());
    }
    ids(doc).iter().find_map(|sid| root_of(cfg, sid))
}

/// The `.json` file in `dir` that holds `root`'s conversation under other
/// ids — the newest, if several do — among the documents `accept` takes.
/// What a session with no file of its own adopts.
pub fn continued(
    dir: &Path,
    session: &str,
    root: &str,
    cfg: Option<&Path>,
    accept: impl Fn(&Value) -> bool,
) -> Option<PathBuf> {
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue; // `.tmp` files mid-write, and anything else
        }
        let Some(doc) = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .filter(Value::is_object)
        else {
            continue;
        };
        if names(&doc, session) || !accept(&doc) {
            continue;
        }
        if root_of_doc(&doc, cfg).as_deref() != Some(root) {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if best.as_ref().is_none_or(|(t, _)| mtime > *t) {
            best = Some((mtime, path));
        }
    }
    best.map(|(_, p)| p)
}

/// Make `doc` `session`'s: it becomes the document's `session`, and the id
/// it was for joins its `aliases` (oldest first). A no-op for a document
/// already for it.
pub fn adopt(doc: &mut Value, session: &str) {
    let Some(obj) = doc.as_object_mut() else {
        return;
    };
    let before = obj.get("session").and_then(Value::as_str).map(String::from);
    if before.as_deref() == Some(session) {
        return;
    }
    let mut aliases: Vec<String> = obj
        .get("aliases")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(String::from)
        .filter(|a| a != session)
        .collect();
    if let Some(b) = before.filter(|b| !b.is_empty() && !aliases.contains(b)) {
        aliases.push(b);
    }
    obj.insert("session".into(), json!(session));
    if aliases.is_empty() {
        obj.remove("aliases");
    } else {
        obj.insert("aliases".into(), json!(aliases));
    }
}

/// Record the conversation's root on `doc`, once: a root already there
/// stands.
pub fn stamp_root(doc: &mut Value, root: Option<&str>) {
    if let (Some(obj), Some(r)) = (doc.as_object_mut(), root)
        && !obj.contains_key("root")
    {
        obj.insert("root".into(), json!(r));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A Claude config dir holding `session`'s transcript, rooted at `root`.
    pub(crate) fn transcript(cfg: &Path, session: &str, root: &str) {
        let dir = cfg.join("projects").join("-w");
        std::fs::create_dir_all(&dir).unwrap();
        let line = json!({"type": "user", "uuid": root, "parentUuid": null,
                          "message": {"role": "user", "content": "go"}});
        std::fs::write(dir.join(format!("{session}.jsonl")), format!("{line}\n")).unwrap();
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("giverny-cont-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_continued_file_is_found_by_root_recorded_or_read() {
        let dir = temp("find");
        let cfg = dir.join("cfg");
        let feeds = dir.join("feeds");
        std::fs::create_dir_all(&feeds).unwrap();
        transcript(&cfg, "old", "r1");
        transcript(&cfg, "other", "r9");
        // One records its root; one predates roots and is read by its id.
        std::fs::write(feeds.join("a.json"), r#"{"session":"x","root":"r1"}"#).unwrap();
        std::fs::write(feeds.join("b.json"), r#"{"session":"other"}"#).unwrap();
        let found = continued(&feeds, "new", "r1", Some(&cfg), |_| true);
        assert_eq!(found, Some(feeds.join("a.json")));
        std::fs::remove_file(feeds.join("a.json")).unwrap();
        std::fs::write(feeds.join("c.json"), r#"{"session":"old"}"#).unwrap();
        let found = continued(&feeds, "new", "r1", Some(&cfg), |_| true);
        assert_eq!(found, Some(feeds.join("c.json")));
        assert_eq!(continued(&feeds, "new", "r1", Some(&cfg), |_| false), None);
        assert_eq!(continued(&feeds, "new", "r7", Some(&cfg), |_| true), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
