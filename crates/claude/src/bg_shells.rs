//! A task worker's own background shells: recorded as it
//! starts them, checked when it ends its turn, and named to its manager.
//! Giverny suggests, managers decide: nothing here stops or signals
//! anything.
//!
//! `giverny-manage pause` lists a task's live `run` commands,
//! but a worker's Bash `run_in_background` shells ran on unseen: that is
//! what kept one task going after its worker ended its turn.
//!
//! - **Recorded.** The plugin's `PostToolUse` hook sees a worker's Bash
//!   call that left a shell in the background (its response carries
//!   `backgroundTaskId`, measured on Claude Code 2.1.303; a call its
//!   timeout moved there carries it too). When the worker holds a row of
//!   its manager session, the shell is recorded against that row's task:
//!   its id, the worker, the command and the time ([`record`]).
//! - **At its turn's end.** The worker's `SubagentStop` payload lists the
//!   session's whole task registry in `background_tasks` (2.1.303: the
//!   manager's shells and every worker's alike, each with `id`, `type`,
//!   `status`, `command`, but no owner). A recorded shell no longer in
//!   flight there has ended and drops off; one still running is told to
//!   the manager once, on its next tool call ([`turn_ended`], [`deliver`]):
//!   "`g472`'s worker ended its turn and is asleep, waiting on 1
//!   background shell of its own still running: `sleep 600` (40m) …".
//!   A shell the hook never recorded (one started before it was installed,
//!   as a `wrangler deploy` once was) is recorded then,
//!   from the worker's transcript ([`crate::plugin_hook`]'s backfill).
//! - **Asleep, in the pane**: [`Watch`] follows the recorded
//!   shells for the management panel, which reads a worker whose turn
//!   ended with one still running as asleep ([`gist`], [`named`]).
//! - **On `pause` and `resume`** the task's live shells are listed with how
//!   long each has run and how to stop it ([`task_shells`], [`lines`]).
//!
//! **How to stop one.** The registry is the session's, so the manager's
//! own `TaskStop <shell id>` stops a worker's shell (measured on 2.1.303:
//! "Successfully stopped task: bq4h1i6dn (sleep 601)", its process gone).
//! That is always named. A pid is named only when it is certain
//! ([`Probe`]): the shell's output file, `…/<session>/tasks/<id>.output`,
//! is held open by exactly one top process, a session leader (Claude Code
//! starts each command's `bash -c` wrapper in a session of its own) whose
//! `eval` is the recorded command. Then `pkill -s <pid>` stops the wrapper
//! and all it started (the command itself runs in another process group of
//! that session, so a group kill would miss it).
//!
//! **Dropping off.** A shell drops off when a `SubagentStop` no longer
//! lists it in flight, when a look at `/proc` finds nothing holding its
//! output file, or two days after it started. The file goes when its last
//! shell does.
//!
//! **Cheap.** A `PostToolUse` writes only for a background shell; a
//! `SubagentStop` with no record `stat`s one file; a manager's other calls
//! `stat` the tell file, there only while a notice waits.
//!
//! **Files**, under `<feed dir>/bgshells/`: `<session>.json`, the shells
//! recorded; `<session>.tell.json`, the notices the manager has yet to
//! hear.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::manage::{self, Lock};

/// The directory under the feed dir the files live in.
pub const DIR: &str = "bgshells";

/// A record is kept at most this long, live or not.
pub const KEEP_MS: u64 = 2 * 24 * 60 * 60 * 1000;

/// A worker's background shell, as recorded when it started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shell {
    /// Claude Code's task id for it (`backgroundTaskId`).
    pub id: String,
    /// The worker that started it.
    pub agent: String,
    /// The task its worker held.
    pub task: String,
    /// Its command, as the worker gave it.
    pub cmd: String,
    /// When it started, epoch ms.
    pub started_ms: u64,
}

/// What a look at a shell finds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Found {
    /// Whether anything still runs it; `None` where that cannot be read.
    pub running: Option<bool>,
    /// Its wrapper's pid, only when certain: what `pkill -s` would stop.
    pub pid: Option<u32>,
}

/// A live shell, as `pause`, `resume` and the notice name it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Live {
    pub shell: Shell,
    pub pid: Option<u32>,
}

/// How shells are looked at: the system's processes, or a test's.
pub trait Probe {
    /// One look at each of `session`'s `shells`, in order.
    fn look(&self, session: &str, shells: &[Shell]) -> Vec<Found>;
}

/// The real look: on Linux, `/proc` for the processes holding each shell's
/// output file open. Elsewhere nothing is known.
pub struct System;

impl Probe for System {
    fn look(&self, session: &str, shells: &[Shell]) -> Vec<Found> {
        look_proc(session, shells)
    }
}

/// The processes holding each shell's output file, and the one top among
/// them when it is certainly the shell's wrapper. The caller's own
/// ancestors (the claude that runs it, which may be reading the file) are
/// not the shell's.
#[cfg(target_os = "linux")]
fn look_proc(session: &str, shells: &[Shell]) -> Vec<Found> {
    let Some(held) = holders_proc(session, shells) else {
        return vec![Found::default(); shells.len()];
    };
    shells
        .iter()
        .zip(held.pids)
        .map(|(shell, pids)| {
            if pids.is_empty() {
                return Found {
                    running: Some(false),
                    pid: None,
                };
            }
            let pids: Vec<u32> = pids.iter().map(|(p, _)| *p).collect();
            let tops: Vec<u32> = pids
                .iter()
                .copied()
                .filter(|p| held.parent.get(p).is_none_or(|pp| !pids.contains(pp)))
                .collect();
            let pid = match tops[..] {
                [top] if wrapper_of(top, &shell.cmd) => Some(top),
                _ => None,
            };
            Found {
                running: Some(true),
                pid,
            }
        })
        .collect()
}

/// One look at `/proc`: for each shell, the processes holding its output
/// file (`…/<session>/tasks/<id>.output`), each with its start (clock
/// ticks after boot), and every process's parent. `None` when `/proc`
/// could not be read.
#[cfg(target_os = "linux")]
struct Held {
    pids: Vec<Vec<(u32, u64)>>,
    parent: std::collections::HashMap<u32, u32>,
}

#[cfg(target_os = "linux")]
fn holders_proc(session: &str, shells: &[Shell]) -> Option<Held> {
    use std::collections::HashSet;
    if shells.is_empty() {
        return Some(Held {
            pids: Vec::new(),
            parent: Default::default(),
        });
    }
    let table = crate::session_use::proc_table(4);
    if table.is_empty() {
        return None;
    }
    let parent: std::collections::HashMap<u32, u32> =
        table.iter().map(|p| (p.pid, p.ppid)).collect();
    let mut mine = HashSet::new();
    let mut at = std::process::id();
    while mine.insert(at) {
        match parent.get(&at) {
            Some(&p) if p > 1 => at = p,
            _ => break,
        }
    }
    let wanted: Vec<String> = shells
        .iter()
        .map(|s| format!("/{session}/tasks/{}.output", s.id))
        .collect();
    let mut pids: Vec<Vec<(u32, u64)>> = vec![Vec::new(); shells.len()];
    for p in &table {
        if mine.contains(&p.pid) {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(format!("/proc/{}/fd", p.pid)) else {
            continue; // another user's, or gone
        };
        for fd in fds.flatten() {
            let Ok(target) = std::fs::read_link(fd.path()) else {
                continue;
            };
            let Some(target) = target.to_str() else {
                continue;
            };
            for (i, w) in wanted.iter().enumerate() {
                if target.ends_with(w.as_str()) && !pids[i].iter().any(|(x, _)| *x == p.pid) {
                    pids[i].push((p.pid, p.start));
                }
            }
        }
    }
    Some(Held { pids, parent })
}

#[cfg(not(target_os = "linux"))]
fn look_proc(_session: &str, shells: &[Shell]) -> Vec<Found> {
    vec![Found::default(); shells.len()]
}

/// Whether `pid` is Claude Code's wrapper for `cmd`: a session leader
/// (`/proc/<pid>/stat` field 6 is itself) running `bash -c …`, whose
/// script evals exactly `cmd` ([`crate::worker_pids::eval_command`]).
#[cfg(target_os = "linux")]
fn wrapper_of(pid: u32, cmd: &str) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some((_, rest)) = stat.rsplit_once(')') else {
        return false;
    };
    // Fields count from `state` (3): `session` is 6.
    let sid = rest
        .split_whitespace()
        .nth(3)
        .and_then(|s| s.parse::<u32>().ok());
    if sid != Some(pid) {
        return false;
    }
    let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let argv: Vec<&[u8]> = raw.split(|b| *b == 0).collect();
    let script = match argv.iter().position(|a| *a == b"-c") {
        Some(i) => argv.get(i + 1).copied().unwrap_or_default(),
        None => return false,
    };
    crate::worker_pids::eval_command(&String::from_utf8_lossy(script)).as_deref() == Some(cmd)
}

/// A notice the manager has yet to hear: a worker ended its turn with
/// these still running.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Tell {
    task: String,
    agent: String,
    at_ms: u64,
    shells: Vec<Shell>,
}

/// The shells recorded in a session.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Book {
    #[serde(default)]
    shells: Vec<Shell>,
}

fn safe(session: &str) -> bool {
    !session.is_empty()
        && session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// `<feed dir>/bgshells/<session>.json`: the shells recorded.
pub fn path(feed_dir: &Path, session: &str) -> PathBuf {
    feed_dir.join(DIR).join(format!("{session}.json"))
}

/// `<feed dir>/bgshells/<session>.tell.json`: the notices to come.
pub fn tell_path(feed_dir: &Path, session: &str) -> PathBuf {
    feed_dir.join(DIR).join(format!("{session}.tell.json"))
}

fn read<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Write the book, or remove its file when it holds nothing.
fn put(file: &Path, book: &Book) {
    if book.shells.is_empty() {
        let _ = std::fs::remove_file(file);
    } else {
        let _ = manage::write(file, &json!(book));
    }
}

/// Record `shell`, started by a worker holding its task. Records past
/// [`KEEP_MS`] go.
pub fn record(feed_dir: &Path, session: &str, shell: Shell, now: u64) {
    if !safe(session) || shell.id.is_empty() {
        return;
    }
    let file = path(feed_dir, session);
    let Some(dir) = file.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let Ok(_lock) = Lock::take(&file) else { return };
    let mut book: Book = read(&file);
    book.shells
        .retain(|s| s.id != shell.id && now.saturating_sub(s.started_ms) < KEEP_MS);
    book.shells.push(shell);
    put(&file, &book);
}

/// Whether shell `id` is recorded as a task worker's: its turn-end notice
/// is this module's, so [`crate::leftovers`] does not tell it again.
pub fn is_recorded(feed_dir: &Path, session: &str, id: &str) -> bool {
    if !safe(session) {
        return false;
    }
    let file = path(feed_dir, session);
    if std::fs::metadata(&file).is_err() {
        return false;
    }
    read::<Book>(&file).shells.iter().any(|s| s.id == id)
}

/// A worker's `SubagentStop` was let through: its turn ended. `in_flight`
/// is the payload's `background_tasks` ids still running or pending (the
/// session's whole registry). A recorded shell not among them, or that
/// `probe` finds nothing runs, has ended and drops off; the worker's shells
/// still running are queued to its manager, once for this turn's end.
pub fn turn_ended(
    feed_dir: &Path,
    session: &str,
    agent: &str,
    in_flight: &[&str],
    now: u64,
    probe: &dyn Probe,
) {
    if !safe(session) {
        return;
    }
    let file = path(feed_dir, session);
    if std::fs::metadata(&file).is_err() {
        return;
    }
    let Ok(_lock) = Lock::take(&file) else { return };
    let mut book: Book = read(&file);
    book.shells.retain(|s| {
        in_flight.contains(&s.id.as_str()) && now.saturating_sub(s.started_ms) < KEEP_MS
    });
    let theirs: Vec<Shell> = book
        .shells
        .iter()
        .filter(|s| s.agent == agent)
        .cloned()
        .collect();
    let found = probe.look(session, &theirs);
    let ended: Vec<&str> = theirs
        .iter()
        .zip(&found)
        .filter(|(_, f)| f.running == Some(false))
        .map(|(s, _)| s.id.as_str())
        .collect();
    let live: Vec<Shell> = theirs
        .iter()
        .filter(|s| !ended.contains(&s.id.as_str()))
        .cloned()
        .collect();
    let ended: Vec<String> = ended.into_iter().map(String::from).collect();
    book.shells.retain(|s| !ended.contains(&s.id));
    put(&file, &book);
    let tell_file = tell_path(feed_dir, session);
    let mut tell: Vec<Tell> = read(&tell_file);
    tell.retain(|t| t.agent != agent);
    if let Some(task) = live.first().map(|s| s.task.clone()) {
        tell.push(Tell {
            task,
            agent: agent.to_string(),
            at_ms: now,
            shells: live,
        });
    }
    if tell.is_empty() {
        let _ = std::fs::remove_file(&tell_file);
    } else {
        let _ = manage::write(&tell_file, &json!(tell));
    }
}

/// The manager's notices, on its own hook call, each once: a `stat` when
/// there are none. A shell that ended since its worker's turn did is left
/// out.
pub fn deliver(feed_dir: &Path, session: &str, now: u64, probe: &dyn Probe) -> Option<String> {
    if !safe(session) {
        return None;
    }
    let tell_file = tell_path(feed_dir, session);
    std::fs::metadata(&tell_file).ok()?;
    let _lock = Lock::take(&path(feed_dir, session)).ok()?;
    let tell: Vec<Tell> = read(&tell_file);
    let _ = std::fs::remove_file(&tell_file);
    let lines: Vec<String> = tell
        .into_iter()
        .filter_map(|t| {
            let found = probe.look(session, &t.shells);
            let live: Vec<Live> = t
                .shells
                .into_iter()
                .zip(found)
                .filter(|(_, f)| f.running != Some(false))
                .map(|(shell, f)| Live { shell, pid: f.pid })
                .collect();
            (!live.is_empty()).then(|| notice(&t.task, &live, now))
        })
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n\n"))
}

/// The turn-end notice: "`g472`'s worker ended its turn and is asleep,
/// waiting on 1 background shell of its own still running: `sleep 600`
/// (40m) …". Asleep, not ended: Claude Code wakes the worker when the shell
/// ends, the word the pane uses too.
pub fn notice(task: &str, live: &[Live], now: u64) -> String {
    let n = live.len();
    let (what, it, when) = if n == 1 {
        (
            "1 background shell of its own still running".to_string(),
            "it",
            "that ends",
        )
    } else {
        (
            format!("{n} background shells of its own still running"),
            "each",
            "one ends",
        )
    };
    let list: Vec<String> = live
        .iter()
        .map(|l| {
            format!(
                "`{}` ({}, shell {})",
                crate::strays::quote(&l.shell.cmd),
                ran(l.shell.started_ms, now),
                l.shell.id
            )
        })
        .collect();
    let stop: Vec<String> = live.iter().map(stop_with).collect();
    format!(
        "Giverny: `{task}`'s worker ended its turn and is asleep, waiting on {what}: {}. \
         Claude Code wakes it when {when}. Giverny stops nothing: if the work is meant to \
         stop, stop {it} ({}), or let {it} run on if needed.",
        list.join(", "),
        stop.join("; ")
    )
}

/// How long a shell has run: `40m`, `12s`.
fn ran(started_ms: u64, now: u64) -> String {
    let s = now.saturating_sub(started_ms) / 1000;
    match s {
        0..60 => format!("{s}s"),
        _ => crate::feed::fmt_span(s as i64),
    }
}

/// What stops a shell: `TaskStop <id>` from the session, which always
/// works on a worker's shell, and its wrapper's session when its pid is
/// certain.
fn stop_with(l: &Live) -> String {
    match l.pid {
        Some(pid) => format!("`TaskStop {}` or `pkill -s {pid}`", l.shell.id),
        None => format!("`TaskStop {}`", l.shell.id),
    }
}

/// `task`'s live background shells in `session`, oldest first. Those
/// `probe` finds ended drop off the record.
pub fn task_shells(feed_dir: &Path, session: &str, task: &str, probe: &dyn Probe) -> Vec<Live> {
    live_shells(feed_dir, session, probe, |s| s.task == task)
}

/// Worker `agent`'s live background shells in `session`, of any task,
/// oldest first. Those `probe` finds ended drop off the record.
pub fn agent_shells(feed_dir: &Path, session: &str, agent: &str, probe: &dyn Probe) -> Vec<Live> {
    live_shells(feed_dir, session, probe, |s| s.agent == agent)
}

/// The recorded shells `wanted` picks that `probe` does not find ended,
/// oldest first; those it finds ended drop off the record.
fn live_shells(
    feed_dir: &Path,
    session: &str,
    probe: &dyn Probe,
    wanted: impl Fn(&Shell) -> bool,
) -> Vec<Live> {
    if !safe(session) {
        return Vec::new();
    }
    let file = path(feed_dir, session);
    if std::fs::metadata(&file).is_err() {
        return Vec::new();
    }
    let Ok(_lock) = Lock::take(&file) else {
        return Vec::new();
    };
    let mut book: Book = read(&file);
    let found = probe.look(session, &book.shells);
    let mut live = Vec::new();
    let mut ended = Vec::new();
    for (s, f) in book.shells.iter().zip(found) {
        if f.running == Some(false) {
            ended.push(s.id.clone());
        } else if wanted(s) {
            live.push(Live {
                shell: s.clone(),
                pid: f.pid,
            });
        }
    }
    if !ended.is_empty() {
        book.shells.retain(|s| !ended.contains(&s.id));
        put(&file, &book);
    }
    live.sort_by_key(|l| l.shell.started_ms);
    live
}

/// Each live shell as a line: `` `sleep 600` (shell bjj5phx0d), 40m in:
/// `TaskStop bjj5phx0d` from this session stops it, or `pkill -s 4242` ``.
pub fn lines(live: &[Live], now: u64) -> Vec<String> {
    live.iter()
        .map(|l| {
            let how = match l.pid {
                Some(pid) => format!(
                    "`TaskStop {id}` from this session stops it, or `pkill -s {pid}` (its \
                     shell, pid {pid}, and all it started)",
                    id = l.shell.id
                ),
                None => format!(
                    "`TaskStop {}` from this session stops it (its pid is not certain)",
                    l.shell.id
                ),
            };
            format!(
                "`{}` (shell {}), {} in: {how}",
                crate::strays::quote(&l.shell.cmd),
                l.shell.id,
                ran(l.shell.started_ms, now)
            )
        })
        .collect()
}

/// `<feed dir>/bgshells/<session>.held.json`: the tasks whose worker's
/// report was held back once already.
pub fn held_path(feed_dir: &Path, session: &str) -> PathBuf {
    feed_dir.join(DIR).join(format!("{session}.held.json"))
}

/// The tasks whose report was held, newest last.
#[derive(Debug, Default, Serialize, Deserialize)]
struct HeldBack {
    #[serde(default)]
    tasks: Vec<String>,
}

/// At most this many held tasks are remembered, the oldest going first.
const HELD_KEEP: usize = 200;

/// A task worker's final report (its `SubagentHandback` call)
/// while background shells of its own still run: the reason to hold it
/// back, once per task (`task`, the row it holds, else its shells' task).
/// `None` lets it through: no shell of its own runs, or this task's report
/// was held before. Nothing is stopped; the worker is told to stop them.
/// Cheap: a `stat` when the session has no shell recorded.
pub fn handback(
    feed_dir: &Path,
    session: &str,
    agent: &str,
    task: Option<&str>,
    now: u64,
    probe: &dyn Probe,
) -> Option<String> {
    if !safe(session) || agent.is_empty() {
        return None;
    }
    std::fs::metadata(path(feed_dir, session)).ok()?;
    let live = agent_shells(feed_dir, session, agent, probe);
    let first = live.first()?;
    let key = task.map_or_else(|| first.shell.task.clone(), str::to_string);
    let file = held_path(feed_dir, session);
    let _lock = Lock::take(&file).ok()?;
    let mut held: HeldBack = read(&file);
    if held.tasks.contains(&key) {
        return None;
    }
    held.tasks.push(key.clone());
    if held.tasks.len() > HELD_KEEP {
        let cut = held.tasks.len() - HELD_KEEP;
        held.tasks.drain(..cut);
    }
    manage::write(&file, &json!(held)).ok()?;
    Some(handback_reason(&key, &live, now))
}

/// Why a worker's report is held back: its shells still
/// running, each with how to stop it.
pub fn handback_reason(task: &str, live: &[Live], now: u64) -> String {
    let what = match live.len() {
        1 => "1 background shell of your own still runs".to_string(),
        n => format!("{n} background shells of your own still run"),
    };
    let mut out = format!(
        "Giverny held your report back, once: {what} (task `{task}`), and a task with \
         anything still running is not done:"
    );
    for line in lines(live, now) {
        out.push_str(&format!("\n  {line}"));
    }
    let ids: Vec<String> = live
        .iter()
        .map(|l| format!("`TaskStop {}`", l.shell.id))
        .collect();
    out.push_str(&format!(
        "\nStop {} ({}) before you report, then send your report again. Your next report \
         goes through, and your manager's `giverny-manage land` refuses the task while any \
         of them still runs.",
        if live.len() == 1 { "it" } else { "each" },
        ids.join(", ")
    ));
    out
}

/// What `pause` and `resume` add for `task`'s live shells: nothing when
/// none runs.
pub fn still_running(task: &str, live: &[Live], now: u64) -> String {
    if live.is_empty() {
        return String::new();
    }
    let what = match live.len() {
        1 => "1 background shell its worker started still runs".to_string(),
        n => format!("{n} background shells its worker started still run"),
    };
    let mut out = format!("\n  {what} for {task} (Giverny stops nothing):");
    for line in lines(live, now) {
        out.push_str(&format!("\n    {line}"));
    }
    out
}

/// A shell's command as the pane names it: the program and
/// its first words, past the setup around it — `cd`, variables, `echo`,
/// `npx`, `giverny-manage run <task> --`, redirections — and with paths cut
/// to their last part: `cd ~/site; T=…; npx wrangler deploy > $T/d.log
/// 2>&1; echo $?` is `wrangler deploy`.
pub fn gist(cmd: &str) -> String {
    const SETUP: &[&str] = &[
        "cd", "pushd", "popd", "export", "set", "unset", "source", ".", "echo", "printf", "true",
        ":", "mkdir", "local", "declare", "trap", "ulimit", "umask", "chmod", "ln",
    ];
    // A path is named after these only: `node sim.js`, but `deploy.sh`,
    // not `deploy.sh wt178`.
    const INTERPRETERS: &[&str] = &[
        "node", "python", "python3", "bash", "sh", "zsh", "deno", "bun", "ruby", "perl", "tsx",
        "ts-node", "Rscript",
    ];
    const WRAPPERS: &[&str] = &[
        "npx", "sudo", "nohup", "time", "env", "exec", "command", "nice", "stdbuf", "setsid",
        "timeout", "ionice", "chrt",
    ];
    let segments = segments(cmd);
    let pick = |seg: &[String]| -> Option<Vec<String>> {
        let mut w: Vec<String> = seg.to_vec();
        loop {
            // `X=1 cmd`: the assignments go.
            while w.first().is_some_and(|x| is_assignment(x)) {
                w.remove(0);
            }
            let first = w.first()?.clone();
            let name = base(&first);
            if WRAPPERS.contains(&name) {
                w.remove(0);
                // Its own flags and a bare number (`timeout 600`, `nice -n 5`).
                while w
                    .first()
                    .is_some_and(|x| x.starts_with('-') || x.parse::<f64>().is_ok())
                {
                    w.remove(0);
                }
                continue;
            }
            // `giverny-manage run <task> -- cmd`, `systemd-run … -- cmd`:
            // the command after `--`.
            if (name.starts_with("giverny") || name == "systemd-run")
                && let Some(i) = w.iter().position(|x| x == "--")
            {
                w.drain(..=i);
                continue;
            }
            return Some(w);
        }
    };
    // `cat > f <<'EOF'`, a file written: setup too.
    let setup = |w: &[String]| {
        w.first().is_some_and(|f| {
            SETUP.contains(&base(f))
                || (matches!(base(f), "cat" | "tee") && w.iter().any(|x| x == "<<"))
        })
    };
    let chosen = segments
        .iter()
        .filter_map(|seg| pick(seg))
        .find(|w| !setup(w))
        .or_else(|| segments.iter().find_map(|seg| pick(seg)));
    let Some(words) = chosen else {
        return crate::strays::quote(cmd);
    };
    let mut out: Vec<&str> = Vec::new();
    let interpreter = words
        .first()
        .is_some_and(|f| INTERPRETERS.contains(&base(f)));
    for (i, w) in words.iter().enumerate() {
        let path = w.contains('/') && !interpreter;
        if i > 0 && (w.starts_with('-') || is_redirect(w) || path || out.len() >= 3) {
            break;
        }
        out.push(base(w));
    }
    let one = out.join(" ");
    const MAX: usize = 32;
    if one.chars().count() <= MAX {
        return one;
    }
    let cut: String = one.chars().take(MAX - 1).collect();
    format!("{}…", cut.trim_end())
}

/// The last part of a path: `/tmp/x/run.sh` is `run.sh`.
fn base(word: &str) -> &str {
    let w = word.trim_end_matches('/');
    w.rsplit('/').next().filter(|b| !b.is_empty()).unwrap_or(w)
}

fn is_assignment(word: &str) -> bool {
    match word.split_once('=') {
        Some((name, _)) => {
            !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                && !name.starts_with(|c: char| c.is_ascii_digit())
        }
        None => false,
    }
}

fn is_redirect(word: &str) -> bool {
    word.starts_with('>')
        || word.starts_with('<')
        || word.starts_with("2>")
        || word.starts_with("&>")
        || word.starts_with("1>")
}

/// A command line cut into its simple commands, each as words: split at
/// `;`, `&&`, `||`, `|`, `&` and new lines outside quotes, quotes taken
/// off. Rough, as a name needs no more.
fn segments(cmd: &str) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    let mut seg: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = cmd.chars().peekable();
    // Here-documents opened on this line, by their delimiters: their bodies
    // are skipped at its end.
    let mut heredocs: Vec<String> = Vec::new();
    let end_word = |seg: &mut Vec<String>, word: &mut String, in_word: &mut bool| {
        if *in_word {
            seg.push(std::mem::take(word));
            *in_word = false;
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '<' if chars.peek() == Some(&'<') => {
                chars.next();
                if chars.peek() == Some(&'<') {
                    // `<<<`, a here-string: a word, no body.
                    chars.next();
                    end_word(&mut seg, &mut word, &mut in_word);
                    seg.push("<<<".into());
                    continue;
                }
                end_word(&mut seg, &mut word, &mut in_word);
                seg.push("<<".into());
                if chars.peek() == Some(&'-') {
                    chars.next();
                }
                while chars.peek().is_some_and(|c| *c == ' ' || *c == '\t') {
                    chars.next();
                }
                let mut delim = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() || matches!(c, ';' | '&' | '|' | '<' | '>') {
                        break;
                    }
                    chars.next();
                    if !matches!(c, '\'' | '"' | '\\') {
                        delim.push(c);
                    }
                }
                if !delim.is_empty() {
                    heredocs.push(delim);
                }
            }
            '\'' => {
                in_word = true;
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    word.push(c);
                }
            }
            '"' => {
                in_word = true;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => {
                            if let Some(n) = chars.next() {
                                word.push(n);
                            }
                        }
                        c => word.push(c),
                    }
                }
            }
            '\\' => {
                if let Some(n) = chars.next()
                    && n != '\n'
                {
                    in_word = true;
                    word.push(n);
                }
            }
            ';' | '|' | '\n' => {
                end_word(&mut seg, &mut word, &mut in_word);
                if chars.peek() == Some(&c) {
                    chars.next();
                }
                out.push(std::mem::take(&mut seg));
                if c == '\n' {
                    for delim in std::mem::take(&mut heredocs) {
                        let mut line = String::new();
                        for c in chars.by_ref() {
                            if c != '\n' {
                                line.push(c);
                                continue;
                            }
                            if line.trim() == delim {
                                break;
                            }
                            line.clear();
                        }
                    }
                }
            }
            '&' => {
                // `2>&1`, `&>` stay words; `&&` and a lone `&` split.
                if in_word && word.ends_with('>') {
                    word.push(c);
                    continue;
                }
                if chars.peek() == Some(&'>') {
                    end_word(&mut seg, &mut word, &mut in_word);
                    in_word = true;
                    word.push(c);
                    continue;
                }
                end_word(&mut seg, &mut word, &mut in_word);
                if chars.peek() == Some(&'&') {
                    chars.next();
                }
                out.push(std::mem::take(&mut seg));
            }
            c if c.is_whitespace() => end_word(&mut seg, &mut word, &mut in_word),
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    end_word(&mut seg, &mut word, &mut in_word);
    out.push(seg);
    out.retain(|s| !s.is_empty());
    out
}

/// What an asleep worker waits on, as `resume` says it:
/// `waiting on wrangler deploy (30m)`, and `+N` for more than one.
pub fn waiting_on(shells: &[Shell], now: u64) -> String {
    format!("waiting on {}", named(shells, now))
}

/// The shells an asleep worker waits on, short, as the pane's NOW names
/// them: the oldest's [`gist`] and how long it has run,
/// `wrangler deploy (30m)`, and `+N` for the rest.
pub fn named(shells: &[Shell], now: u64) -> String {
    let mut sorted: Vec<&Shell> = shells.iter().collect();
    sorted.sort_by_key(|s| s.started_ms);
    let Some(first) = sorted.first() else {
        return String::new();
    };
    let more = match sorted.len() {
        1 => String::new(),
        n => format!(" +{}", n - 1),
    };
    format!(
        "{} ({}){more}",
        gist(&first.cmd),
        ran(first.started_ms, now)
    )
}

/// The processes holding a shell's output file: pid and start (clock ticks
/// after boot).
pub type Pids = Vec<(u32, u64)>;

/// How the pane looks at shells: who holds their output files, and
/// whether a process that did is still the one running.
pub trait Holders {
    /// For each of `session`'s `shells`, the processes holding its output
    /// file, with their starts; `None` when nothing could be read.
    fn holders(&self, session: &str, shells: &[Shell]) -> Option<Vec<Pids>>;
    /// Whether `pid` runs still, and is the process that started at
    /// `start`.
    fn alive(&self, pid: u32, start: u64) -> bool;
}

impl Holders for System {
    #[cfg(target_os = "linux")]
    fn holders(&self, session: &str, shells: &[Shell]) -> Option<Vec<Pids>> {
        holders_proc(session, shells).map(|h| h.pids)
    }

    #[cfg(not(target_os = "linux"))]
    fn holders(&self, _session: &str, _shells: &[Shell]) -> Option<Vec<Pids>> {
        None
    }

    fn alive(&self, pid: u32, start: u64) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| crate::registry::stat_start_ticks(&s))
            == Some(start)
    }
}

/// What the pane knows of one shell.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seen {
    /// These processes held its output file when it was first looked at.
    Held(Pids),
    /// Nothing runs it any more: final.
    Ended,
}

/// The pane's view of a session's background shells: what
/// tells a worker asleep on its own work from one that ended.
///
/// The recorded shells ([`record`]) are re-read when their file changes (a
/// `stat` a poll otherwise). A shell is looked for in `/proc` once, when it
/// is first seen: the processes holding its output file then are its own.
/// After that each poll asks only whether one of them still runs, by pid
/// and start time, which is cheap and never wavers: a pid that came back
/// as another process is not it. A shell found ended stays ended, and a
/// look that could not read `/proc` decides nothing, so a row does not
/// flicker between asleep and ended.
#[derive(Debug, Default)]
pub struct Watch {
    stamps: Vec<(PathBuf, Option<std::time::SystemTime>)>,
    recorded: Vec<(String, Shell)>,
    seen: std::collections::HashMap<String, Seen>,
    live: std::collections::HashMap<String, Vec<Shell>>,
}

impl Watch {
    /// Look again at `sessions`' shells (the session and its earlier ids).
    pub fn poll(&mut self, feed_dir: &Path, sessions: &[&str], now: u64) {
        self.poll_with(feed_dir, sessions, now, &System);
    }

    /// [`Watch::poll`], with `look` for `/proc`.
    pub fn poll_with(&mut self, feed_dir: &Path, sessions: &[&str], now: u64, look: &dyn Holders) {
        let files: Vec<(String, PathBuf)> = sessions
            .iter()
            .filter(|s| safe(s))
            .map(|s| (s.to_string(), path(feed_dir, s)))
            .collect();
        let stamps: Vec<(PathBuf, Option<std::time::SystemTime>)> = files
            .iter()
            .map(|(_, f)| {
                let m = std::fs::metadata(f).and_then(|m| m.modified()).ok();
                (f.clone(), m)
            })
            .collect();
        if stamps != self.stamps {
            self.stamps = stamps;
            self.recorded = files
                .iter()
                .flat_map(|(s, f)| {
                    read::<Book>(f)
                        .shells
                        .into_iter()
                        .map(move |sh| (s.clone(), sh))
                })
                .collect();
            let ids: Vec<&str> = self.recorded.iter().map(|(_, s)| s.id.as_str()).collect();
            self.seen.retain(|id, _| ids.contains(&id.as_str()));
        }
        // First looks, one `/proc` walk per session for all its new shells.
        let mut by_session: Vec<(&str, Vec<Shell>)> = Vec::new();
        for (session, shell) in &self.recorded {
            if self.seen.contains_key(&shell.id) {
                continue;
            }
            match by_session.iter_mut().find(|(s, _)| *s == session.as_str()) {
                Some((_, v)) => v.push(shell.clone()),
                None => by_session.push((session.as_str(), vec![shell.clone()])),
            }
        }
        let mut first = Vec::new();
        for (session, shells) in by_session {
            if let Some(held) = look.holders(session, &shells) {
                for (shell, pids) in shells.into_iter().zip(held) {
                    let seen = if pids.is_empty() {
                        Seen::Ended
                    } else {
                        Seen::Held(pids)
                    };
                    first.push((shell.id, seen));
                }
            }
        }
        self.seen.extend(first);
        // Later looks: does one of its processes still run?
        for seen in self.seen.values_mut() {
            if let Seen::Held(pids) = seen
                && !pids.iter().any(|(p, s)| look.alive(*p, *s))
            {
                *seen = Seen::Ended;
            }
        }
        let mut live: std::collections::HashMap<String, Vec<Shell>> = Default::default();
        for (_, shell) in &self.recorded {
            if matches!(self.seen.get(&shell.id), Some(Seen::Held(_)))
                && now.saturating_sub(shell.started_ms) < KEEP_MS
            {
                live.entry(shell.agent.clone())
                    .or_default()
                    .push(shell.clone());
            }
        }
        for v in live.values_mut() {
            v.sort_by_key(|s| s.started_ms);
        }
        self.live = live;
    }

    /// The live shells, by the worker that started them, oldest first.
    pub fn live(&self) -> &std::collections::HashMap<String, Vec<Shell>> {
        &self.live
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const T0: u64 = 1_790_000_000_000;
    const MIN: u64 = 60_000;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("giverny-bg-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A probe that says what it was told: ids not listed are unknown.
    #[derive(Default)]
    struct Fake {
        found: RefCell<Vec<(String, Found)>>,
    }

    impl Fake {
        fn set(&self, id: &str, running: Option<bool>, pid: Option<u32>) {
            let mut f = self.found.borrow_mut();
            f.retain(|(i, _)| i != id);
            f.push((id.into(), Found { running, pid }));
        }
    }

    impl Probe for Fake {
        fn look(&self, _session: &str, shells: &[Shell]) -> Vec<Found> {
            let f = self.found.borrow();
            shells
                .iter()
                .map(|s| {
                    f.iter()
                        .find(|(i, _)| *i == s.id)
                        .map(|(_, x)| *x)
                        .unwrap_or_default()
                })
                .collect()
        }
    }

    fn shell(id: &str, agent: &str, task: &str, cmd: &str, at: u64) -> Shell {
        Shell {
            id: id.into(),
            agent: agent.into(),
            task: task.into(),
            cmd: cmd.into(),
            started_ms: at,
        }
    }

    #[test]
    fn a_worker_ending_its_turn_with_a_shell_running_is_told_once() {
        let dir = temp("told");
        let probe = Fake::default();
        record(&dir, "s1", shell("b1", "a1", "g472", "sleep 600", T0), T0);
        record(&dir, "s1", shell("b2", "a1", "g472", "make", T0), T0);
        record(&dir, "s1", shell("b3", "a2", "other", "tail -f x", T0), T0);
        assert!(is_recorded(&dir, "s1", "b1"));
        assert_eq!(deliver(&dir, "s1", T0, &probe), None, "nothing yet");
        // b2 finished before the turn ended: not in flight.
        turn_ended(&dir, "s1", "a1", &["b1", "b3", "a1"], T0 + 40 * MIN, &probe);
        assert!(!is_recorded(&dir, "s1", "b2"), "finished: dropped");
        assert!(is_recorded(&dir, "s1", "b3"), "another worker's stays");
        probe.set("b1", Some(true), Some(4242));
        assert_eq!(
            deliver(&dir, "s1", T0 + 41 * MIN, &probe).as_deref(),
            Some(
                "Giverny: `g472`'s worker ended its turn and is asleep, waiting on 1 \
                 background shell of its own still running: `sleep 600` (41m, shell b1). \
                 Claude Code wakes it when that ends. Giverny stops nothing: if the work is \
                 meant to stop, stop it (`TaskStop b1` or `pkill -s 4242`), or let it run on \
                 if needed."
            )
        );
        assert_eq!(deliver(&dir, "s1", T0 + 42 * MIN, &probe), None, "once");
        // pause lists it, and it is still recorded: nothing was stopped.
        let live = task_shells(&dir, "s1", "g472", &probe);
        assert_eq!(live.len(), 1);
        assert_eq!(
            still_running("g472", &live, T0 + 50 * MIN),
            "\n  1 background shell its worker started still runs for g472 (Giverny stops \
             nothing):\n    `sleep 600` (shell b1), 50m in: `TaskStop b1` from this session \
             stops it, or `pkill -s 4242` (its shell, pid 4242, and all it started)"
        );
        assert!(task_shells(&dir, "s1", "nope", &probe).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
