//! The manager mark: which sessions are manager sessions.
//!
//! A manager writes the session's feed (`feed.rs`), and it marks the
//! session it runs in as a manager. The mark is what tells a manager from
//! any other session that wrote a row: any agent can write one, and a feed
//! row alone would open the management panel with no worker behind it.
//!
//! **What the mark decides.** The pane opens for a manager before its first
//! spawn, so its Next up shows, and draws its Next up rows, which no worker
//! stands behind yet; a session that is no manager gets a pane only from
//! real subagents ([`crate::feed::Drawn`]).
//!
//! **Where it lives.** Beside the feeds, never in one: the manager writes
//! `<feeds>/managers/<session>.json`, and may write the same under
//! `<feeds>/managers/roots/<root>.json`, keyed by the conversation's root
//! ([`crate::subagents::conversation_root`]). A resumed or compacted
//! session has a new id but the same root, so it is found there. Giverny
//! only reads them; what the files hold is the writer's.

use std::path::{Path, PathBuf};

/// The marks directory under the feed directory.
pub const DIR: &str = "managers";

/// `<feeds>/managers`.
pub fn dir(feeds: &Path) -> PathBuf {
    feeds.join(DIR)
}

/// An id usable as a file name.
fn safe(id: &str) -> bool {
    !id.is_empty() && !id.contains(['/', '\\']) && !id.starts_with('.')
}

/// `<feeds>/managers/<session>.json`.
pub fn session_file(feeds: &Path, session: &str) -> Option<PathBuf> {
    safe(session).then(|| dir(feeds).join(format!("{session}.json")))
}

/// `<feeds>/managers/roots/<root>.json`.
pub fn root_file(feeds: &Path, root: &str) -> Option<PathBuf> {
    safe(root).then(|| dir(feeds).join("roots").join(format!("{root}.json")))
}

/// Is `session` a manager: marked under its own id, else under one of its
/// earlier ids (`aliases`, a tab's tracker keeps them), else under its
/// conversation's root, which `root` gives. `root` is asked only when no id
/// is marked, as it reads the transcript's head. A `stat` per id.
pub fn is_marked(
    feeds: &Path,
    session: &str,
    aliases: &[&str],
    root: impl FnOnce() -> Option<String>,
) -> bool {
    let marked = std::iter::once(session)
        .chain(aliases.iter().copied())
        .filter_map(|id| session_file(feeds, id))
        .any(|f| f.is_file());
    marked
        || root()
            .and_then(|r| root_file(feeds, &r))
            .is_some_and(|f| f.is_file())
}
