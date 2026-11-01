//! A manager's chores, told to the manager rather than drawn for the person.
//!
//! Four of the management panel's states are the dispatcher's to act on,
//! not the person's: a Running row whose worker has **finished** and that
//! nobody has landed; the same row **paused**, its worker finished before
//! or during the pause; a Next up row that has sat with
//! **nothing recorded to wait on** (no hold, no queued lease, no task before
//! it); and a Running row with **no worker** at all, that its manager has
//! not paused. A paused row whose worker was still working
//! and has ended its turn since is a fifth: the task is now
//! really paused, told at once, with whether every task paused has
//! stopped. The panel draws each as an ordinary row (the
//! paused one under Next up, `paused · worker finished`; the last there
//! too, `no worker yet`); the manager session that owns them hears of each
//! once, as one line on its next tool call.
//!
//! **Only of drawn rows**. The chores come from the rows the
//! pane draws, and a session that is no manager draws only rows a real
//! subagent stands behind: a stray `start` of its own is no row, so it never
//! hears "no worker", which would push it to spawn workers nobody asked
//! for. A manager ([`crate::managers`]) still does: its Next up draws the
//! row, and the chore catches a manager that started a task and forgot to
//! spawn.
//!
//! **Who knows what.** Only the app sees Claude Code's live worker list (the
//! relayed `subagentStatusLine`), so only the app can tell a finished worker
//! from a busy one. It runs the panel's own row logic over every manager
//! tab ([`Chore`]s, `management_panel::chores`) and keeps what is due in
//! `<feed dir>/chores/<session>.json` ([`publish`]); the file is there only
//! while something is due. The plugin's `PostToolUse` hook ([`deliver`]),
//! on the dispatcher's own calls, `stat`s that file, and hands each due chore
//! to the session as `additionalContext`.
//!
//! **Once per row.** Delivering stamps the row in the manager session's file
//! (`land_nudged` / `start_nudged`, the chore's [`Chore::since`]), and the
//! app drops a stamped chore from what it keeps. A row that becomes a chore
//! again from a new instant — a worker messaged again that finishes again,
//! a hold that ends — is told again; the same one never is.
//!
//! **Thresholds.** A Next up row is the manager's after [`IDLE_PLANNED_MS`]
//! (the panel's idle mark). A finished worker's row,
//! paused or not, after [`LAND_GRACE_MS`]. A row with no worker [`feed::HANDOFF_WINDOW_MS`] after
//! it was handed out, when a spawn or a pick-up would have come.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::feed::{self, FeedRow, Stage};
use crate::manage::{self, Lock};

/// How long a Next up row may sit with nothing recorded to wait on before
/// its manager is told to start or hold it.
pub const IDLE_PLANNED_MS: u64 = 5 * 60 * 1000;

/// How long after its worker finished an unlanded Running row is its
/// manager's to be told about. The dispatcher hears of the end at once
/// (Claude Code's task notification reaches its next turn), and reading the
/// report and running `land` takes it a turn or two, a minute or so; three
/// minutes is past that without leaving a finished task sitting for long.
/// It is the span [`feed::HANDOFF_WINDOW_MS`] gives the other end of a
/// task, between `start` and the worker's spawn.
pub const LAND_GRACE_MS: u64 = 3 * 60 * 1000;

/// Where the chores live, under the feed directory.
pub const CHORES_DIR: &str = "chores";

/// What the manager is to do about a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Its worker finished: read the report and land it.
    Land,
    /// Its worker finished and its row is paused: land it, or
    /// resume it. A row paused for a Giverny restart whose worker finished
    /// meanwhile sat under Next up as `paused since …`, work done, and
    /// nothing told its manager: [`Kind::Land`] spoke only of Running rows.
    /// The same finish as [`Kind::Land`], stamped on the same field, so a
    /// row told of it Running and then paused is not told again.
    #[serde(rename = "land_paused")]
    LandPaused,
    /// Its row was paused while its worker still worked, and the worker has
    /// ended its turn since: the task is now really paused.
    /// Told at once, not after [`LAND_GRACE_MS`]: the manager waits on
    /// this to tell the user it has paused. Stamped on [`Kind::Land`]'s
    /// field, the same finish.
    #[serde(rename = "now_paused")]
    NowPaused,
    /// It waits on nothing recorded: start it, or hold it with a reason.
    Start,
    /// It is Running with no worker: spawn or hand it one, or pause or drop
    /// it.
    Worker,
}

impl Kind {
    /// The row field delivering it stamps.
    fn field(self) -> &'static str {
        match self {
            Kind::Land | Kind::LandPaused | Kind::NowPaused => "land_nudged",
            Kind::Start => "start_nudged",
            Kind::Worker => "worker_nudged",
        }
    }

    /// Whether a row of the manager session's file in `stage` is still
    /// this chore's: a Next up row still planned; a finished worker's row
    /// not landed — Running, or still planned in the file for a task the
    /// dispatcher handed over by message and never `start`ed — paused or not
    /// (whether it still is, [`stamp`] asks); a row with no worker still
    /// Running.
    fn stands_in(self, stage: Option<Stage>) -> bool {
        match self {
            Kind::Land | Kind::LandPaused | Kind::NowPaused => {
                matches!(stage, Some(Stage::Running | Stage::Planned))
            }
            Kind::Start => stage == Some(Stage::Planned),
            Kind::Worker => stage == Some(Stage::Running),
        }
    }

    fn threshold_ms(self) -> u64 {
        match self {
            Kind::Land | Kind::LandPaused => LAND_GRACE_MS,
            Kind::NowPaused => 0,
            Kind::Start => IDLE_PLANNED_MS,
            Kind::Worker => feed::HANDOFF_WINDOW_MS,
        }
    }
}

/// One row the manager has to act on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chore {
    /// The row's key in the manager session's file.
    pub key: String,
    pub kind: Kind,
    /// Since when, epoch ms: the worker's end ([`Kind::Land`],
    /// [`Kind::LandPaused`]), the last
    /// instant the row could have started ([`Kind::Start`]: planned, unheld,
    /// the task before it landed, its lease granted), or when it was handed
    /// out ([`Kind::Worker`]: started, or handed to a worker that never
    /// picked it up).
    pub since: u64,
    /// How the worker ended (`finished`, `failed`, `killed`, `stopped`):
    /// [`Kind::Land`] only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub how: Option<String>,
}

impl Chore {
    /// Past its threshold at `now`.
    pub fn is_due(&self, now: u64) -> bool {
        now >= self.since.saturating_add(self.kind.threshold_ms())
    }

    /// The manager has been told of it already: `row` carries its stamp.
    pub fn told(&self, row: &FeedRow) -> bool {
        let stamp = match self.kind {
            Kind::Land | Kind::LandPaused | Kind::NowPaused => row.land_nudged_ms,
            Kind::Start => row.start_nudged_ms,
            Kind::Worker => row.worker_nudged_ms,
        };
        if self.kind == Kind::NowPaused {
            return told_this_pause(stamp, row.paused_since_ms);
        }
        stamp == Some(self.since)
    }

    /// The line the manager reads, at `now`.
    pub fn line(&self, now: u64) -> String {
        let key = &self.key;
        let ago = feed::fmt_span((now.saturating_sub(self.since) / 1000) as i64);
        match self.kind {
            Kind::Land => {
                let how = match self.how.as_deref() {
                    None | Some("finished") => "finished",
                    Some("killed") => "was killed",
                    Some(h) => h,
                };
                format!(
                    "Giverny: `{key}`'s worker {how} {ago} ago: check its report and \
                     `giverny-manage land {key}`."
                )
            }
            // Its worker may have finished before the pause as well as
            // during it: the line says only that both are so.
            Kind::LandPaused => format!(
                "Giverny: `{key}`'s worker finished {ago} ago and its row is paused: \
                 check its report and `giverny-manage land {key}`, or \
                 `giverny-manage resume {key}`."
            ),
            Kind::NowPaused => format!(
                "Giverny: `{key}` is now really paused: its worker has stopped (ended its \
                 turn {} ago). Read its report: `giverny-manage land {key}` if it is \
                 done, else `giverny-manage resume {key}` when the work goes on.",
                match now.saturating_sub(self.since) / 1000 {
                    s @ 0..60 => format!("{s}s"),
                    _ => ago.clone(),
                }
            ),
            Kind::Start => format!(
                "Giverny: `{key}` has waited {ago} with a free lane: start it, or \
                 `giverny-manage hold {key} --why \"<reason>\"`."
            ),
            Kind::Worker => format!(
                "Giverny: `{key}` is Running with no worker ({ago} since it was handed \
                 out): spawn one for it, hand it to an idle one (`giverny-manage start \
                 {key} --agent <id>`), or `giverny-manage pause {key}` / \
                 `giverny-manage drop {key}`."
            ),
        }
    }
}

/// `<feed dir>/chores/<session>.json`: what `session`'s manager has to be
/// told.
pub fn path(feed_dir: &Path, session: &str) -> PathBuf {
    feed_dir.join(CHORES_DIR).join(format!("{session}.json"))
}

/// A session id safe to name a file by.
/// A task now really paused is told once a pause: its stamp
/// is an end at or after the pause began, whatever end is read later.
fn told_this_pause(stamp: Option<u64>, paused_since: Option<u64>) -> bool {
    stamp.zip(paused_since).is_some_and(|(s, p)| s >= p)
}

fn safe(session: &str) -> bool {
    !session.is_empty()
        && session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The chores kept, and the paused rows whose workers were still finishing
/// when they were worked out.
fn read_all(path: &Path) -> (Vec<Chore>, Vec<String>) {
    let v = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    let field = |k: &str| v.as_ref().and_then(|v| v.get(k)).cloned();
    (
        field("chores")
            .and_then(|c| serde_json::from_value(c).ok())
            .unwrap_or_default(),
        field("finishing")
            .and_then(|c| serde_json::from_value(c).ok())
            .unwrap_or_default(),
    )
}

/// Write `chores` (or remove the file when there are none), under its lock.
fn write(path: &Path, chores: &[Chore], finishing: &[String]) -> std::io::Result<()> {
    if chores.is_empty() {
        return match std::fs::remove_file(path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    if finishing.is_empty() {
        return manage::write(path, &json!({ "chores": chores }));
    }
    manage::write(path, &json!({ "chores": chores, "finishing": finishing }))
}

/// The app's side: keep `chores` — the ones due and not yet told — as what
/// `session`'s manager is to hear. No file when there are none.
pub fn publish(feed_dir: &Path, session: &str, chores: &[Chore]) -> Result<(), String> {
    publish_with(feed_dir, session, chores, &[])
}

/// [`publish`], with the keys of the paused rows whose workers are still
/// finishing their turns (`finishing`): what lets a [`Kind::NowPaused`]
/// line say whether every task the manager paused has stopped.
pub fn publish_with(
    feed_dir: &Path,
    session: &str,
    chores: &[Chore],
    finishing: &[String],
) -> Result<(), String> {
    if !safe(session) {
        return Err(format!("{session:?} is no session id"));
    }
    let path = path(feed_dir, session);
    if chores.is_empty() && std::fs::metadata(&path).is_err() {
        return Ok(());
    }
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let _lock = Lock::take(&path)?;
    write(&path, chores, finishing).map_err(|e| format!("{}: {e}", path.display()))
}

/// The hook's side, on a dispatcher's own call: `session`'s due chores
/// that still stand, each stamped on its row and returned once as the
/// lines to put in the session's context. One `stat` when there are none.
///
/// A chore stands while its row is still in its stage under its key in the
/// session's file (this writer's), unstamped for that instant, and — for a
/// Next up row, or a Running one with no worker — still not held (nor, for
/// the latter, paused), and for a paused row's finished worker still paused
/// (resumed, it is [`Kind::Land`]'s to tell). Every chore read leaves the
/// file but the ones not yet due.
pub fn deliver(feed_dir: &Path, session: &str, now: u64) -> Option<String> {
    if !safe(session) {
        return None;
    }
    let path = path(feed_dir, session);
    std::fs::metadata(&path).ok()?;
    let _lock = Lock::take(&path).ok()?;
    let (chores, finishing) = read_all(&path);
    let (due, later): (Vec<Chore>, Vec<Chore>) = chores.into_iter().partition(|c| c.is_due(now));
    let lines = if due.is_empty() {
        Vec::new()
    } else {
        stamp(feed_dir, session, &due, &finishing, now)
    };
    let _ = write(&path, &later, &finishing);
    (!lines.is_empty()).then(|| lines.join("\n\n"))
}

/// Stamp each of `due` that still stands on its row in `session`'s file, and
/// say it. A task now really paused ([`Kind::NowPaused`]) is followed by
/// whether every task the manager paused has stopped, from `finishing`.
fn stamp(
    feed_dir: &Path,
    session: &str,
    due: &[Chore],
    finishing: &[String],
    now: u64,
) -> Vec<String> {
    let file = manage::file_for(feed_dir, session);
    let Ok(_lock) = Lock::take(&file) else {
        return Vec::new();
    };
    let Some(mut doc) = std::fs::read(&file)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .filter(|d| manage::writer_of(d) == Some(manage::WRITER))
    else {
        return Vec::new();
    };
    let Some(rows) = doc.get_mut("rows").and_then(Value::as_array_mut) else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    let mut stopped: Vec<String> = Vec::new();
    for c in due {
        let row = rows.iter_mut().filter_map(Value::as_object_mut).find(|r| {
            r.get("key").and_then(Value::as_str) == Some(c.key.as_str())
                && c.kind.stands_in(manage::stage_of(r))
        });
        let Some(row) = row else { continue };
        let stamp = manage::u64_of(row, c.kind.field());
        let told = if c.kind == Kind::NowPaused {
            let paused = row
                .get("paused_since")
                .and_then(Value::as_str)
                .and_then(|s| s.trim().parse::<jiff::Timestamp>().ok())
                .map(|t| t.as_millisecond().max(0) as u64);
            told_this_pause(stamp, paused)
        } else {
            stamp == Some(c.since)
        };
        if told {
            continue; // told already
        }
        if c.kind == Kind::Start && row.contains_key("held") {
            continue; // it waits on a reason now
        }
        if c.kind == Kind::Worker && (row.contains_key("held") || row.contains_key("paused_since"))
        {
            continue; // set aside now
        }
        if matches!(c.kind, Kind::LandPaused | Kind::NowPaused) && !row.contains_key("paused_since")
        {
            continue; // resumed: the Running row's land chore says it
        }
        row.insert(c.kind.field().into(), json!(c.since));
        lines.push(c.line(now));
        if c.kind == Kind::NowPaused {
            stopped.push(c.key.clone());
        }
    }
    if !stopped.is_empty() {
        lines.push(all_stopped(rows, &stopped, finishing));
    }
    if !lines.is_empty() && manage::write(&file, &doc).is_err() {
        return Vec::new();
    }
    lines
}

/// The line after a task is now really paused: which paused
/// tasks still have a worker finishing (`finishing`, less those just told
/// of and those no longer paused in `rows`), or that every task paused has
/// stopped, for the manager to pass on.
fn all_stopped(rows: &[Value], stopped: &[String], finishing: &[String]) -> String {
    let paused = |k: &str| {
        rows.iter().filter_map(Value::as_object).any(|r| {
            r.get("key").and_then(Value::as_str) == Some(k)
                && r.contains_key("paused_since")
                && manage::stage_of(r) == Some(Stage::Running)
        })
    };
    let left: Vec<&str> = finishing
        .iter()
        .map(String::as_str)
        .filter(|k| !stopped.iter().any(|s| s == k) && paused(k))
        .collect();
    let tick = |ks: &[&str]| {
        ks.iter()
            .map(|k| format!("`{k}`"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if !left.is_empty() {
        let (verb, it) = if left.len() == 1 {
            ("has", "it is")
        } else {
            ("have", "they are")
        };
        return format!(
            "Giverny: {} still {verb} a worker finishing, so {it} not paused yet.",
            tick(&left)
        );
    }
    let all: Vec<&str> = rows
        .iter()
        .filter_map(Value::as_object)
        .filter(|r| r.contains_key("paused_since") && manage::stage_of(r) == Some(Stage::Running))
        .filter_map(|r| r.get("key").and_then(Value::as_str))
        .collect();
    let it = if all.len() == 1 { "it is" } else { "they are" };
    format!(
        "Giverny: every task you paused has stopped ({}): you can tell the user {it} paused.",
        tick(&all)
    )
}
