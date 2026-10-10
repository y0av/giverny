//! Claude Code's live session registry: `$CONFIG_DIR/sessions/<pid>.json`.
//!
//! Written by Claude Code itself (verified against 2.1.220): gives per-session
//! status with zero hook setup. Stale files are never cleaned up upstream, so
//! PID liveness gating is mandatory.

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct SessionEntry {
    pub pid: u32,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(default)]
    pub cwd: PathBuf,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub status: String,
    #[serde(rename = "statusUpdatedAt", default)]
    pub status_updated_at: u64,
    /// Session start, ms since epoch. Compared against `settings.json`'s
    /// mtime to tell whether this session loaded Giverny's hooks.
    #[serde(rename = "startedAt", default)]
    pub started_at_ms: u64,
}

impl SessionEntry {
    /// The agent is working: thinking, or running a tool call. Measured, not
    /// assumed — a session stays `busy` through minutes of back-to-back Bash
    /// calls.
    pub fn busy(&self) -> bool {
        self.status == "busy"
    }

    /// A background shell is alive while the agent itself is at its prompt —
    /// a `run_in_background` command, or one that outlived its timeout and
    /// was moved to the background.
    ///
    /// Claude Code's own session list counts this as "working", which is a
    /// fair answer to "is anything happening in there" and the wrong one for
    /// a spinner: the agent is waiting on *you*, often with a question, while
    /// something polls in the background. Marked, not animated.
    pub fn background_shell(&self) -> bool {
        self.status == "shell"
    }

    /// Claude is blocked on the user. Same bucket Claude Code puts it in, and
    /// the reason a session waiting on a permission prompt used to read as
    /// idle here when no hook reported it.
    pub fn waiting(&self) -> bool {
        self.status == "waiting"
    }
}

#[derive(Debug, Clone)]
pub struct LiveSession {
    pub entry: SessionEntry,
    pub config_dir: PathBuf,
}

/// Is the session behind this entry still running?
///
/// A pid only means something on the machine that issued it. An account
/// inside WSL, read from Windows, hands us Linux pids: checking those against
/// the Windows process table is not a weaker answer, it is an unrelated one.
/// The distribution is asked instead, by the sweep that already walks its
/// `/proc`, and this reads what that sweep left behind.
fn entry_is_live(config_dir: &Path, entry: &SessionEntry) -> bool {
    if crate::wsl::is_wsl_path(config_dir) {
        // The pids in there are the distribution's, and the last sweep asked
        // it which ones exist. Not knowing means no: an unanswered question
        // used to read as "still running", which is the answer that refuses
        // to bring a conversation back — and it was wrong for the two minutes
        // after a restart, which is precisely when the app restarts.
        return crate::wsl::pid_alive_in(config_dir, entry.pid).unwrap_or(false);
    }
    pid_alive(entry.pid)
}

fn pid_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        // macOS/BSD: signal 0 probes existence without delivering anything.
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(windows)]
    {
        use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
        let mut sys = System::new();
        sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
            true,
            ProcessRefreshKind::nothing(),
        );
        sys.process(Pid::from_u32(pid)).is_some()
    }
}

/// Scan the registries of every config dir for live sessions.
pub fn scan(config_dirs: impl IntoIterator<Item = PathBuf>) -> Vec<LiveSession> {
    let mut out = Vec::new();
    for dir in config_dirs {
        let sessions = dir.join("sessions");
        let Ok(entries) = std::fs::read_dir(&sessions) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let Ok(entry) = serde_json::from_slice::<SessionEntry>(&bytes) else {
                continue;
            };
            if entry_is_live(&dir, &entry) {
                out.push(LiveSession {
                    entry,
                    config_dir: dir.clone(),
                });
            }
        }
    }
    out
}

/// Is this claude session currently live in ANY of the given config dirs?
/// (Resuming it twice would interleave two writers into one transcript.)
pub fn session_is_live(config_dirs: impl IntoIterator<Item = PathBuf>, session_id: &str) -> bool {
    scan(config_dirs)
        .iter()
        .any(|s| s.entry.session_id == session_id)
}

/// Locate a session's transcript inside one config dir:
/// `projects/<munged-cwd>/<session_id>.jsonl`. The munging is lossy, so we
/// scan project dirs instead of reconstructing it.
pub fn find_transcript(config_dir: &Path, session_id: &str) -> Option<PathBuf> {
    let projects = config_dir.join("projects");
    for entry in std::fs::read_dir(projects).ok()?.flatten() {
        let candidate = entry.path().join(format!("{session_id}.jsonl"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// The working directory a transcript's conversation ran in — the *only*
/// directory `claude --resume` will find it from. Early lines carry a `cwd`
/// field (format is internal; we scan a bounded prefix and tolerate misses).
pub fn transcript_cwd(path: &Path) -> Option<PathBuf> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    for line in reader.lines().take(50) {
        let Ok(line) = line else { break };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if let Some(cwd) = value.get("cwd").and_then(|c| c.as_str())
            && !cwd.is_empty()
        {
            return Some(PathBuf::from(cwd));
        }
    }
    None
}

/// Claude's project-dir name for a cwd: every non-alphanumeric byte becomes
/// `-`, case preserved (verified against 2.1.220 layouts).
pub fn munge_cwd(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// A past conversation in some project dir, for the resume picker.
#[derive(Debug, Clone)]
pub struct PastSession {
    pub id: String,
    /// AI title / last prompt (best-effort from the transcript tail).
    pub title: String,
    pub path: PathBuf,
    pub config_dir: PathBuf,
    pub modified: Option<std::time::SystemTime>,
    /// Currently open in some terminal — resuming would corrupt it.
    pub live: bool,
}

/// List past sessions for `cwd` across config dirs, newest first (capped).
pub fn list_sessions(config_dirs: &[PathBuf], cwd: &Path) -> Vec<PastSession> {
    use std::collections::HashSet;
    let live_ids: HashSet<String> = scan(config_dirs.iter().cloned())
        .into_iter()
        .map(|s| s.entry.session_id)
        .collect();
    let munged = munge_cwd(cwd);
    let mut out: Vec<PastSession> = Vec::new();
    for dir in config_dirs {
        let proj = dir.join("projects").join(&munged);
        let Ok(entries) = std::fs::read_dir(&proj) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(id) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            if id.len() != 36 {
                continue;
            }
            let modified = e.metadata().ok().and_then(|m| m.modified().ok());
            out.push(PastSession {
                live: live_ids.contains(&id),
                id,
                title: String::new(),
                path,
                config_dir: dir.clone(),
                modified,
            });
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.modified));
    out.truncate(15);
    for s in &mut out {
        s.title = tail_title(&s.path).unwrap_or_else(|| s.id[..8].to_string());
    }
    out
}

/// Best-effort session title from the transcript's tail: the last `aiTitle`
/// line, else the last `lastPrompt` (truncated).
fn tail_title(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    const TAIL: u64 = 128 * 1024;
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(TAIL))).ok()?;
    let mut buf = String::new();
    file.take(TAIL).read_to_string(&mut buf).ok()?;

    let mut title: Option<String> = None;
    let mut prompt: Option<String> = None;
    for line in buf.lines() {
        if !line.contains("\"aiTitle\"") && !line.contains("\"lastPrompt\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(t) = v.get("aiTitle").and_then(|t| t.as_str()) {
            title = Some(t.to_string());
        } else if let Some(p) = v.get("lastPrompt").and_then(|p| p.as_str())
            && !is_injected_prompt(p)
        {
            prompt = Some(p.to_string());
        }
    }
    let mut best = title.or(prompt)?;
    best = best.replace(['\n', '\r'], " ");
    if best.chars().count() > 60 {
        best = best.chars().take(59).collect::<String>() + "…";
    }
    (!best.is_empty()).then_some(best)
}

/// The last prompt the user sent in a transcript: the last of
/// [`prompt_history`].
pub fn last_prompt(path: &Path) -> Option<String> {
    prompt_history(path).pop()
}

/// The prompts the user sent in a transcript, oldest first, for a session
/// whose prompts no hook reported: one adopted mid-way, or resumed after a
/// restart. Read from the tail, so a very long session gives its later turns.
///
/// The prompts are the user messages that are not tool results, notes Claude
/// Code adds (`isMeta`, compaction summaries), turns someone else sent (an
/// `origin` other than `human`, or text [`is_injected_prompt`] knows) or
/// things it wraps in a tag (a command, `!` shell input). A message typed
/// while a turn ran, which Claude Code files as a `queued_command`
/// attachment rather than a user message, is one of them, and so is one that
/// starts with a paste, listed without the paste's tags.
///
/// The last one is checked against Claude Code's own `lastPrompt` marker,
/// which is cut at 200 characters with an ellipsis and has its line breaks
/// flattened. The message the marker agrees with is taken in full; when none
/// does, the marker itself ends the list: it is the one that says which
/// message was typed (a `!` command's, say), and a cut prompt beats none.
pub fn prompt_history(path: &Path) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    // Long answers with tool output in them push the messages that started
    // their turns a long way back.
    const TAIL: u64 = 2 * 1024 * 1024;
    let Some(buf) = (|| {
        let mut file = std::fs::File::open(path).ok()?;
        let len = file.metadata().ok()?.len();
        file.seek(SeekFrom::Start(len.saturating_sub(TAIL))).ok()?;
        let mut bytes = Vec::new();
        file.take(TAIL).read_to_end(&mut bytes).ok()?;
        // The seek can land inside a character; only the first line is cut.
        Some(String::from_utf8_lossy(&bytes).into_owned())
    })() else {
        return Vec::new();
    };

    // The marker flattens line breaks and drops a paste's tags; whitespace
    // runs are where the two spellings differ.
    let squash = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let flag =
        |v: &serde_json::Value, name: &str| v.get(name).and_then(|m| m.as_bool()) == Some(true);
    let mut marker: Option<String> = None;
    // Each message, and whether Claude Code says the user sent it.
    let mut typed: Vec<(String, bool)> = Vec::new();
    // Who sent a turn, when Claude Code says: `human` is the user, anything
    // else (`task-notification`, `peer`, `plugin`, `auto-continuation`) is not.
    let not_human = |v: &serde_json::Value| {
        v.get("origin")
            .and_then(|o| o.get("kind"))
            .and_then(|k| k.as_str())
            .is_some_and(|k| k != "human")
    };
    let human = |v: &serde_json::Value| {
        v.get("origin")
            .and_then(|o| o.get("kind"))
            .and_then(|k| k.as_str())
            == Some("human")
    };
    for line in buf.lines() {
        let has_marker = line.contains("\"lastPrompt\"");
        let queued = line.contains("\"queued_command\"");
        if !has_marker && !queued && !line.contains("\"user\"") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if has_marker {
            if let Some(p) = v.get("lastPrompt").and_then(|p| p.as_str())
                && !is_injected_prompt(p)
            {
                marker = Some(p.to_string());
            }
            continue;
        }
        let (text, by_human) = match v.get("type").and_then(|t| t.as_str()) {
            Some("user") if !flag(&v, "isMeta") && !flag(&v, "isCompactSummary") => {
                if not_human(&v) {
                    continue;
                }
                (
                    v.get("message").and_then(|m| user_text(m.get("content")?)),
                    human(&v),
                )
            }
            // A message typed while a turn ran: only a typed one.
            Some("attachment") => {
                let Some(a) = v.get("attachment").filter(|a| {
                    a.get("type").and_then(|t| t.as_str()) == Some("queued_command")
                        && a.get("origin")
                            .and_then(|o| o.get("kind"))
                            .and_then(|k| k.as_str())
                            == Some("human")
                        && !flag(a, "isMeta")
                }) else {
                    continue;
                };
                (a.get("prompt").and_then(user_text), true)
            }
            _ => continue,
        };
        if let Some(text) = text {
            let text = unwrap_pastes(&text).trim().to_string();
            if !text.is_empty() && !is_injected_prompt(&text) {
                typed.push((text, by_human));
            }
        }
    }
    // The last prompt: the message the marker names, in full.
    let last = marker.map(|m| {
        let stem = m.strip_suffix('…').filter(|_| m.chars().count() > 200);
        let (m_key, stem_key) = (squash(&m), stem.map(squash));
        typed
            .iter()
            .rev()
            .map(|(t, _)| t)
            .find(|t| {
                let t = squash(t);
                t == m_key || stem_key.as_ref().is_some_and(|stem| t.starts_with(stem))
            })
            .cloned()
            .unwrap_or_else(|| m.trim().to_string())
    });
    // What Claude Code wraps in a tag is its own (a command, `!` input, its
    // output). A turn the user sent can still start with `<` (pasted HTML);
    // one that does not say who sent it is taken for a wrapper.
    let mut prompts: Vec<String> = typed
        .into_iter()
        .filter(|(t, by_human)| !is_wrapper(t) && (*by_human || !t.starts_with('<')))
        .map(|(t, _)| t)
        .collect();
    // A marker no listed message agrees with names one Claude Code wrapped
    // (`!` shell input): it is the latest prompt.
    if let Some(last) = last.filter(|l| !l.is_empty())
        && !prompts.contains(&last)
    {
        prompts.push(last);
    }
    prompts
}

/// Whether a user-role turn is one Claude Code or another agent sent, not
/// something the user typed: a background task's `<task-notification>`, a
/// subagent's or peer's `<agent-message>` hand-back, another session's
/// `<cross-session-message>`, a teammate's or a channel's message, a
/// `<system-reminder>` Claude Code wrapped one in, a coordinator's message
/// to its subagent, a plugin's message, the "usage limit has reset" nudge,
/// background agents the user stopped, an interrupted request. Known by its
/// text alone, because the `UserPromptSubmit` hook fires for these turns too
/// and its payload (Claude Code 2.1.296) carries no `origin`; a transcript's
/// `origin` says it first.
pub fn is_injected_prompt(text: &str) -> bool {
    let text = text.trim_start();
    // Claude Code's envelopes, as `<tag>` or `<tag attr="…">`.
    const TAGS: &[&str] = &[
        "task-notification",
        "agent-message",
        "cross-session-message",
        "teammate-message",
        "channel-message",
        "channel",
        "system-reminder",
    ];
    if TAGS.iter().any(|t| opens_tag(text, t)) {
        return true;
    }
    const PREFIXES: &[&str] = &[
        // "…sent a message:" and "…sent a message while you were working:"
        "Another Claude session sent a message",
        "A peer session sent a message",
        "The coordinator sent a message",
        "[SYSTEM NOTIFICATION - NOT USER INPUT]",
        "Your claude.ai usage limit has reset.",
        "[Request interrupted by user",
    ];
    if PREFIXES.iter().any(|p| text.starts_with(p)) {
        return true;
    }
    let first = text.lines().next().unwrap_or("");
    // "The pass-spike plugin sent a message:"
    if first.starts_with("The ") && first.ends_with(" plugin sent a message:") {
        return true;
    }
    // "3 background agents were stopped by the user: …"
    let count = first.trim_start_matches(|c: char| c.is_ascii_digit());
    count.len() < first.len()
        && count.starts_with(" background ")
        && (count.contains(" were stopped by the user")
            || count.contains(" was stopped by the user"))
}

/// Whether `text` starts with the tag `name` opened: `<name>`, or `<name`
/// followed by whitespace and its attributes, so `channel` is not taken
/// for `<channels>`.
fn opens_tag(text: &str, name: &str) -> bool {
    text.strip_prefix('<')
        .and_then(|t| t.strip_prefix(name))
        .and_then(|t| t.chars().next())
        .is_some_and(|c| c == '>' || c.is_whitespace())
}

/// Whether a user message is one Claude Code wrote around something that was
/// not a prompt: a slash command, `!` shell input and its output, a local
/// command's output, editor context. Each tag seen in real transcripts.
fn is_wrapper(text: &str) -> bool {
    const TAGS: &[&str] = &[
        "<command-name>",
        "<command-message>",
        "<command-args>",
        "<local-command-",
        "<bash-input>",
        "<bash-stdout>",
        "<bash-stderr>",
        "<ide_",
        "<system-reminder>",
        "<user-prompt-submit-hook>",
    ];
    let text = text.trim_start();
    TAGS.iter().any(|t| text.starts_with(t))
}

/// A prompt with Claude Code's paste tags taken off, so it reads as typed:
/// a paste is filed as `<pasted_content id="d87c">\n…\n</pasted_content
/// id="d87c">` inside the message, and its `lastPrompt` marker has the text
/// without them.
pub fn unwrap_pastes(text: &str) -> String {
    const OPEN: &str = "<pasted_content";
    const CLOSE: &str = "</pasted_content";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let open = rest.find(OPEN);
        let close = rest.find(CLOSE);
        let (at, is_open) = match (open, close) {
            (Some(o), Some(c)) if c < o => (c, false),
            (Some(o), _) => (o, true),
            (None, Some(c)) => (c, false),
            (None, None) => break,
        };
        let Some(end) = rest[at..].find('>').map(|e| at + e + 1) else {
            break;
        };
        let mut before = &rest[..at];
        let mut after = &rest[end..];
        // The tag sits on a line of its own.
        if is_open {
            after = after.strip_prefix('\n').unwrap_or(after);
        } else {
            before = before.strip_suffix('\n').unwrap_or(before);
        }
        out.push_str(before);
        rest = after;
    }
    out.push_str(rest);
    out
}

/// The text of a user message, or `None` when it is a tool result. An
/// editor's context (the file open, the lines selected) arrives as items of
/// its own before what was typed, and is left out.
fn user_text(content: &serde_json::Value) -> Option<String> {
    if let Some(text) = content.as_str() {
        return Some(text.to_string());
    }
    let items = content.as_array()?;
    let mut parts = Vec::new();
    for item in items {
        match item.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                let text = item.get("text")?.as_str()?;
                if !text.trim_start().starts_with("<ide_") {
                    parts.push(text.to_string());
                }
            }
            Some("tool_result") => return None,
            _ => {}
        }
    }
    (!parts.is_empty()).then(|| parts.join("\n"))
}

/// Walk `/proc/<pid>/stat` parent links; true when `ancestor` is in the chain.
/// Maps a claude process to the Giverny tab whose shell spawned it.
#[cfg(target_os = "linux")]
pub fn has_ancestor(mut pid: u32, ancestor: u32) -> bool {
    for _ in 0..64 {
        if pid == ancestor {
            return true;
        }
        if pid <= 1 {
            return false;
        }
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        // Field 4 (ppid) comes after the parenthesized comm, which may itself
        // contain spaces/parens — split after the LAST ')'.
        let Some((_, rest)) = stat.rsplit_once(')') else {
            return false;
        };
        let mut fields = rest.split_whitespace();
        let _state = fields.next();
        let Some(ppid) = fields.next().and_then(|p| p.parse::<u32>().ok()) else {
            return false;
        };
        pid = ppid;
    }
    false
}

/// Same walk on non-Linux platforms, via `sysinfo`'s parent links.
#[cfg(not(target_os = "linux"))]
pub fn has_ancestor(pid: u32, ancestor: u32) -> bool {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
    let mut current = Pid::from_u32(pid);
    let target = Pid::from_u32(ancestor);
    for _ in 0..64 {
        if current == target {
            return true;
        }
        match sys.process(current).and_then(|p| p.parent()) {
            Some(parent) => current = parent,
            None => return false,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_registry_entry() {
        let json = r#"{ "pid": 1234, "sessionId": "da89-uuid", "cwd": "/home/u/dev",
            "startedAt": 1785250651905, "procStart": "50262977", "version": "2.1.220",
            "kind": "interactive", "entrypoint": "cli", "name": "dev-13",
            "nameSource": "derived", "status": "busy", "updatedAt": 1, "statusUpdatedAt": 2 }"#;
        let e: SessionEntry = serde_json::from_str(json).unwrap();
        assert_eq!(e.pid, 1234);
        assert_eq!(e.started_at_ms, 1785250651905);
        assert!(e.busy());
        assert_eq!(e.name.as_deref(), Some("dev-13"));
        assert_eq!(e.cwd, PathBuf::from("/home/u/dev"));
    }

    #[test]
    fn scan_filters_dead_pids() {
        let dir = std::env::temp_dir().join(format!("giverny-reg-{}", std::process::id()));
        let sessions = dir.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        // Our own pid = alive; pid 4194304+1 range = almost surely dead.
        let me = std::process::id();
        std::fs::write(
            sessions.join(format!("{me}.json")),
            format!(r#"{{"pid":{me},"sessionId":"alive","status":"idle"}}"#),
        )
        .unwrap();
        std::fs::write(
            sessions.join("4194301.json"),
            r#"{"pid":4194301,"sessionId":"dead","status":"busy"}"#,
        )
        .unwrap();
        let live = scan([dir.clone()]);
        assert_eq!(live.len(), 1, "{live:?}");
        assert_eq!(live[0].entry.session_id, "alive");
        assert!(session_is_live([dir.clone()], "alive"));
        assert!(!session_is_live([dir.clone()], "dead"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn munge_matches_claude_layout() {
        assert_eq!(
            munge_cwd(Path::new("/home/yoz/Dev/claude_test")),
            "-home-yoz-Dev-claude-test",
            "underscores and slashes become dashes, case preserved"
        );
        assert_eq!(
            munge_cwd(Path::new("/home/yoz/Dev/yoav.xyz.next")),
            "-home-yoz-Dev-yoav-xyz-next"
        );
    }

    #[test]
    fn lists_sessions_with_tail_titles() {
        let dir = std::env::temp_dir().join(format!("giverny-list-{}", std::process::id()));
        let cwd = Path::new("/home/u/proj_x");
        let proj = dir.join("projects").join(munge_cwd(cwd));
        std::fs::create_dir_all(&proj).unwrap();
        let sid_a = "aaaaaaaa-1111-2222-3333-444444444444";
        let sid_b = "bbbbbbbb-1111-2222-3333-444444444444";
        std::fs::write(
            proj.join(format!("{sid_a}.jsonl")),
            "{\"type\":\"ai-title\",\"aiTitle\":\"fix auth bug\",\"sessionId\":\"a\"}\n",
        )
        .unwrap();
        std::fs::write(
            proj.join(format!("{sid_b}.jsonl")),
            "{\"type\":\"last-prompt\",\"lastPrompt\":\"run the tests\",\"sessionId\":\"b\"}\n",
        )
        .unwrap();
        std::fs::write(proj.join("not-a-session.jsonl"), "junk").unwrap();

        let sessions = list_sessions(std::slice::from_ref(&dir), cwd);
        assert_eq!(sessions.len(), 2, "{sessions:?}");
        let a = sessions.iter().find(|s| s.id == sid_a).unwrap();
        assert_eq!(a.title, "fix auth bug");
        let b = sessions.iter().find(|s| s.id == sid_b).unwrap();
        assert_eq!(b.title, "run the tests");
        assert!(!a.live);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finds_transcript_and_its_cwd() {
        let dir = std::env::temp_dir().join(format!("giverny-transcript-{}", std::process::id()));
        let proj = dir.join("projects").join("-home-u-Dev-myproj");
        std::fs::create_dir_all(&proj).unwrap();
        let sid = "b263c7bf-2cc6-4ee1-b00a-948a4152f6ab";
        std::fs::write(
            proj.join(format!("{sid}.jsonl")),
            concat!(
                "{\"mode\":\"default\",\"sessionId\":\"x\",\"type\":\"mode\"}\n",
                "{\"type\":\"user\",\"cwd\":\"/home/u/Dev/myproj\",\"sessionId\":\"x\"}\n",
            ),
        )
        .unwrap();

        let found = find_transcript(&dir, sid).expect("transcript located");
        assert_eq!(
            transcript_cwd(&found),
            Some(PathBuf::from("/home/u/Dev/myproj")),
            "cwd read from early transcript lines"
        );
        assert!(find_transcript(&dir, "0000-not-there").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn transcript(name: &str, lines: &[serde_json::Value]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("giverny-prompt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}.jsonl"));
        let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(&path, body).unwrap();
        path
    }

    fn user(content: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"type": "user", "message": {"role": "user", "content": content}})
    }

    fn marker(prompt: &str) -> serde_json::Value {
        serde_json::json!({"type": "last-prompt", "lastPrompt": prompt, "sessionId": "s"})
    }

    #[test]
    fn last_prompt_is_the_full_message_the_marker_names() {
        let long = format!("first line\nsecond line {}", "x".repeat(300));
        // Claude Code's own marker: flattened and cut at 200 characters.
        let cut: String = long
            .replace('\n', " ")
            .chars()
            .take(200)
            .collect::<String>()
            + "…";
        let path = transcript(
            "full",
            &[
                user(serde_json::json!("an earlier prompt")),
                user(serde_json::json!(long)),
                user(serde_json::json!([{"type": "tool_result", "content": "output"}])),
                serde_json::json!({"type": "user", "isMeta": true,
                    "message": {"content": "<local-command-caveat>…"}}),
                user(serde_json::json!(
                    "<task-notification>done</task-notification>"
                )),
                serde_json::json!({"type": "assistant", "message": {"content": "sure"}}),
                marker(&cut),
            ],
        );
        assert_eq!(last_prompt(&path).as_deref(), Some(long.as_str()));
    }

    #[test]
    fn prompt_history_lists_the_typed_prompts_in_order() {
        let path = transcript(
            "history",
            &[
                user(serde_json::json!("first")),
                serde_json::json!({"type": "assistant", "message": {"content": "ok"}}),
                user(serde_json::json!([{"type": "tool_result", "content": "out"}])),
                user(serde_json::json!("<command-name>/model</command-name>")),
                serde_json::json!({"type": "user", "isCompactSummary": true,
                    "message": {"content": "This session is being continued"}}),
                user(serde_json::json!("second\nwith a second line")),
                marker("second with a second line"),
                user(serde_json::json!("third")),
                marker("third"),
            ],
        );
        assert_eq!(
            prompt_history(&path),
            vec!["first", "second\nwith a second line", "third"]
        );
        // `!` shell input: the wrapped message is not listed, the marker ends it.
        let path = transcript(
            "history-bash",
            &[
                user(serde_json::json!("first")),
                user(serde_json::json!("<bash-input>ls</bash-input>")),
                marker("!  ls"),
            ],
        );
        assert_eq!(prompt_history(&path), vec!["first", "!  ls"]);
        assert!(prompt_history(Path::new("/nonexistent/x.jsonl")).is_empty());
    }

    #[test]
    fn last_prompt_falls_back_to_the_marker() {
        // `!` bash mode: the message is wrapped, the marker is what was typed.
        let path = transcript(
            "bash",
            &[
                user(serde_json::json!("<bash-input>ls</bash-input>")),
                marker("!  ls"),
            ],
        );
        assert_eq!(last_prompt(&path).as_deref(), Some("!  ls"));

        // Text-and-image prompts arrive as an array.
        let path = transcript(
            "array",
            &[
                user(serde_json::json!([{"type": "text", "text": "look at this"},
                                         {"type": "image"}])),
                marker("look at this"),
            ],
        );
        assert_eq!(last_prompt(&path).as_deref(), Some("look at this"));

        // No marker: the last plain message.
        let path = transcript(
            "old",
            &[
                user(serde_json::json!("fix the build")),
                user(serde_json::json!("<command-name>/clear</command-name>")),
            ],
        );
        assert_eq!(last_prompt(&path).as_deref(), Some("fix the build"));

        let path = transcript("empty", &[serde_json::json!({"type": "mode"})]);
        assert_eq!(last_prompt(&path), None);
        assert_eq!(last_prompt(Path::new("/nonexistent/x.jsonl")), None);
    }

    /// A user-role turn as Claude Code writes one it took from someone else.
    fn sent(kind: &str, meta: bool, text: &str) -> serde_json::Value {
        serde_json::json!({"type": "user", "isMeta": meta, "origin": {"kind": kind},
            "promptSource": "system", "message": {"role": "user", "content": text}})
    }

    fn queued(kind: &str, prompt: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"type": "attachment", "attachment": {"type": "queued_command",
            "prompt": prompt, "commandMode": "prompt", "origin": {"kind": kind}}})
    }

    fn typed(text: &str) -> serde_json::Value {
        serde_json::json!({"type": "user", "origin": {"kind": "human"},
            "promptSource": "typed", "message": {"role": "user", "content": text}})
    }

    const NOTIFICATION: &str = "<task-notification>\n<task-id>bvfpg99dj</task-id>\n\
        <status>completed</status>\n<summary>Background command finished</summary>\n\
        </task-notification>";
    const HAND_BACK: &str = "Another Claude session sent a message:\n\
        <agent-message from=\"a587ea9422f15b56a\">\n[Subagent hand-back] The report \
        follows:\n  giverny#261 is finished.\n</agent-message>";
    const STOPPED: &str = "2 background agents were stopped by the user: \"giverny#21 drop \
        pane total row\", \"giverny#22 statusline token counts\"";

    #[test]
    fn turns_nobody_typed_are_not_prompts() {
        // Each kind seen in real transcripts, after the prompt the user typed.
        let kinds = [
            (
                "task-notification",
                sent("task-notification", false, NOTIFICATION),
            ),
            ("hand-back", sent("peer", true, HAND_BACK)),
            ("stopped agents", sent("task-notification", false, STOPPED)),
            (
                "plugin",
                sent(
                    "plugin",
                    false,
                    "The pass-spike plugin sent a message:\npass-spike: worker t3 asks.",
                ),
            ),
            (
                "usage reset",
                sent(
                    "auto-continuation",
                    true,
                    "Your claude.ai usage limit has reset. Continue the task you were working on.",
                ),
            ),
            (
                "interrupted",
                user(serde_json::json!("[Request interrupted by user]")),
            ),
            (
                "interrupted tool use",
                user(serde_json::json!(
                    "[Request interrupted by user for tool use]"
                )),
            ),
            // Delivered mid-turn as attachments instead.
            ("queued notification", {
                let mut q = queued("task-notification", serde_json::json!(NOTIFICATION));
                q["attachment"]["commandMode"] = serde_json::json!("task-notification");
                q
            }),
            ("queued hand-back", {
                let mut q = queued("peer", serde_json::json!(&HAND_BACK[39..]));
                q["attachment"]["isMeta"] = serde_json::json!(true);
                q
            }),
        ];
        for (name, turn) in kinds {
            let path = transcript(
                &format!("sent-{}", name.replace(' ', "-")),
                &[
                    typed("start the two workers"),
                    marker("start the two workers"),
                    turn.clone(),
                ],
            );
            assert_eq!(prompt_history(&path), ["start the two workers"], "{name}");

            // Older Claude Code writes no `origin`: the text alone tells.
            let mut bare = turn;
            if let Some(o) = bare.as_object_mut() {
                o.remove("origin");
                o.remove("isMeta");
            }
            let path = transcript(
                &format!("bare-{}", name.replace(' ', "-")),
                &[user(serde_json::json!("start the two workers")), bare],
            );
            assert_eq!(
                prompt_history(&path),
                ["start the two workers"],
                "bare {name}"
            );
        }
    }

    #[test]
    fn a_marker_naming_an_injected_turn_is_not_the_last_prompt() {
        // Seen live: Claude Code's `lastPrompt` named the untagged notice
        // that background agents were stopped.
        let path = transcript(
            "marker-stopped",
            &[
                typed("start the two workers"),
                marker("start the two workers"),
                sent("task-notification", false, STOPPED),
                marker(STOPPED),
            ],
        );
        assert_eq!(last_prompt(&path).as_deref(), Some("start the two workers"));
        assert_eq!(prompt_history(&path), ["start the two workers"]);
        assert_eq!(tail_title(&path).as_deref(), Some("start the two workers"));
    }

    #[test]
    fn a_message_typed_while_a_turn_ran_is_a_prompt() {
        // Claude Code files it as an attachment, and its marker still names
        // the turn's own prompt.
        let path = transcript(
            "queued",
            &[
                typed("run the slow command"),
                queued("human", serde_json::json!("and then lint")),
                queued(
                    "human",
                    serde_json::json!([{"type": "text", "text": "and test"}]),
                ),
                marker("run the slow command"),
            ],
        );
        assert_eq!(
            prompt_history(&path),
            ["run the slow command", "and then lint", "and test"]
        );
    }

    #[test]
    fn typed_look_alikes_are_prompts() {
        for typed in [
            "<div> why does this not render",
            "The plugin sent a message: what does that mean",
            "3 background agents are slow, why?",
            "were stopped by the user",
            "Another Claude session, can it see mine?",
        ] {
            assert!(!is_injected_prompt(typed), "{typed}");
        }
        for injected in [NOTIFICATION, HAND_BACK, &HAND_BACK[39..], STOPPED] {
            assert!(is_injected_prompt(injected), "{injected}");
        }
        assert!(is_injected_prompt(
            "1 background agent was stopped by the user: \"x\""
        ));
    }

    /// Another session's message, in the shapes Claude Code 2.1.296 gives it:
    /// enqueued, queued as the turn's prompt, and rendered into the turn.
    const PEER: [&str; 3] = [
        include_str!("../testdata/peer-message-enqueued.txt"),
        include_str!("../testdata/peer-message-prompt.txt"),
        include_str!("../testdata/peer-message-rendered.txt"),
    ];

    #[test]
    fn another_sessions_message_is_not_a_prompt() {
        for text in PEER {
            assert!(text.contains("from-name=\"docs\""));
            assert!(is_injected_prompt(text), "{text}");
        }
        // Its other framings, and the other envelopes Claude Code sends in.
        for injected in [
            "Another Claude session sent a message while you were working:\n\
             <cross-session-message from=\"uds:/run/user/1000/cc-socks/1.sock\">hi\
             </cross-session-message>",
            "A peer session sent a message while you were working:\nhi",
            "<cross-session-message>hi</cross-session-message>",
            "<teammate-message teammate_id=\"tester\">\nall green\n</teammate-message>",
            "<channel source=\"telegram\" chat_id=\"1\">hi</channel>",
            "<channel-message from=\"ops\">hi</channel-message>",
            "<system-reminder id=\"7abcbf89088787d4\">\nhi\n</system-reminder>",
            "The coordinator sent a message while you were working:\nrun the tests too",
            "[SYSTEM NOTIFICATION - NOT USER INPUT] Background agent completed",
        ] {
            assert!(is_injected_prompt(injected), "{injected}");
        }
        for typed in [
            "<channels> is the tag I meant",
            "<cross-session-messages are leaking, fix it",
            "Another session? no, this one",
        ] {
            assert!(!is_injected_prompt(typed), "{typed}");
        }
    }

    #[test]
    fn another_sessions_message_is_not_in_the_history() {
        // As the transcript files it: a `queued_command` attachment saying
        // `origin.kind = "peer"`, and nothing in a `lastPrompt` marker.
        let path = transcript(
            "peer",
            &[
                user(serde_json::json!("tell me when the docs session answers")),
                serde_json::json!({
                    "type": "attachment",
                    "attachment": {
                        "type": "queued_command",
                        "prompt": PEER[1],
                        "commandMode": "prompt",
                        "origin": {"kind": "peer", "name": "docs"},
                    },
                    "isMeta": true,
                }),
            ],
        );
        assert_eq!(
            prompt_history(&path),
            ["tell me when the docs session answers"]
        );
    }

    #[test]
    fn an_interrupted_request_is_not_a_prompt() {
        // As Esc mid-turn writes it: a text item, no `origin`, no `isMeta`,
        // and the marker still naming the prompt it interrupted.
        for note in [
            "[Request interrupted by user]",
            "[Request interrupted by user for tool use]",
        ] {
            let path = transcript(
                "interrupted",
                &[
                    typed("run the slow command"),
                    marker("run the slow command"),
                    serde_json::json!({"type": "user", "message": {"role": "user",
                        "content": [{"type": "text", "text": note}]}}),
                    marker("run the slow command"),
                ],
            );
            assert_eq!(prompt_history(&path), ["run the slow command"], "{note}");
        }
    }

    #[test]
    fn a_prompt_that_starts_with_a_paste_is_a_prompt() {
        // Its real shape, cut down: the paste tagged on lines of its own,
        // the marker without the tags and with its spacing flattened.
        let pasted = "\n\n<pasted_content id=\"5f7c\">\n\u{a0}1. A\n  2. <b>bold</b>\n\
            </pasted_content id=\"5f7c\">\n\n\nfile these plz";
        let path = transcript(
            "pasted",
            &[
                typed("first"),
                typed(pasted),
                marker("1. A   2. <b>bold</b>    file these plz"),
            ],
        );
        let want = "\u{a0}1. A\n  2. <b>bold</b>\n\n\nfile these plz".trim();
        assert_eq!(prompt_history(&path), ["first", want]);

        // Nothing but a paste, which itself starts with a tag; and the same
        // typed while a turn ran.
        let html = "<pasted_content id=\"cd88\">\n<div>why</div>\n</pasted_content id=\"cd88\">";
        let path = transcript(
            "pasted-html",
            &[
                typed(html),
                queued(
                    "human",
                    serde_json::json!(
                        "<pasted_content id=\"a1\">\n<p>and this</p>\n</pasted_content id=\"a1\">"
                    ),
                ),
                marker("<div>why</div>"),
            ],
        );
        assert_eq!(prompt_history(&path), ["<div>why</div>", "<p>and this</p>"]);
    }

    #[test]
    fn claude_codes_own_tags_are_not_prompts_even_from_the_user() {
        let path = transcript(
            "wrappers",
            &[
                typed("first"),
                typed(
                    "<command-message>orchestrate</command-message>\n<command-name>/orchestrate</command-name>",
                ),
                typed("<local-command-stdout>ok</local-command-stdout>"),
                // An editor's context comes before what was typed.
                serde_json::json!({"type": "user", "origin": {"kind": "human"},
                    "promptSource": "sdk", "message": {"role": "user", "content": [
                        {"type": "text", "text": "<ide_selection>The user selected lines 1 to 1</ide_selection>"},
                        {"type": "text", "text": "why so much stuff?"}]}}),
                serde_json::json!({"type": "user", "origin": {"kind": "human"},
                    "message": {"role": "user", "content": [
                        {"type": "text", "text": "<ide_opened_file>The user opened a.rs</ide_opened_file>"}]}}),
                user(serde_json::json!("<b>no origin</b>")),
            ],
        );
        assert_eq!(prompt_history(&path), ["first", "why so much stuff?"]);
    }

    #[test]
    fn paste_tags_come_off() {
        assert_eq!(unwrap_pastes("no paste"), "no paste");
        assert_eq!(
            unwrap_pastes("a\n<pasted_content id=\"1\">\nb\n</pasted_content id=\"1\">\nc"),
            "a\nb\nc"
        );
        assert_eq!(
            unwrap_pastes(
                "<pasted_content id=\"1\">\nx\n</pasted_content id=\"1\"> and <pasted_content id=\"2\">\ny\n</pasted_content>"
            ),
            "x and y"
        );
        // A tag never closed is left as it is.
        assert_eq!(
            unwrap_pastes("x <pasted_content id"),
            "x <pasted_content id"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ancestor_chain_finds_self_and_parent() {
        let me = std::process::id();
        assert!(has_ancestor(me, me));
        // Our parent chain reaches pid 1 eventually without panicking.
        assert!(!has_ancestor(me, 4194301));
    }
}
