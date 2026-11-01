//! A session's own background commands left running: asked
//! about once each, at its turn's end or on its next hook call.
//!
//! A dispatcher's Bash command (a `head` with an empty file name, waiting on
//! stdin) hit its 120 s timeout. Claude Code moved it to the background
//! rather than ending it, and tells a session only when a background command
//! *ends*, so nothing looked at it again: for 30 minutes its tab drew the
//! working dots with nothing working. [`crate::leftovers`] covers a
//! subagent's commands; this covers the session's own.
//!
//! - **At the turn's end.** The plugin's `Stop` hook ([`stop`]) holds the
//!   stop once per command, with a question rather than an order — what it
//!   is, how long it has run, "stop it with TaskStop if it is not needed, or
//!   say why it runs on" — for a shell of the session's own still running
//!   that either was moved to the background by its timeout (never meant to
//!   run on), or has been quiet for [`QUIET_MS`]: its output file has not
//!   grown and its processes have used no CPU. Giverny never stops one
//!   itself: a long quiet download or server may be on purpose.
//! - **Between turns.** The session's next hook call — a tool call's
//!   `PostToolUse` or its next prompt ([`between`]) — says the same, once
//!   per command, for any that went quiet since.
//! - **Never** for a command started in the background that is still making
//!   progress, nor for a subagent's ([`crate::leftovers`] asks those), nor
//!   twice for one command. A `Stop` that is already a held stop's second
//!   (`stop_hook_active`) is let through, so the hook never loops.
//!
//! **What is the session's own.** Measured on Claude Code 2.1.302: a main
//! session's `Stop` payload carries `background_tasks`
//! ([`crate::hooks::in_flight`]) from the session's one task registry, each
//! with its `id`, `type`, `status`, `command`, but no owner and no start.
//! The session's transcript (`transcript_path`) tells its own: the tool
//! result that started one carries `toolUseResult.backgroundTaskId` and says
//! `Command running in background with ID: <id>`, or, for one its timeout
//! moved, `Command did not complete within its 3s timeout and was moved to
//! the background (ID: <id>)` with `timedOutAfterMs`. That line's timestamp
//! is its start, and it names the file its output goes to. A subagent's
//! commands start in the subagent's own transcript, so they are not found
//! here.
//!
//! **Quiet** is measured, not guessed: each check records the output file's
//! size and the CPU time of the processes holding that file open (the
//! command's shell writes there, and so does everything it starts) and
//! everything below them; a command is quiet once neither has moved for
//! [`QUIET_MS`]. Both only grow, so two equal readings mean nothing happened
//! between them. Where `/proc` is not there to read, output alone counts.
//! A command no process holds any more has ended.
//!
//! **Only in a Giverny tab** ([`crate::plugin_hook::in_giverny_tab`]).
//! **Cheap**: a `Stop` with no shell in flight reads no file;
//! another hook call `stat`s one file, and with commands watched looks at
//! them at most once every [`CHECK_EVERY_MS`].
//!
//! **File:** `<feed dir>/leftovers/<session>.own.json`, the commands
//! watched, there only while one is.

use std::io::BufRead;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::manage::{self, Lock};

/// How long a command must go without output or CPU to be asked about.
pub const QUIET_MS: u64 = 10 * 60_000;

/// How often a session's hook calls between turns look at its commands.
pub const CHECK_EVERY_MS: u64 = 60_000;

/// The task types that are a shell (a monitor is a wait, meant to run on).
const SHELLS: &[&str] = &["shell", "local_bash"];

/// One shell of the session's in flight, from the `Stop` payload.
#[derive(Debug, Clone, PartialEq)]
pub struct Task {
    pub id: String,
    pub what: String,
}

/// What a main session's `Stop` payload says.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Stop {
    pub session: String,
    pub transcript: Option<PathBuf>,
    /// A held stop's second: let through.
    pub active: bool,
    pub shells: Vec<Task>,
}

/// A main session's `Stop` payload; `None` for any other, a subagent's
/// `SubagentStop` among them (read whole only when it names the event).
pub fn stop_payload(input: &str) -> Option<Stop> {
    if !input.contains("\"Stop\"") {
        return None;
    }
    let v: Value = serde_json::from_str(input).ok()?;
    if v.get("hook_event_name").and_then(Value::as_str) != Some("Stop") {
        return None;
    }
    let text = |v: &Value, k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    let session = text(&v, "session_id")?;
    let shells = crate::hooks::in_flight(&v)
        .filter(|t| SHELLS.contains(&t.get("type").and_then(Value::as_str).unwrap_or("")))
        .filter_map(|t| {
            let id = text(t, "id")?;
            let what = text(t, "command")
                .or_else(|| text(t, "description"))
                .unwrap_or_else(|| id.clone());
            Some(Task { id, what })
        })
        .collect();
    Some(Stop {
        session,
        transcript: text(&v, "transcript_path").map(PathBuf::from),
        active: v.get("stop_hook_active").and_then(Value::as_bool) == Some(true),
        shells,
    })
}

/// How a command of the session's own started, from its transcript.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Start {
    pub started_ms: Option<u64>,
    /// The file its output goes to.
    pub output: Option<String>,
    /// The timeout that moved it to the background (`3s`, `2m`), when one
    /// did.
    pub timeout: Option<String>,
}

/// Which of `ids` the session started itself, and how: the tool results in
/// its `transcript` that started them. Read line by line; only a line
/// naming one of them is parsed.
pub fn starts(transcript: &Path, ids: &[&str]) -> Vec<(String, Start)> {
    let Ok(file) = std::fs::File::open(transcript) else {
        return Vec::new();
    };
    let mut found: Vec<Option<Start>> = vec![None; ids.len()];
    for line in std::io::BufReader::new(file).lines() {
        let Ok(line) = line else { break };
        if !line.contains("\"tool_result\"") || !line.contains("background") {
            continue;
        }
        for (i, id) in ids.iter().enumerate() {
            if found[i].is_some() || !line.contains(id) {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let Some(start) = started(&v, id) {
                found[i] = Some(start);
            }
        }
    }
    ids.iter()
        .zip(found)
        .filter_map(|(id, s)| Some((id.to_string(), s?)))
        .collect()
}

/// The start of background command `id`, when this transcript line is the
/// tool result that started it.
fn started(line: &Value, id: &str) -> Option<Start> {
    let result = line.get("toolUseResult");
    let by_field = result
        .and_then(|r| r.get("backgroundTaskId"))
        .and_then(Value::as_str)
        == Some(id);
    let text = result_text(line);
    let by_text = text.contains(&format!("Command running in background with ID: {id}"))
        || (text.contains("moved to the background") && text.contains(&format!("ID: {id}")));
    if !by_field && !by_text {
        return None;
    }
    let timed_out = result.and_then(|r| r.get("timedOutAfterMs")).is_some()
        || text.contains("did not complete within its");
    let timeout = timed_out.then(|| {
        between_words(&text, "did not complete within its ", " timeout")
            .map(String::from)
            .or_else(|| {
                let ms = result?.get("timedOutAfterMs")?.as_u64()?;
                Some(span(ms))
            })
            .unwrap_or_else(|| "its".into())
    });
    let output = between_words(&text, "Output is being written to: ", ".output")
        .map(|p| format!("{p}.output"));
    let started_ms = line
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(|t| t.parse::<jiff::Timestamp>().ok())
        .map(|t| t.as_millisecond().max(0) as u64);
    Some(Start {
        started_ms,
        output,
        timeout,
    })
}

/// A tool-result line's text: its `content`, a string or text blocks.
fn result_text(line: &Value) -> String {
    let mut out = String::new();
    let blocks = line
        .pointer("/message/content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"));
    for b in blocks {
        match b.get("content") {
            Some(Value::String(s)) => out.push_str(s),
            Some(Value::Array(parts)) => {
                for p in parts {
                    if let Some(s) = p.get("text").and_then(Value::as_str) {
                        out.push_str(s);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// The text between `from` and the next `to`, on one line.
fn between_words<'a>(text: &'a str, from: &str, to: &str) -> Option<&'a str> {
    let rest = &text[text.find(from)? + from.len()..];
    let end = rest.find(to)?;
    let got = rest[..end].trim();
    (!got.is_empty() && !got.contains('\n')).then_some(got)
}

/// A span as a line says it: `40s`, `12m`, `2h 5m`.
fn span(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        _ if s % 3600 / 60 == 0 => format!("{}h", s / 3600),
        _ => format!("{}h {}m", s / 3600, s % 3600 / 60),
    }
}

/// What a look at a command finds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Look {
    /// Its output file's size.
    pub size: Option<u64>,
    /// CPU time, clock ticks, of the processes holding its output file open
    /// and everything below them.
    pub cpu: Option<u64>,
    /// Whether any process still holds its output file; `None` where that
    /// cannot be read.
    pub running: Option<bool>,
}

/// How commands are looked at: the system's files and processes, or a
/// test's.
pub trait Probe {
    /// One look at each output file, in order.
    fn look(&self, outputs: &[Option<&str>]) -> Vec<Look>;
}

/// The real look: the file's size, and on Linux `/proc` for the processes
/// that hold it.
pub struct System;

impl Probe for System {
    fn look(&self, outputs: &[Option<&str>]) -> Vec<Look> {
        let sizes: Vec<Option<u64>> = outputs
            .iter()
            .map(|o| std::fs::metadata((*o)?).ok().map(|m| m.len()))
            .collect();
        let cpu = cpu_of(outputs);
        sizes
            .into_iter()
            .zip(cpu)
            .map(|(size, (cpu, running))| Look { size, cpu, running })
            .collect()
    }
}

/// Each output file's (CPU ticks, running): the processes with it open,
/// and everything below them. The hook's own ancestors (the claude that ran
/// it, which may be reading the file) are not the command's.
#[cfg(target_os = "linux")]
fn cpu_of(outputs: &[Option<&str>]) -> Vec<(Option<u64>, Option<bool>)> {
    use std::collections::HashSet;
    let wanted: Vec<Option<PathBuf>> = outputs.iter().map(|o| o.map(PathBuf::from)).collect();
    let mut holders: Vec<Vec<u32>> = vec![Vec::new(); outputs.len()];
    let table = crate::session_use::proc_table(4);
    let me = std::process::id();
    for p in &table {
        let Ok(fds) = std::fs::read_dir(format!("/proc/{}/fd", p.pid)) else {
            continue; // another user's, or gone
        };
        for fd in fds.flatten() {
            let Ok(target) = std::fs::read_link(fd.path()) else {
                continue;
            };
            for (i, w) in wanted.iter().enumerate() {
                if w.as_deref() == Some(target.as_path()) && !holders[i].contains(&p.pid) {
                    holders[i].push(p.pid);
                }
            }
        }
    }
    holders
        .into_iter()
        .map(|pids| {
            let mut tree: HashSet<u32> = HashSet::new();
            for pid in pids {
                let below = crate::session_use::subtree(&table, pid);
                if !below.contains(&me) {
                    tree.extend(below);
                }
            }
            if tree.is_empty() {
                return (None, Some(false));
            }
            let ticks = table
                .iter()
                .filter(|p| tree.contains(&p.pid))
                .map(|p| p.ticks)
                .sum();
            (Some(ticks), Some(true))
        })
        .collect()
}

#[cfg(not(target_os = "linux"))]
fn cpu_of(outputs: &[Option<&str>]) -> Vec<(Option<u64>, Option<bool>)> {
    vec![(None, None); outputs.len()]
}

/// A command watched: how it started, and its last readings.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct Cmd {
    id: String,
    what: String,
    #[serde(default)]
    output: Option<String>,
    #[serde(default)]
    started_ms: Option<u64>,
    #[serde(default)]
    timeout: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    cpu: Option<u64>,
    /// When its size or CPU last moved (or it was first looked at).
    #[serde(default)]
    changed_ms: u64,
    #[serde(default)]
    asked: bool,
}

/// A session's commands watched.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Watch {
    #[serde(default)]
    cmds: Vec<Cmd>,
    /// In flight but not the session's own (a subagent's): not read for
    /// again.
    #[serde(default)]
    others: Vec<String>,
    /// When its commands were last looked at.
    #[serde(default)]
    checked_ms: u64,
}

impl Cmd {
    /// Take in a look; whether it found it ended.
    fn take(&mut self, look: Look, now: u64) -> bool {
        if look.running == Some(false) {
            return true;
        }
        if look.size != self.size || look.cpu != self.cpu || self.changed_ms == 0 {
            self.size = look.size;
            self.cpu = look.cpu;
            self.changed_ms = now;
        }
        false
    }

    fn quiet(&self, now: u64) -> bool {
        self.changed_ms > 0 && now.saturating_sub(self.changed_ms) >= QUIET_MS
    }

    /// Whether it is to be asked about now.
    fn due(&self, now: u64) -> bool {
        !self.asked && (self.timeout.is_some() || self.quiet(now))
    }

    /// Its line: what, which task, how long, and why it is asked about.
    fn line(&self, now: u64) -> String {
        let ran = self
            .started_ms
            .map(|t| match now.saturating_sub(t) {
                d if d < 60_000 => ", under a minute in".to_string(),
                d => format!(", {} in", span(d)),
            })
            .unwrap_or_default();
        let why = match &self.timeout {
            Some(t) => format!("moved there by its {t} timeout"),
            None => format!(
                "no output and no CPU for {}",
                span(now.saturating_sub(self.changed_ms))
            ),
        };
        format!("`{}` (task {}{ran}; {why})", quote(&self.what), self.id)
    }
}

/// How much of a command a line quotes.
const QUOTE_CHARS: usize = 60;

/// A command as a line quotes it: its first line, backticks made quotes,
/// cut to [`QUOTE_CHARS`].
pub(crate) fn quote(what: &str) -> String {
    let first = what.lines().next().unwrap_or("").trim().replace('`', "'");
    if first.chars().count() <= QUOTE_CHARS {
        return first;
    }
    let cut: String = first.chars().take(QUOTE_CHARS - 1).collect();
    format!("{}…", cut.trim_end())
}

/// The question about `cmds`: one line, or a list.
pub(crate) fn question(lines: &[String]) -> String {
    if let [one] = lines {
        return format!(
            "Giverny: {one} is still running in the background. Stop it with TaskStop if it \
             is not needed, or say why it runs on."
        );
    }
    let list: Vec<String> = lines.iter().map(|l| format!("- {l}")).collect();
    format!(
        "Giverny: these are still running in the background:\n{}\nStop each with TaskStop \
         if it is not needed, or say why it runs on.",
        list.join("\n")
    )
}

fn safe(session: &str) -> bool {
    !session.is_empty()
        && session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// `<feed dir>/leftovers/<session>.own.json`: the session's commands
/// watched.
pub fn watch_path(feed_dir: &Path, session: &str) -> PathBuf {
    feed_dir
        .join(crate::leftovers::DIR)
        .join(format!("{session}.own.json"))
}

fn read(path: &Path) -> Watch {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Write the watch, or remove its file when nothing is watched.
fn save(path: &Path, w: &Watch) -> Option<()> {
    if w.cmds.is_empty() {
        let _ = std::fs::remove_file(path);
        return Some(());
    }
    manage::write(path, &json!(w)).ok()
}

/// Look at every command in `w`, dropping those that ended.
fn look_all(w: &mut Watch, probe: &dyn Probe, now: u64) {
    let outputs: Vec<Option<&str>> = w.cmds.iter().map(|c| c.output.as_deref()).collect();
    let looks = probe.look(&outputs);
    let mut i = 0;
    w.cmds.retain_mut(|c| {
        let look = looks.get(i).copied().unwrap_or_default();
        i += 1;
        !c.take(look, now)
    });
    w.checked_ms = now;
}

/// The `Stop` hook's run: the reply holding the stop, or nothing. `in_tab`
/// is asked only when a shell is in flight.
pub fn stop(
    stop: &Stop,
    feed_dir: &Path,
    now: u64,
    probe: &dyn Probe,
    in_tab: impl FnOnce() -> bool,
) -> Option<String> {
    if !safe(&stop.session) {
        return None;
    }
    let file = watch_path(feed_dir, &stop.session);
    if stop.shells.is_empty() {
        // Nothing in flight: nothing to watch.
        if std::fs::metadata(&file).is_ok() {
            let _lock = Lock::take(&file).ok()?;
            let _ = std::fs::remove_file(&file);
        }
        return None;
    }
    if !in_tab() {
        return None;
    }
    std::fs::create_dir_all(file.parent()?).ok()?;
    let _lock = Lock::take(&file).ok()?;
    let mut w = read(&file);
    // What is no longer in flight has ended.
    w.cmds.retain(|c| stop.shells.iter().any(|t| t.id == c.id));
    w.others.retain(|o| stop.shells.iter().any(|t| &t.id == o));
    let new: Vec<&Task> = stop
        .shells
        .iter()
        .filter(|t| !w.cmds.iter().any(|c| c.id == t.id) && !w.others.contains(&t.id))
        .collect();
    if !new.is_empty() {
        let ids: Vec<&str> = new.iter().map(|t| t.id.as_str()).collect();
        let found = stop
            .transcript
            .as_deref()
            .map(|t| starts(t, &ids))
            .unwrap_or_default();
        for t in new {
            match found.iter().find(|(id, _)| *id == t.id) {
                Some((_, s)) => w.cmds.push(Cmd {
                    id: t.id.clone(),
                    what: t.what.clone(),
                    output: s.output.clone(),
                    started_ms: s.started_ms,
                    timeout: s.timeout.clone(),
                    ..Cmd::default()
                }),
                None => w.others.push(t.id.clone()),
            }
        }
    }
    look_all(&mut w, probe, now);
    let mut lines = Vec::new();
    if !stop.active {
        for c in w.cmds.iter_mut().filter(|c| c.due(now)) {
            lines.push(c.line(now));
            c.asked = true;
        }
    }
    // Others alone are kept only while a command of its own is watched.
    save(&file, &w)?;
    (!lines.is_empty()).then(|| block(&question(&lines)))
}

/// The `Stop` reply holding the stop with `reason`.
fn block(reason: &str) -> String {
    json!({"decision": "block", "reason": reason}).to_string()
}

/// A session's own hook call between turns (its `PostToolUse`, its
/// prompt): the line about each command that went quiet since its last
/// `Stop`, once. A `stat` when nothing is watched; a look at most once
/// every [`CHECK_EVERY_MS`].
pub fn between(feed_dir: &Path, session: &str, now: u64, probe: &dyn Probe) -> Option<String> {
    if !safe(session) {
        return None;
    }
    let file = watch_path(feed_dir, session);
    std::fs::metadata(&file).ok()?;
    let _lock = Lock::take(&file).ok()?;
    let mut w = read(&file);
    if now.saturating_sub(w.checked_ms) < CHECK_EVERY_MS || !w.cmds.iter().any(|c| !c.asked) {
        return None;
    }
    look_all(&mut w, probe, now);
    let mut lines = Vec::new();
    for c in w.cmds.iter_mut().filter(|c| c.due(now)) {
        lines.push(c.line(now));
        c.asked = true;
    }
    save(&file, &w)?;
    (!lines.is_empty()).then(|| question(&lines))
}
