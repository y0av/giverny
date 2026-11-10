//! The management-panel feed: what a manager knows that Claude Code does not.
//!
//! Claude Code itself tells Giverny which subagents a session is running and
//! which have finished (see `subagents`). What it cannot say is what those
//! workers are *for* — the task each one holds, the work queued behind them,
//! when each is expected to land and who reviews it. A manager that
//! knows that writes it to one JSON file per Claude session, and the pane
//! merges it with the live rows. The contract is `docs/management-panel.md`; this
//! module is its reader, and the doc is what a writer codes against.
//!
//! The file is someone else's output and will drift, so it is read the way
//! `jobs.rs` reads Claude Code's state: through `serde_json::Value`, one field
//! at a time. A surprise costs that field, or at worst that row — never the
//! feed.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;

use crate::worker_log::WorkerLog;

/// The feed format this build reads. Bumped only by an *incompatible* change;
/// new fields are added without a bump, and readers ignore what they do not
/// know.
pub const FEED_VERSION: u64 = 1;

/// Where feeds live. Giverny exports this into every tab so a writer running
/// inside one needs no configuration; outside Giverny the writer falls back to
/// [`default_dir`] itself.
pub const DIR_ENV: &str = "GIVERNY_FEED_DIR";

/// `<config>/giverny/feeds` — the same base as Giverny's own state
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
/// Inside a background job always [`default_dir`]: the variable there is
/// whichever Giverny started the job's daemon, not the one showing it
/// ([`crate::lineage::giverny_var_in`]).
pub fn feed_dir() -> PathBuf {
    feed_dir_in(&|name| std::env::var(name).ok())
}

/// [`feed_dir`], reading the environment through `get`.
pub fn feed_dir_in(get: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    crate::lineage::giverny_var_in(DIR_ENV, get)
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

/// One row as the feed wrote it. Every field but `key` and `stage` is
/// optional; see `docs/management-panel.md` for what each one drives.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FeedRow {
    /// What the row is — a task id such as `acme#158`. Falls back to
    /// `agent_id` when the writer gave none.
    pub key: String,
    pub stage: Option<Stage>,
    pub title: Option<String>,
    /// The Claude Code subagent id holding this row — the join key with the
    /// live rows. Several rows may share one.
    pub agent_id: Option<String>,
    pub started_ms: Option<u64>,
    pub ended_ms: Option<u64>,
    /// The true spawn, when `started` was moved on by paused spans.
    pub spawned_ms: Option<u64>,
    /// Seconds the row has spent paused, an open pause counted up to when
    /// the feed was written; `started` is already moved on by them.
    pub paused_s: Option<u64>,
    /// When the row's still-open pause began: its clock is stopped.
    pub paused_since_ms: Option<u64>,
    /// `giverny manage`: when the row was found killed:
    /// Giverny, WSL or its manager session went off and its worker with
    /// it. Its clock is stopped at `paused_since`, the last sign of life,
    /// until its manager revives it (`resume`), and NOW reads `killed`.
    pub killed_ms: Option<u64>,
    /// `giverny manage`: when the row's session last went off with its
    /// worker, kept past the revive: a worker not seen at
    /// work since died then ([`died_with_session`]).
    pub went_off_ms: Option<u64>,
    /// Estimated total duration, in seconds, from `started`.
    pub eta_s: Option<u64>,
    /// How late (`+`) or early (`-`) a Done row landed against `eta_s`, when
    /// the writer knows better than `(ended − started) − eta_s`.
    pub eta_delta_s: Option<i64>,
    /// What the worker said was left at its latest `eta` on the Running row,
    /// in seconds: the pane counts down from it, less the work
    /// done since [`Self::left_at_ms`]. `eta_s` beside it adds the wall time
    /// up to the figure, so counting down from `eta_s` less the work so far
    /// took every off-clock span before the figure off twice. `None` on a
    /// row never re-estimated since its `start`, and in feeds from before.
    pub left_s: Option<u64>,
    /// When `left_s` was given, on the row's clock: a pause closed after it
    /// moves it on as it moves `started` on, and one open when it was given
    /// puts it at the pause's start.
    pub left_at_ms: Option<u64>,
    /// `giverny manage`: the seconds of closed waiting spans (`eta --why
    /// wait`), left out of the working time a Done row is measured by.
    pub wait_s: Option<u64>,
    /// `giverny manage`: when the worker's still-open wait began (`eta
    /// --why wait`); the pane says so on a row nothing works on now.
    pub waiting_since_ms: Option<u64>,
    /// `giverny manage`: the dispatcher's figure at the start, kept when a
    /// worker's `eta` replaced `eta_s`; `eta_guess_s` the raw guess a row
    /// written by an older version kept beside a corrected `eta_s`. A Done row
    /// with no worker's re-estimate is measured against the first of
    /// them, else `eta_s` ([`done_delta`]).
    pub eta_first_s: Option<u64>,
    pub eta_guess_s: Option<u64>,
    /// `giverny manage`: the worker's first re-estimate of the time left,
    /// and the working time it was given at: what a Done row is measured
    /// against when it has one ([`done_delta`]).
    pub reest_s: Option<u64>,
    pub reest_at_s: Option<u64>,
    pub landing: Option<String>,
    pub tokens: Option<u64>,
    /// What a reused worker's task spent, written when it landed: its
    /// turns' added tokens from its start to its landing. A Done row's
    /// count, frozen there, whatever the worker does next.
    pub task_tokens: Option<u64>,
    pub group: Option<String>,
    pub brief: Option<PathBuf>,
    pub open: Option<String>,
    pub note: Option<String>,
    /// What a person has to look at before a Done row counts
    /// (`<who> — <what> — <where>`): the top line of the row's overlay. The
    /// line comes only from `land --review`; without it the row has no Review line.
    pub review: Option<String>,
    /// The row was started by an `eta` with no `start` before it (a
    /// dispatcher that forgot it, or a worker of an older version),
    /// so no dispatcher will land it: it is Done when its worker is.
    pub follows_worker: bool,
    /// When the row joined the plan (`giverny manage plan`).
    pub planned_ms: Option<u64>,
    /// Planned rows: the task this one waits for, until it lands
    /// (`plan --after`).
    pub after: Option<String>,
    /// Planned rows: the dispatcher holds it, for this reason (`hold --why`).
    pub held: Option<String>,
    /// When a hold was last ended (`unhold`): the row's idle clock starts
    /// there.
    pub unheld_ms: Option<u64>,
    /// What the machine ledger answered this row's task: a
    /// copy written by `giverny manage claim`, gone on `release`/`land`. The
    /// ledger itself (`resources`) is the truth; this is for drawing.
    pub lease: Option<RowLease>,
    /// What the task's `giverny manage run` commands used.
    pub usage: Option<RowUsage>,
    /// What its running `giverny manage run` commands use now, sampled by the
    /// pane from their cgroups. Never in the feed file.
    pub live: Option<crate::run_live::RunLive>,
    /// When the worker's end its manager was told to land was
    /// ([`crate::manage_chores`], `land_nudged`).
    pub land_nudged_ms: Option<u64>,
    /// Since when the idle Next up row its manager was told to start or
    /// hold had waited (`start_nudged`).
    pub start_nudged_ms: Option<u64>,
    /// Since when the Running row its manager was told has no worker had
    /// been handed out (`worker_nudged`).
    pub worker_nudged_ms: Option<u64>,
}

/// A row's `usage` object: the task's `giverny manage run` commands so far,
/// measured. Totals over every run, and the last run's command and cap.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RowUsage {
    pub runs: u64,
    /// The highest peak memory of any run, MiB.
    pub peak_mb: Option<u64>,
    /// CPU time over every run, seconds.
    pub cpu_s: u64,
    /// Wall time over every run, seconds.
    pub wall_s: u64,
    /// Runs the memory cap killed, ever: the history counts them.
    pub oom_kills: u64,
    /// The OOM is still news: the latest run was killed by the memory cap
    /// and no fresh, bigger grant came since (`oom_open`). A
    /// row from before reads open only when its last run exited as the
    /// kill does (137, with a kill counted).
    pub oom_open: bool,
    pub last_cmd: Option<String>,
    pub last_exit: Option<i64>,
    pub last_peak_mb: Option<u64>,
    /// The last run's cap: its RAM, when it was held to one (a `cap_cpu`
    /// from an older version is ignored: a lease holds no cores).
    pub cap_ram_mb: Option<u64>,
    /// The last run was capped by a systemd scope (else ran plain).
    pub capped: bool,
}

/// The exit `giverny manage run` records for a command the OOM killer took:
/// 128 + SIGKILL.
pub const OOM_EXIT: i64 = 137;

/// Whether a row's `usage` holds an OOM still open ([`RowUsage::oom_open`]).
pub fn oom_open(u: &Value) -> bool {
    match u.get("oom_open").and_then(Value::as_bool) {
        Some(open) => open,
        None => {
            u64_field(u, "oom_kills").unwrap_or(0) > 0
                && i64_field(u, "last_exit") == Some(OOM_EXIT)
        }
    }
}

fn parse_usage(v: &Value) -> Option<RowUsage> {
    let u = v.get("usage")?;
    if !u.is_object() {
        return None;
    }
    Some(RowUsage {
        runs: u64_field(u, "runs").unwrap_or(0),
        peak_mb: u64_field(u, "peak_mb"),
        cpu_s: u64_field(u, "cpu_s").unwrap_or(0),
        wall_s: u64_field(u, "wall_s").unwrap_or(0),
        oom_kills: u64_field(u, "oom_kills").unwrap_or(0),
        oom_open: oom_open(u),
        last_cmd: str_field(u, "last_cmd"),
        last_exit: i64_field(u, "last_exit"),
        last_peak_mb: u64_field(u, "last_peak_mb"),
        cap_ram_mb: u64_field(u, "cap_ram_mb"),
        capped: u.get("capped").and_then(Value::as_bool) == Some(true),
    })
}

/// Where a row's task stands with the machine ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseState {
    /// Holds what it asked for.
    Granted,
    /// Holds less RAM (`wanted_ram_mb`) than it asked for.
    Smaller,
    /// Waiting, at `position`, `behind` a holder.
    Queued,
}

/// A row's `lease` object. For a queued row the figures are the request.
/// No cores: a `cpu` or `wanted_cpu` from an older version is ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowLease {
    pub state: LeaseState,
    /// The ledger's lease id (`<session>:<task>`); none while queued.
    pub id: Option<String>,
    pub ram_mb: u64,
    pub gpus: Vec<u32>,
    pub vram_mb: u64,
    pub slots: Vec<String>,
    pub granted_ms: Option<u64>,
    pub wanted_ram_mb: Option<u64>,
    /// 1-based place in the ledger's queue when it was last asked.
    pub position: Option<u64>,
    /// Who it really waits on: the queued request ahead that wants what it
    /// is short of, else a lease holding that ([`crate::resources::in_the_way`]).
    pub behind: Option<String>,
    /// What it is short of, as of its last claim (`cpu` — the CPU was
    /// busy — `ram`, `gpu`, `slot <name>`); empty when not known.
    pub short: Vec<String>,
    /// Granted on another session's call, not yet taken up by its manager.
    pub uncollected: bool,
    /// Queued again after such a grant lapsed untaken: waits for its manager.
    pub lapsed: bool,
}

fn parse_lease(v: &Value) -> Option<RowLease> {
    let l = v.get("lease")?;
    if !l.is_object() {
        return None;
    }
    let state = match l
        .get("state")?
        .as_str()?
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "granted" => LeaseState::Granted,
        "smaller" => LeaseState::Smaller,
        "queued" => LeaseState::Queued,
        _ => return None,
    };
    let strings = |k: &str| -> Vec<String> {
        l.get(k)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
    };
    Some(RowLease {
        state,
        id: str_field(l, "id"),
        ram_mb: u64_field(l, "ram_mb").unwrap_or(0),
        gpus: l
            .get("gpus")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_u64)
                    .map(|g| g as u32)
                    .collect()
            })
            .unwrap_or_default(),
        vram_mb: u64_field(l, "vram_mb").unwrap_or(0),
        slots: strings("slots"),
        granted_ms: millis_field(l, "granted_at"),
        wanted_ram_mb: u64_field(l, "wanted_ram_mb"),
        position: u64_field(l, "position"),
        behind: str_field(l, "behind"),
        short: strings("short"),
        uncollected: l.get("uncollected").and_then(Value::as_bool) == Some(true),
        lapsed: l.get("lapsed").and_then(Value::as_bool) == Some(true),
    })
}

impl FeedRow {
    /// The section this row is drawn in. Rows are only ever built with one;
    /// the `Option` is for `Default`.
    pub fn stage(&self) -> Stage {
        self.stage.unwrap_or(Stage::Planned)
    }

    /// A row that runs `giverny manage run` commands of its own: it holds a
    /// lease, has measured runs, or has one running. A manager's build row
    /// (`integ-build`) is one, with no worker behind it.
    pub fn is_run(&self) -> bool {
        self.lease.is_some() || self.usage.is_some() || self.live.is_some()
    }
}

/// A parsed feed file.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Feed {
    pub version: u64,
    /// The Claude session id the feed is for.
    pub session: Option<String>,
    /// The manager's name, as `giverny manage name` wrote it before names
    /// had their own home ([`crate::names`]); the status line still shows it
    /// for its session while names are on.
    pub name: Option<String>,
    /// Earlier ids of the same conversation (a resume or a re-id), so a tab
    /// still holding an old id finds the feed.
    pub aliases: Vec<String>,
    pub rows: Vec<FeedRow>,
    /// One line drawn under the table, verbatim.
    pub footer: Option<String>,
    /// When the file was last written (its mtime), for the reader's own
    /// arithmetic: a paused row's `started` is only true as of this instant.
    /// `None` for a feed parsed from bytes.
    pub written_ms: Option<u64>,
    /// Who wrote it (`writer`): `giverny/manage` ([`crate::manage::WRITER`])
    /// or another.
    pub writer: Option<String>,
}

impl Feed {
    /// The instant a paused row's `started` is true as of, for a writer that
    /// moves it on by an open pause each time it writes: the write time.
    /// `giverny manage` moves `started` only as a pause closes, so its
    /// paused row's clock stands at `paused_since` however often the file is
    /// written since (a killed row's stop is hours before the
    /// write that killed it): `None`.
    pub fn started_true_at(&self) -> Option<u64> {
        if self.writer.as_deref() == Some(crate::manage::WRITER) {
            return None;
        }
        self.written_ms
    }

    /// The feed without the Done rows that landed by `cutoff_ms` (or never
    /// said when): what the pane shows after its Done rows were cleared by
    /// hand, whoever wrote the file.
    pub fn without_done_by(&self, cutoff_ms: u64) -> Feed {
        let mut f = self.clone();
        f.rows
            .retain(|r| r.stage() != Stage::Done || r.ended_ms.is_some_and(|e| e > cutoff_ms));
        f
    }

    /// Is this feed about `session` — by its own id or by an alias?
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
    let stage = Stage::parse(v.get("stage")?.as_str()?)?;
    let agent_id = str_field(v, "agent_id");
    let key = str_field(v, "key").or_else(|| agent_id.clone())?;
    Some(FeedRow {
        key,
        stage: Some(stage),
        title: str_field(v, "title"),
        agent_id,
        started_ms: millis_field(v, "started"),
        ended_ms: millis_field(v, "ended"),
        spawned_ms: millis_field(v, "spawned"),
        paused_s: u64_field(v, "paused_s"),
        paused_since_ms: millis_field(v, "paused_since"),
        killed_ms: millis_field(v, "killed"),
        went_off_ms: millis_field(v, "went_off"),
        eta_s: u64_field(v, "eta_s"),
        eta_delta_s: i64_field(v, "eta_delta_s"),
        left_s: u64_field(v, "left_s"),
        left_at_ms: millis_field(v, "left_at"),
        wait_s: u64_field(v, "wait_s"),
        waiting_since_ms: millis_field(v, "waiting_since"),
        eta_first_s: u64_field(v, "eta_first_s"),
        eta_guess_s: u64_field(v, "eta_guess_s"),
        reest_s: u64_field(v, "reest_s"),
        reest_at_s: u64_field(v, "reest_at_s"),
        landing: str_field(v, "landing"),
        tokens: u64_field(v, "tokens"),
        task_tokens: u64_field(v, "task_tokens"),
        group: str_field(v, "group"),
        brief: str_field(v, "brief").map(PathBuf::from),
        open: str_field(v, "open"),
        note: str_field(v, "note"),
        review: str_field(v, "review"),
        follows_worker: v.get("follows_worker").and_then(Value::as_bool) == Some(true),
        planned_ms: millis_field(v, "planned"),
        after: str_field(v, "after"),
        held: str_field(v, "held"),
        unheld_ms: millis_field(v, "unheld"),
        lease: parse_lease(v),
        usage: parse_usage(v),
        live: None,
        land_nudged_ms: millis_field(v, "land_nudged"),
        start_nudged_ms: millis_field(v, "start_nudged"),
        worker_nudged_ms: millis_field(v, "worker_nudged"),
    })
}

/// Parse a feed from its bytes.
///
/// Only three things refuse the whole file: it is not JSON, it is not an
/// object, or it declares a version newer than [`FEED_VERSION`]. A row with
/// no usable `stage`, or neither `key` nor `agent_id`, is dropped; a field of
/// the wrong type reads as absent.
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
        name: str_field(&v, "name"),
        aliases,
        rows,
        footer,
        written_ms: None,
        writer: str_field(&v, "writer"),
    })
}

/// Read and parse one file. `None` when it is missing, unreadable or not a
/// feed — the pane then simply has no feed.
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
    /// The feed is the first of them that [`find`] finds — the old id's
    /// until the writer adopts it under the new one, which names both.
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
        // a file of its own for (a writer that could not adopt the old).
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
    /// When it was last seen at work: its latest turn, else its start.
    fn last_sign_ms(&self) -> Option<u64> {
        self.started_ms()
    }
    /// The agent that spawned it, when another agent did.
    fn parent_id(&self) -> Option<&str> {
        None
    }
}

/// Did worker `l` of feed row `f` die with its session: the
/// row is killed, or went off since the worker was last seen at work. A
/// tracker restored over a restart may still list such a worker running.
pub fn died_with_session<L: LiveAgent>(f: &FeedRow, l: &L) -> bool {
    f.killed_ms.is_some()
        || f.went_off_ms
            .is_some_and(|off| l.last_sign_ms().is_none_or(|t| t <= off))
}

/// Does `text` name `key` as a whole word? `demo#82` is named by
/// `Work demo#82 pane` and by `owner/demo#82`, never by
/// `demo#820` nor `xdemo#82`.
pub fn names_key(text: &str, key: &str) -> bool {
    key_at(text, key).next().is_some()
}

/// Where `text` names `key` as a whole word ([`names_key`]), by byte offset.
fn key_at<'t>(text: &'t str, key: &'t str) -> impl Iterator<Item = usize> + 't {
    let word = |c: char| c.is_alphanumeric() || c == '_' || c == '-';
    text.match_indices(key)
        .filter(move |_| !key.is_empty())
        .filter(move |(i, _)| {
            let before = text[..*i].chars().next_back();
            let after = text[i + key.len()..].chars().next();
            before.is_none_or(|c| !word(c) && c != '#') && after.is_none_or(|c| !word(c))
        })
        .map(|(i, _)| i)
}

/// When worker `l` was spawned: the earlier of Claude Code's `startTime` and
/// its transcript's first turn. A worker woken by a message is listed afresh
/// with its `startTime` moved to the message; its first turn stays put.
pub fn spawned_at<L: LiveAgent>(l: &L, log: Option<&WorkerLog>) -> Option<u64> {
    let first = log.and_then(WorkerLog::first_ms);
    match (l.started_ms(), first) {
        (Some(s), Some(f)) => Some(s.min(f)),
        (s, f) => s.or(f),
    }
}

/// How long after its worker a feed row must have begun to be a later task
/// of that worker, keeping its own clock, rather than the spawn the writer
/// stamped a little early. A writer stamps `started` seconds
/// to a minute or two before the spawn; a second task sent to a worker that
/// finished its first comes a task's length later.
pub const LATER_TASK_MS: u64 = 5 * 60 * 1000;

/// One row of the pane: a feed row, a live row, or both joined.
#[derive(Debug)]
pub struct PaneRow<'a, L> {
    pub stage: Stage,
    pub feed: Option<&'a FeedRow>,
    pub live: Option<&'a L>,
    /// Same worker as the row directly above: draw its per-worker cells
    /// (agent, elapsed, tokens, activity) as `"`.
    pub ditto: bool,
    /// The start of the same worker's next task, and its key: this row's
    /// task ended there at the latest.
    pub next_ms: Option<u64>,
    pub next_key: Option<String>,
    /// One of a worker's finished tasks taken one after another: what this
    /// task spent, the turns from its hand-off to the next one's. `Some(None)`
    /// when the transcript could not be read: never the worker's whole
    /// count, which every such row would repeat.
    pub task_tokens: Option<Option<u64>>,
    /// A Planned row queued on a worker busy with another task: that task.
    pub after_key: Option<String>,
    /// Which of its worker's tasks-in-turn this row is (`0` for a worker
    /// that took its tasks at once): rows of one batch share the worker's
    /// cells and are dittoed, rows of different ones never are.
    pub batch: usize,
    /// When its worker was spawned: the earlier of Claude Code's
    /// `startTime` and the worker's first turn. Claude Code lists a worker
    /// woken by a message afresh, its `startTime` moved to that message, so
    /// its own figure alone would put every task the worker held after it.
    pub worker_ms: Option<u64>,
}

impl<L> PaneRow<'_, L> {
    /// A row with nothing in it, for `..PaneRow::bare()`.
    fn bare() -> Self {
        PaneRow {
            stage: Stage::Planned,
            feed: None,
            live: None,
            ditto: false,
            next_ms: None,
            next_key: None,
            task_tokens: None,
            after_key: None,
            batch: 0,
            worker_ms: None,
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

    /// Start of the row's clock. With a worker, the worker's own start —
    /// Claude Code's `startTime`, the clock its agents view counts from —
    /// so the two agree: a writer stamps `started` when it
    /// is about to spawn, a little before the worker exists. The feed's
    /// `started` still counts where it is the truer one:
    ///
    /// - a row with no worker, or whose worker gave no start;
    /// - a row that began well after its worker did
    ///   ([`LATER_TASK_MS`]): one worker given a second task later holds a
    ///   row per task, each started at its own time;
    /// - a row that began well before its worker did ([`LATER_TASK_MS`]): a
    ///   fresh worker taking over a task, often after a pause. Its pauses
    ///   came before that worker, so the worker's start moved on by them
    ///   would lie ahead and hold the clock at zero; the
    ///   feed's `started` already has them off;
    /// - the row's paused spans, which Claude Code knows nothing of: the
    ///   worker's start is moved on by the same amount the writer moved
    ///   `started` on by (`started − spawned`, else `paused_s`).
    pub fn started_ms(&self) -> Option<u64> {
        self.row_started_ms()
    }

    /// When this row's clock stopped, if it is paused right now: the feed's
    /// `paused_since`, on a Running row only.
    pub fn paused_since_ms(&self) -> Option<u64> {
        if self.stage != Stage::Running {
            return None;
        }
        self.feed.and_then(|f| f.paused_since_ms)
    }

    fn row_started_ms(&self) -> Option<u64> {
        let worker = self
            .worker_ms
            .or_else(|| self.live.and_then(|l| l.started_ms()));
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

    /// When this row's task was handed to a worker that was already there:
    /// the feed's `started` (less its pauses) — `start <key>
    /// --agent <worker>` is the hand-off, and the only one.
    /// `None` for the task the spawn named, which the worker had from its
    /// first turn. A worker that ended before this instant ended its
    /// previous task, not this one.
    pub fn handed_at_ms(&self) -> Option<u64> {
        if self.spawn_task() {
            return None;
        }
        let f = self.feed?;
        f.started_ms.map(|s| s.saturating_sub(moved_ms(f)))
    }

    /// Was this row's task given at the spawn: the worker's description
    /// names its key.
    fn spawn_task(&self) -> bool {
        match (self.feed, self.live.and_then(|l| l.description())) {
            (Some(f), Some(d)) => names_key(d, &f.key),
            _ => false,
        }
    }

    /// When the row's task ended: the feed's `ended`, else its finished
    /// worker's end — and no later than the same worker's next task began.
    pub fn ended_ms(&self) -> Option<u64> {
        let own = self.feed.and_then(|f| f.ended_ms).or_else(|| {
            self.live
                .filter(|l| !l.running())
                .and_then(|l| l.ended_ms())
        });
        match (own, self.next_ms) {
            (Some(e), Some(n)) => Some(e.min(n)),
            (e, n) => e.or(n),
        }
    }

    /// Tokens: for one of a worker's finished tasks taken one after
    /// another, what that task spent ([`PaneRow::task_tokens`]); else (a
    /// Running row, a reused worker's too) the worker's count
    /// (Claude Code's own, as its agents view shows it), else what the feed
    /// wrote.
    pub fn tokens(&self) -> Option<u64> {
        // Counted when it landed: frozen there.
        if self.stage == Stage::Done
            && let Some(t) = self.feed.and_then(|f| f.task_tokens)
        {
            return Some(t);
        }
        if let Some(t) = self.task_tokens {
            return t;
        }
        // Nothing is spent on a task before it starts, whoever it is
        // queued on.
        if self.stage == Stage::Planned {
            return None;
        }
        self.live
            .and_then(|l| l.tokens())
            .or_else(|| self.feed.and_then(|f| f.tokens))
    }

    /// The worker's latest figure on a re-estimated row: when
    /// it was given, never before the row's clock starts, and the seconds it
    /// said were left then. A Running row's time left is that figure less
    /// the work done since; `None` on a row never re-estimated since its
    /// `start`, which counts down from `eta_s` and its start.
    pub fn last_figure(&self) -> Option<(u64, u64)> {
        let f = self.feed?;
        let (at, left) = f.left_at_ms.zip(f.left_s)?;
        Some((self.row_started_ms().map_or(at, |s| at.max(s)), left))
    }

    /// A Done row's ETA cell, in seconds, and the figure it is measured
    /// against: `eta_delta_s` when the feed sent it, else [`done_delta`]
    /// over the row's working time as the history counts it — its clock's
    /// end less its start (already moved on by pauses), its waits
    /// (`wait_s`) and the ms `off_ms` says of it nothing ran (the machine
    /// slept, [`crate::sleeps`]). `None` on any other stage, or when there is no
    /// estimate to compare with.
    pub fn eta_delta_s(&self, off_ms: &dyn Fn(u64, u64) -> u64) -> Option<(i64, Against)> {
        if self.stage != Stage::Done {
            return None;
        }
        let f = self.feed?;
        if let Some(d) = f.eta_delta_s {
            return Some((d, Against::Given));
        }
        // The row's own clock, as its ELAPSED: a writer may stamp a reused
        // worker's task late. `giverny manage`'s `started` is the worker's
        // start to within the seconds of a spawn, which is where
        // `reest_at_s` and the history count from.
        let (start, end) = (self.row_started_ms()?, self.ended_ms()?);
        let work_s = end.saturating_sub(start).saturating_sub(off_ms(start, end)) / 1000;
        let work_s = work_s.saturating_sub(f.wait_s.unwrap_or(0));
        let estimate = f.eta_guess_s.or(f.eta_first_s).or(f.eta_s);
        done_delta(work_s, f.reest_s.zip(f.reest_at_s), estimate)
    }
}

/// What a Done row's ± was measured against: the overlay
/// header names it, `vs worker's 40m` or `vs estimate 30m`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Against {
    /// The worker's first re-estimate of the time left, in seconds.
    Worker(u64),
    /// The dispatcher's figure at the start, in seconds.
    Estimate(u64),
    /// The writer's own `eta_delta_s`, whose figure is not known.
    Given,
}

impl Against {
    /// The words after `vs` in the overlay header.
    pub fn label(self) -> String {
        match self {
            Against::Worker(s) => format!("worker's {}", fmt_span(s as i64)),
            Against::Estimate(s) => format!("estimate {}", fmt_span(s as i64)),
            Against::Given => "estimate".into(),
        }
    }
}

/// How late (`+`) or early (`-`) a landed task was, in seconds, and against
/// what: with the worker's first re-estimate (`reest`: the
/// seconds it said were left, and the working seconds it was given at),
/// the work done after it less that figure; else the whole work less the
/// dispatcher's `estimate`. The two pairs `manage_history`'s tracks score
/// (`Track::Reestimate`, `Track::Guess`), so the pane's ± and `accuracy`
/// read the same. Not the latest figure: one given five minutes before the
/// landing says nothing of how the estimate went. `work_s` is the task's
/// working time as the history counts it.
pub fn done_delta(
    work_s: u64,
    reest: Option<(u64, u64)>,
    estimate: Option<u64>,
) -> Option<(i64, Against)> {
    if let Some((left, at)) = reest {
        let after = work_s.saturating_sub(at);
        return Some((after as i64 - left as i64, Against::Worker(left)));
    }
    let e = estimate?;
    Some((work_s as i64 - e as i64, Against::Estimate(e)))
}

/// Which of a session's merged rows its pane draws: every
/// drawn row is a real agent. A feed row annotates a worker — its title,
/// ETA, lease, brief — and with no subagent behind it is not drawn: any
/// agent can leave itself a note to run `giverny manage start`, and such a
/// row once opened the pane alone, blank where its worker's cells go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drawn {
    /// A session that is no manager: only rows a subagent Claude Code
    /// lists stands behind.
    Agents,
    /// A manager session ([`crate::managers`]): those, and its own rows no
    /// worker stands behind — Next up (a Running row with no worker is drawn
    /// there too) and its build rows ([`FeedRow::is_run`]), in any stage.
    Manager,
    /// Every merged row: for the tests and debug lookups of the merge
    /// itself.
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
            Drawn::Manager => row.stage == Stage::Planned || row.feed.is_some_and(FeedRow::is_run),
        }
    }
}

/// Does a session get a pane at all: only when subagents are
/// involved — its tracker holds a real one, running or done this session
/// (`has_agents`) — or it is a manager, whose Next up shows before its
/// first spawn. Its feed never opens one by itself.
pub fn opens(has_agents: bool, manager: bool) -> bool {
    has_agents || manager
}

/// Merge the feed with the live rows.
///
/// - Every feed row is a row, in feed order, in the stage the feed gave it;
///   but one that `follows_worker` is Done once its finished worker is.
///   It carries the live row its `agent_id` names; failing that (no
///   `agent_id`, or one Claude Code no longer lists), the live row whose
///   description names the row's key — `Work demo#82 …` holds
///   `demo#82`, and one naming several keys holds each of them.
///   A Running row whose named worker has finished, or was killed,
///   carries a running one spawned after it whose
///   description names the key, if there is one.
///   The feed's stage wins: one worker may hold a Done row and
///   a Running one at once.
/// - A live row that no feed row carries, and whose description names no
///   feed key, is a row of its own, Running or Done by its own state, after
///   the feed's rows of that stage. One that names a key is that row's
///   worker and is never drawn twice.
/// - Rows are grouped Running, Planned, Done; order within Running and
///   Planned is kept, Done is newest landed first (undated rows last).
/// - A row whose worker is the one directly above it is a ditto.
pub fn merge<'a, L: LiveAgent>(feed: Option<&'a Feed>, live: &'a [L]) -> Vec<PaneRow<'a, L>> {
    merge_with(feed, live, |_| None)
}

/// [`merge`], with each worker's transcript ([`WorkerLog`], by agent id) to
/// split a worker that took tasks one after another into a row per task
/// ([`queue`]).
pub fn merge_with<'a, 'w, L: LiveAgent>(
    feed: Option<&'a Feed>,
    live: &'a [L],
    log: impl Fn(&str) -> Option<&'w WorkerLog>,
) -> Vec<PaneRow<'a, L>> {
    let feed_rows: &[FeedRow] = feed.map(|f| f.rows.as_slice()).unwrap_or(&[]);
    let find_live = |id: &str| live.iter().find(|l| l.agent_id() == id);
    // An agent another listed agent spawned is never a feed row's worker by
    // its description: it works for its parent, whose task it may well
    // name, and is drawn under its parent's row. Only an
    // `agent_id` makes it a row's worker.
    let nested = |l: &L| {
        l.parent_id()
            .is_some_and(|p| live.iter().any(|o| o.agent_id() == p))
    };
    let names = |l: &L, key: &str| !nested(l) && l.description().is_some_and(|d| names_key(d, key));
    // A Running row wants the worker still running; any other the one that
    // finished. Either takes the other when that is all there is.
    let by_description = |f: &FeedRow| -> Option<&'a L> {
        let want_running = f.stage() == Stage::Running;
        let mut named = live.iter().filter(|l| names(l, &f.key));
        let first = named.clone().next()?;
        Some(named.find(|l| l.running() == want_running).unwrap_or(first))
    };
    // A Running row whose worker has finished, or was killed with its
    // session (a killed worker never finishes, it may well
    // still read running), and a worker spawned after it for the same task
    // is running: the dispatcher resumed the task with a fresh worker and
    // never moved the row. One another row names by id is
    // that row's.
    let successor = |f: &FeedRow, held: &L| -> Option<&'a L> {
        live.iter().find(|l| {
            l.running()
                && l.agent_id() != held.agent_id()
                && names(l, &f.key)
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
        .map(|f| {
            let live = match f.agent_id.as_deref().and_then(find_live) {
                Some(held)
                    if f.stage() == Stage::Running
                        && (!held.running() || died_with_session(f, held)) =>
                {
                    successor(f, held).or(Some(held))
                }
                Some(held) => Some(held),
                None => by_description(f),
            };
            let finished = f.follows_worker && live.is_some_and(|l| !l.running());
            PaneRow {
                stage: if finished { Stage::Done } else { f.stage() },
                feed: Some(f),
                live,
                worker_ms: live.and_then(|l| spawned_at(l, log(l.agent_id()))),
                ..PaneRow::bare()
            }
        })
        .collect();
    for l in live {
        let carried = out
            .iter()
            .any(|r| r.live.is_some_and(|c| c.agent_id() == l.agent_id()));
        let named = carried
            || feed_rows
                .iter()
                .any(|f| f.agent_id.as_deref() == Some(l.agent_id()) || names(l, &f.key));
        if !named {
            out.push(PaneRow {
                stage: if l.running() {
                    Stage::Running
                } else {
                    Stage::Done
                },
                feed: None,
                live: Some(l),
                worker_ms: spawned_at(l, log(l.agent_id())),
                ..PaneRow::bare()
            });
        }
    }
    queue(&mut out, &log);
    // Running and Next up keep feed order, then live order. Done is newest
    // landed first; a worker's batch is kept together and
    // sorts by its latest end, and rows with no end time go after the dated
    // ones, in their old order.
    let group_end = |r: &PaneRow<'a, L>| {
        let id = r.agent_id()?;
        out.iter()
            .filter(|o| o.stage == Stage::Done && o.agent_id() == Some(id) && o.batch == r.batch)
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
            (Some(a), Some(b)) => a == b && out[i - 1].batch == out[i].batch,
            _ => false,
        };
        out[i].ditto = same;
    }
    out
}

/// One worker, tasks one after another. Rows are already
/// joined to their workers; this works out which of a worker's rows came
/// after which.
///
/// - **Tasks at once, tasks in turn.** A worker's Running and Done rows are
///   grouped by start: rows that began within [`LATER_TASK_MS`] of each other
///   are one batch (`Work #144 #145`, one count dittoed), and each later
///   batch is the next task.
/// - **The next task closes the one before.** A row of an earlier batch ends
///   when the next batch began, at the latest; still Running in the feed, it
///   is Done there — `start <next> --agent <worker>` is the whole hand-off.
/// - **Tokens per task.** Each Done batch shows what its turns added, from
///   its start (the first from the worker's first turn) to the next batch's.
///   A Running batch shows the worker's whole live context, as a fresh
///   worker's does: what it works with now. The Done rows
///   of a worker add up to everything it added, and nothing is counted
///   twice. A count `giverny manage` froze on the row when it landed
///   (`task_tokens`) is drawn in place of either. The worker's start is its
///   spawn ([`spawned_at`]), never the later `startTime` Claude Code lists a
///   worker woken by a message with.
/// - **Queued on a worker.** A Planned row whose `agent_id` names a worker
///   running another task is queued after it.
fn queue<'w, L: LiveAgent>(
    rows: &mut [PaneRow<'_, L>],
    log: &impl Fn(&str) -> Option<&'w WorkerLog>,
) {
    let mut ids: Vec<String> = Vec::new();
    for r in rows.iter() {
        if let Some(id) = r.agent_id()
            && !ids.iter().any(|x| x == id)
        {
            ids.push(id.to_string());
        }
    }
    for id in &ids {
        let mut held: Vec<(usize, u64)> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.stage != Stage::Planned && r.agent_id() == Some(id))
            .filter_map(|(i, r)| Some((i, r.started_ms()?)))
            .collect();
        held.sort_by_key(|&(i, s)| (s, i));
        // Batches: (first start, rows).
        let mut batches: Vec<(u64, Vec<usize>)> = Vec::new();
        for (i, s) in held {
            match batches.last_mut() {
                Some((first, b)) if s < first.saturating_add(LATER_TASK_MS) => b.push(i),
                _ => batches.push((s, vec![i])),
            }
        }
        let n = batches.len();
        if n >= 2 {
            let w = rows
                .iter()
                .find(|r| r.agent_id() == Some(id))
                .and_then(|r| r.live)
                .and_then(|l| log(l.agent_id()));
            for (k, (start, batch)) in batches.iter().enumerate() {
                let next = batches.get(k + 1);
                let from = if k == 0 { 0 } else { *start };
                let to = next.map(|(s, _)| *s);
                let next_key = next
                    .and_then(|(_, b)| rows[b[0]].feed)
                    .map(|f| f.key.clone())
                    .filter(|k| !k.is_empty());
                for &i in batch {
                    let r = &mut rows[i];
                    r.batch = k;
                    if let Some(to) = to {
                        r.next_ms = Some(to);
                        r.next_key = next_key.clone();
                        r.stage = Stage::Done;
                    }
                    // A task still running shows the context its worker
                    // works with now, the tasks before it and all:
                    // the live count, as a fresh worker's
                    // row does and `giverny manage start` says an idle
                    // worker carries. Only a finished task is its own span.
                    if r.stage == Stage::Done {
                        r.task_tokens = Some(w.map(|w| w.added(from, to)));
                    }
                }
            }
        }
        let running = rows
            .iter()
            .find(|r| r.stage == Stage::Running && r.agent_id() == Some(id))
            .and_then(|r| r.feed)
            .map(|f| f.key.clone())
            .filter(|k| !k.is_empty());
        if let Some(key) = running {
            for r in rows.iter_mut() {
                if r.stage == Stage::Planned
                    && r.feed.and_then(|f| f.agent_id.as_deref()) == Some(id)
                {
                    r.after_key = Some(key.clone());
                }
            }
        }
    }
}

/// How long after a Running row started, or was handed on, its worker may
/// take to show up: a dispatcher runs `start <task>` a moment before the
/// spawn, or `start <task> --agent <worker>` a moment before it sends the
/// worker its brief.
pub const HANDOFF_WINDOW_MS: u64 = 3 * 60 * 1000;

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

    const FULL: &str = r#"{"version":1,"session":"s-new","aliases":["s-old"],"name":"Juniper Belfry",
      "rows":[
        {"key":"acme#158","stage":"running","title":"FEATURE: pane","agent_id":"a1",
         "started":"2026-09-23T10:00:00Z","eta_s":2400,"landing":"Review — alice",
         "tokens":123456,"group":"lane-1","brief":"/tmp/b.md","open":"claude --resume x","note":"n"},
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
        assert_eq!(f.name.as_deref(), Some("Juniper Belfry"));
        assert_eq!(
            f.footer.as_deref(),
            Some("session: 3 · subagents: 1.2M · total: 1.2M")
        );
        assert_eq!(f.rows.len(), 3);
        let r = &f.rows[0];
        assert_eq!(r.key, "acme#158");
        assert_eq!(r.stage(), Stage::Running);
        assert_eq!(r.title.as_deref(), Some("FEATURE: pane"));
        assert_eq!(r.agent_id.as_deref(), Some("a1"));
        assert_eq!(r.started_ms, Some(1_790_157_600_000));
        assert_eq!(r.eta_s, Some(2400));
        assert_eq!(r.landing.as_deref(), Some("Review — alice"));
        assert_eq!(r.tokens, Some(123_456));
        assert_eq!(r.group.as_deref(), Some("lane-1"));
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
              {"key":"k3"}
            ],"footer":"plain string","extra":[1,2]}"#,
        )
        .unwrap();
        assert_eq!(f.version, 1, "an absent version is 1");
        assert_eq!(f.name, None, "no name, no header");
        let keys: Vec<_> = f.rows.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(
            keys,
            ["k1", "a9"],
            "unplaceable and unnamed rows are dropped"
        );
        let r = &f.rows[0];
        assert_eq!(r.stage(), Stage::Running);
        assert_eq!(r.title.as_deref(), Some("42"));
        assert_eq!(r.eta_s, Some(90));
        assert_eq!(r.tokens, None);
        assert_eq!(r.started_ms, None);
        assert_eq!(r.eta_delta_s, Some(-120));
        assert_eq!(f.footer.as_deref(), Some("plain string"));
    }
}
