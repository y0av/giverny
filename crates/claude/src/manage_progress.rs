//! The progress check: a task's worker that runs out of its
//! figure, or asks for more time, is put before its manager for a look.
//!
//! **Giverny suggests, managers decide.** The check goes into the manager's
//! context on its next own tool call, once per event: what the worker asked
//! for or how far past its figure it is, its note, what it has done so far
//! (working time, what it is doing now, its `run` commands and background
//! shells, its transcript), and the ask — does the work make sense, is it all
//! needed, could it be done faster? Giverny stops, pauses and re-estimates
//! nothing; the manager reads the worker's work and answers it, or lets it
//! go on.
//!
//! **Two triggers, each once.**
//! - **More time.** A worker's `eta` whose finish (now + the minutes left)
//!   is later than the finish its figure before implied by at least
//!   [`MORE_MIN_MS`] and at least [`MORE_FRAC`] of that figure
//!   ([`more_time`]): a small slip does not nag. The figure before is the
//!   latest one given (`left_s`, else the whole `eta_s`), counted down by the
//!   work since as the pane counts it. The row keeps the ask pending
//!   (`progress_more`: the finish before, the finish asked for, how many
//!   asks); a later `eta` before it is told folds in, against the same
//!   finish before, and one that takes it back under the threshold drops it.
//! - **Overdue.** The worker's figure has run out: its row's clock (the same
//!   wall clock the worker's own overdue ask counts by) is past `eta_s`. A
//!   fact about the row, found off the clock by whoever checks — the
//!   manager's hook, its watch, the worker's own hook — with no call from
//!   the worker needed; the row keeps the figure it is for
//!   (`progress_overdue`), and once told (`overdue_told`) the same figure is
//!   not told again.
//!
//! **One notice per [`QUIET_MS`] per task.** An event that comes sooner after
//! the task's last notice (`progress_told`) waits, pending, and is told when
//! the quiet is up, folded with any that came meanwhile. One that no longer
//! stands by then — the figure extended since, the row paused or landed — is
//! dropped, or kept for a paused row until it runs again.
//!
//! **The manager's own `eta`** on a task is its decision, not an ask: the
//! dispatcher's own Bash call running `eta <task>` drops that task's pending
//! more-time ask ([`own_eta`]).
//!
//! **Two ways in, one state.** The manager hears a check on its next own
//! tool call ([`deliver`]), or, idle, from `giverny manage watch`
//! ([`watch`]): a command it keeps running in the background, which sleeps
//! until the next check falls due ([`next_due`]: a figure running out, a
//! pending check's quiet ending) or the feed changes, prints the check and
//! ends — and Claude Code wakes a manager whose background command ended.
//! Both tell through [`check`] and stamp the row, so what one told the
//! other does not tell again. A more-time ask waits [`GRACE_MS`] first, so
//! the manager's own `eta` is dropped on its hook before the watch sees it.
//!
//! **Cheap.** Whoever leaves an event pending touches
//! `<feed dir>/progress/<session>`, and each check leaves when the next one
//! falls due in `progress/<session>.due`, keyed by the feed's mtime: the
//! manager's calls cost a `stat` or two and a small read while nothing is
//! pending or due. The watch looks at the feed's mtime every [`POLL_MS`].

use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::manage::{self, Lock};
use crate::{feed, sleeps};

/// A more-time ask that moves the finish less than this is not told.
pub const MORE_MIN_MS: u64 = 10 * 60 * 1000;

/// Nor one that moves it less than this share of the figure before.
pub const MORE_FRAC: f64 = 0.25;

/// At most one progress check per task in this span.
pub const QUIET_MS: u64 = 10 * 60 * 1000;

/// A more-time ask this fresh waits: the manager's own `eta` drops it on
/// its hook first ([`own_eta`]), and the watch must not tell it meanwhile.
pub const GRACE_MS: u64 = 15_000;

/// Where the pending markers live, under the feed directory.
pub const DIR: &str = "progress";

/// The fields a pending event keeps on its row.
const MORE: &str = "progress_more";
const OVERDUE: &str = "progress_overdue";

/// `<feed dir>/progress/<session>`: there while an event is pending.
pub fn marker(feed_dir: &Path, session: &str) -> PathBuf {
    feed_dir.join(DIR).join(session)
}

fn safe(session: &str) -> bool {
    !session.is_empty()
        && session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Whether a finish `by_s` later than the one before passes the threshold,
/// for a figure before of `figure_s`.
pub fn passes(by_s: i64, figure_s: u64) -> bool {
    by_s >= (MORE_MIN_MS / 1000) as i64 && by_s as f64 >= MORE_FRAC * figure_s as f64
}

/// A worker's `eta` on its Running `row`, after the new figure is written:
/// `before` is what the figure before still had left at `now` and that
/// figure itself, in seconds ([`manage::figure_left`]); `left` the new
/// figure. Leaves a more-time ask pending on the row when it passes the
/// threshold ([`passes`]), folds it into one already pending, and drops a
/// pending one this figure takes back under it. True when one is pending.
pub fn more_time(
    row: &mut Map<String, Value>,
    before: Option<(i64, u64)>,
    left: u64,
    now: u64,
) -> bool {
    let to = now + left * 1000;
    let pending = row.get(MORE).and_then(Value::as_object).cloned();
    let (from, figure, asks) = match &pending {
        Some(p) => (
            p.get("from").and_then(Value::as_u64),
            p.get("figure_s").and_then(Value::as_u64),
            p.get("asks").and_then(Value::as_u64).unwrap_or(1) + 1,
        ),
        None => match before {
            Some((rem, fig)) => (Some((now as i64 + rem * 1000).max(0) as u64), Some(fig), 1),
            None => (None, None, 1),
        },
    };
    let (Some(from), Some(figure)) = (from, figure) else {
        return false;
    };
    let by_s = (to as i64 - from as i64) / 1000;
    if !passes(by_s, figure) {
        row.remove(MORE);
        return false;
    }
    row.insert(
        MORE.into(),
        json!({"from": from, "to": to, "figure_s": figure, "asks": asks, "at": now}),
    );
    true
}

/// On a worker's own hook call, its Running `row`: leave the overdue event
/// pending when its figure has run out and has not been told. True when it
/// did (the row changed).
pub fn overdue(row: &mut Map<String, Value>, now: u64) -> bool {
    if row.contains_key("paused_since") {
        return false;
    }
    let Some(eta) = manage::u64_of(row, "eta_s").filter(|e| *e > 0) else {
        return false;
    };
    let Some(started) = manage::ms_of(row, "started") else {
        return false;
    };
    if now <= started + eta * 1000
        || manage::u64_of(row, "overdue_told") == Some(eta)
        || manage::u64_of(row, OVERDUE) == Some(eta)
    {
        return false;
    }
    row.insert(OVERDUE.into(), json!(eta));
    true
}

/// Whether any row of `doc` has an event pending.
pub fn any_pending(doc: &Value) -> bool {
    doc.get("rows")
        .and_then(Value::as_array)
        .is_some_and(|rows| {
            rows.iter()
                .filter_map(Value::as_object)
                .any(|r| r.contains_key(MORE) || r.contains_key(OVERDUE))
        })
}

/// Touch `session`'s marker when `doc` has an event pending.
pub fn mark(feed_dir: &Path, session: &str, doc: &Value) {
    if !safe(session) || !any_pending(doc) {
        return;
    }
    let m = marker(feed_dir, session);
    let _ = std::fs::create_dir_all(m.parent().unwrap_or(feed_dir));
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&m);
}

/// The task a Bash command of the dispatcher's own re-estimates, if it
/// runs `giverny-manage eta <task>` (or `giverny manage eta <task>`). The
/// hook's payload carries it as `own_eta` ([`crate::plugin_hook::payload_of`]).
pub fn own_eta(cmd: &str) -> Option<String> {
    let words: Vec<&str> = cmd.split_whitespace().collect();
    words.windows(3).find_map(|w| {
        let manage = w[0].ends_with("giverny-manage") || w[0] == "manage";
        (manage && w[1] == "eta").then(|| {
            w[2].trim_matches(|c| c == '"' || c == '\'' || c == ';')
                .to_string()
        })
    })
}

/// What the notice says of a task's worker, read only when one is told.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Work {
    /// Its working time so far, seconds.
    pub worked_s: u64,
    /// What it is doing now: its last tool call.
    pub doing: Option<String>,
    /// When it last wrote.
    pub last_ms: Option<u64>,
    /// Its `run` commands still going: command and start.
    pub runs: Vec<(String, Option<u64>)>,
    /// Its background shells still going: command, start, shell id.
    pub shells: Vec<(String, u64, String)>,
    /// Its transcript, when there is one.
    pub transcript: Option<PathBuf>,
}

/// How the notice reads a task's worker: the system, or a test's.
pub trait Look {
    fn work(&self, row: &Map<String, Value>, key: &str, now: u64) -> Work;
}

/// The real look: the row's clocks, the worker's transcript under the
/// dispatcher's (`transcript`), the session's live runs and shells.
pub struct System<'a> {
    pub feed_dir: &'a Path,
    pub session: &'a str,
    pub transcript: Option<&'a Path>,
}

impl Look for System<'_> {
    fn work(&self, row: &Map<String, Value>, key: &str, now: u64) -> Work {
        let sleeps = sleeps::load(self.feed_dir);
        let agent = row.get("agent_id").and_then(Value::as_str);
        let transcript = self.transcript.zip(agent).map(|(t, a)| {
            crate::subagents::agent_transcript(&t.with_extension("").join("subagents"), a)
        });
        let transcript = transcript.filter(|t| t.is_file());
        let act = transcript
            .as_deref()
            .map(crate::subagents::read_activity)
            .unwrap_or_default();
        let runs_dir = crate::run_live::runs_dir(&crate::resources::ledger_path(self.feed_dir));
        let runs = crate::run_live::task_runs(&runs_dir, self.session, key)
            .into_iter()
            .map(|r| (r.cmd.unwrap_or_default(), r.started_ms))
            .collect();
        let shells = crate::bg_shells::task_shells(
            self.feed_dir,
            self.session,
            key,
            &crate::bg_shells::System,
        )
        .into_iter()
        .map(|l| (l.shell.cmd, l.shell.started_ms, l.shell.id))
        .collect();
        Work {
            worked_s: manage::worked_s(row, now, &sleeps),
            doing: act.doing,
            last_ms: act.last_turn_ms,
            runs,
            shells,
            transcript,
        }
    }
}

/// On the dispatcher's own call: each task's pending event told once, when
/// its quiet is up ([`QUIET_MS`]), and a figure run out found off the clock,
/// whoever last called. A `stat` or two when nothing
/// is pending or due ([`due_by`]).
pub fn deliver(
    feed_dir: &Path,
    session: &str,
    payload: &Value,
    now: u64,
    look: &dyn Look,
) -> Option<String> {
    if !safe(session) {
        return None;
    }
    if !marker(feed_dir, session).exists() && !due_by(feed_dir, session, now) {
        return None;
    }
    let mine = payload.get("own_eta").and_then(Value::as_str);
    check(feed_dir, session, mine, now, look).0
}

/// `<feed dir>/progress/<session>.due`: when the session's next check falls
/// due ([`next_due`]), for the feed as it stood (its mtime, nanoseconds).
fn due_cache(feed_dir: &Path, session: &str) -> PathBuf {
    feed_dir.join(DIR).join(format!("{session}.due"))
}

fn mtime_ns(path: &Path) -> Option<u128> {
    let t = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(t.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos())
}

/// Whether a check may be due on `session`'s feed at `now`: its feed changed
/// since the cache was written, or the cache's due time has come. A session
/// with no feed of its own name has none.
fn due_by(feed_dir: &Path, session: &str, now: u64) -> bool {
    let Some(mtime) = mtime_ns(&feed::feed_path(feed_dir, session)) else {
        return false;
    };
    let cached = std::fs::read_to_string(due_cache(feed_dir, session)).ok();
    let mut words = cached.as_deref().unwrap_or("").split_whitespace();
    match (
        words.next().and_then(|m| m.parse::<u128>().ok()),
        words.next(),
    ) {
        (Some(m), Some(due)) if m == mtime => due.parse::<u64>().is_ok_and(|d| d <= now),
        _ => true,
    }
}

fn write_cache(feed_dir: &Path, session: &str, file: &Path, doc: &Value) {
    if file != feed::feed_path(feed_dir, session) {
        return; // a feed under an earlier id: the watch reads it whole
    }
    let Some(mtime) = mtime_ns(file) else {
        return;
    };
    let due = next_due(doc).map_or("-".to_string(), |d| d.to_string());
    let path = due_cache(feed_dir, session);
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(feed_dir));
    let _ = std::fs::write(path, format!("{mtime} {due}\n"));
}

/// When the next check on `doc` falls due, epoch ms (in the past: due
/// already): a Running row's figure running out, a pending event's
/// quiet ending, a fresh more-time ask's [`GRACE_MS`] passing. `None` when
/// nothing falls due without the feed changing: no Running row with a
/// figure still to run out, or only paused ones.
pub fn next_due(doc: &Value) -> Option<u64> {
    let rows = doc.get("rows").and_then(Value::as_array)?;
    rows.iter()
        .filter_map(Value::as_object)
        .filter_map(|row| {
            let pending = row.contains_key(MORE) || row.contains_key(OVERDUE);
            if manage::stage_of(row) != Some(feed::Stage::Running) {
                return pending.then_some(0); // moot: dropped on the next check
            }
            if row.contains_key("paused_since") {
                return None; // its resume changes the feed
            }
            let quiet = manage::ms_of(row, "progress_told").map_or(0, |t| t + QUIET_MS);
            let more = row
                .get(MORE)
                .and_then(Value::as_object)
                .map(|m| (m.get("at").and_then(Value::as_u64).unwrap_or(0) + GRACE_MS).max(quiet));
            let late = if row.contains_key(OVERDUE) {
                Some(quiet)
            } else {
                figure_end(row).map(|end| (end + 1).max(quiet))
            };
            [more, late].into_iter().flatten().min()
        })
        .min()
}

/// When a Running row's figure runs out, on the worker's own overdue
/// clock, unless that figure was told overdue already.
fn figure_end(row: &Map<String, Value>) -> Option<u64> {
    let eta = manage::u64_of(row, "eta_s").filter(|e| *e > 0)?;
    if manage::u64_of(row, "overdue_told") == Some(eta) {
        return None;
    }
    Some(manage::ms_of(row, "started")? + eta * 1000)
}

/// The check itself, for the hook and the watch: read the feed; leave the
/// overdue event pending on each Running row whose figure has run out; tell
/// what is due; keep the marker and the due cache. Returns what to tell
/// and the feed as it stands after (`None` with no feed of this writer's).
fn check(
    feed_dir: &Path,
    session: &str,
    mine: Option<&str>,
    now: u64,
    look: &dyn Look,
) -> (Option<String>, Option<Value>) {
    let m = marker(feed_dir, session);
    let file = manage::file_for(feed_dir, session);
    let read = || {
        std::fs::read(&file)
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .filter(|d| manage::writer_of(d) == Some(manage::WRITER))
    };
    let Some(doc) = read() else {
        let _ = std::fs::remove_file(&m);
        return (None, None);
    };
    // Nothing pending or due: no lock, no write.
    let due = |d: &Value| next_due(d).is_some_and(|t| t <= now);
    if !due(&doc) && mine.is_none_or(|k| !pending_on(&doc, k)) {
        if !any_pending(&doc) {
            let _ = std::fs::remove_file(&m);
        }
        write_cache(feed_dir, session, &file, &doc);
        return (None, Some(doc));
    }
    let Ok(_lock) = Lock::take(&file) else {
        return (None, Some(doc));
    };
    let Some(mut doc) = read() else {
        return (None, None);
    };
    let mut lines = Vec::new();
    let mut changed = false;
    for row in doc
        .get_mut("rows")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object_mut)
    {
        let running = manage::stage_of(row) == Some(feed::Stage::Running);
        // Off the clock: a figure run out is an event, whoever looks.
        if running {
            changed |= overdue(row, now);
        }
        if !row.contains_key(MORE) && !row.contains_key(OVERDUE) {
            continue;
        }
        let key = row
            .get("key")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string();
        if !running {
            changed |= row.remove(MORE).is_some() | row.remove(OVERDUE).is_some();
            continue;
        }
        if mine == Some(key.as_str()) && row.remove(MORE).is_some() {
            changed = true; // the manager's own re-estimate: its decision
        }
        // A figure extended since is not overdue any more.
        let late = late_s(row, now);
        if row.contains_key(OVERDUE)
            && (late.is_none() || manage::u64_of(row, OVERDUE) != manage::u64_of(row, "eta_s"))
        {
            row.remove(OVERDUE);
            changed = true;
        }
        if !row.contains_key(MORE) && !row.contains_key(OVERDUE) {
            continue;
        }
        let quiet =
            manage::ms_of(row, "progress_told").is_some_and(|t| now.saturating_sub(t) < QUIET_MS);
        // A more-time ask just made waits a moment: the manager's own `eta`
        // drops it on its hook ([`own_eta`]) before the watch tells it.
        let fresh = row
            .get(MORE)
            .and_then(|m| m.get("at"))
            .and_then(Value::as_u64)
            .is_some_and(|at| now < at + GRACE_MS);
        if row.contains_key("paused_since") || quiet || fresh {
            continue; // kept: told when it runs, or when the quiet is up
        }
        let work = look.work(row, &key, now);
        lines.push(notice(row, &key, late, &work, now));
        if let Some(eta) = row.remove(OVERDUE) {
            row.insert("overdue_told".into(), eta);
        }
        row.remove(MORE);
        row.insert("progress_told".into(), json!(manage::stamp(now)));
        changed = true;
    }
    if changed && manage::write(&file, &doc).is_err() {
        return (None, Some(doc));
    }
    if any_pending(&doc) {
        mark(feed_dir, session, &doc);
    } else {
        let _ = std::fs::remove_file(&m);
    }
    write_cache(feed_dir, session, &file, &doc);
    ((!lines.is_empty()).then(|| lines.join("\n\n")), Some(doc))
}

/// Whether the row `key` of `doc` has a more-time ask pending.
fn pending_on(doc: &Value, key: &str) -> bool {
    doc.get("rows")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_object)
        .any(|r| r.get("key").and_then(Value::as_str) == Some(key) && r.contains_key(MORE))
}

/// What one look of the watch found.
#[derive(Debug, Clone, PartialEq)]
pub enum Watch {
    /// A progress check (or several) to print.
    Told(String),
    /// No Running row: nothing to watch.
    Idle,
    /// Nothing yet; look again at this time (epoch ms), or when the feed
    /// changes (`None`: only then).
    Until(Option<u64>),
}

/// One look of `giverny manage watch` at `now`: the same check the hook
/// makes ([`check`]), so a check told here is not told again on the hook,
/// nor one told there here.
pub fn watch_step(feed_dir: &Path, session: &str, now: u64, look: &dyn Look) -> Watch {
    let (told, doc) = check(feed_dir, session, None, now, look);
    if let Some(t) = told {
        return Watch::Told(t);
    }
    let running = doc
        .as_ref()
        .and_then(|d| d.get("rows"))
        .and_then(Value::as_array)
        .is_some_and(|rows| {
            rows.iter()
                .filter_map(Value::as_object)
                .any(|r| manage::stage_of(r) == Some(feed::Stage::Running))
        });
    if !running {
        return Watch::Idle;
    }
    // Never sooner than a moment on: what is due now was just looked at.
    Watch::Until(doc.as_ref().and_then(next_due).map(|t| t.max(now + 1000)))
}

/// How often the watch looks at the feed between due times.
pub const POLL_MS: u64 = 2000;

/// `<feed dir>/progress/<session>.watch`: held (flock) by the session's
/// watch while it runs, with its pid.
fn watch_lock_path(feed_dir: &Path, session: &str) -> PathBuf {
    feed_dir.join(DIR).join(format!("{session}.watch"))
}

/// The session's one watch: held while it runs. `Err` with the running
/// one's pid (if it wrote one) when another holds it.
struct WatchLock {
    _file: std::fs::File,
}

impl WatchLock {
    fn take(feed_dir: &Path, session: &str) -> Result<WatchLock, Option<u32>> {
        let path = watch_lock_path(feed_dir, session);
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(feed_dir));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|_| None)?;
        #[cfg(unix)]
        {
            use std::io::{Read, Seek, Write};
            use std::os::fd::AsRawFd;
            // SAFETY: a valid open descriptor; LOCK_NB never blocks.
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                let mut s = String::new();
                let _ = (&file).read_to_string(&mut s);
                return Err(s.trim().parse().ok());
            }
            let _ = file.set_len(0);
            let _ = (&file).seek(std::io::SeekFrom::Start(0));
            let _ = write!(&file, "{}", std::process::id());
        }
        Ok(WatchLock { _file: file })
    }
}

/// What tells the watch the feed changed: the feed's mtime and whether a
/// check is pending.
fn feed_stamp(feed_dir: &Path, session: &str, file: &Path) -> (Option<u128>, bool) {
    (mtime_ns(file), marker(feed_dir, session).exists())
}

/// `giverny manage watch`: wait until a progress check is due on this
/// session's rows — a figure running out, a pending check's quiet ending,
/// a new ask — print it and end, so the manager's background command ends
/// and Claude Code wakes the manager even mid-idle. It only prints: it
/// stops, pauses and re-estimates nothing. With nothing Running it ends at
/// once; a second watch on the session ends at once too.
pub fn watch(feed_dir: &Path, session: &str, look: &dyn Look) -> String {
    if !safe(session) {
        return format!("giverny manage watch: not a session id: {session:?}");
    }
    let _held = match WatchLock::take(feed_dir, session) {
        Ok(l) => l,
        Err(pid) => {
            let pid = pid.map(|p| format!(" (pid {p})")).unwrap_or_default();
            return format!(
                "giverny manage watch: one is already running for this session{pid}; this one \
                 ends. Keep that one."
            );
        }
    };
    loop {
        let now = manage::now_ms();
        let until = match watch_step(feed_dir, session, now, look) {
            Watch::Told(t) => return t,
            Watch::Idle => {
                return "giverny manage watch: no task is Running in this session, so there is \
                        nothing to watch; it ends. Start one again once a task runs."
                    .to_string();
            }
            Watch::Until(t) => t,
        };
        let file = manage::file_for(feed_dir, session);
        let seen = feed_stamp(feed_dir, session, &file);
        loop {
            let now = manage::now_ms();
            if until.is_some_and(|t| now >= t) {
                break;
            }
            let nap = until.map_or(POLL_MS, |t| (t - now).min(POLL_MS));
            std::thread::sleep(std::time::Duration::from_millis(nap));
            if feed_stamp(feed_dir, session, &file) != seen {
                break;
            }
        }
    }
}

/// How far past its figure a Running row is, seconds, on the clock the
/// worker's own overdue ask counts by.
fn late_s(row: &Map<String, Value>, now: u64) -> Option<u64> {
    let eta = manage::u64_of(row, "eta_s").filter(|e| *e > 0)?;
    let end = manage::ms_of(row, "started")? + eta * 1000;
    (now > end).then(|| (now - end) / 1000)
}

fn span(s: u64) -> String {
    feed::fmt_span(s as i64)
}

/// `40m` or `12s` since `t`.
fn ago(t: u64, now: u64) -> String {
    match now.saturating_sub(t) / 1000 {
        s @ 0..60 => format!("{s}s"),
        s => span(s),
    }
}

/// The progress check for `key`: see the module's doc.
pub fn notice(
    row: &Map<String, Value>,
    key: &str,
    late: Option<u64>,
    work: &Work,
    now: u64,
) -> String {
    let clock = |ms: u64| crate::away::local_clock(ms, now);
    let mut why = Vec::new();
    if let Some(m) = row.get(MORE).and_then(Value::as_object) {
        let get = |k: &str| m.get(k).and_then(Value::as_u64).unwrap_or(0);
        let (from, to) = (get("from"), get("to"));
        let asks = match get("asks") {
            0 | 1 => String::new(),
            n => format!(" ({n} asks)"),
        };
        why.push(format!(
            "its worker asked for {} more{asks}: finish {} → {}",
            span(to.saturating_sub(from) / 1000),
            clock(from),
            clock(to)
        ));
    }
    if row.contains_key(OVERDUE)
        && let Some(late) = late
    {
        let eta = manage::u64_of(row, "eta_s").unwrap_or(0);
        // The watch tells it the moment it runs out: not "overdue by 0m".
        let by = match late {
            0..60 => "its worker's figure has just run out".to_string(),
            _ => format!("its worker is overdue by {}", span(late)),
        };
        why.push(format!(
            "{by} (its figure, {} in all, ran out at {})",
            span(eta),
            clock(now.saturating_sub(late * 1000))
        ));
    }
    let mut out = format!("Giverny: progress check on `{key}`: {}.", why.join("; "));
    if let Some(note) = row
        .get("note")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
    {
        out.push_str(&format!(" Its note: \"{note}\"."));
    }
    let mut done = vec![format!("{} of work", span(work.worked_s))];
    let waiting = manage::ms_of(row, "waiting_since");
    match (waiting, &work.doing) {
        (Some(w), _) => done.push(format!("waiting since {}", clock(w))),
        (None, Some(d)) => done.push(format!("now `{}`", crate::strays::quote(d))),
        (None, None) => {}
    }
    if let Some(t) = work.last_ms {
        done.push(format!("last wrote {} ago", ago(t, now)));
    }
    if !work.runs.is_empty() {
        let runs: Vec<String> = work
            .runs
            .iter()
            .map(|(cmd, at)| {
                let ran = at
                    .map(|t| format!(" ({} in)", ago(t, now)))
                    .unwrap_or_default();
                format!("{}{ran}", manage::short_cmd(cmd))
            })
            .collect();
        done.push(format!("runs {}", runs.join(", ")));
    }
    if !work.shells.is_empty() {
        let shells: Vec<String> = work
            .shells
            .iter()
            .map(|(cmd, at, id)| {
                format!("{} ({}, shell {id})", manage::short_cmd(cmd), ago(*at, now))
            })
            .collect();
        done.push(format!("background shells {}", shells.join(", ")));
    }
    out.push_str(&format!("\nSo far: {}.", done.join("; ")));
    if let Some(t) = &work.transcript {
        out.push_str(&format!(" Its transcript: {}", t.display()));
    }
    let answer = match row.get("agent_id").and_then(Value::as_str) {
        Some(a) => {
            format!("Answer it (SendMessage `{a}`): tell it to cut, change course or carry on")
        }
        None => "Answer its worker: tell it to cut, change course or carry on".to_string(),
    };
    out.push_str(&format!(
        "\nLook at what it has done: does it make sense, is it all needed, could it be done \
         faster? {answer}; or let it go on. Giverny stops and re-estimates nothing."
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_threshold_is_ten_minutes_and_a_quarter_of_the_figure() {
        assert!(!passes(9 * 60, 10 * 60), "under 10m");
        assert!(passes(10 * 60, 30 * 60), "10m of 30m");
        assert!(passes(10 * 60, 40 * 60), "10m is a quarter of 40m");
        assert!(!passes(14 * 60, 60 * 60), "14m of 1h is under a quarter");
        assert!(passes(15 * 60, 60 * 60));
        assert!(!passes(-20 * 60, 30 * 60), "sooner is no ask");
    }
}
