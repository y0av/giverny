//! The management-panel feed: what a manager knows that Claude Code does not.
//!
//! Claude Code itself tells Giverny which subagents a session is running and
//! which have finished (see `subagents`). What it cannot say is what those
//! workers are *for*: the task each one holds, the work queued behind them,
//! when each is expected to land and who reviews it. A manager that knows
//! that writes it to one JSON file per Claude session, and the pane merges
//! it with the live rows. The contract is `docs/management-panel.md`; this
//! module is its reader, and the doc is what a writer codes against.
//!
//! The file is someone else's output and will drift, so it is read the way
//! `jobs.rs` reads Claude Code's state: through `serde_json::Value`, one field
//! at a time. A surprise costs that field, or at worst that row, never the
//! feed.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;

/// The feed format this build reads. Bumped only by an *incompatible* change;
/// new fields are added without a bump, and readers ignore what they do not
/// know.
pub const FEED_VERSION: u64 = 1;

/// Where feeds live. Giverny exports this into every tab so a writer running
/// inside one needs no configuration; outside Giverny the writer falls back to
/// [`default_dir`] itself.
pub const DIR_ENV: &str = "GIVERNY_FEED_DIR";

/// `<config>/giverny/feeds`: the same base as Giverny's own state
/// (`~/.config/giverny` on Linux).
pub fn default_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config")
        })
        .join("giverny")
        .join("feeds")
}

/// `$GIVERNY_FEED_DIR` when set and non-empty, else [`default_dir`].
pub fn feed_dir() -> PathBuf {
    std::env::var(DIR_ENV)
        .ok()
        .filter(|d| !d.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(default_dir)
}

/// The file a writer for `session` writes: `<dir>/<session>.json`.
pub fn feed_path(dir: &Path, session: &str) -> PathBuf {
    dir.join(format!("{session}.json"))
}

/// Which section of the pane a row sits in, in drawing order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Stage {
    Running,
    Planned,
    Done,
}

impl Stage {
    /// Case-insensitive, with the obvious synonyms. `None` for anything else:
    /// a row that cannot be placed is dropped rather than guessed at.
    pub fn parse(s: &str) -> Option<Stage> {
        match s.trim().to_ascii_lowercase().as_str() {
            "running" | "live" | "working" | "in progress" | "in_progress" => Some(Stage::Running),
            "planned" | "queued" | "next" => Some(Stage::Planned),
            "done" | "finished" | "completed" | "landed" => Some(Stage::Done),
            _ => None,
        }
    }
}

impl From<crate::subagents::Stage> for Stage {
    fn from(s: crate::subagents::Stage) -> Stage {
        match s {
            crate::subagents::Stage::Running => Stage::Running,
            crate::subagents::Stage::Done => Stage::Done,
        }
    }
}

/// One row as the feed wrote it. Every field but `key` is optional; see
/// `docs/management-panel.md` for what each one drives.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FeedRow {
    /// What the row is: a task id such as `acme#158`. Falls back to
    /// `agent_id` when the writer gave none ([`FeedRow::keyed`]).
    pub key: String,
    /// The writer gave the row a `key`. A row with only an `agent_id`
    /// annotates that worker (its estimate, say) and names no task: the
    /// pane draws the worker as it would with no feed.
    pub keyed: bool,
    /// The section the writer put the row in. `None` only on a row with an
    /// `agent_id` and no `stage`, which follows its worker: Running while it
    /// runs, Done once it has finished.
    pub stage: Option<Stage>,
    pub title: Option<String>,
    /// The Claude Code subagent id holding this row: the join key with the
    /// live rows. Several rows may share one.
    pub agent_id: Option<String>,
    pub started_ms: Option<u64>,
    pub ended_ms: Option<u64>,
    /// The true start, when `started` was moved on by paused spans.
    pub spawned_ms: Option<u64>,
    /// Seconds the row has spent paused, an open pause counted up to when
    /// the feed was written; `started` is already moved on by them.
    pub paused_s: Option<u64>,
    /// When the row's still-open pause began: its clock is stopped.
    pub paused_since_ms: Option<u64>,
    /// Estimated total duration, in seconds, from `started`.
    pub eta_s: Option<u64>,
    /// How late (`+`) or early (`-`) a Done row landed against `eta_s`, when
    /// the writer knows better than `(ended − started) − eta_s`.
    pub eta_delta_s: Option<i64>,
    /// The latest figure given for the time left on a Running row, in
    /// seconds: the pane counts down from it, less the work done since
    /// [`Self::left_at_ms`]. `None` on a row never re-estimated.
    pub left_s: Option<u64>,
    /// When `left_s` was given, on the row's clock.
    pub left_at_ms: Option<u64>,
    pub landing: Option<String>,
    pub tokens: Option<u64>,
    pub brief: Option<PathBuf>,
    pub open: Option<String>,
    pub note: Option<String>,
    /// What a person has to look at before a Done row counts
    /// (`<who> — <what> — <where>`): the header's Review line over the
    /// row's worker page.
    pub review: Option<String>,
    /// Planned rows: the task this one waits for, until it lands.
    pub after: Option<String>,
    /// Planned rows: the manager holds it, for this reason.
    pub held: Option<String>,
}

/// A parsed feed file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Feed {
    pub version: u64,
    /// The Claude session id the feed is for.
    pub session: Option<String>,
    /// Earlier ids of the same conversation (a resume or a re-id), so a tab
    /// still holding an old id finds the feed.
    pub aliases: Vec<String>,
    pub rows: Vec<FeedRow>,
    /// One line drawn under the table, verbatim.
    pub footer: Option<String>,
    /// When the file was last written (its mtime): a paused row's `started`
    /// is only true as of this instant. `None` for a feed parsed from bytes.
    pub written_ms: Option<u64>,
}

impl Feed {
    /// The feed without the Done rows that landed by `cutoff_ms` (or never
    /// said when): what the pane shows after its Done rows were cleared by
    /// hand, whoever wrote the file.
    pub fn without_done_by(&self, cutoff_ms: u64) -> Feed {
        let mut f = self.clone();
        f.rows
            .retain(|r| r.stage != Some(Stage::Done) || r.ended_ms.is_some_and(|e| e > cutoff_ms));
        f
    }

    /// Is this feed about `session`, by its own id or by an alias?
    pub fn names(&self, session: &str) -> bool {
        self.session.as_deref() == Some(session) || self.aliases.iter().any(|a| a == session)
    }
}

/// Why a file is not a feed at all. Anything smaller costs a field or a row.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FeedError {
    #[error("feed is not JSON: {0}")]
    NotJson(String),
    #[error("feed is not a JSON object")]
    NotObject,
    #[error("feed version {0} is newer than this Giverny reads ({FEED_VERSION})")]
    Unsupported(u64),
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    match v.get(key)? {
        Value::String(s) => Some(s.trim()).filter(|s| !s.is_empty()).map(Into::into),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A non-negative whole number, from a number (fractions truncated) or a
/// numeric string.
fn u64_field(v: &Value, key: &str) -> Option<u64> {
    match v.get(key)? {
        Value::Number(n) => n.as_u64().or_else(|| {
            n.as_f64()
                .filter(|f| f.is_finite() && *f >= 0.0)
                .map(|f| f as u64)
        }),
        Value::String(s) => s
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|f| f.is_finite() && *f >= 0.0)
            .map(|f| f as u64),
        _ => None,
    }
}

/// A signed whole number, from a number or a numeric string.
fn i64_field(v: &Value, key: &str) -> Option<i64> {
    match v.get(key)? {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f as i64)),
        Value::String(s) => s
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|f| f.is_finite())
            .map(|f| f as i64),
        _ => None,
    }
}

/// Milliseconds since the epoch, from an RFC 3339 string or a number of
/// epoch milliseconds.
fn millis_field(v: &Value, key: &str) -> Option<u64> {
    match v.get(key)? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s
            .trim()
            .parse::<jiff::Timestamp>()
            .ok()
            .map(|t| t.as_millisecond().max(0) as u64),
        _ => None,
    }
}

fn parse_row(v: &Value) -> Option<FeedRow> {
    if !v.is_object() {
        return None;
    }
    let agent_id = str_field(v, "agent_id");
    // A stage Giverny does not know drops the row; no stage at all is a
    // row that follows its worker, so it needs one.
    let stage = match v.get("stage").filter(|s| !s.is_null()) {
        Some(s) => Some(Stage::parse(s.as_str()?)?),
        None => {
            agent_id.as_ref()?;
            None
        }
    };
    let given = str_field(v, "key");
    let keyed = given.is_some();
    let key = given.or_else(|| agent_id.clone())?;
    Some(FeedRow {
        key,
        keyed,
        stage,
        title: str_field(v, "title"),
        agent_id,
        started_ms: millis_field(v, "started"),
        ended_ms: millis_field(v, "ended"),
        spawned_ms: millis_field(v, "spawned"),
        paused_s: u64_field(v, "paused_s"),
        paused_since_ms: millis_field(v, "paused_since"),
        eta_s: u64_field(v, "eta_s"),
        eta_delta_s: i64_field(v, "eta_delta_s"),
        left_s: u64_field(v, "left_s"),
        left_at_ms: millis_field(v, "left_at"),
        landing: str_field(v, "landing"),
        tokens: u64_field(v, "tokens"),
        brief: str_field(v, "brief").map(PathBuf::from),
        open: str_field(v, "open"),
        note: str_field(v, "note"),
        review: str_field(v, "review"),
        after: str_field(v, "after"),
        held: str_field(v, "held"),
    })
}

/// Parse a feed from its bytes.
///
/// Only three things refuse the whole file: it is not JSON, it is not an
/// object, or it declares a version newer than [`FEED_VERSION`]. A row with
/// an unknown `stage`, or neither `key` nor `agent_id`, or neither `stage`
/// nor `agent_id`, is dropped; a field of the wrong type reads as absent.
pub fn parse(bytes: &[u8]) -> Result<Feed, FeedError> {
    let v: Value = serde_json::from_slice(bytes).map_err(|e| FeedError::NotJson(e.to_string()))?;
    if !v.is_object() {
        return Err(FeedError::NotObject);
    }
    let version = u64_field(&v, "version").unwrap_or(1);
    if version > FEED_VERSION {
        return Err(FeedError::Unsupported(version));
    }
    let aliases = v
        .get("aliases")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(Into::into)
                .collect()
        })
        .unwrap_or_default();
    let rows = v
        .get("rows")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(parse_row).collect())
        .unwrap_or_default();
    let footer = v.get("footer").and_then(|f| match f {
        Value::String(_) => str_field(&v, "footer"),
        Value::Object(_) => str_field(f, "text"),
        _ => None,
    });
    Ok(Feed {
        version,
        session: str_field(&v, "session"),
        aliases,
        rows,
        footer,
        written_ms: None,
    })
}

/// Read and parse one file. `None` when it is missing, unreadable or not a
/// feed: the pane then simply has no feed.
pub fn read(path: &Path) -> Option<Feed> {
    let bytes = std::fs::read(path).ok()?;
    let written_ms = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64);
    match parse(&bytes) {
        Ok(f) => Some(Feed { written_ms, ..f }),
        Err(e) => {
            tracing::debug!("agents feed {}: {e}", path.display());
            None
        }
    }
}

/// Find the feed for `session` in `dir`: `<session>.json` if it exists, else
/// the most recently modified `*.json` there whose `session` or `aliases`
/// names it (a writer that kept its old file name after a re-id).
pub fn find(dir: &Path, session: &str) -> Option<(PathBuf, Feed)> {
    let direct = feed_path(dir, session);
    if let Some(f) = read(&direct) {
        return Some((direct, f));
    }
    let mut best: Option<(SystemTime, PathBuf, Feed)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue; // `.tmp` files mid-write, and anything else
        }
        let Some(feed) = read(&path) else { continue };
        if !feed.names(session) {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        if best.as_ref().is_none_or(|(t, _, _)| mtime > *t) {
            best = Some((mtime, path, feed));
        }
    }
    best.map(|(_, p, f)| (p, f))
}

/// Re-reads a session's feed only when its file changed. Call [`poll`] once
/// a second; it costs one `stat` when nothing moved.
///
/// [`poll`]: FeedCache::poll
#[derive(Debug, Default)]
pub struct FeedCache {
    sessions: Vec<String>,
    path: Option<PathBuf>,
    stamp: Option<(SystemTime, u64)>,
    feed: Option<Feed>,
}

impl FeedCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The current feed for `session` in `dir`, re-read if the file's mtime
    /// or length changed, searched for afresh if it vanished or the session
    /// changed.
    pub fn poll(&mut self, dir: &Path, session: &str) -> Option<&Feed> {
        self.poll_any(dir, &[session])
    }

    /// [`FeedCache::poll`] for a conversation known by `sessions`: its
    /// current id first, then the ids it had before Claude Code re-id'd it.
    /// The feed is the first of them that [`find`] finds: the old id's
    /// until the writer names the new one.
    pub fn poll_any(&mut self, dir: &Path, sessions: &[&str]) -> Option<&Feed> {
        if self
            .sessions
            .iter()
            .map(String::as_str)
            .ne(sessions.iter().copied())
        {
            *self = FeedCache {
                sessions: sessions.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            };
        }
        let stamp_of = |p: &Path| {
            std::fs::metadata(p)
                .ok()
                .map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()))
        };
        // A feed found by an old id gives way to one the current id gets
        // a file of its own for.
        let own = sessions.first().map(|s| feed_path(dir, s));
        let outdated = self.path.is_some()
            && self.path != own
            && own.as_ref().is_some_and(|o| o.is_file())
            && !self
                .feed
                .as_ref()
                .is_some_and(|f| sessions.first().is_some_and(|s| f.names(s)));
        let fresh = self
            .path
            .as_deref()
            .filter(|_| !outdated)
            .and_then(|p| stamp_of(p).map(|s| (p.to_path_buf(), s)));
        match fresh {
            Some((_, s)) if Some(s) == self.stamp => {}
            Some((p, s)) => {
                // Changed in place: re-read. A half-written file (a writer
                // that ignored the rename rule) keeps the last good feed.
                if let Some(f) = read(&p) {
                    self.feed = Some(f);
                }
                self.stamp = Some(s);
            }
            None => {
                self.path = None;
                self.stamp = None;
                self.feed = None;
                if let Some((p, f)) = sessions.iter().find_map(|s| find(dir, s)) {
                    self.stamp = stamp_of(&p);
                    self.path = Some(p);
                    self.feed = Some(f);
                }
            }
        }
        self.feed.as_ref()
    }
}

// ---------------------------------------------------------------- merge ----

/// What the pane needs from a live row (Claude Code's own view of a
/// subagent) to merge it with the feed. The live-row model implements this;
/// the join key is [`agent_id`], the subagent's id as Claude Code reports it
/// (`tasks[].id` on the `subagentStatusLine` payload, the `<id>` of
/// `subagents/agent-<id>.jsonl`).
///
/// [`agent_id`]: LiveAgent::agent_id
pub trait LiveAgent {
    fn agent_id(&self) -> &str;
    /// Still running (else it has finished).
    fn running(&self) -> bool;
    fn started_ms(&self) -> Option<u64>;
    fn tokens(&self) -> Option<u64>;
    /// What the spawn said the worker is for (the Agent tool's
    /// `description`, e.g. `Work demo#82 …`): the second join key, for a
    /// feed row that carries no `agent_id`.
    fn description(&self) -> Option<&str> {
        None
    }
    /// When it finished, if it has.
    fn ended_ms(&self) -> Option<u64> {
        None
    }
    /// The agent that spawned it, when another agent did.
    fn parent_id(&self) -> Option<&str> {
        None
    }
}

/// Does `text` name `key` as a whole word? `demo#82` is named by
/// `Work demo#82 pane` and by `owner/demo#82`, never by
/// `demo#820` nor `xdemo#82`.
pub fn names_key(text: &str, key: &str) -> bool {
    let word = |c: char| c.is_alphanumeric() || c == '_' || c == '-';
    !key.is_empty()
        && text.match_indices(key).any(|(i, _)| {
            let before = text[..i].chars().next_back();
            let after = text[i + key.len()..].chars().next();
            before.is_none_or(|c| !word(c) && c != '#') && after.is_none_or(|c| !word(c))
        })
}

/// How long after its worker a feed row must have begun to be a later task
/// of that worker, keeping its own clock, rather than the spawn the writer
/// stamped a little early. A writer stamps `started` seconds to a minute or
/// two before the spawn; a second task sent to a worker that finished its
/// first comes a task's length later.
pub const LATER_TASK_MS: u64 = 5 * 60 * 1000;

/// One row of the pane: a feed row, a live row, or both joined.
#[derive(Debug)]
pub struct PaneRow<'a, L> {
    pub stage: Stage,
    pub feed: Option<&'a FeedRow>,
    pub live: Option<&'a L>,
    /// Same worker as the row directly above: draw its per-worker cells
    /// (elapsed, ETA, activity, tokens) as `"`.
    pub ditto: bool,
    /// A Planned row queued on a worker busy with another task: that task.
    pub after_key: Option<String>,
}

impl<L> PaneRow<'_, L> {
    /// A row with nothing in it, for `..PaneRow::bare()`.
    fn bare() -> Self {
        PaneRow {
            stage: Stage::Planned,
            feed: None,
            live: None,
            ditto: false,
            after_key: None,
        }
    }
}

/// How far a writer moved a row's `started` on for its pauses
/// (`started − spawned`, else `paused_s`).
fn moved_ms(f: &FeedRow) -> u64 {
    match (f.spawned_ms, f.started_ms) {
        (Some(spawned), Some(started)) => started.saturating_sub(spawned),
        _ => f.paused_s.unwrap_or(0).saturating_mul(1000),
    }
}

impl<L: LiveAgent> PaneRow<'_, L> {
    pub fn agent_id(&self) -> Option<&str> {
        self.feed
            .and_then(|f| f.agent_id.as_deref())
            .or_else(|| self.live.map(|l| l.agent_id()))
    }

    /// Start of the row's clock. With a worker, the worker's own start
    /// (Claude Code's `startTime`, the clock its agents view counts from),
    /// so the two agree: a writer stamps `started` when it is about to
    /// spawn, a little before the worker exists. The feed's `started` still
    /// counts where it is the truer one:
    ///
    /// - a row with no worker, or whose worker gave no start;
    /// - a row that began well after its worker did ([`LATER_TASK_MS`]): one
    ///   worker given a second task later holds a row per task, each
    ///   started at its own time;
    /// - a row that began well before its worker did ([`LATER_TASK_MS`]): a
    ///   fresh worker taking over a task, often after a pause. Its pauses
    ///   came before that worker, so the worker's start moved on by them
    ///   would lie ahead and hold the clock at zero; the feed's `started`
    ///   already has them off;
    /// - the row's paused spans, which Claude Code knows nothing of: the
    ///   worker's start is moved on by the same amount the writer moved
    ///   `started` on by (`started − spawned`, else `paused_s`).
    pub fn started_ms(&self) -> Option<u64> {
        let worker = self.live.and_then(|l| l.started_ms());
        let Some(f) = self.feed else { return worker };
        let moved = moved_ms(f);
        let Some(worker) = worker else {
            return f.started_ms;
        };
        let Some(started) = f.started_ms else {
            return Some(worker);
        };
        let began = started.saturating_sub(moved);
        // A task the spawn named is the worker's first, whenever the writer
        // stamped it.
        if began > worker.saturating_add(LATER_TASK_MS) && !self.spawn_task()
            || worker > began.saturating_add(LATER_TASK_MS)
        {
            Some(started)
        } else {
            Some(worker.saturating_add(moved))
        }
    }

    /// When this row's clock stopped, if it is paused right now: the feed's
    /// `paused_since`, on a Running row only.
    pub fn paused_since_ms(&self) -> Option<u64> {
        if self.stage != Stage::Running {
            return None;
        }
        self.feed.and_then(|f| f.paused_since_ms)
    }

    /// Was this row's task given at the spawn: the worker's description
    /// names its key.
    fn spawn_task(&self) -> bool {
        match (self.feed, self.live.and_then(|l| l.description())) {
            (Some(f), Some(d)) => f.keyed && names_key(d, &f.key),
            _ => false,
        }
    }

    /// When the row's task ended: the feed's `ended`, else its finished
    /// worker's end.
    pub fn ended_ms(&self) -> Option<u64> {
        self.feed.and_then(|f| f.ended_ms).or_else(|| {
            self.live
                .filter(|l| !l.running())
                .and_then(|l| l.ended_ms())
        })
    }

    /// Tokens: the worker's count (Claude Code's own, as its agents view
    /// shows it), else what the feed wrote. A Planned row has spent
    /// nothing.
    pub fn tokens(&self) -> Option<u64> {
        if self.stage == Stage::Planned {
            return None;
        }
        self.live
            .and_then(|l| l.tokens())
            .or_else(|| self.feed.and_then(|f| f.tokens))
    }

    /// The latest figure on a re-estimated row: when it was given, never
    /// before the row's clock starts, and the seconds it said were left
    /// then. A Running row's time left is that figure less the work done
    /// since; `None` on a row never re-estimated, which counts down from
    /// `eta_s` and its start.
    pub fn last_figure(&self) -> Option<(u64, u64)> {
        let f = self.feed?;
        let (at, left) = f.left_at_ms.zip(f.left_s)?;
        Some((self.started_ms().map_or(at, |s| at.max(s)), left))
    }

    /// A Done row's ETA cell, in seconds, and the figure it is measured
    /// against: `eta_delta_s` when the feed sent it, else [`done_delta`]
    /// over the row's working time: its clock's end less its start
    /// (already moved on by pauses), less the ms `off_ms` says of it
    /// nothing ran. `None` on any other stage, or when there is no estimate
    /// to compare with.
    pub fn eta_delta_s(&self, off_ms: &dyn Fn(u64, u64) -> u64) -> Option<(i64, Against)> {
        if self.stage != Stage::Done {
            return None;
        }
        let f = self.feed?;
        if let Some(d) = f.eta_delta_s {
            return Some((d, Against::Given));
        }
        let (start, end) = (self.started_ms()?, self.ended_ms()?);
        let work_s = end.saturating_sub(start).saturating_sub(off_ms(start, end)) / 1000;
        done_delta(work_s, f.eta_s)
    }
}

/// What a Done row's ± was measured against: the header over its page
/// names it, `vs estimate 30m`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Against {
    /// The row's `eta_s`, in seconds.
    Estimate(u64),
    /// The writer's own `eta_delta_s`, whose figure is not known.
    Given,
}

impl Against {
    /// The words after `vs` in the header.
    pub fn label(self) -> String {
        match self {
            Against::Estimate(s) => format!("estimate {}", fmt_span(s as i64)),
            Against::Given => "estimate".into(),
        }
    }
}

/// How late (`+`) or early (`-`) a landed task was, in seconds: its work
/// less its `estimate`.
pub fn done_delta(work_s: u64, estimate: Option<u64>) -> Option<(i64, Against)> {
    let e = estimate?;
    Some((work_s as i64 - e as i64, Against::Estimate(e)))
}

/// Which of a session's merged rows its pane draws: every drawn row is a
/// real agent. A feed row annotates a worker (its title, ETA, brief), and
/// with no subagent behind it is not drawn: any agent can write a row, and
/// such a row would open the pane alone, blank where its worker's cells go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drawn {
    /// A session that is no manager: only rows a subagent Claude Code
    /// lists stands behind.
    Agents,
    /// A manager session ([`crate::managers`]): those, and its Next up rows,
    /// which no worker stands behind yet.
    Manager,
    /// Every merged row: for the tests of the merge itself.
    Every,
}

impl Drawn {
    /// What a session's pane draws: [`Drawn::Manager`] for a manager, else
    /// [`Drawn::Agents`].
    pub fn of(manager: bool) -> Drawn {
        if manager {
            Drawn::Manager
        } else {
            Drawn::Agents
        }
    }

    /// Is `row` drawn? Asked of the rows as the pane places them: after a
    /// Running row with no worker is moved under Next up.
    pub fn keeps<L>(self, row: &PaneRow<'_, L>) -> bool {
        match self {
            Drawn::Every => true,
            _ if row.live.is_some() => true,
            Drawn::Agents => false,
            Drawn::Manager => row.stage == Stage::Planned,
        }
    }
}

/// Does a session get a pane at all: only when subagents are involved (its
/// tracker holds a real one, running or done this session, `has_agents`)
/// or it is a manager, whose Next up shows before its first spawn. Its feed
/// never opens one by itself.
pub fn opens(has_agents: bool, manager: bool) -> bool {
    has_agents || manager
}

/// Merge the feed with the live rows.
///
/// - Every feed row is a row, in feed order, in the stage the feed gave it.
///   It carries the live row its `agent_id` names; failing that (no
///   `agent_id`, or one Claude Code no longer lists), the live row whose
///   description names the row's key: `Work demo#82 …` holds `demo#82`,
///   and one naming several keys holds each of them. A Running row whose
///   named worker has finished carries a running one spawned after it
///   whose description names the key, if there is one. The feed's stage
///   wins: one worker may hold a Done row and a Running one at once.
/// - A row with no `stage` follows the worker its `agent_id` names, Running
///   or Done; with no such worker it is no row.
/// - A live row that no feed row carries, and whose description names no
///   feed key, is a row of its own, Running or Done by its own state, after
///   the feed's rows of that stage. One that names a key is that row's
///   worker and is never drawn twice.
/// - Rows are grouped Running, Planned, Done; order within Running and
///   Planned is kept, Done is newest landed first (undated rows last).
/// - A row whose worker is the one directly above it is a ditto.
pub fn merge<'a, L: LiveAgent>(feed: Option<&'a Feed>, live: &'a [L]) -> Vec<PaneRow<'a, L>> {
    let feed_rows: &[FeedRow] = feed.map(|f| f.rows.as_slice()).unwrap_or(&[]);
    let find_live = |id: &str| live.iter().find(|l| l.agent_id() == id);
    // An agent another listed agent spawned is never a feed row's worker by
    // its description: it works for its parent, whose task it may well
    // name, and is drawn under its parent's row. Only an `agent_id` makes it
    // a row's worker.
    let nested = |l: &L| {
        l.parent_id()
            .is_some_and(|p| live.iter().any(|o| o.agent_id() == p))
    };
    let names = |l: &L, f: &FeedRow| {
        f.keyed && !nested(l) && l.description().is_some_and(|d| names_key(d, &f.key))
    };
    // A Running row wants the worker still running; any other the one that
    // finished. Either takes the other when that is all there is.
    let by_description = |f: &FeedRow| -> Option<&'a L> {
        let want_running = f.stage == Some(Stage::Running);
        let mut named = live.iter().filter(|l| names(l, f));
        let first = named.clone().next()?;
        Some(named.find(|l| l.running() == want_running).unwrap_or(first))
    };
    // A Running row whose worker has finished, and a worker spawned after it
    // for the same task is running: the manager resumed the task with a
    // fresh worker and never moved the row. One another row names by id is
    // that row's.
    let successor = |f: &FeedRow, held: &L| -> Option<&'a L> {
        live.iter().find(|l| {
            l.running()
                && l.agent_id() != held.agent_id()
                && names(l, f)
                && !feed_rows
                    .iter()
                    .any(|o| o.agent_id.as_deref() == Some(l.agent_id()))
                && match (l.started_ms(), held.started_ms()) {
                    (Some(new), Some(old)) => new > old,
                    _ => true,
                }
        })
    };
    let mut out: Vec<PaneRow<'a, L>> = feed_rows
        .iter()
        .filter_map(|f| {
            let live = match f.agent_id.as_deref().and_then(find_live) {
                Some(held) if f.stage == Some(Stage::Running) && !held.running() => {
                    successor(f, held).or(Some(held))
                }
                Some(held) => Some(held),
                None => by_description(f),
            };
            let stage = match (f.stage, live) {
                (Some(s), _) => s,
                (None, Some(l)) if l.running() => Stage::Running,
                (None, Some(_)) => Stage::Done,
                (None, None) => return None,
            };
            Some(PaneRow {
                stage,
                feed: Some(f),
                live,
                ..PaneRow::bare()
            })
        })
        .collect();
    for l in live {
        let carried = out
            .iter()
            .any(|r| r.live.is_some_and(|c| c.agent_id() == l.agent_id()));
        let named = carried
            || feed_rows
                .iter()
                .any(|f| f.agent_id.as_deref() == Some(l.agent_id()) || names(l, f));
        if !named {
            out.push(PaneRow {
                stage: if l.running() {
                    Stage::Running
                } else {
                    Stage::Done
                },
                feed: None,
                live: Some(l),
                ..PaneRow::bare()
            });
        }
    }
    queued(&mut out);
    // Running and Next up keep feed order, then live order. Done is newest
    // landed first; a worker's rows are kept together and sort by its
    // latest end, and rows with no end time go after the dated ones, in
    // their old order.
    let group_end = |r: &PaneRow<'a, L>| {
        let id = r.agent_id()?;
        out.iter()
            .filter(|o| o.stage == Stage::Done && o.agent_id() == Some(id))
            .filter_map(|o| o.ended_ms())
            .max()
    };
    let keys: Vec<_> = out
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let end = if r.stage == Stage::Done {
                group_end(r).or_else(|| r.ended_ms())
            } else {
                None
            };
            // Newest first; undated last.
            (r.stage, end.is_none(), std::cmp::Reverse(end), i)
        })
        .collect();
    let mut order: Vec<usize> = (0..out.len()).collect();
    order.sort_by(|&a, &b| keys[a].cmp(&keys[b]));
    let mut slots: Vec<Option<PaneRow<'a, L>>> = out.into_iter().map(Some).collect();
    let mut out: Vec<PaneRow<'a, L>> = order.into_iter().filter_map(|i| slots[i].take()).collect();
    for i in 1..out.len() {
        let same = match (out[i - 1].agent_id(), out[i].agent_id()) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        };
        out[i].ditto = same;
    }
    out
}

/// A Planned row whose `agent_id` names a worker running another task is
/// queued after it: [`PaneRow::after_key`] is that task.
fn queued<L: LiveAgent>(rows: &mut [PaneRow<'_, L>]) {
    let busy: Vec<(String, String)> = rows
        .iter()
        .filter(|r| r.stage == Stage::Running)
        .filter_map(|r| {
            let f = r.feed.filter(|f| f.keyed)?;
            Some((r.agent_id()?.to_string(), f.key.clone()))
        })
        .collect();
    for r in rows.iter_mut().filter(|r| r.stage == Stage::Planned) {
        let id = r.feed.and_then(|f| f.agent_id.as_deref());
        r.after_key = busy
            .iter()
            .find(|(w, _)| Some(w.as_str()) == id)
            .map(|(_, k)| k.clone());
    }
}

// ----------------------------------------------------------- formatting ----

/// A span as the pane's clock columns write it: `5m`, `1h3m`, `1d3h12m`,
/// whole minutes (rounded), zero units left out, `0m` for nothing. A
/// negative span keeps a leading `-`. The caller adds `~` for an estimate.
pub fn fmt_span(secs: i64) -> String {
    let mins = (secs.abs() + 30) / 60;
    let (d, rest) = (mins / 1440, mins % 1440);
    let (h, m) = (rest / 60, rest % 60);
    let body: String = [(d, "d"), (h, "h"), (m, "m")]
        .iter()
        .filter(|(n, _)| *n != 0)
        .map(|(n, u)| format!("{n}{u}"))
        .collect();
    let body = if body.is_empty() { "0m".into() } else { body };
    if secs < 0 && mins != 0 {
        format!("-{body}")
    } else {
        body
    }
}

/// A Done row's ETA cell: `(+5m)` late, `(-1h20m)` early, `(±0m)` on the
/// minute.
pub fn fmt_delta(secs: i64) -> String {
    let s = fmt_span(secs);
    if s == "0m" {
        "(±0m)".into()
    } else if secs > 0 {
        format!("(+{s})")
    } else {
        format!("({s})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"{"version":1,"session":"s-new","aliases":["s-old"],
      "rows":[
        {"key":"acme#158","stage":"running","title":"FEATURE: pane","agent_id":"a1",
         "started":"2026-09-23T10:00:00Z","eta_s":2400,"landing":"Review — alex",
         "tokens":123456,"brief":"/tmp/b.md","open":"claude --resume x","note":"n"},
        {"key":"acme#159","stage":"done","agent_id":"a1",
         "started":"2026-09-23T09:00:00Z","ended":"2026-09-23T09:45:00Z","eta_s":2400},
        {"key":"acme#160","stage":"planned","title":"later","eta_s":600}
      ],
      "footer":{"text":"session: 3 · subagents: 1.2M · total: 1.2M"}}"#;

    #[test]
    fn parses_every_field() {
        let f = parse(FULL.as_bytes()).unwrap();
        assert_eq!(f.version, 1);
        assert_eq!(f.session.as_deref(), Some("s-new"));
        assert_eq!(f.aliases, vec!["s-old".to_string()]);
        assert_eq!(
            f.footer.as_deref(),
            Some("session: 3 · subagents: 1.2M · total: 1.2M")
        );
        assert_eq!(f.rows.len(), 3);
        let r = &f.rows[0];
        assert_eq!(r.key, "acme#158");
        assert_eq!(r.stage, Some(Stage::Running));
        assert_eq!(r.title.as_deref(), Some("FEATURE: pane"));
        assert_eq!(r.agent_id.as_deref(), Some("a1"));
        assert_eq!(r.started_ms, Some(1_790_157_600_000));
        assert_eq!(r.eta_s, Some(2400));
        assert_eq!(r.landing.as_deref(), Some("Review — alex"));
        assert_eq!(r.tokens, Some(123_456));
        assert_eq!(r.brief.as_deref(), Some(Path::new("/tmp/b.md")));
        assert_eq!(r.open.as_deref(), Some("claude --resume x"));
        assert_eq!(r.note.as_deref(), Some("n"));
        assert_eq!(f.rows[1].ended_ms, Some(1_790_156_700_000));
        assert!(f.names("s-new") && f.names("s-old") && !f.names("s-x"));
    }

    #[test]
    fn a_surprise_costs_a_field_or_a_row_never_the_feed() {
        let f = parse(
            br#"{"rows":[
              {"key":"k1","stage":"RUNNING","title":42,"eta_s":"90","tokens":-5,
               "started":"not a date","eta_delta_s":"-120","unknown":{"x":1}},
              {"key":"k2","stage":"sideways"},
              {"stage":"done"},
              {"stage":"done","agent_id":"a9"},
              "not an object",
              {"key":"k3"},
              {"agent_id":"a7","eta_s":600}
            ],"footer":"plain string","extra":[1,2]}"#,
        )
        .unwrap();
        assert_eq!(f.version, 1, "an absent version is 1");
        let keys: Vec<_> = f.rows.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(
            keys,
            ["k1", "a9", "a7"],
            "unplaceable and unnamed rows are dropped"
        );
        let r = &f.rows[0];
        assert_eq!(r.stage, Some(Stage::Running));
        assert_eq!(r.title.as_deref(), Some("42"));
        assert_eq!(r.eta_s, Some(90));
        assert_eq!(r.tokens, None);
        assert_eq!(r.started_ms, None);
        assert_eq!(r.eta_delta_s, Some(-120));
        // An agent id alone: the worker's own row, following its stage.
        let a7 = &f.rows[2];
        assert_eq!((a7.stage, a7.keyed, a7.eta_s), (None, false, Some(600)));
        assert_eq!(f.footer.as_deref(), Some("plain string"));
    }
}
