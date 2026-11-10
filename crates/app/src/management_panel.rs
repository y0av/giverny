//! The management panel: a table under a tab's terminal listing that tab's Claude
//! Code subagents — Running, Planned and Done — one row each.
//!
//! On by default (`claude.management_panel`); off, nothing here runs. On, it is
//! drawn as a resizable bottom panel inside the terminal's area, only when
//! subagents are involved: in a tab whose tracker holds a
//! real subagent, running or done this session, or whose session is a
//! manager — the manage skill ran in it ([`giverny_claude::managers`]) — so
//! its Next up shows before its first spawn. A feed never opens it alone
//! ([`feed::opens`]).
//!
//! The rows are a [`Tracker`] (Claude Code's own data: live list,
//! transcripts, notifications) merged with the optional feed file
//! ([`feed::merge`], `docs/management-panel.md`), and **every row drawn is a
//! real agent** ([`Drawn`]): a feed row with no subagent behind it is not
//! drawn, save a manager's own Next up and build rows. The manager's chores
//! are worked out from the same drawn rows, so a session that is no manager
//! is never told of a row it cannot see. This module owns only the look:
//! the columns STAGE, TASK, ELAPSED, ETA, NOW and TOKENS, every one but TASK a
//! fixed width, no column headings, the whole row tinted by its stage, a
//! stopwatch ELAPSED ticking every second, `~1h3m` ETAs, a Done row's
//! `(+5m)`/`(-1h20m)`, `"` for a cell that repeats the same worker's row
//! above it — a compact status table pinned under
//! Claude Code. The session's token total is not here: it is on the status
//! line's model row, and so is the manager's name (`giverny manage name`):
//! `Opus 5.5 (Juniper)`.
//!
//! **The rows are not kept here.** [`show`] is handed the tab's tracker —
//! `ClaudeWatch::agents` (`management_live.rs`), fed by the relay, persisted,
//! emptied on `/clear` and refreshed once a second — and keeps only what the
//! pane itself needs per tab: the feed it last read.
//!
//! **Clocks stop while nothing can run**: while the tab's
//! account is out of a usage limit ([`Limit`]), a Running row's ELAPSED
//! stops and its ETA holds instead of counting down past zero, and NOW says
//! `5h limit → 13:00`. The pane remembers each such span ([`Hold`]) and
//! both clocks carry on from where they stopped once the limit resets. A
//! feed row the manager has paused (`paused_since`) is held
//! the same way, and reads `paused since 12:58`. Planned ETAs are durations
//! and hold by themselves; a Done row is measured history and never moves.
//!
//! **Clicks** produce a [`RowClick`], which the app receives as
//! `Action::AgentRowClicked`. What a click *does* is not decided here, and
//! it leaves no mark: a row is tinted only while the pointer is on it
//! ([`row_tint`]), so nothing stays highlighted after a click.
//!
//! **Nesting**: an agent a worker spawned is drawn under
//! that worker's row, indented a step per level, and goes where that row
//! goes ([`nest`]); a toggle at the head of the worker's title folds them
//! away ([`Fold`]), kept per tab while Giverny runs.
//!
//! **Resources**: what a row's `giverny manage run`
//! commands use in a quiet column — live CPU and memory while one runs, the
//! memory peak once the row is Done, zero when there is neither — its lease from the machine ledger in
//! its overlay header, a Next up row's place in the ledger's queue in NOW,
//! and the machine's leases against its limits on a dim bottom line. The
//! ledger and the commands' cgroups are read off the UI thread
//! ([`LedgerWatch`]); the ledger wins over a row's own copy ([`with_ledger`]).
//!
//! **Text is selectable**: a drag — never a click — selects
//! the pane's text the way the terminal does, as a stream of cells across
//! rows, copies it as plain text line by line when the drag ends, and
//! `Ctrl+Shift+C` copies it again. The next press anywhere lets it go.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use eframe::egui::{self, Color32, CursorIcon, Sense, Ui};
use giverny_claude::agent_eta::{self, Etas};
use giverny_claude::bg_shells;
use giverny_claude::feed::{
    self, Drawn, Feed, FeedCache, LeaseState, PaneRow, RowLease, RowUsage, Stage,
};
use giverny_claude::manage_chores::{self, Chore};
use giverny_claude::resources::{self, Ledger};
use giverny_claude::run_live::{RunLive, TaskLive};
use giverny_claude::session_use;
use giverny_claude::sleeps;
use giverny_claude::subagents::{Outcome, SubagentRow, Tracker};
use giverny_claude::worker_log::WorkerLog;
use giverny_core::config::{DoneRows, ManagementPanelConfig, PaneColumns};
use giverny_core::limits::{Limits, Machine, Mem, Resolved};
use giverny_core::tabs::TabId;
use giverny_term::widget::RenderShared;

use crate::chrome::Chrome;
use crate::claude_watch::ClaudeWatch;
use crate::sessions_load::{self, Load};

/// The three stage tints, the 256-colour codes 38;5;32, 38;5;67 and
/// 38;5;28, picked to read on dark and light
/// themes alike.
pub const RUNNING: Color32 = Color32::from_rgb(0x00, 0x87, 0xd7);
pub const PLANNED: Color32 = Color32::from_rgb(0x5f, 0x87, 0xaf);
pub const DONE: Color32 = Color32::from_rgb(0x00, 0x87, 0x00);

/// How often the feed file is stat'ed.
const POLL: Duration = Duration::from_secs(1);

// Column widths, in characters.
const STAGE_W: usize = 7;
const EL_W: usize = 8;
const ETA_W: usize = 10;
const NOW_W: usize = 28;
const TOK_W: usize = 6;
const GAP: usize = 2;
const MIN_TITLE: usize = 12;

/// What a Running row's ETA cell says when nobody gave it an estimate —
/// drawn dim, so a row the dispatcher forgot to estimate reads as missing
/// one rather than as a blank.
pub const NO_ETA: &str = "no ETA";
/// Where in [`Cols::segments`] the ETA cell sits.
const ETA_SEG: usize = 3;
/// Where in [`Cols::segments`] the NOW cell sits.
const NOW_SEG: usize = 4;
/// Where in [`Cols::segments`] the use cell sits.
const LEASE_SEG: usize = 6;

// --------------------------------------------------------- view state ----

/// What the pane itself keeps per tab: the feed it last read and the text
/// dragged over. Never the rows — those are the tracker's — and no row
/// selection: a click acts and leaves no mark.
#[derive(Default)]
struct View {
    feed: FeedCache,
    feed_now: Option<Feed>,
    /// The session's agent ETAs: the estimates of workers that are no
    /// manager session's task.
    etas: agent_eta::Cache,
    etas_now: Etas,
    feed_session: Option<String>,
    /// The session is a manager: the manage skill ran in it
    /// ([`giverny_claude::managers`]). Looked up with the feed.
    manager: bool,
    /// The session's conversation root, once read: what a resumed
    /// manager's mark is found by, read from its transcript once per id.
    root: Option<String>,
    last_poll: Option<Instant>,
    /// The height the pane last sized itself to, while the user has not
    /// dragged it; `None` once they have.
    fit: Option<f32>,
    /// Every usage-limit span this tab's clocks were held through.
    holds: Vec<Hold>,
    /// The text dragged over, until the next press.
    sel: Option<Selection>,
    /// The rows' clicks as drawn last frame: what a line sent from a Next
    /// up row's page watches for its worker (`row_keyed`).
    clicks: Vec<RowClick>,
    /// The pointer over the rows last frame: `Some(None)` on the pane but
    /// not on a worker's row, `Some(Some(row))` on that row.
    hover: Option<Option<RowClick>>,
    /// The transcripts are due a poll (with the feed, once a [`POLL`]).
    logs_due: bool,
    /// Each worker's whole transcript, by agent id: its turns' tokens and
    /// its dispatcher's messages, to split a worker that took tasks one
    /// after another into a row per task.
    logs: Logs,
    /// The chores last kept for this tab's manager, and for which session
    /// ([`Views::keep_chores`]): written again only when they change.
    chores: Option<(String, Vec<Chore>)>,
    /// The Done rows this tab's pane has shown, by [`done_ident`]: what of
    /// it was seen, kept up every frame it is drawn. `None`
    /// until it is first drawn.
    seen_done: Option<HashSet<String>>,
    /// The tab was just looked at with its unseen-finish tick on
    /// ([`Views::flash_new_done`]): the next drawing flashes what landed
    /// since the last look.
    flash_due: bool,
    /// The newly done rows breathing now, if any.
    flash: Option<Flash>,
    /// The rows whose agents' own agents are folded away under them,
    /// by [`fold_ident`]: kept for as long as the tab is.
    folded: HashSet<String>,
    /// The session this view first saw, and when: a session's hook mark
    /// is looked for only after [`HOOKLESS_GRACE`].
    since: Option<(String, Instant)>,
    /// No Giverny plugin hook ever ran in the session: the
    /// pane says so ([`HOOKLESS`]). Looked up with the feed.
    hookless: bool,
    /// The workers' own background shells still running:
    /// what tells a worker asleep on its own work from one that ended.
    /// Looked at with the feed.
    shells: giverny_claude::bg_shells::Watch,
}

/// How long a session may run before its missing hook mark is shown: its
/// `SessionStart` hook writes it, but the pane may see the session first.
const HOOKLESS_GRACE: Duration = Duration::from_secs(20);

/// The pane's mark for a session no Giverny plugin hook ran in
/// ([`giverny_claude::escapes::hooked`]).
pub const HOOKLESS: &str = "no guard: this session ran no Giverny plugin hook (plugin off, or an older claude); the memory cap still holds";

/// The table's footer with the hookless mark, when the session has it,
/// put first.
fn with_hookless(footer: Option<String>, hookless: bool) -> Option<String> {
    if !hookless {
        return footer;
    }
    Some(match footer {
        Some(f) => format!("{HOOKLESS} · {f}"),
        None => HOOKLESS.to_string(),
    })
}

/// The newly done rows' frames breathing together: which rows
/// ([`done_ident`]) and from when, on the animation clock.
struct Flash {
    rows: HashSet<String>,
    from: f64,
}

/// How many breaths the newly done rows' frames take: a few, together, then
/// still.
const FLASH_BREATHS: f64 = 3.0;

/// How long a flash lasts, in seconds: its breaths on the rail's calm
/// timing ([`crate::rail::CALM_PERIOD`]), so the pane and the rail's marks
/// breathe alike.
const FLASH_S: f64 = FLASH_BREATHS * crate::rail::CALM_PERIOD;

/// How lit a flashing row's frame is `t` seconds into the flash, 0 to 1:
/// the calm breath, out from nothing and back, [`FLASH_BREATHS`] times, so
/// it ends where it began, unframed. With `rail.animate` off (`animate`) a
/// steady frame for the same span instead. `None` once the flash is over
/// (or before it began).
fn flash_glow(t: f64, animate: bool) -> Option<f32> {
    if !(0.0..FLASH_S).contains(&t) {
        return None;
    }
    Some(if animate {
        crate::rail::calm_breath(Some(t))
    } else {
        1.0
    })
}

/// What a Done row is known by from one look at the pane to the next: the
/// feed's key, else its worker's id, else its name — as
/// [`RowClick::same_row`] tells rows apart.
fn done_ident(c: &RowClick) -> String {
    if !c.key.is_empty() {
        format!("k:{}", c.key)
    } else if let Some(a) = &c.agent_id {
        format!("a:{a}")
    } else {
        format!("n:{}", c.name)
    }
}

/// The Done rows of `lines` that landed since the pane was last looked at:
/// those it never showed (`seen`). A pane not drawn since Giverny started
/// (`seen` is `None`) knows nothing of what was seen before, so only rows
/// that landed since then (`born_ms`) count as new.
fn newly_done(lines: &[Line], seen: Option<&HashSet<String>>, born_ms: u64) -> HashSet<String> {
    lines
        .iter()
        .filter(|l| l.stage == Stage::Done)
        .filter(|l| match seen {
            Some(seen) => !seen.contains(&done_ident(&l.click)),
            None => l.landed_ms.is_some_and(|e| e >= born_ms),
        })
        .map(|l| done_ident(&l.click))
        .collect()
}

/// Note every Done row of `lines` as seen: the pane is showing them.
fn note_seen(seen: &mut Option<HashSet<String>>, lines: &[Line]) {
    let seen = seen.get_or_insert_with(HashSet::new);
    for l in lines.iter().filter(|l| l.stage == Stage::Done) {
        seen.insert(done_ident(&l.click));
    }
}

impl View {
    /// Re-read the feed for `session`, at most once a [`POLL`] (or at once
    /// when the session changed). A session Claude Code re-id'd is looked
    /// up by its earlier ids too, newest first, until its files name the
    /// new one.
    ///
    /// The manager mark is looked up with it ([`View::manager`]): a `stat`
    /// of the session's mark and its earlier ids', and of its
    /// conversation's, whose root is read from the transcript (under
    /// `config`) once.
    fn poll_feed(&mut self, session: Option<&str>, aliases: &[String], config: Option<&Path>) {
        let changed = self.feed_session.as_deref() != session;
        if !changed && self.last_poll.is_some_and(|t| t.elapsed() < POLL) {
            return;
        }
        self.last_poll = Some(Instant::now());
        if changed {
            self.root = None;
        }
        self.feed_session = session.map(str::to_string);
        let ids: Vec<&str> = session
            .into_iter()
            .chain(aliases.iter().rev().map(String::as_str))
            .collect();
        let dir = feed::feed_dir();
        self.manager = session.is_some_and(|s| {
            let root = &mut self.root;
            giverny_claude::managers::is_marked(&dir, s, &ids[1..], || {
                if root.is_none() {
                    *root = giverny_claude::continuation::root_of(config, s);
                }
                root.clone()
            })
        });
        self.feed_now = session.and_then(|_| self.feed.poll_any(&dir, &ids).cloned());
        if self.since.as_ref().map(|s| s.0.as_str()) != session {
            self.since = session.map(|s| (s.to_string(), Instant::now()));
        }
        self.hookless = self
            .since
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() >= HOOKLESS_GRACE)
            && !giverny_claude::escapes::hooked(&dir, &ids);
        self.etas_now = session
            .map(|_| self.etas.poll_any(&dir, &ids).clone())
            .unwrap_or_default();
        self.shells.poll(&dir, &ids, now_ms());
        self.logs_due = true;
    }
}

/// Every worker's [`WorkerLog`], by agent id.
pub type Logs = HashMap<String, WorkerLog>;

/// Follow each worker's transcript: a new one is read whole once, a known
/// one only for what was appended (a `stat` when nothing was). Workers no
/// longer listed are forgotten.
pub fn poll_logs(logs: &mut Logs, live: &[SubagentRow]) {
    logs.retain(|id, _| live.iter().any(|l| &l.id == id));
    for l in live {
        let Some(path) = &l.transcript else { continue };
        let log = logs
            .entry(l.id.clone())
            .or_insert_with(|| WorkerLog::new(path));
        if log.path() != path {
            *log = WorkerLog::new(path);
        }
        log.poll();
    }
}

/// Every tab's pane view state.
pub struct Views {
    tabs: HashMap<TabId, View>,
    /// The machine ledger, read off the UI thread: one for every tab, as
    /// there is one ledger for the whole machine.
    ledger: LedgerWatch,
    /// When every manager tab's chores were last worked out.
    chores_at: Option<Instant>,
    /// What the machine slept through: one log for the whole machine.
    sleeps: sleeps::Cache,
    /// When Giverny started (epoch ms): a pane not drawn since knows only
    /// that rows landed after it went unseen ([`newly_done`]).
    born_ms: u64,
}

impl Default for Views {
    fn default() -> Self {
        Views {
            tabs: HashMap::new(),
            ledger: LedgerWatch::default(),
            chores_at: None,
            sleeps: sleeps::Cache::default(),
            born_ms: now_ms(),
        }
    }
}

/// How often every manager tab's chores are worked out ([`Views::keep_chores`]).
const CHORES_EVERY: Duration = Duration::from_secs(10);

impl Views {
    /// Keep what each tab's manager has to be told ([`chores`]) where the
    /// plugin's hook finds it ([`manage_chores::publish`]): the due chores
    /// its rows were not told of yet, worked out from the very rows the
    /// pane draws, for every tab whose session is up (`tabs`) — the one in
    /// view or not. Call every frame; it works once a [`CHORES_EVERY`].
    pub fn keep_chores<'t>(&mut self, tabs: impl IntoIterator<Item = (TabId, &'t Tracker)>) {
        if self.chores_at.is_some_and(|t| t.elapsed() < CHORES_EVERY) {
            return;
        }
        self.chores_at = Some(Instant::now());
        let now = now_ms();
        let dir = feed::feed_dir();
        let ledger = giverny_claude::manage_inbox::read_ledger(&ledger_file(), now);
        for (tab, tracker) in tabs {
            let Some(session) = tracker.session_id.clone() else {
                continue;
            };
            let view = self.tabs.entry(tab).or_default();
            view.poll_feed(
                Some(&session),
                &tracker.aliases,
                tracker.config_dir.as_deref(),
            );
            if std::mem::take(&mut view.logs_due) {
                poll_logs(&mut view.logs, tracker.rows());
            }
            // Worked out from the rows the pane draws: a session with
            // nothing drawn has nothing to be told.
            let opens = feed::opens(!tracker.is_empty(), view.manager);
            let drawn = Drawn::of(view.manager);
            let mut still: Vec<String> = Vec::new();
            let due = match view.feed_now.as_ref().filter(|_| opens) {
                Some(f) => {
                    let cleared = tracker.done_cleared_ms.map(|c| f.without_done_by(c));
                    let f = cleared.as_ref().unwrap_or(f);
                    let leased = ledger.as_ref().map(|l| {
                        let mut sessions: Vec<&str> =
                            f.session.iter().map(String::as_str).collect();
                        sessions.extend(f.aliases.iter().map(String::as_str));
                        sessions.push(&session);
                        sessions.extend(tracker.aliases.iter().map(String::as_str));
                        with_ledger(f, &sessions, l)
                    });
                    let f = leased.as_ref().unwrap_or(f);
                    let due = due_chores(
                        drawn,
                        f,
                        tracker.rows(),
                        &view.logs,
                        view.shells.live(),
                        now,
                    );
                    if due.iter().any(|c| c.kind == manage_chores::Kind::NowPaused) {
                        still = finishing(drawn, f, tracker.rows(), &view.logs, now);
                    }
                    due
                }
                None => Vec::new(),
            };
            let same = view
                .chores
                .as_ref()
                .is_some_and(|(s, c)| *s == session && *c == due);
            if same {
                continue;
            }
            match manage_chores::publish_with(&dir, &session, &due, &still) {
                Ok(()) => view.chores = Some((session, due)),
                Err(e) => tracing::warn!("management panel: chores not kept: {e}"),
            }
        }
    }

    /// The user looked at `tab` with its unseen-finish tick on:
    /// when its pane is next drawn, the Done rows that landed
    /// since they last looked breathe their frames together a few times,
    /// then go still.
    pub fn flash_new_done(&mut self, tab: TabId) {
        self.tabs.entry(tab).or_default().flash_due = true;
    }

    /// Forget a closed tab.
    pub fn forget(&mut self, tab: TabId) {
        self.tabs.remove(&tab);
    }

    /// Where the pointer was over `tab`'s rows when the pane was last drawn,
    /// taken so it is never read twice: `None` off the pane (or when the
    /// pane was not drawn), `Some(None)` on it but off every row,
    /// `Some(Some(row))` on a row.
    pub fn take_hover(&mut self, tab: TabId) -> Option<Option<RowClick>> {
        self.tabs.get_mut(&tab)?.hover.take()
    }

    /// The row of `tab`'s pane with feed key `key`, as drawn last frame.
    pub fn row_keyed(&self, tab: TabId, key: &str) -> Option<&RowClick> {
        let v = self.tabs.get(&tab)?;
        v.clicks.iter().find(|c| !key.is_empty() && c.key == key)
    }
}

// ------------------------------------------------------------ limits ----

/// A usage limit the tab's account is out on right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limit {
    /// When it reopens (epoch ms), when anything knows.
    pub reopens_ms: Option<u64>,
    /// Which window: `5h`, `7d`; `None` when the source does not say.
    pub window: Option<&'static str>,
}

impl Limit {
    /// Still out at `now_ms`: a reset time that has come round is no limit.
    fn out_at(&self, now_ms: u64) -> bool {
        self.reopens_ms.is_none_or(|r| r > now_ms)
    }
}

/// One span in which the account could run nothing. `until_ms` is `None`
/// while it is still out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hold {
    pub since_ms: u64,
    pub until_ms: Option<u64>,
    /// The reset the limit named, so a hold closed late (the tab was not
    /// being drawn when the window reopened) ends at the reset, not then.
    pub reopens_ms: Option<u64>,
}

/// How many holds a tab remembers; older ones are before any row's start.
const MAX_HOLDS: usize = 32;

/// Open a hold when a limit is first seen out, close it when it is gone.
pub fn track(holds: &mut Vec<Hold>, limit: Option<&Limit>, now_ms: u64) {
    let out = limit.filter(|l| l.out_at(now_ms));
    let open = holds.last_mut().filter(|h| h.until_ms.is_none());
    match (out, open) {
        (Some(l), Some(h)) => h.reopens_ms = l.reopens_ms.or(h.reopens_ms),
        (Some(l), None) => {
            holds.push(Hold {
                since_ms: now_ms,
                until_ms: None,
                reopens_ms: l.reopens_ms,
            });
            if holds.len() > MAX_HOLDS {
                holds.remove(0);
            }
        }
        (None, Some(h)) => {
            let end = h.reopens_ms.map_or(now_ms, |r| r.min(now_ms));
            h.until_ms = Some(end.max(h.since_ms));
        }
        (None, None) => {}
    }
}

/// Milliseconds of `[start, upto]` that fall inside a hold or a sleep —
/// what comes off a row's clock. A sleep inside a hold comes off once.
pub fn off_ms(holds: &[Hold], slept: &[sleeps::Span], start: u64, upto: u64) -> u64 {
    let held = holds
        .iter()
        .map(|h| (h.since_ms, h.until_ms.unwrap_or(upto)));
    let slept = slept.iter().map(|z| (z.since, z.until));
    sleeps::off_ms(held.chain(slept), start, upto)
}

/// The usage limit `tab` is stopped by, if any: the app's own record of a
/// tab that stopped on a limit (`stopped`, its reset time if known), else
/// the tab's account's meters showing a window used up with a reset still
/// to come. Read-only on `claude`.
pub fn limit_for(
    claude: &ClaudeWatch,
    tab: TabId,
    stopped: Option<Option<jiff::Timestamp>>,
    now: jiff::Timestamp,
) -> Option<Limit> {
    let account = claude
        .tabs
        .get(&tab)
        .and_then(|t| t.account.as_deref())
        .and_then(|name| claude.accounts.iter().find(|a| a.profile.name == name));
    let mut windows: Vec<(&'static str, f64, Option<jiff::Timestamp>)> = Vec::new();
    if let Some(panel) = account {
        for (kind, short, pick, reset) in [
            (
                "session",
                "5h",
                (|l: &crate::claude_watch::LiveUsage| l.five_hour)
                    as fn(&crate::claude_watch::LiveUsage) -> Option<f64>,
                (|l: &crate::claude_watch::LiveUsage| l.five_hour_resets)
                    as fn(&crate::claude_watch::LiveUsage) -> Option<jiff::Timestamp>,
            ),
            ("weekly_all", "7d", |l| l.seven_day, |l| l.seven_day_resets),
        ] {
            let cached = panel
                .usage
                .as_ref()
                .and_then(|u| u.limits.iter().find(|l| l.kind == kind));
            match cached {
                Some(entry) => {
                    let r = ClaudeWatch::reading(panel, entry, now);
                    windows.push((short, r.percent, r.resets));
                }
                None => {
                    if let Some(live) = &panel.live
                        && let Some(pct) = pick(live)
                    {
                        windows.push((short, pct, reset(live)));
                    }
                }
            }
        }
    }
    let ms = |t: jiff::Timestamp| t.as_millisecond().max(0) as u64;
    spent(&windows, ms(now)).or_else(|| {
        stopped.map(|reopens| Limit {
            reopens_ms: reopens.map(ms),
            window: None,
        })
    })
}

/// The window that is used up, from `(window, percent, resets)` readings:
/// out is 100%, and only with a reset still to come (a meter whose reset has
/// passed is describing a window that is over). Two out at once: the one
/// that reopens last, since nothing runs until both have.
pub fn spent(
    windows: &[(&'static str, f64, Option<jiff::Timestamp>)],
    now_ms: u64,
) -> Option<Limit> {
    windows
        .iter()
        .filter(|(_, pct, _)| *pct >= 100.0)
        .filter_map(|(w, _, at)| {
            let at = at.map(|t| t.as_millisecond().max(0) as u64)?;
            (at > now_ms).then_some((*w, at))
        })
        .max_by_key(|(_, at)| *at)
        .map(|(w, at)| Limit {
            reopens_ms: Some(at),
            window: Some(w),
        })
}

/// NOW on a held row: `5h limit → 13:00`, the weekday in front for a reset
/// not today, `limit` alone when nothing says when.
pub fn limit_note(limit: &Limit, now_ms: u64, tz: &jiff::tz::TimeZone) -> String {
    let what = match limit.window {
        Some(w) => format!("{w} limit"),
        None => "limit".to_string(),
    };
    match limit.reopens_ms {
        Some(at) => format!("{what} → {}", clock_at(at, now_ms, tz)),
        None => what,
    }
}

/// `13:00` for an instant later today, `Mon 08:00` for one further off.
fn clock_at(at_ms: u64, now_ms: u64, tz: &jiff::tz::TimeZone) -> String {
    let z = |ms: u64| {
        jiff::Timestamp::from_millisecond(ms as i64)
            .unwrap_or(jiff::Timestamp::UNIX_EPOCH)
            .to_zoned(tz.clone())
    };
    let (at, now) = (z(at_ms), z(now_ms));
    if at.date() == now.date() {
        at.strftime("%H:%M").to_string()
    } else {
        at.strftime("%a %H:%M").to_string()
    }
}

/// What the table's clocks need beyond `now`: the holds, the limit out now
/// (for NOW), and the zone NOW writes times in.
pub struct Clock<'a> {
    pub holds: &'a [Hold],
    /// What the machine slept through ([`sleeps`]): off every clock.
    pub sleeps: &'a [sleeps::Span],
    pub limit: Option<&'a Limit>,
    pub tz: jiff::tz::TimeZone,
    /// Each worker's processes' use now, by agent id: what a Running row
    /// with no feed figure of its own shows.
    pub workers: &'a HashMap<String, RunLive>,
    /// Each worker's agent ETA, by agent id: what a row with no feed
    /// estimate counts down from.
    pub etas: &'a Etas,
    /// Each worker's own background shells still running, by agent id:
    /// a worker whose turn ended with one is asleep.
    pub shells: &'a Shells,
}

/// The workers' live background shells, by agent id ([`bg_shells::Watch`]).
pub type Shells = HashMap<String, Vec<bg_shells::Shell>>;

/// No worker measured.
#[cfg(test)]
static NO_WORKERS: std::sync::LazyLock<HashMap<String, RunLive>> =
    std::sync::LazyLock::new(HashMap::new);

/// No agent ETAs.
#[cfg(test)]
static NO_ETAS: std::sync::LazyLock<Etas> = std::sync::LazyLock::new(Etas::new);

/// No background shells.
#[cfg(test)]
static NO_SHELLS: std::sync::LazyLock<Shells> = std::sync::LazyLock::new(Shells::new);

#[cfg(test)]
impl Clock<'_> {
    pub(crate) fn plain() -> Clock<'static> {
        Clock {
            holds: &[],
            sleeps: &[],
            limit: None,
            tz: jiff::tz::TimeZone::system(),
            workers: &NO_WORKERS,
            etas: &NO_ETAS,
            shells: &NO_SHELLS,
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// --------------------------------------------------------- resources ----

/// How often the ledger is re-read while a pane is up.
const LEDGER_POLL: Duration = Duration::from_secs(2);
/// No pane drawn for this long: the reader stops reading until one is.
const LEDGER_IDLE_MS: u64 = 10_000;
/// How often the machine itself (cores, RAM, `nvidia-smi`) is re-detected.
const MACHINE_EVERY: Duration = Duration::from_secs(300);

/// What the machine ledger said when it was last read: its live leases and
/// queue (expired entries dropped, nothing written back) and the limits
/// they count against.
#[derive(Debug, Clone, PartialEq)]
pub struct LedgerView {
    pub ledger: Ledger,
    /// `[manager.limits]` resolved for this machine; `None` when the
    /// config could not be read.
    pub limits: Option<Resolved>,
}

/// The ledger, read by a thread of its own: the read takes the
/// ledger's `flock`, which a writer may hold, and resolving the limits runs
/// `nvidia-smi` — neither belongs on the UI thread. The pane
/// only ever takes the last snapshot.
#[derive(Default)]
struct LedgerWatch {
    shared: Option<Arc<LedgerShared>>,
}

#[derive(Default)]
struct LedgerShared {
    snap: Mutex<Option<Arc<LedgerView>>>,
    /// When a pane last asked: the reader idles once nothing has for
    /// [`LEDGER_IDLE_MS`].
    wanted_ms: AtomicU64,
}

impl LedgerWatch {
    /// The last snapshot, starting the reader on first use. `None` until
    /// its first read.
    fn get(&mut self, ctx: &egui::Context) -> Option<Arc<LedgerView>> {
        let shared = self.shared.get_or_insert_with(|| {
            let shared = Arc::new(LedgerShared::default());
            let (s, ctx) = (shared.clone(), ctx.clone());
            if let Err(err) = std::thread::Builder::new()
                .name("management-panel-ledger".into())
                .spawn(move || read_ledger_loop(&s, &ctx))
            {
                tracing::warn!("management panel: the ledger reader did not start: {err}");
            }
            shared
        });
        shared.wanted_ms.store(now_ms(), Ordering::Relaxed);
        shared.snap.lock().ok().and_then(|s| s.clone())
    }
}

fn read_ledger_loop(shared: &LedgerShared, ctx: &egui::Context) {
    let mut machine: Option<(Machine, Instant)> = None;
    loop {
        let now = now_ms();
        if now.saturating_sub(shared.wanted_ms.load(Ordering::Relaxed)) <= LEDGER_IDLE_MS {
            let m = match &machine {
                Some((m, at)) if at.elapsed() < MACHINE_EVERY => m.clone(),
                _ => {
                    let m = Machine::detect();
                    machine = Some((m.clone(), Instant::now()));
                    m
                }
            };
            if let Some(view) = read_ledger(&m, now) {
                let mut snap = match shared.snap.lock() {
                    Ok(s) => s,
                    Err(p) => p.into_inner(),
                };
                if snap.as_deref() != Some(&view) {
                    *snap = Some(Arc::new(view));
                    drop(snap);
                    ctx.request_repaint();
                }
            }
        }
        std::thread::sleep(LEDGER_POLL);
    }
}

/// One read of the ledger, under its lock, changing nothing on disk. No
/// ledger is an empty one; an unreadable one is `None` (the last good read
/// stays).
fn read_ledger(machine: &Machine, now: u64) -> Option<LedgerView> {
    let path = ledger_file();
    let mut ledger = if path.exists() {
        let _lock = resources::LedgerLock::take(&path).ok()?;
        Ledger::parse(&std::fs::read(&path).ok()?).ok()?
    } else {
        Ledger::default()
    };
    ledger.expire(now);
    let limits = Limits::load().ok().map(|l| l.resolve(machine));
    Some(LedgerView { ledger, limits })
}

fn ledger_file() -> PathBuf {
    resources::ledger_path(&feed::feed_dir())
}

/// `feed` with each row's `live` use: its task's running commands under any
/// of `sessions`, matched as [`with_ledger`] matches leases.
pub fn with_live(mut feed: Feed, sessions: &[&str], live: &[TaskLive]) -> Feed {
    for row in &mut feed.rows {
        row.live = live
            .iter()
            .find(|t| t.task == row.key && sessions.contains(&t.session.as_str()))
            .map(|t| t.live);
    }
    feed
}

/// `feed` with each row whose worker's processes were measured showing
/// them: everything its Bash commands started ([`giverny_claude::worker_pids`]),
/// whether or not they ran under `giverny manage run`. A run of the row's
/// task under that worker is part of the worker's figure already; one
/// started elsewhere is added to it.
pub fn with_workers(
    mut feed: Feed,
    sessions: &[&str],
    live: &[TaskLive],
    workers: &HashMap<String, RunLive>,
) -> Feed {
    for row in &mut feed.rows {
        let Some(w) = row.agent_id.as_deref().and_then(|id| workers.get(id)) else {
            continue;
        };
        let run = live
            .iter()
            .find(|t| t.task == row.key && sessions.contains(&t.session.as_str()));
        row.live = Some(match run {
            Some(t) if t.agent.is_some() && t.agent == row.agent_id => *w,
            Some(t) => w.plus(t.live),
            None => *w,
        });
    }
    feed
}

/// `feed` with every row's `lease` taken from the ledger rather than the
/// row's own copy, which is only as fresh as its session's last `claim`: a
/// lease another session released, or one that expired, is not pushed back
/// to rows. A row's lease is the ledger's entry for its key
/// under any of `sessions` (the feed's and the tab's ids); a row with none
/// holds nothing. A queued row's place is its place now. It is short of
/// what its copy says (its last claim), and waits behind whoever is in the
/// way of that now ([`resources::in_the_way`]). A copy that says nothing
/// short (written by an older version) waits behind the task it named while
/// that one still holds or waits ahead, else whoever holds a slot it asks
/// for, else the request queued before it.
pub fn with_ledger(feed: &Feed, sessions: &[&str], ledger: &Ledger) -> Feed {
    let mut f = feed.clone();
    let mine = |s: &str| sessions.contains(&s);
    let queue = ledger.ordered_queue();
    for row in &mut f.rows {
        let copy = row.lease.take();
        if let Some(l) = ledger
            .leases
            .iter()
            .find(|l| l.task == row.key && mine(&l.session))
        {
            // Smaller is only on the copy: the ledger keeps what was granted.
            let smaller = copy
                .as_ref()
                .filter(|c| c.state == LeaseState::Smaller && c.id.as_deref() == Some(&l.id));
            row.lease = Some(RowLease {
                state: if smaller.is_some() {
                    LeaseState::Smaller
                } else {
                    LeaseState::Granted
                },
                id: Some(l.id.clone()),
                ram_mb: l.ram_mb,
                gpus: l.gpus.clone(),
                vram_mb: l.vram_mb,
                slots: l.slots.clone(),
                granted_ms: Some(l.granted_at),
                wanted_ram_mb: smaller.and_then(|c| c.wanted_ram_mb),
                position: None,
                behind: None,
                short: Vec::new(),
                uncollected: l.uncollected.is_some(),
                lapsed: false,
            });
        } else if let Some(i) = queue
            .iter()
            .position(|w| w.task == row.key && mine(&w.session))
        {
            let w = queue[i];
            let ahead = &queue[..i];
            // What it is short of is as of its last claim (the ledger keeps
            // no verdict); who is in the way of that is looked up now.
            let short = copy
                .as_ref()
                .filter(|c| c.state == LeaseState::Queued)
                .map(|c| c.short.clone())
                .unwrap_or_default();
            let behind = if short.is_empty() {
                let still = |t: &str| {
                    ledger.leases.iter().any(|l| l.task == t) || ahead.iter().any(|a| a.task == t)
                };
                copy.as_ref()
                    .and_then(|c| c.behind.clone())
                    .filter(|b| still(b))
                    .or_else(|| {
                        ledger
                            .leases
                            .iter()
                            .find(|l| l.slots.iter().any(|s| w.request.slots.contains(s)))
                            .map(|l| l.task.clone())
                    })
                    .or_else(|| ahead.last().map(|a| a.task.clone()))
            } else {
                // Which leases are building the pane does not know; a busy
                // CPU is then waited out behind the request ahead, if any.
                resources::in_the_way(
                    &ledger.leases,
                    ahead.iter().copied(),
                    &w.request,
                    &short,
                    &|_| false,
                )
            };
            row.lease = Some(RowLease {
                state: LeaseState::Queued,
                id: None,
                ram_mb: w.request.ram_mb,
                gpus: Vec::new(),
                vram_mb: w.request.vram_mb,
                slots: w.request.slots.clone(),
                granted_ms: None,
                wanted_ram_mb: None,
                position: Some(i as u64 + 1),
                behind,
                short,
                uncollected: false,
                lapsed: w.lapsed.is_some(),
            });
        }
    }
    f
}

/// An asked or leased size, as it was set ([`Mem`]): `3G`, `1.5G`,
/// `512M`, and `0` for nothing. Measured sizes are [`gb`].
fn mem(mb: u64) -> String {
    if mb == 0 {
        "0".into()
    } else {
        Mem(mb).to_string()
    }
}

/// A measured size in exactly four characters, for [`usage_cell`]: always
/// in G (T from a thousand G), `0.0G` for nothing, `0.1G` at the least for
/// anything, `4.2G`, ` 12G`, `999G`, `1.5T`. Tenths only below ten, so the
/// figure never grows a fifth character.
fn mem4(mb: u64) -> String {
    session_use::gb4(mb)
}

/// A measured size in G, unpadded: [`mem4`]'s figure, for the overlay
/// header's `peak 0.5G of 3G`.
fn gb(mb: u64) -> String {
    giverny_claude::session_use::gb(mb)
}

/// The OOM slot at the head of a [`usage_cell`].
const OOM_SLOT: &str = "OOM ";

/// The GPU slot at the tail of a [`usage_cell`] on a machine with a GPU:
/// `  ·  gpu 1.2G`.
const GPU_W: usize = 13;

/// A row's use in its cell: a Running row's commands now,
/// ` 14% CPU  ·  4.2G` (CPU as a share of the whole machine, so at most
/// `100%`), a Done row's memory peak alone in the same place, `4.2G`; on a
/// machine with a GPU (`gpu`), a Running row's GPU memory after,
/// `  ·  gpu 1.2G`. `OOM`
/// leads when the memory cap killed a run, so the figures keep their
/// places. Nothing measured reads as zero, `0.0G`, never as a blank: the
/// cell is always there. Every slot has a fixed width, so every cell is
/// as wide as every other and the column never jumps as the figures
/// change. The lease is not here: the overlay header says it
/// ([`lease_fact`]).
pub fn usage_cell(live: Option<RunLive>, peak_mb: Option<u64>, oom: bool, gpu: bool) -> String {
    let blank = |n: usize| " ".repeat(n);
    let sep = session_use::SEP;
    let oom = if oom {
        OOM_SLOT.to_string()
    } else {
        blank(OOM_SLOT.len())
    };
    let (cpu, memory) = match live {
        Some(l) => (
            format!("{}{sep}", session_use::cpu8(l.cpu_pct)),
            mem4(l.mem_mb),
        ),
        None => (blank(8 + sep.chars().count()), mem4(peak_mb.unwrap_or(0))),
    };
    let gpu = match live.and_then(|l| l.gpu_mb) {
        Some(g) if gpu => format!("{sep}gpu {}", mem4(g)),
        _ if gpu => blank(GPU_W),
        _ => String::new(),
    };
    format!("{oom}{cpu}{memory}{gpu}")
}

/// How heavy a run's live CPU and memory are on the whole machine of
/// `ram_mb`, as the rail weighs a tab's ([`sessions_load::machine_shares`]).
fn run_load(l: RunLive, ram_mb: u64) -> (Load, Load) {
    let u = giverny_claude::use_reading::Use {
        cpu_pct: l.cpu_pct,
        mem_mb: l.mem_mb,
        gpu_mb: None,
        gpu_pct: None,
    };
    let (cpu, mem) = sessions_load::machine_shares(&u, ram_mb);
    (Load::of(cpu), Load::of(mem))
}

/// A use cell as drawn, in runs of one colour: `(column in the cell, text,
/// colour)`. Everything is `ink`, the cell's own colour, but a Running
/// row's CPU and memory figures (`load`), each coloured by its weight on
/// the machine ([`Load::color`]): the same columns as [`usage_cell`]'s
/// slots, so nothing moves. The OOM tag, the separator and the GPU slot
/// keep `ink`.
fn usage_inks(
    cell: &str,
    load: Option<(Load, Load)>,
    ink: Color32,
    c: &Chrome,
) -> Vec<(usize, String, Color32)> {
    let chars: Vec<char> = cell.chars().collect();
    let piece = |a: usize, b: usize| -> String {
        chars[a.min(chars.len())..b.min(chars.len())]
            .iter()
            .collect()
    };
    let Some((cpu, mem)) = load else {
        return vec![(0, cell.to_string(), ink)];
    };
    let cpu_at = OOM_SLOT.len();
    let mem_at = cpu_at + 8 + session_use::SEP.chars().count();
    let runs = [
        (0, cpu_at, ink),
        (cpu_at, cpu_at + 8, cpu.color(ink, *c)),
        (cpu_at + 8, mem_at, ink),
        (mem_at, mem_at + 4, mem.color(ink, *c)),
        (mem_at + 4, chars.len(), ink),
    ];
    runs.into_iter()
        .map(|(a, b, color)| (a, piece(a, b), color))
        .filter(|(_, t, _)| !t.trim().is_empty())
        .collect()
}

/// The usage in full for the row's overlay header: `peak 2.1G of 3G, 45s
/// CPU, killed by the memory cap (OOM) x2`.
fn usage_fact(u: &RowUsage) -> String {
    let mut s = String::new();
    if let Some(p) = u.peak_mb.filter(|&p| p > 0) {
        s = format!("peak {}", gb(p));
        if let Some(cap) = u.cap_ram_mb.filter(|&c| c > 0) {
            s.push_str(&format!(" of {}", mem(cap)));
        }
    }
    if u.cpu_s > 0 {
        if !s.is_empty() {
            s.push_str(", ");
        }
        s.push_str(&match u.cpu_s {
            n if n < 60 => format!("{n}s CPU"),
            n => format!("{} CPU", feed::fmt_span(n as i64)),
        });
    }
    if u.oom_kills > 0 {
        if !s.is_empty() {
            s.push_str(", ");
        }
        s.push_str(&format!("OOM-killed x{}", u.oom_kills));
        // A later clean run or a bigger grant closed it.
        if !u.oom_open {
            s.push_str(" earlier");
        }
    }
    s
}

/// A Next up row waiting in the ledger, in its NOW cell: `queued for 3G
/// behind 899x`, `queued for CPU` — what it is short of, in its own sizes
/// ([`resources::short_need`], as `claim` says it), and who is really in
/// the way. Nothing named for a lease whose shortage is not known.
pub fn queued_note(l: &RowLease) -> String {
    let need = resources::short_need(&l.short, l.ram_mb, l.vram_mb);
    let mut s = "queued".to_string();
    if l.lapsed {
        // Its grant went untaken; nothing is held for it now.
        s.push_str(", grant lapsed, waiting for its manager");
        return s;
    }
    if !need.is_empty() {
        s.push_str(&format!(" for {need}"));
    }
    if let Some(b) = &l.behind {
        s.push_str(&format!(" behind {b}"));
    }
    s
}

/// [`queued_note`] in the row's hover: `queued — waiting for
/// 3G behind 899x in the machine ledger`.
fn queued_why(l: &RowLease) -> String {
    if l.lapsed {
        return "queued — its grant lapsed untaken; waiting for its manager".into();
    }
    let need = resources::short_need(&l.short, l.ram_mb, l.vram_mb);
    let mut s = "queued — waiting".to_string();
    if !need.is_empty() {
        s.push_str(&format!(" for {need}"));
    }
    if let Some(b) = &l.behind {
        s.push_str(&format!(" behind {b}"));
    }
    s.push_str(" in the machine ledger");
    s
}

/// What a row whose lease was granted on another session's call, and not
/// yet taken up, waits on.
pub const GRANTED_WAITING: &str = "granted, waiting for its manager";

/// The lease in full for the row's overlay header: `holds 3G, slot
/// cargo:/x`, or `queued #2 for …`.
fn lease_fact(l: &RowLease) -> String {
    match l.state {
        LeaseState::Queued => {
            let note = queued_note(l);
            match l.position {
                Some(p) => note.replacen("queued", &format!("queued #{p}"), 1),
                None => note,
            }
        }
        _ => {
            let mut s = format!(
                "holds {}",
                resources::describe_held(l.ram_mb, &l.gpus, l.vram_mb, &l.slots)
            );
            if l.state == LeaseState::Smaller
                && let Some(w) = l.wanted_ram_mb
            {
                s.push_str(&format!(" (asked {})", mem(w)));
            }
            if l.uncollected {
                s.push_str(&format!("; {GRANTED_WAITING}"));
            }
            s
        }
    }
}

// ------------------------------------------------------------- clicks ----

/// What a click on a row names — everything the action behind it
/// ([`crate::agent_open`]) could want, so deciding what to do never needs the pane again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowClick {
    pub stage: Stage,
    /// The feed's key (`acme#158`), else empty.
    pub key: String,
    /// The Claude Code subagent id, when a worker holds the row.
    pub agent_id: Option<String>,
    /// A Running row whose worker has finished — handed back, failed,
    /// killed or stopped — and nobody has landed it (NOW says `worker
    /// finished — not landed`). Claude Code has dropped that worker from
    /// its agent list, so there is no view to walk into: a click opens its
    /// transcript as a Done row's does.
    pub finished: bool,
    /// That finished worker is asleep on background shells of its own:
    /// Claude Code wakes it when one ends, so its transcript
    /// is not final and is followed as it grows.
    pub asleep: bool,
    /// The worker's name or description, for a tab title.
    pub name: String,
    /// The worker's `agent-<id>.jsonl`, once known.
    pub transcript: Option<PathBuf>,
    /// The feed's `open` command (Running/Done).
    pub open: Option<String>,
    /// The feed's `brief` (Planned).
    pub brief: Option<PathBuf>,
    /// The feed's `note`.
    pub note: Option<String>,
    /// The feed's `review` line (Done): shown at the overlay's top as is.
    pub review: Option<String>,
    /// The row's state in words, for the overlay's header: how it landed,
    /// how long it took against its estimate, its tokens.
    pub facts: Vec<String>,
}

impl RowClick {
    /// Whether `other` is a click on the same row: the feed's key when
    /// there is one, else the worker's id, else its name. The stage is left
    /// out — a row viewed while Running is still that row once Done.
    pub fn same_row(&self, other: &RowClick) -> bool {
        if !self.key.is_empty() || !other.key.is_empty() {
            return self.key == other.key;
        }
        if self.agent_id.is_some() || other.agent_id.is_some() {
            return self.agent_id == other.agent_id;
        }
        self.name == other.name
    }
}

/// The worker a tab shows: in Claude Code's own view, by its id; on
/// Giverny's page (`worker_page`), by its row, which a Next up
/// row with no worker yet is known by.
#[derive(Debug, Clone, Copy)]
pub enum Shown<'a> {
    Worker(&'a str),
    Row(&'a RowClick),
}

impl Shown<'_> {
    /// Whether `click` is the row of the worker shown.
    pub fn is(&self, click: &RowClick) -> bool {
        match self {
            Shown::Worker(id) => click.agent_id.as_deref() == Some(*id),
            Shown::Row(row) => row.same_row(click),
        }
    }
}

/// What a row click does: a click on the row the tab is
/// already showing takes it back to the manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toggle {
    /// Open the row: its worker's view, its overlay, its brief.
    Open,
    /// The tab shows (or is walking to) this row's worker: walk it home.
    Home,
    /// This row's overlay is up over the tab: close it.
    Close,
}

/// Decide a click on `click`, given the worker id the tab shows or is on
/// its way to (`viewed`) and the row whose overlay is up (`overlay`).
pub fn toggle(click: &RowClick, viewed: Option<&str>, overlay: Option<&RowClick>) -> Toggle {
    if overlay.is_some_and(|o| o.same_row(click)) {
        return Toggle::Close;
    }
    if viewed.is_some() && click.agent_id.as_deref() == viewed {
        return Toggle::Home;
    }
    Toggle::Open
}

// -------------------------------------------------------------- table ----

/// One drawn row, every cell already formatted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub stage: Stage,
    pub id: String,
    pub title: String,
    pub elapsed: String,
    pub eta: String,
    /// A Running row with no estimate: `eta` is empty, and the cell is
    /// drawn as a dim [`NO_ETA`].
    pub no_eta: bool,
    pub now: String,
    pub tokens: String,
    /// What the row's commands use ([`usage_cell`]): live on a Running row
    /// (zero between its commands), the memory peak on a Done one (zero when
    /// nothing was measured); empty on a Next up row.
    pub usage: String,
    /// How heavy a Running row's live CPU and memory are on the whole
    /// machine ([`Load`]), each drawn in its own colour over the cell's
    /// ([`usage_inks`]); `None` on any other row, whose cell is one colour.
    pub usage_load: Option<(Load, Load)>,
    /// A `giverny manage run` of the row was killed by its memory cap: the
    /// lease cell is drawn in the warning colour.
    pub oom: bool,
    /// A Running row the person has to know about ([`unworked`]): its
    /// worker stopped on an API error, or it has none. NOW says which, in
    /// the warning colour. A worker that finished is its manager's to land,
    /// not the person's ([`chores`]): its row is drawn as any Running row.
    pub flag: bool,
    /// When a Done row landed (epoch ms), when known; `None` on any other
    /// row.
    pub landed_ms: Option<u64>,
    /// The hover's lines under the title: where the row is
    /// and why, in words ([`running_why`], [`Wait::why`], [`done_why`]).
    pub why: Vec<String>,
    /// How deep under another row it hangs: 0 for a row of
    /// its own, 1 for an agent a worker spawned, 2 for one that agent
    /// spawned. Its title is drawn indented a step per level.
    pub depth: usize,
    /// The row's agent spawned agents of its own, drawn under it: whether
    /// they are folded away and how many there are.
    pub fold: Option<Fold>,
    pub click: RowClick,
}

/// A row whose agent spawned agents of its own: the toggle
/// on its row folds them away under it or shows them again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fold {
    /// What the pane keeps the fold by ([`fold_ident`]).
    pub ident: String,
    /// They are folded away.
    pub folded: bool,
    /// How many rows hang under it, every level down.
    pub under: usize,
    /// How many of those still run.
    pub running: usize,
}

impl Fold {
    /// The toggle drawn at the head of the title: `▸` while folded,
    /// `▾` while open.
    pub fn glyph(&self) -> char {
        if self.folded { '▸' } else { '▾' }
    }

    /// What a folded row says of what it hides: `+6`, `+6, 2 running`.
    /// Nothing while open.
    pub fn note(&self) -> String {
        if !self.folded {
            return String::new();
        }
        match self.running {
            0 => format!("+{}", self.under),
            r => format!("+{}, {r} running", self.under),
        }
    }

    /// [`Fold::note`] for a narrow row: just `+6`.
    pub fn short_note(&self) -> String {
        if self.folded {
            format!("+{}", self.under)
        } else {
            String::new()
        }
    }
}

impl Line {
    /// The row's tooltip: its whole title, then [`Line::why`].
    pub fn tip(&self) -> String {
        let head = if self.title.is_empty() {
            &self.id
        } else {
            &self.title
        };
        std::iter::once(head.as_str())
            .chain(self.why.iter().map(String::as_str))
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The whole table: rows and the feed's footer line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Table {
    pub lines: Vec<Line>,
    pub footer: Option<String>,
    /// Some row has agents hung under it ([`Line::fold`]): every
    /// top-level title leaves room for the toggle, so they stay aligned.
    pub nested: bool,
}

impl Table {
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
    fn any_running(&self) -> bool {
        self.lines.iter().any(|l| l.stage == Stage::Running)
    }
}

/// Merge and format with no limit ever seen. Pure: `now_ms` in, text
/// out.
#[cfg(test)]
pub fn build(feed: Option<&Feed>, live: &[SubagentRow], now_ms: u64) -> Table {
    build_at(feed, live, now_ms, &Clock::plain(), &Logs::new())
}

/// [`build`], with the clocks held through `clock`'s usage-limit spans and
/// each worker's transcript ([`Logs`]) to give a worker that took tasks one
/// after another a row per task.
#[cfg(test)]
pub fn build_at(
    feed: Option<&Feed>,
    live: &[SubagentRow],
    now_ms: u64,
    clock: &Clock,
    logs: &Logs,
) -> Table {
    build_with(
        feed,
        live,
        now_ms,
        clock,
        logs,
        DoneRows::All,
        Drawn::Every,
        &HashSet::new(),
    )
}

/// [`build_at`], showing only the Done rows `done` keeps
/// (`management_panel.done_rows`) and the rows `drawn` keeps.
/// Rows are dropped before they are formatted, so a `"` never stands for a
/// row that is not drawn.
///
/// An agent another drawn agent spawned hangs under that agent's row
/// ([`nest`]), unless that row is in `folded`.
#[allow(clippy::too_many_arguments)]
pub fn build_with(
    feed: Option<&Feed>,
    live: &[SubagentRow],
    now_ms: u64,
    clock: &Clock,
    logs: &Logs,
    done: DoneRows,
    drawn: Drawn,
    folded: &HashSet<String>,
) -> Table {
    with_rows(feed, live, logs, now_ms, |feed, rows| {
        // What a Next up row waits for is read off every row, drawn or not:
        // the task it was planned after may have no worker yet.
        let waits: Vec<Option<Wait>> = rows.iter().map(|r| planned_wait(r, &rows)).collect();
        let order = start_order(&rows, &waits);
        let mut kept: Vec<_> = rows
            .into_iter()
            .zip(waits)
            .zip(order)
            .filter(|((r, _), _)| drawn.keeps(r))
            .collect();
        // Next up in the order its rows will start, the next on top;
        // Running and Done as they were.
        kept.sort_by_key(|(_, o)| *o);
        let (rows, waits): (Vec<_>, Vec<_>) = kept.into_iter().map(|(rw, _)| rw).unzip();
        let nested = nest(&rows, done, folded);
        let mut slots: Vec<Option<_>> = rows.into_iter().zip(waits).map(Some).collect();
        let (mut rows, waits): (Vec<_>, Vec<_>) =
            nested.iter().filter_map(|n| slots[n.at].take()).unzip();
        reditto(&mut rows);
        let written = feed.and_then(Feed::started_true_at);
        let mut lines: Vec<Line> = Vec::with_capacity(rows.len());
        for ((row, wait), n) in rows.iter().zip(waits).zip(&nested) {
            let mut line = format_row(row, now_ms, written, clock, wait);
            line.depth = n.depth;
            line.fold = n.fold.clone();
            if let Some(f) = &n.fold {
                line.why.push(fold_why(f));
            }
            if let Some(p) = &n.parent {
                line.why.push(format!("spawned by {p}"));
            }
            lines.push(line);
        }
        dittos(&rows, &mut lines);
        Table {
            nested: lines.iter().any(|l| l.fold.is_some()),
            lines,
            footer: feed.and_then(|f| f.footer.clone()),
        }
    })
}

/// One row as [`nest`] places it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Nested {
    /// Its place among the rows handed to [`nest`].
    at: usize,
    depth: usize,
    fold: Option<Fold>,
    /// The row it hangs under, by name, for its hover.
    parent: Option<String>,
}

/// What a row's fold is kept by: its feed key, else its agent's id — as
/// [`done_ident`] knows a row, so a worker's fold holds from Running to
/// Done.
fn fold_ident(row: &PaneRow<'_, SubagentRow>) -> String {
    match (
        row.feed.map(|f| f.key.as_str()).filter(|k| !k.is_empty()),
        row.agent_id(),
    ) {
        (Some(k), _) => format!("k:{k}"),
        (None, Some(a)) => format!("a:{a}"),
        (None, None) => String::new(),
    }
}

/// A row by name, for a hover: its feed key, else its agent's name.
fn row_name(row: &PaneRow<'_, SubagentRow>) -> String {
    row.feed
        .map(|f| f.key.clone())
        .filter(|k| !k.is_empty())
        .or_else(|| row.live.map(|l| l.display_name().to_string()))
        .unwrap_or_default()
}

/// The row each row hangs under, by place: the drawn row of the agent that
/// spawned it ([`SubagentRow::parent_id`]). A worker that holds several
/// rows (tasks one after another) takes it under the one whose task was
/// under way when it spawned it — the latest to start no later — else its
/// first. An agent whose spawner holds no row stands on its own, and so
/// does one caught in a loop of parents (never seen; never trusted).
fn parent_rows(rows: &[PaneRow<'_, SubagentRow>]) -> Vec<Option<usize>> {
    let mut up: Vec<Option<usize>> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let l = r.live?;
            let p = l.parent_id.as_deref()?;
            let holds = |(j, o): &(usize, &PaneRow<'_, SubagentRow>)| {
                *j != i && o.stage != Stage::Planned && o.agent_id() == Some(p)
            };
            let under_way = rows
                .iter()
                .enumerate()
                .filter(holds)
                .filter(|(_, o)| match (o.started_ms(), l.started_ms) {
                    (Some(a), Some(b)) => a <= b,
                    _ => false,
                })
                .max_by_key(|(_, o)| o.started_ms());
            under_way
                .or_else(|| rows.iter().enumerate().find(holds))
                .map(|(j, _)| j)
        })
        .collect();
    for i in 0..up.len() {
        let mut at = up[i];
        let mut steps = 0;
        while let Some(j) = at {
            if j == i || steps > up.len() {
                up[i] = None;
                break;
            }
            at = up[j];
            steps += 1;
        }
    }
    up
}

/// Place the rows as the pane draws them: each agent another
/// drawn agent spawned right under its spawner's row, a level deeper, in
/// the order the rows came; its spawner's Running or Done row carries it
/// wherever that row is. A row in `folded` hides everything under it and
/// says how much ([`Fold`]).
///
/// `done` keeps the newest Done rows of their own (`Last(n)` counts only
/// those) and what hangs under them. What hung under a Done row that is
/// not kept goes with it, but an agent still running there stands on its
/// own instead: a running agent is never hidden by the Done setting.
/// `Hide` hides every Done row, nested or not.
fn nest(
    rows: &[PaneRow<'_, SubagentRow>],
    done: DoneRows,
    folded: &HashSet<String>,
) -> Vec<Nested> {
    let n = rows.len();
    let mut up = parent_rows(rows);
    let level = |up: &[Option<usize>], i: usize| {
        let (mut d, mut at) = (0, up[i]);
        while let Some(j) = at {
            d += 1;
            at = up[j];
        }
        d
    };
    let mut dropped = vec![false; n];
    if done != DoneRows::All {
        let keep = match done {
            DoneRows::Last(k) => k,
            _ => 0,
        };
        let mut seen = 0;
        for i in 0..n {
            if rows[i].stage != Stage::Done {
                continue;
            }
            if done == DoneRows::Hide {
                dropped[i] = true;
            } else if up[i].is_none() {
                seen += 1;
                dropped[i] = seen > keep;
            }
        }
        // Shallowest first, so a row's spawner is settled before it is.
        let mut by_level: Vec<usize> = (0..n).collect();
        let levels: Vec<usize> = (0..n).map(|i| level(&up, i)).collect();
        by_level.sort_by_key(|&i| levels[i]);
        for i in by_level {
            if let Some(p) = up[i]
                && dropped[p]
            {
                if rows[i].stage == Stage::Done {
                    dropped[i] = true;
                } else {
                    up[i] = None;
                }
            }
        }
    }
    let mut kids: Vec<Vec<usize>> = vec![Vec::new(); n];
    for i in (0..n).filter(|&i| !dropped[i]) {
        if let Some(p) = up[i] {
            kids[p].push(i);
        }
    }
    // Everything under a row, every level down, and how much of it runs.
    fn under(i: usize, kids: &[Vec<usize>], rows: &[PaneRow<'_, SubagentRow>]) -> (usize, usize) {
        kids[i].iter().fold((0, 0), |(all, run), &k| {
            let (a, r) = under(k, kids, rows);
            let me = usize::from(rows[k].stage == Stage::Running);
            (all + 1 + a, run + me + r)
        })
    }
    #[allow(clippy::too_many_arguments)]
    fn place(
        i: usize,
        depth: usize,
        parent: Option<String>,
        kids: &[Vec<usize>],
        rows: &[PaneRow<'_, SubagentRow>],
        folded: &HashSet<String>,
        out: &mut Vec<Nested>,
    ) {
        let fold = (!kids[i].is_empty()).then(|| {
            let (under, running) = under(i, kids, rows);
            let ident = fold_ident(&rows[i]);
            Fold {
                folded: folded.contains(&ident),
                ident,
                under,
                running,
            }
        });
        let open = fold.as_ref().is_none_or(|f| !f.folded);
        out.push(Nested {
            at: i,
            depth,
            fold,
            parent,
        });
        if open {
            for &k in &kids[i] {
                place(
                    k,
                    depth + 1,
                    Some(row_name(&rows[i])),
                    kids,
                    rows,
                    folded,
                    out,
                );
            }
        }
    }
    let mut out = Vec::with_capacity(n);
    for i in (0..n).filter(|&i| !dropped[i] && up[i].is_none()) {
        place(i, 0, None, &kids, rows, folded, &mut out);
    }
    out
}

/// A row with agents under it, in its hover: how many, how many run, and
/// what the toggle does.
fn fold_why(f: &Fold) -> String {
    let agents = if f.under == 1 { "agent" } else { "agents" };
    let running = match f.running {
        0 => String::new(),
        r => format!(", {r} running"),
    };
    if f.folded {
        format!(
            "{} {agents} of its own folded under it{running} — ▸ shows them",
            f.under
        )
    } else {
        format!(
            "{} {agents} of its own under it{running} — ▾ folds them",
            f.under
        )
    }
}

/// The pane's rows, as it places them at `now_ms`: the feed merged with
/// the live rows, and the Running rows nobody works on moved to Next up
/// ([`benched`]): every row, drawn or not ([`Drawn`] says which are). A
/// row is a worker's only by the feed (`start <task> --agent <worker>`) or
/// the worker's spawn: no message to a worker hands it a row.
/// `f` gets the feed it was merged from.
fn with_rows<T>(
    feed: Option<&Feed>,
    live: &[SubagentRow],
    logs: &Logs,
    now_ms: u64,
    f: impl FnOnce(Option<&Feed>, Vec<PaneRow<'_, SubagentRow>>) -> T,
) -> T {
    let log = |id: &str| logs.get(id);
    let mut rows = feed::merge_with(feed, live, log);
    bench(&mut rows, now_ms);
    f(feed, rows)
}

/// A Running row nobody works on: no worker at all once a
/// spawn would have come ([`Unworked::NoWorker`]); one its manager paused
/// (or held) with no worker on it, at once; or a worker that handed it back
/// and a row its manager paused or held rather than landed. It is
/// drawn under Next up, its NOW saying why ([`planned_wait`]): `held: …`,
/// `paused since …`, [`NO_WORKER_YET`]. A worker stopped on an API error, or
/// one that failed or was killed, keeps its row Running, for the person to
/// see. The feed still says Running: a worker picking it up moves it back.
fn benched(row: &PaneRow<'_, SubagentRow>, now_ms: u64) -> bool {
    let Some(f) = row.feed.filter(|_| row.stage == Stage::Running) else {
        return false;
    };
    // Killed with its session: it stays Running, reading `killed`, until
    // its manager revives it.
    if killed(row) {
        return false;
    }
    let set_aside = f.held.is_some() || f.paused_since_ms.is_some();
    match unworked(row, now_ms) {
        Some(Unworked::NoWorker { .. }) => true,
        Some(Unworked::Finished {
            how: "finished", ..
        }) => set_aside,
        // Just started or handed on, no worker on it yet: set aside, none
        // is to come.
        None => set_aside && f.live.is_none() && row.live.is_none_or(|l| !l.running()),
        _ => false,
    }
}

/// Move the [`benched`] rows to Next up, keeping every other row where it
/// was, and ditto again ([`PaneRow::ditto`]) where rows moved. Where in Next
/// up a benched row is drawn is its own wait's call ([`start_order`]).
fn bench(rows: &mut [PaneRow<'_, SubagentRow>], now_ms: u64) {
    let mut moved = false;
    for row in rows.iter_mut() {
        if benched(row, now_ms) {
            row.stage = Stage::Planned;
            moved = true;
        }
    }
    if !moved {
        return;
    }
    // Stable: every other row keeps its place.
    rows.sort_by_key(|r| r.stage);
    reditto(rows);
}

/// Ditto again ([`PaneRow::ditto`]) where rows moved or went: a row is a
/// ditto of the one now directly above it, the same worker's same batch.
fn reditto(rows: &mut [PaneRow<'_, SubagentRow>]) {
    for i in 0..rows.len() {
        let same = i > 0
            && match (rows[i - 1].agent_id(), rows[i].agent_id()) {
                (Some(a), Some(b)) => a == b && rows[i - 1].batch == rows[i].batch,
                _ => false,
            };
        rows[i].ditto = same;
    }
}

/// The chores of [`chores`] that are due at `now_ms` and that the feed's
/// rows were not told of yet: what the manager is to hear.
pub fn due_chores(
    drawn: Drawn,
    feed: &Feed,
    live: &[SubagentRow],
    logs: &Logs,
    shells: &Shells,
    now_ms: u64,
) -> Vec<Chore> {
    chores(drawn, Some(feed), live, logs, shells, now_ms)
        .into_iter()
        .filter(|c| c.is_due(now_ms))
        .filter(|c| !feed.rows.iter().any(|r| r.key == c.key && c.told(r)))
        .collect()
}

/// What the rows ask of their manager rather than of the person:
/// a Running row whose worker has finished, unlanded
/// ([`Unworked::Finished`]), drawn under Next up when its manager paused it;
/// a Next up row waiting on nothing recorded
/// ([`Wait::idle_since`]); and a Running row with no worker at all, drawn
/// under Next up ([`Unworked::NoWorker`]) unless its manager
/// paused or held it — the very rows the pane once drew in amber, from the
/// same logic. Every one, due or not, told or not:
/// [`manage_chores::Chore::is_due`] and [`manage_chores::Chore::told`] say
/// which to keep.
///
/// Only of the rows the pane draws (`drawn`): nothing drawn,
/// nothing to nag about. A session that is no manager ([`Drawn::Agents`])
/// never draws a row with no worker, so it never hears "no worker"; a
/// manager still does, as its Next up draws such a row: one that ran
/// `start` and forgot to spawn is the one it catches.
pub fn chores(
    drawn: Drawn,
    feed: Option<&Feed>,
    live: &[SubagentRow],
    logs: &Logs,
    shells: &Shells,
    now_ms: u64,
) -> Vec<Chore> {
    with_rows(feed, live, logs, now_ms, |_, rows| {
        let mut out = Vec::new();
        for row in rows.iter().filter(|r| drawn.keeps(r)) {
            let Some(key) = row.feed.map(|f| f.key.clone()) else {
                continue;
            };
            let chore = match row.stage {
                // Asleep on its own shell: Claude Code wakes it, nothing to
                // land.
                Stage::Running if asleep_on(row, shells, now_ms).is_some() => None,
                // Killed with its session: its worker never finished, and
                // the manager heard it on its comeback.
                Stage::Running if killed(row) => None,
                Stage::Running => match unworked(row, now_ms) {
                    Some(Unworked::Finished { how, ended_ms }) => ended_ms.map(|since| Chore {
                        key,
                        kind: manage_chores::Kind::Land,
                        since,
                        how: Some(how.to_string()),
                    }),
                    _ => None,
                },
                // A Running row benched for want of a worker: spawn or hand
                // it one, or set it aside. One its manager paused or held is
                // set aside already. A paused one whose worker finished,
                // before the pause or during it, is set aside with its work
                // done: land it or resume it.
                Stage::Planned if row.feed.is_some_and(|f| f.stage() == Stage::Running) => {
                    let paused = row.feed.is_some_and(|f| f.paused_since_ms.is_some());
                    let set_aside = paused || row.feed.is_some_and(|f| f.held.is_some());
                    match unworked(row, now_ms) {
                        Some(Unworked::NoWorker { since }) if !set_aside => Some(Chore {
                            key,
                            kind: manage_chores::Kind::Worker,
                            since,
                            how: None,
                        }),
                        // Ended after the pause began: it stopped for it, and
                        // the task is now really paused, told at once.
                        Some(Unworked::Finished { how, ended_ms }) if paused => {
                            let kind = if stopped_for_pause(row, ended_ms) {
                                manage_chores::Kind::NowPaused
                            } else {
                                manage_chores::Kind::LandPaused
                            };
                            ended_ms.map(|since| Chore {
                                key,
                                kind,
                                since,
                                how: Some(how.to_string()),
                            })
                        }
                        _ => None,
                    }
                }
                Stage::Planned => {
                    planned_wait(row, &rows)
                        .and_then(|w| w.idle_since)
                        .map(|since| Chore {
                            key,
                            kind: manage_chores::Kind::Start,
                            since,
                            how: None,
                        })
                }
                Stage::Done => None,
            };
            out.extend(chore);
        }
        out
    })
}

/// Whether a paused row's worker ended its turn (`ended_ms`) after the
/// pause began: it stopped for the pause, and the task is now really
/// paused, rather than its work done before it.
fn stopped_for_pause(row: &PaneRow<'_, SubagentRow>, ended_ms: Option<u64>) -> bool {
    match (row.feed.and_then(|f| f.paused_since_ms), ended_ms) {
        (Some(p), Some(e)) => e >= p,
        _ => false,
    }
}

/// The keys of the paused rows whose workers are still working:
/// Running in the pane, drawn as [`PAUSED_FINISHING`]. What
/// tells a manager whether every task it paused has stopped.
pub fn finishing(
    drawn: Drawn,
    feed: &Feed,
    live: &[SubagentRow],
    logs: &Logs,
    now_ms: u64,
) -> Vec<String> {
    with_rows(Some(feed), live, logs, now_ms, |_, rows| {
        rows.iter()
            .filter(|r| drawn.keeps(r) && still_finishing(r))
            .filter_map(|r| r.feed.map(|f| f.key.clone()))
            .collect()
    })
}

/// A Running row its manager paused whose worker is still at work: only its
/// clock is paused.
fn still_finishing(row: &PaneRow<'_, SubagentRow>) -> bool {
    row.stage == Stage::Running
        && row.feed.is_some_and(|f| f.paused_since_ms.is_some())
        && row.live.is_some_and(|l| l.running())
        && !killed(row)
}

/// A Running row whose worker was killed with its session: Giverny, WSL or
/// the manager session went off. Its clock stands stopped
/// where the session last showed life until its manager revives it
/// (`resume`): it reads [`KILLED`].
///
/// A row its manager resumed still holding that worker reads it too, its
/// clock running: the worker is dead, though a tracker restored over the
/// restart may still list it running.
fn killed(row: &PaneRow<'_, SubagentRow>) -> bool {
    let Some(f) = row.feed.filter(|_| row.stage == Stage::Running) else {
        return false;
    };
    (f.killed_ms.is_some() && f.paused_since_ms.is_some())
        || row.live.is_some_and(|l| feed::died_with_session(f, l))
}

/// A cell is `"` when its row is the same worker as the row above
/// ([`PaneRow::ditto`]) and its text is that row's text — compared before
/// either was replaced, so a run of three dittos all the way down.
fn dittos(rows: &[PaneRow<'_, SubagentRow>], lines: &mut [Line]) {
    let raw: Vec<[String; 4]> = lines
        .iter()
        .map(|l| {
            [
                l.elapsed.clone(),
                l.eta.clone(),
                l.now.clone(),
                l.tokens.clone(),
            ]
        })
        .collect();
    for i in 1..lines.len() {
        if !rows[i].ditto {
            continue;
        }
        let out = &mut lines[i];
        let cells = [
            &mut out.elapsed,
            &mut out.eta,
            &mut out.now,
            &mut out.tokens,
        ];
        for (k, cell) in cells.into_iter().enumerate() {
            if !raw[i][k].is_empty() && raw[i][k] == raw[i - 1][k] {
                *cell = "\"".into();
            }
        }
    }
}

/// Where a Running row's clock stands: `now`, or for a row paused in the
/// feed, the instant its `started` is true as of — the feed's write time
/// (a writer that pauses moves `started` on by the open pause up to then), never before the
/// pause began nor after `now`.
fn clock_stop(paused_since: Option<u64>, written: Option<u64>, now_ms: u64) -> u64 {
    match paused_since {
        Some(p) => written.map_or(p, |w| w.max(p)).min(now_ms),
        None => now_ms,
    }
}

fn format_row(
    row: &PaneRow<'_, SubagentRow>,
    now_ms: u64,
    written: Option<u64>,
    clock: &Clock,
    wait: Option<Wait>,
) -> Line {
    let f = row.feed;
    let l = row.live;
    let key = f.map(|f| f.key.clone()).unwrap_or_default();
    // A feed row's key and title; a live-only row's name and description.
    let (id, title) = match (f, l) {
        (Some(f), _) => (
            Some(shown_key(f))
                .filter(|k| !k.is_empty())
                .or_else(|| l.and_then(|l| l.model.as_deref()).and_then(short_model))
                .unwrap_or_default(),
            f.title
                .clone()
                .or_else(|| l.map(|l| l.display_name().to_string()))
                .unwrap_or_default(),
        ),
        (None, Some(l)) => match (&l.name, &l.description) {
            (Some(n), Some(d)) => (n.clone(), d.clone()),
            _ => (
                l.model.as_deref().and_then(short_model).unwrap_or_default(),
                l.display_name().to_string(),
            ),
        },
        (None, None) => (String::new(), String::new()),
    };
    // This row's own start: the feed's (a worker holding several tasks
    // started each at a different time, and a pausing writer moves it on by the row's
    // pauses), else the worker's.
    let row_start = row.started_ms();
    let paused = row.paused_since_ms();
    // A Running row drawn under Next up ([`benched`]) that its manager paused.
    let benched_pause = f
        .filter(|f| row.stage == Stage::Planned && f.stage() == Stage::Running)
        .and_then(|f| f.paused_since_ms);
    // A feed row that carries its pauses has its stops taken off `started`
    // already (a writer may also pause rows for a limit itself): the
    // pane's own holds would take them off twice.
    let writer_pauses = f.is_some_and(|f| f.paused_s.is_some() || f.paused_since_ms.is_some());
    let holds = if writer_pauses { &[][..] } else { clock.holds };
    let slept_ms = |s: u64, e: u64| sleeps::slept_ms(clock.sleeps, s, e);
    // A Running row's work so far, in seconds: wall time up to where its
    // clock stands, less every span the account was out of its limit or
    // the machine slept.
    // A worker stopped on an API error, or ended badly under a Running
    // row, stands still from then, and its error spans are off its clock.
    let stopped = l
        .filter(|_| row.stage == Stage::Running)
        .and_then(|l| l.stopped());
    let error_ms = |s: u64, e: u64| l.map_or(0, |l| l.stopped_ms(s, e));
    // Where a Running row's clock stands now.
    let stop = match (row.stage, row_start) {
        (Stage::Running, Some(s)) => {
            let mut stop = clock_stop(paused, written, now_ms);
            if let Some((since, _)) = &stopped {
                stop = stop.min((*since).max(s));
            }
            Some(stop)
        }
        _ => None,
    };
    // The work in `[from, stop]`, in seconds: its wall time less every span
    // the account was out of its limit, the machine slept or the worker
    // stood on an error.
    let work_since = |from: u64, stop: u64| {
        let from = from.min(stop);
        stop.saturating_sub(from)
            .saturating_sub(off_ms(holds, clock.sleeps, from, stop))
            .saturating_sub(error_ms(from, stop))
            / 1000
    };
    let work_s = row_start.zip(stop).map(|(s, stop)| work_since(s, stop));
    let elapsed = match row.stage {
        Stage::Planned => String::new(),
        Stage::Running => work_s.map(stopwatch).unwrap_or_default(),
        Stage::Done => {
            let end = row.ended_ms();
            match (row_start, end) {
                (Some(s), Some(e)) => stopwatch(
                    e.saturating_sub(s)
                        .saturating_sub(error_ms(s, e))
                        .saturating_sub(slept_ms(s, e))
                        / 1000,
                ),
                _ => String::new(),
            }
        }
    };
    // A worker no feed row carries: its agent ETA, counted from its start.
    let agent = l
        .filter(|_| f.is_none())
        .and_then(|l| clock.etas.get(&l.id));
    let agent_eta = agent.zip(row_start).map(|(e, s)| e.total_s(s));
    let eta_s = f.and_then(|f| f.eta_s).or(agent_eta);
    // The latest figure given mid-run, and when: a re-estimated feed row's,
    // else the agent ETA's, never before the row's clock starts.
    let figure = row
        .last_figure()
        .or_else(|| agent.map(|e| (row_start.map_or(e.at_ms, |s| e.at_ms.max(s)), e.left_s)));
    // A Done row's ± and the figure it was measured against:
    // the feed row's ([`feed::PaneRow::eta_delta_s`]), else an agent ETA's
    // first figure, by the work since it. Its later figures are the
    // worker's, but which came first from it is not kept.
    let delta = match row.stage {
        Stage::Done => row.eta_delta_s(&slept_ms).or_else(|| {
            let e = agent?;
            let at = row_start.map_or(e.first_at_ms, |s| e.first_at_ms.max(s));
            let end = row.ended_ms()?.max(at);
            let work_s = (end - at).saturating_sub(slept_ms(at, end)) / 1000;
            feed::done_delta(work_s, None, Some(e.first_left_s))
        }),
        _ => None,
    };
    // Re-estimated: the worker's latest figure less the work since it
    // was given. Off-clock spans before it are in neither,
    // those after still come off; `eta_s` less the work since the start
    // took the ones before it off twice, being wall time up to the figure.
    let left_s = match (row.stage, figure, stop, eta_s, work_s) {
        (Stage::Running, Some((at, left)), Some(stop), _, _) => {
            Some(left as i64 - work_since(at, stop) as i64)
        }
        (Stage::Running, _, _, Some(eta), Some(work)) => Some(eta as i64 - work as i64),
        _ => None,
    };
    let eta = match row.stage {
        Stage::Running => left_s.map(countdown).unwrap_or_default(),
        Stage::Planned => eta_s
            .map(|e| format!("~{}", feed::fmt_span(e as i64)))
            .unwrap_or_default(),
        Stage::Done => delta.map(|(d, _)| feed::fmt_delta(d)).unwrap_or_default(),
    };
    let no_eta = row.stage == Stage::Running && eta_s.is_none();
    // What the task holds in the machine ledger is in the overlay header; a
    // Next up row that waits for it says so in NOW.
    let held = f
        .and_then(|f| f.lease.as_ref())
        .filter(|_| row.stage != Stage::Done);
    // The cell is the row's use: a Running row's commands now,
    // a Done row's memory peak alone.
    let measured = f.and_then(|f| f.usage.as_ref());
    let oom = measured.is_some_and(|u| u.oom_open) && row.stage != Stage::Planned;
    // Running and Done rows always have a cell, zero when nothing is going
    // or was measured; Next up rows say what they wait for in NOW instead.
    let peak = measured.and_then(|u| u.peak_mb);
    let gpu = session_use::gpu::present();
    let live = (row.stage == Stage::Running).then(|| {
        f.and_then(|f| f.live)
            .or_else(|| row.agent_id().and_then(|id| clock.workers.get(id)).copied())
            .unwrap_or_default()
    });
    let usage = match row.stage {
        Stage::Running => usage_cell(live, None, oom, gpu),
        Stage::Done => usage_cell(None, peak, oom, gpu),
        Stage::Planned => String::new(),
    };
    let usage_load = live.map(|l| run_load(l, sessions_load::machine_ram_mb()));
    let limit = clock.limit.filter(|l| l.out_at(now_ms));
    // Only what the person has to know is flagged: a worker stopped on an
    // API error, a row nothing runs. A finished worker's row and an idle
    // Next up row are their manager's ([`chores`]), drawn plain.
    // The worker's open wait (`eta --why wait`) and what it said of it.
    let waiting = f
        .filter(|_| row.stage == Stage::Running)
        .and_then(|f| f.waiting_since_ms.map(|w| (w, f.note.as_deref())));
    let flag = match row.stage {
        Stage::Running => unworked(row, now_ms).and_then(Unworked::flag),
        Stage::Planned | Stage::Done => None,
    };
    // Its worker's turn ended with shells of its own still running: asleep.
    let asleep = (row.stage == Stage::Running)
        .then(|| asleep_on(row, clock.shells, now_ms))
        .flatten();
    let now = match row.stage {
        // What is not running comes first: a Running row whose worker
        // stopped on an error, or that has none, says so before anything
        // else.
        // Killed with its session first: nothing works on it, and only its
        // manager revives it.
        Stage::Running if killed(row) => KILLED.to_string(),
        Stage::Running if flag.is_some() => flag.clone().unwrap_or_default(),
        // The limit next: a row the writer paused for it says why.
        Stage::Running => match (limit, paused) {
            (Some(limit), _) => limit_note(limit, now_ms, &clock.tz),
            // Paused, its worker still at work: not paused until it ends
            // its turn.
            (None, Some(_)) if still_finishing(row) => PAUSED_FINISHING.to_string(),
            (None, Some(p)) => format!("paused since {}", clock_at(p, now_ms, &clock.tz)),
            (None, None) if stopped.is_some() => stopped.map(|(_, why)| why).unwrap_or_default(),
            // Its worker ended its turn: when, and the wait it marked, so
            // the row does not read as work going on.
            (None, None) => match unworked(row, now_ms) {
                // Asleep on its own work, not ended.
                Some(Unworked::Finished { .. }) if asleep.is_some() => {
                    asleep_now(asleep.unwrap_or_default(), waiting, now_ms)
                }
                Some(Unworked::Finished { ended_ms, .. }) => {
                    turn_ended(ended_ms, waiting, now_ms, &clock.tz)
                }
                _ => l
                    .filter(|l| l.running())
                    .and_then(|l| l.activity.clone())
                    .unwrap_or_default(),
            },
        },
        // What it waits for ([`planned_wait`]); nothing when it waits on
        // nothing recorded. A Running row benched while paused: since when,
        // or that its worker finished, when it has: `paused
        // since …` alone read as work not yet begun, when it was work done
        // and waiting to be landed.
        Stage::Planned => {
            match benched_pause.filter(|_| wait.as_ref().is_none_or(|w| w.held.is_none())) {
                // Its worker ended its turn before the pause: work done.
                // After it, it stopped for the pause.
                Some(_)
                    if matches!(unworked(row, now_ms),
                        Some(Unworked::Finished { ended_ms, .. })
                            if !stopped_for_pause(row, ended_ms)) =>
                {
                    PAUSED_FINISHED.to_string()
                }
                Some(p) => format!("paused since {}", clock_at(p, now_ms, &clock.tz)),
                None => wait.as_ref().map(|w| w.note.clone()).unwrap_or_default(),
            }
        }
        Stage::Done => f
            .and_then(|f| f.landing.clone())
            .or_else(|| row.next_key.as_ref().map(|k| format!("→ {k}")))
            .or_else(|| l.and_then(|l| l.outcome).map(outcome_word))
            .unwrap_or_default(),
    };
    let tokens = row.tokens().map(fmt_tokens).unwrap_or_default();
    let vs = delta.map_or_else(|| "estimate".into(), |(_, a)| a.label());
    let mut facts = row_facts(row.stage, &elapsed, &eta, &vs, &now, &tokens);
    if no_eta {
        facts.insert(
            1.min(facts.len()),
            no_eta_hint(&key, l.map(|l| l.agent_id())),
        );
    }
    if let Some(why) = wait.as_ref().and_then(|w| w.held.as_deref()) {
        facts.push(format!("held: {why}"));
    }
    if let Some(h) = held {
        facts.push(lease_fact(h));
    }
    if let Some(u) = measured.filter(|_| row.stage != Stage::Planned) {
        let fact = usage_fact(u);
        if !fact.is_empty() {
            facts.push(fact);
        }
    }
    // The hover's lines under the title: where the row is and why,
    // from the cells above before any ditto.
    let note = f.and_then(|f| f.note.as_deref());
    let why = match row.stage {
        Stage::Running => {
            let who = match l {
                Some(l) => {
                    let mut w = format!("worker {}", l.display_name());
                    if let Some(m) = l.model.as_deref().and_then(short_model) {
                        w.push_str(&format!(" ({m})"));
                    }
                    w
                }
                None if f.is_some_and(|f| f.live.is_some()) => {
                    "no worker; its own commands run".into()
                }
                None => "no worker on it".into(),
            };
            let since = f
                .and_then(|f| f.spawned_ms)
                .or(row_start)
                .map(|s| clock_at(s, now_ms, &clock.tz));
            let state =
                match unworked(row, now_ms) {
                    _ if killed(row) => Some(killed_why(row, now_ms, &clock.tz)),
                    Some(Unworked::Stopped(r)) => {
                        Some(format!("stopped: {r} — its worker is not working"))
                    }
                    Some(Unworked::Finished { ended_ms, .. }) if asleep.is_some() => Some(
                        asleep_why(asleep.unwrap_or_default(), ended_ms, now_ms, &clock.tz),
                    ),
                    Some(Unworked::Finished { how, ended_ms }) => Some(match ended_ms {
                        Some(e) => format!(
                            "its worker {how} at {} — not landed yet; nothing works on it \
                         until its manager wakes the worker or lands it",
                            clock_at(e, now_ms, &clock.tz)
                        ),
                        None => format!("its worker {how} — not landed yet"),
                    }),
                    _ => None,
                }
                .or_else(|| {
                    limit.map(|lim| {
                        format!(
                            "{} — out of usage; its clock is held",
                            limit_note(lim, now_ms, &clock.tz)
                        )
                    })
                })
                .or_else(|| {
                    paused.map(|p| {
                        let since = clock_at(p, now_ms, &clock.tz);
                        if still_finishing(row) {
                            format!(
                                "paused since {since}, its worker still finishing its turn: \
                                 only its clock is paused until it stops"
                            )
                        } else {
                            paused_why(&since, note)
                        }
                    })
                });
            // The worker's open wait, in full: NOW cuts it.
            let waited = waiting
                .filter(|_| paused.is_none())
                .map(|(w, n)| waiting_why(w, n, now_ms, &clock.tz));
            // What the worker is doing right now, whole: NOW cuts it to fit.
            let doing = l
                .filter(|l| l.running())
                .and_then(|l| l.activity.clone())
                .filter(|a| !a.is_empty());
            doing
                .into_iter()
                .chain(running_why(&who, since.as_deref(), eta_s, left_s, state))
                .chain(waited)
                .collect()
        }
        Stage::Planned => {
            let line =
                match benched_pause.filter(|_| wait.as_ref().is_none_or(|w| w.held.is_none())) {
                    Some(_) if now == PAUSED_FINISHED => {
                        format!("{PAUSED_FINISHED} — its work is done, to land or resume")
                    }
                    Some(p) => paused_why(&clock_at(p, now_ms, &clock.tz), note),
                    None => wait.as_ref().map(|w| w.why.clone()).unwrap_or_default(),
                };
            let est = (!eta.is_empty()).then(|| format!("estimate {eta}"));
            [Some(line).filter(|l| !l.is_empty()), est]
                .into_iter()
                .flatten()
                .collect()
        }
        Stage::Done => done_why(&now, &elapsed, delta, f.and_then(|f| f.review.as_deref())),
    };
    Line {
        stage: row.stage,
        id,
        title,
        why,
        elapsed,
        eta,
        no_eta,
        now,
        tokens,
        usage,
        usage_load,
        oom,
        flag: flag.is_some(),
        landed_ms: row.ended_ms().filter(|_| row.stage == Stage::Done),
        depth: 0,
        fold: None,
        click: RowClick {
            stage: row.stage,
            key,
            // The worker drawn: a fresh one that took the row from a
            // finished or killed one is the one to open and
            // type to, not the id the feed still names.
            agent_id: l
                .map(|l| l.id.clone())
                .or_else(|| row.agent_id().map(str::to_string)),
            finished: row.stage == Stage::Running && l.is_some_and(|l| !l.running()),
            asleep: asleep.is_some(),
            name: l
                .map(|l| l.display_name().to_string())
                .or_else(|| f.and_then(|f| f.title.clone()))
                .unwrap_or_default(),
            transcript: l.and_then(|l| l.transcript.clone()),
            open: f.and_then(|f| f.open.clone()),
            brief: f.and_then(|f| f.brief.clone()),
            note: f.and_then(|f| f.note.clone()),
            review: f.and_then(|f| f.review.clone()),
            facts,
        },
    }
}

/// A Running row nothing is running.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Unworked {
    /// Its worker ended on an API error it never wrote past: that error.
    Stopped(String),
    /// Its worker finished — handed back, failed, killed or stopped
    /// (`how`) — at `ended_ms`, and nobody has landed the row.
    Finished {
        how: &'static str,
        ended_ms: Option<u64>,
    },
    /// No worker at all, and no command of its own runs: since it was
    /// handed out (started, or handed to a worker that never picked it up).
    NoWorker { since: u64 },
}

impl Unworked {
    /// What the pane flags of it, in NOW: the person's to know. A finished
    /// worker is its manager's to land, a row with no worker its manager's
    /// to staff ([`chores`]): never flagged.
    fn flag(self) -> Option<String> {
        match self {
            Unworked::Stopped(reason) => Some(format!("stopped: {reason}")),
            Unworked::NoWorker { .. } | Unworked::Finished { .. } => None,
        }
    }
}

/// Why nothing is running a Running row: the feed says the task runs, but
/// its worker has finished — handed back, failed, killed or stopped (on an
/// API error, that error) — and nobody has landed it; or it has no worker
/// at all, [`feed::HANDOFF_WINDOW_MS`] after it started (a dispatcher runs
/// `start` a moment before the spawn), and no command of its own runs
/// either. A worker counts as finished only if it ended after the row was
/// handed to it ([`PaneRow::handed_at_ms`]); one that ended before is
/// still to pick the row up, and is [`Unworked::NoWorker`] only once
/// [`feed::HANDOFF_WINDOW_MS`] has passed without it doing so. `None` while
/// a worker works on it.
fn unworked(row: &PaneRow<'_, SubagentRow>, now_ms: u64) -> Option<Unworked> {
    if let Some(l) = row.live {
        if l.running() {
            return None;
        }
        let ended_ms = l.ended_ms.or(l.last_turn_ms);
        // An idle worker handed this task (`start --agent`, a message) and
        // not woken yet ended its previous task, not this one:
        // it is about to pick this up. Nothing to land, nothing to flag,
        // until it has had [`feed::HANDOFF_WINDOW_MS`] to; then nothing
        // works on the row.
        if let (Some(end), Some(handed)) = (ended_ms, row.handed_at_ms())
            && end < handed
        {
            let fresh = now_ms < handed.saturating_add(feed::HANDOFF_WINDOW_MS);
            return (!fresh).then_some(Unworked::NoWorker { since: handed });
        }
        // Ended on an API error it never wrote past: that error is why.
        if let Some(s) = l.stops.last().filter(|s| s.to_ms.is_none()) {
            return Some(Unworked::Stopped(s.reason.clone()));
        }
        let how = match l.outcome {
            Some(Outcome::Failed) => "failed",
            Some(Outcome::Killed) => "killed",
            Some(Outcome::Stopped) => "stopped",
            _ => "finished",
        };
        return Some(Unworked::Finished { how, ended_ms });
    }
    let f = row.feed?;
    let started = f.started_ms?;
    let fresh = now_ms < started.saturating_add(feed::HANDOFF_WINDOW_MS);
    (!fresh && f.live.is_none()).then_some(Unworked::NoWorker { since: started })
}

/// What a Next up row's NOW says ([`planned_wait`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wait {
    pub note: String,
    /// It waits on nothing recorded, since this instant (epoch ms): its
    /// manager's to start or hold once that is
    /// [`manage_chores::IDLE_PLANNED_MS`] past ([`chores`]). Drawn as any
    /// Next up row.
    pub idle_since: Option<u64>,
    /// The dispatcher's hold, in full, for the overlay header.
    pub held: Option<String>,
    /// The row's hover line: the note, and why it is there.
    pub why: String,
    /// Where it stands in line to start ([`start_order`]).
    pub start: Start,
    /// The task it waits for (`after <key>`), if any.
    pub after: Option<String>,
}

/// Where a Next up row stands in line to start, first to
/// last: the pane draws Next up in this order, the one that starts next on
/// top ([`start_order`]); rows of one place keep their plan order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Start {
    /// Its lease is granted, or it was started: it waits only for its
    /// worker.
    Ready,
    /// Queued in the machine ledger, at this place (1-based; one with no
    /// place known goes after the rest).
    Queued(u64),
    /// Nothing recorded to wait for: its manager's to start.
    Idle,
    /// After another task ([`Wait::after`]), and after that task's row when
    /// it is in Next up too.
    After,
    /// Held or paused: it starts only on a person's or its manager's call.
    SetAside,
}

/// What a Next up row waits for. A planned task may wait only
/// on a hold the dispatcher recorded (`held: <why>`), a lease queued in the
/// ledger (`queued for 3G behind demo#12`), or another task: the one it was
/// planned `--after` until that lands, or the task its worker is busy with
/// (`after <key>`). A row with none of these waits on nothing, since it last
/// could have started (planned, its hold ended, the task before it landed,
/// its lease granted): [`Wait::idle_since`], and an empty NOW. `None` for
/// any other row; a Next up row whose writer never said when it was planned
/// has no `idle_since`.
fn planned_wait(row: &PaneRow<'_, SubagentRow>, rows: &[PaneRow<'_, SubagentRow>]) -> Option<Wait> {
    if row.stage != Stage::Planned {
        return None;
    }
    let f = row.feed;
    let waits = |note: String, why: String, start: Start| {
        Some(Wait {
            note,
            idle_since: None,
            held: None,
            why,
            start,
            after: None,
        })
    };
    if let Some(why) = f.and_then(|f| f.held.clone()) {
        return Some(Wait {
            note: format!("held: {why}"),
            idle_since: None,
            why: format!("held — not started; waiting for: {why}"),
            held: Some(why),
            start: Start::SetAside,
            after: None,
        });
    }
    // A Running row benched for want of a worker ([`benched`]): a paused
    // one says since when ([`format_row`]), and is set aside.
    if let Some(f) = f.filter(|f| f.stage() == Stage::Running) {
        return waits(
            NO_WORKER_YET.to_string(),
            format!("{NO_WORKER_YET} — started, but no worker has picked it up"),
            if f.paused_since_ms.is_some() {
                Start::SetAside
            } else {
                Start::Ready
            },
        );
    }
    let lease = f.and_then(|f| f.lease.as_ref());
    if let Some(l) = lease.filter(|l| l.state == LeaseState::Queued) {
        let place = l.position.unwrap_or(u64::MAX);
        return waits(queued_note(l), queued_why(l), Start::Queued(place));
    }
    if lease.is_some_and(|l| l.uncollected) {
        return waits(
            GRANTED_WAITING.to_string(),
            format!("{GRANTED_WAITING} — its lease is granted; its manager is to start it"),
            Start::Ready,
        );
    }
    // The task it was planned after: waited for until it is drawn Done; one
    // gone from the feed is no wait.
    let before = f
        .and_then(|f| f.after.as_deref())
        .and_then(|a| rows.iter().find(|r| r.feed.is_some_and(|f| f.key == a)));
    let after = |k: &str, note: String, why: String| {
        Some(Wait {
            after: Some(k.to_string()),
            ..waits(note, why, Start::After)?
        })
    };
    if let Some(r) = before.filter(|r| r.stage != Stage::Done) {
        let k = r.feed.map_or("", |f| f.key.as_str());
        return after(
            k,
            format!("after {k}"),
            format!("after {k} — starts when that lands"),
        );
    }
    if let Some(k) = &row.after_key {
        return after(
            k,
            format!("after {k}"),
            format!("after {k} — starts when its worker is done with that"),
        );
    }
    let f = f?;
    let since = [
        f.planned_ms,
        f.unheld_ms,
        before.and_then(|r| r.ended_ms()),
        lease.and_then(|l| l.granted_ms),
    ]
    .into_iter()
    .flatten()
    .max();
    // A lease already granted: it waits only for its worker.
    let granted = lease.is_some_and(|l| l.state != LeaseState::Queued);
    Some(Wait {
        note: String::new(),
        idle_since: since,
        held: None,
        why: IDLE_WHY.to_string(),
        start: if granted { Start::Ready } else { Start::Idle },
        after: None,
    })
}

/// Each row's place in the pane's drawing order: its section,
/// then, in Next up, where it stands in line to start ([`Wait::start`]) and
/// how deep in a chain of `after`s it sits. A row after another Next up row
/// goes after that row: no sooner than its place, one deeper, so it is set
/// aside too behind a held one. Sorted on stably, rows of one place keep
/// their plan order; across managers the ledger's places decide.
fn start_order(
    rows: &[PaneRow<'_, SubagentRow>],
    waits: &[Option<Wait>],
) -> Vec<(Stage, Start, usize)> {
    fn place(
        i: usize,
        rows: &[PaneRow<'_, SubagentRow>],
        waits: &[Option<Wait>],
        seen: &mut Vec<usize>,
    ) -> (Start, usize) {
        let Some(w) = &waits[i] else {
            return (Start::Idle, 0);
        };
        let before = w.after.as_deref().and_then(|k| {
            rows.iter()
                .position(|r| r.stage == Stage::Planned && r.feed.is_some_and(|f| f.key == k))
        });
        match before {
            // A loop of `after`s is no chain: its rows keep their own place.
            Some(j) if !seen.contains(&j) && j != i => {
                seen.push(i);
                let (start, depth) = place(j, rows, waits, seen);
                (start.max(w.start), depth + 1)
            }
            _ => (w.start, usize::from(w.after.is_some())),
        }
    }
    (0..rows.len())
        .map(|i| match rows[i].stage {
            Stage::Planned => {
                let (start, depth) = place(i, rows, waits, &mut Vec::new());
                (Stage::Planned, start, depth)
            }
            s => (s, Start::Ready, 0),
        })
        .collect()
}

/// The hover of a Next up row that waits on nothing recorded
/// ([`Wait::idle_since`]).
pub const IDLE_WHY: &str = "not started — waits on nothing; its manager is to start it";

/// NOW of a Running row with no worker, drawn under Next up ([`benched`]).
pub const NO_WORKER_YET: &str = "no worker yet";

/// NOW of a paused Running row whose worker finished, drawn under Next up
/// ([`benched`]): its work is done, its manager's to land or
/// resume ([`chores`]). About as wide as the `paused since 15:10` it takes
/// the place of.
pub const PAUSED_FINISHED: &str = "paused · worker finished";

/// NOW of a paused Running row whose worker still works: only
/// its clock is paused, and it stays under Running until the worker ends
/// its turn; then it moves to Next up, `paused since …`.
pub const PAUSED_FINISHING: &str = "paused · worker still finishing";

/// NOW of a Running row whose worker was killed with its session:
/// Giverny, WSL or the manager session went off.
pub const KILLED: &str = "killed";

/// A killed row's hover: when it went off, and that only its manager
/// restarts it.
fn killed_why(row: &PaneRow<'_, SubagentRow>, now_ms: u64, tz: &jiff::tz::TimeZone) -> String {
    let f = row.feed;
    let off = f
        .and_then(|f| f.went_off_ms.or(f.paused_since_ms))
        .map(|p| format!(" at {}", clock_at(p, now_ms, tz)))
        .unwrap_or_default();
    let clock = if row.paused_since_ms().is_some() {
        "its clock stopped there until its manager revives it (a fresh worker, then resume)"
    } else {
        "resumed with no worker: a fresh one whose description names the task takes the row"
    };
    format!(
        "killed: Giverny, WSL or its manager session went off{off}, and its worker with it; {clock}"
    )
}

/// The overlay header's facts, from the row's own cells before any ditto.
/// `vs` names what a Done row's ± was measured against:
/// `worker's 40m`, `estimate 30m`.
fn row_facts(
    stage: Stage,
    elapsed: &str,
    eta: &str,
    vs: &str,
    now: &str,
    tokens: &str,
) -> Vec<String> {
    let some = |s: &str| (!s.is_empty()).then(|| s.to_string());
    let v = match stage {
        Stage::Done => [
            some(now),
            some(elapsed).map(|e| format!("took {e}")),
            some(eta).map(|d| format!("{d} vs {vs}")),
            some(tokens).map(|t| format!("{t} tokens")),
        ],
        Stage::Running => [
            some(elapsed).map(|e| format!("running {e}")),
            some(eta).map(|l| format!("{l} left")),
            some(now),
            some(tokens).map(|t| format!("{t} tokens")),
        ],
        Stage::Planned => [some(eta).map(|e| format!("est {e}")), None, None, None],
    };
    v.into_iter().flatten().collect()
}

/// A Running row's hover: who works on it and since when,
/// what holds it when something does (`state`), and its estimate against
/// the time left (`left_s`, below zero once over).
fn running_why(
    who: &str,
    since: Option<&str>,
    estimate_s: Option<u64>,
    left_s: Option<i64>,
    state: Option<String>,
) -> Vec<String> {
    let mut head = format!("Running: {who}");
    if let Some(s) = since {
        head.push_str(&format!(" since {s}"));
    }
    let est = match (estimate_s, left_s) {
        (Some(e), Some(left)) if left < 0 && feed::fmt_span(left) != "0m" => format!(
            "estimate {}: {} over it",
            feed::fmt_span(e as i64),
            feed::fmt_span(-left)
        ),
        (Some(e), Some(left)) => format!(
            "estimate {}: {} left, on track",
            feed::fmt_span(e as i64),
            countdown(left)
        ),
        (Some(e), None) => format!("estimate {}", feed::fmt_span(e as i64)),
        (None, _) => "no estimate yet".into(),
    };
    std::iter::once(head).chain(state).chain([est]).collect()
}

/// The background shells a Running row's worker sleeps on:
/// its turn ended as a plain finish — Claude Code lists it as completed,
/// "waiting on its own background work" — while shells it started still
/// run ([`bg_shells::Watch`]). Claude Code wakes it when one ends, so it is
/// asleep, not ended: nothing to land, nobody to wake. Oldest first.
fn asleep_on<'s>(
    row: &PaneRow<'_, SubagentRow>,
    shells: &'s Shells,
    now_ms: u64,
) -> Option<&'s [bg_shells::Shell]> {
    let l = row.live?;
    match unworked(row, now_ms) {
        Some(Unworked::Finished {
            how: "finished", ..
        }) => {}
        _ => return None,
    }
    shells
        .get(&l.id)
        .map(Vec::as_slice)
        .filter(|s| !s.is_empty())
}

/// NOW of a Running row whose worker is asleep on its own background
/// shells: `asleep · wrangler deploy (30m) · wakes when it
/// ends`, `+N` for more, and the wait it marked, if any, as [`turn_ended`]
/// has it. What it sleeps on comes first: the cell is cut, the hover has
/// it all.
fn asleep_now(
    shells: &[bg_shells::Shell],
    waiting: Option<(u64, Option<&str>)>,
    now_ms: u64,
) -> String {
    let when = if shells.len() == 1 {
        "it ends"
    } else {
        "one ends"
    };
    let base = format!(
        "asleep · {} · wakes when {when}",
        bg_shells::named(shells, now_ms)
    );
    match waiting {
        Some((_, Some(n))) if !n.trim().is_empty() => format!("{base} · waiting: {}", n.trim()),
        Some(_) => format!("{base} · waiting"),
        None => base,
    }
}

/// The hover's line for an asleep worker: since when, each
/// shell in full, and that Claude Code wakes it.
fn asleep_why(
    shells: &[bg_shells::Shell],
    ended_ms: Option<u64>,
    now_ms: u64,
    tz: &jiff::tz::TimeZone,
) -> String {
    let since = ended_ms.map_or(String::new(), |e| {
        format!(" since {}", clock_at(e, now_ms, tz))
    });
    let list: Vec<String> = shells
        .iter()
        .map(|s| {
            format!(
                "`{}` ({} in, shell {})",
                bg_shells::gist(&s.cmd),
                feed::fmt_span((now_ms.saturating_sub(s.started_ms) / 1000) as i64),
                s.id
            )
        })
        .collect();
    let (what, when) = if shells.len() == 1 {
        ("its own background shell runs", "that ends")
    } else {
        ("its own background shells run", "one ends")
    };
    format!(
        "its worker is asleep{since}: its turn ended while {what}: {}. Claude Code wakes it \
         when {when}; nothing to land yet",
        list.join(", ")
    )
}

/// A paused row's hover: `paused since 15:10: <note>`, else why its
/// clock stands.
/// NOW of a Running row whose worker ended its turn: when,
/// and the wait it marked, if any — `turn ended 11:41 · waiting: machine
/// time`.
fn turn_ended(
    ended_ms: Option<u64>,
    waiting: Option<(u64, Option<&str>)>,
    now_ms: u64,
    tz: &jiff::tz::TimeZone,
) -> String {
    let ended = match ended_ms {
        Some(e) => format!("turn ended {}", clock_at(e, now_ms, tz)),
        None => "turn ended".into(),
    };
    match waiting {
        Some((_, Some(n))) if !n.trim().is_empty() => format!("{ended} · waiting: {}", n.trim()),
        Some(_) => format!("{ended} · waiting"),
        None => ended,
    }
}

/// The hover's line for a worker's open wait.
fn waiting_why(since_ms: u64, note: Option<&str>, now_ms: u64, tz: &jiff::tz::TimeZone) -> String {
    let since = clock_at(since_ms, now_ms, tz);
    match note.map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => format!("waiting since {since}: {n}"),
        None => format!("waiting since {since}, on something that is not the work"),
    }
}

fn paused_why(since: &str, note: Option<&str>) -> String {
    match note.filter(|n| !n.is_empty()) {
        Some(n) => format!("paused since {since}: {n}"),
        None => format!("paused since {since} — its clock is stopped"),
    }
}

/// A Done row's hover: how it landed (`now`, the row's NOW)
/// and how long it took against its estimate, then who is to review it and
/// where, when someone is.
fn done_why(
    now: &str,
    elapsed: &str,
    delta: Option<(i64, feed::Against)>,
    review: Option<&str>,
) -> Vec<String> {
    let mut head = "Done".to_string();
    // A landing that says only Done is not said twice.
    if !now.is_empty() && !now.eq_ignore_ascii_case("done") {
        head.push_str(&format!(": {now}"));
    }
    if !elapsed.is_empty() {
        head.push_str(&format!("; took {elapsed}"));
        head.push_str(&match delta {
            Some((d, a)) => {
                let what = format!("its {}", a.label());
                match feed::fmt_span(d.abs()) {
                    s if s == "0m" => format!(", on {what}"),
                    s if d > 0 => format!(", {s} over {what}"),
                    s => format!(", {s} under {what}"),
                }
            }
            None => ", no estimate".into(),
        });
    }
    std::iter::once(head)
        .chain(review.map(|r| format!("awaiting review: {r}")))
        .collect()
}

/// How a Running row with no estimate gets one, as the overlay header says
/// it: a manager session's task is re-estimated on its row; any other
/// worker is given an agent ETA by its id.
fn no_eta_hint(key: &str, agent_id: Option<&str>) -> String {
    if !key.is_empty() {
        return format!("no ETA — add one: giverny-manage eta {key} <min> --why scope");
    }
    let agent = agent_id.unwrap_or("<agent-id>");
    format!("no ETA — add one: giverny-eta {agent} <min>")
}

/// A feed row's key as the TASK column shows it: none for the
/// `agent-<id>` a worker with no task once started its own row under
/// (an older version did), which names nothing a person knows.
/// A model id in Claude Code's short names (`haiku`, `sonnet`, `opus`,
/// `fable`), without version or date; None for a name of no known family.
fn short_model(m: &str) -> Option<String> {
    let m = m.to_ascii_lowercase();
    ["haiku", "sonnet", "opus", "fable"]
        .into_iter()
        .find(|f| m.contains(f))
        .map(str::to_string)
}

fn shown_key(f: &feed::FeedRow) -> String {
    let made_up = f.follows_worker
        && f.key
            .strip_prefix("agent-")
            .is_some_and(|h| !h.is_empty() && h.chars().all(|c| c.is_ascii_alphanumeric()));
    if made_up {
        String::new()
    } else {
        f.key.clone()
    }
}

fn outcome_word(o: Outcome) -> String {
    match o {
        Outcome::Completed | Outcome::Unknown => "done",
        Outcome::Failed => "failed",
        Outcome::Killed => "killed",
        Outcome::Stopped => "stopped",
    }
    .into()
}

// --------------------------------------------------------- formatting ----

/// ELAPSED as a stopwatch writes it, only the digits it needs: `0:42`,
/// `14:05`, `1:03:07`.
pub fn stopwatch(secs: u64) -> String {
    let (h, rest) = (secs / 3600, secs % 3600);
    let (m, s) = (rest / 60, rest % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// A Running row's time left: `~20m`, and `-~12m` once past its estimate.
pub fn countdown(left_s: i64) -> String {
    if left_s < 0 && feed::fmt_span(left_s) != "0m" {
        format!("-~{}", feed::fmt_span(-left_s))
    } else {
        format!("~{}", feed::fmt_span(left_s.max(0)))
    }
}

/// Claude Code's compact count — `842`, `13.5k`, `124.8k`, `1.2M` — with a
/// capital `M` so a total never reads as minutes.
pub fn fmt_tokens(n: u64) -> String {
    for (floor, div, suffix) in [
        (999_950_000u64, 1_000_000_000f64, "B"),
        (999_950, 1_000_000.0, "M"),
        (1000, 1000.0, "k"),
    ] {
        if n >= floor {
            let v = format!("{:.1}", n as f64 / div);
            let v = v.strip_suffix(".0").unwrap_or(&v);
            return format!("{v}{suffix}");
        }
    }
    n.to_string()
}

fn stage_word(s: Stage) -> &'static str {
    match s {
        Stage::Running => "Running",
        Stage::Planned => "NextUp",
        Stage::Done => "Done",
    }
}

fn stage_color(s: Stage) -> Color32 {
    match s {
        Stage::Running => RUNNING,
        Stage::Planned => PLANNED,
        Stage::Done => DONE,
    }
}

/// Cut to `max` characters, the last one an ellipsis.
/// Start column that makes `s` end at column `end` (right alignment on the
/// cell grid).
fn right_at(end: usize, s: &str) -> usize {
    end.saturating_sub(s.chars().count())
}

fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

// ------------------------------------------------------------ drawing ----

/// Draw `tab`'s pane from `tracker`, if there are rows. Call inside the
/// central panel, before the terminal takes the rest. Returns the row
/// clicked this frame.
///
/// `limit` is the usage limit the tab's account is out on ([`limit_for`]):
/// while it is, the Running rows' clocks are held. `viewed` is the worker
/// the tab shows, in Claude Code's view or on Giverny's page: its row is
/// lit as the selection. `header` names the worker the terminal's
/// header is about, whose row comes back with the click, as
/// the pane drew it.
#[allow(clippy::too_many_arguments)]
pub fn show(
    views: &mut Views,
    tab: TabId,
    reading: Option<&giverny_claude::use_reading::Reading>,
    tracker: Option<&Tracker>,
    viewed: Option<Shown<'_>>,
    header: Option<Shown<'_>>,
    limit: Option<Limit>,
    panel: &ManagementPanelConfig,
    chrome: &Chrome,
    shared: &mut RenderShared,
    ui: &mut Ui,
) -> (Option<RowClick>, Option<Line>) {
    // A look asks for one flash, at the drawing right after it: a pane with
    // nothing to show now does not flash later.
    let flash_due = views
        .tabs
        .get_mut(&tab)
        .is_some_and(|v| std::mem::take(&mut v.flash_due));
    let Some(tracker) = tracker else {
        return (None, None);
    };
    let view = views.tabs.entry(tab).or_default();
    view.poll_feed(
        tracker.session_id.as_deref(),
        &tracker.aliases,
        tracker.config_dir.as_deref(),
    );
    // Only when subagents are involved, or for a manager: a
    // feed row alone opens nothing.
    if !feed::opens(!tracker.is_empty(), view.manager) {
        return (None, None);
    }
    let drawn = Drawn::of(view.manager);
    let now = now_ms();
    track(&mut view.holds, limit.as_ref(), now);
    // What the rows' processes use: this frame's reading, the one the
    // sidebar shows the sums of (`sessions_load::latest`).
    let runs = reading
        .map(crate::sessions_load::task_lives)
        .unwrap_or_default();
    let workers = reading
        .map(crate::sessions_load::workers)
        .unwrap_or_default();
    let clock = Clock {
        holds: &view.holds,
        sleeps: views.sleeps.poll(&feed::feed_dir()),
        limit: limit.as_ref(),
        tz: jiff::tz::TimeZone::system(),
        workers: &workers,
        etas: &view.etas_now,
        shells: view.shells.live(),
    };
    // Done rows cleared by hand go from the feed too, whoever wrote it.
    let cleared = tracker
        .done_cleared_ms
        .and_then(|c| view.feed_now.as_ref().map(|f| f.without_done_by(c)));
    let feed = cleared.as_ref().or(view.feed_now.as_ref());
    // The ledger is the truth about leases; a row's copy is as of its
    // session's last claim.
    let ledger = views.ledger.get(ui.ctx());
    let leased = match (feed, ledger.as_deref()) {
        (Some(f), Some(l)) => {
            let mut sessions: Vec<&str> = f.session.iter().map(String::as_str).collect();
            sessions.extend(f.aliases.iter().map(String::as_str));
            sessions.extend(tracker.session_id.as_deref());
            sessions.extend(tracker.aliases.iter().map(String::as_str));
            Some(with_workers(
                with_live(with_ledger(f, &sessions, &l.ledger), &sessions, &runs),
                &sessions,
                &runs,
                &workers,
            ))
        }
        _ => None,
    };
    let feed = leased.as_ref().or(feed);
    if std::mem::take(&mut view.logs_due) {
        poll_logs(&mut view.logs, tracker.rows());
    }
    let mut table = build_with(
        feed,
        tracker.rows(),
        now,
        &clock,
        &view.logs,
        panel.done(),
        drawn,
        &view.folded,
    );
    table.footer = with_hookless(table.footer.take(), view.hookless);
    if table.is_empty() {
        return (None, None);
    }
    // What landed since the last look breathes; whatever is drawn now has
    // been seen.
    if flash_due {
        let rows = newly_done(&table.lines, view.seen_done.as_ref(), views.born_ms);
        view.flash = (!rows.is_empty()).then(|| Flash {
            rows,
            from: crate::rail::anim_time(),
        });
    }
    note_seen(&mut view.seen_done, &table.lines);
    let animate = crate::rail::animating(ui);
    let glow = view.flash.as_ref().and_then(|f| {
        let t = crate::rail::anim_time() - f.from;
        let glow = flash_glow(t, animate)?;
        if !animate {
            // Still: one frame, where the steady highlight ends.
            ui.ctx()
                .request_repaint_after(Duration::from_secs_f64((FLASH_S - t).max(0.0)));
        }
        Some(glow)
    });
    if glow.is_none() {
        view.flash = None;
    }
    let flash = view.flash.as_ref().zip(glow).map(|(f, g)| (&f.rows, g));
    // The first of a worker's rows: the one with no ditto in it.
    let header_line = header.and_then(|h| table.lines.iter().find(|l| h.is(&l.click)).cloned());
    if table.any_running() {
        // ELAPSED is a stopwatch.
        ui.ctx().request_repaint_after(Duration::from_secs(1));
    }

    // The session's own cell: the pane is drawn in the terminal's glyphs
    // (face, size, hinting, grid), so it follows zoom and the font setting.
    let cell = shared.cell_size(ui.ctx().pixels_per_point());
    // A little air around each row; a table, not a wall of grid.
    let row_h = (cell.y * 1.2).round().max(cell.y);
    let rows = table.lines.len() + usize::from(table.footer.is_some());
    let frame = pane_frame(shared.theme.bg);
    let want = pane_height(
        rows,
        row_h,
        ui.spacing().item_spacing.y,
        frame.total_margin().sum().y,
    );
    let min_h = row_h * 1.5;
    let max_h = (ui.available_height() * 0.5).max(row_h * 3.0);
    let fit = want.clamp(min_h, max_h);
    let id = egui::Id::new(("management_panel", tab));
    follow_rows(ui.ctx(), id, &mut view.fit, fit);

    let mut clicked = None;
    egui::Panel::bottom(id)
        .resizable(true)
        .default_size(fit)
        .size_range(min_h..=max_h)
        .frame(frame)
        .show(ui, |ui| {
            // Measured outside the scroll area: inside it, the width shrinks
            // by the bar's lane only while the rows overflow, and the right
            // columns would jump as the pane is resized across that point.
            let full = ui.available_rect_before_wrap();
            let ppp = ui.ctx().pixels_per_point();
            let cols = table_cols(full.width(), cell.x, ppp, bar_lane(ui));
            // The table on the terminal's grid: INSET whole cells in, as
            // Claude Code's status line is.
            let grid = full.with_min_x(full.min.x + INSET as f32 * cell.x);
            let outer = ui;
            let mut inner = outer.new_child(egui::UiBuilder::new().max_rect(grid));
            let ui = &mut inner;
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    let drew = draw_table(
                        ui,
                        id.with("rows"),
                        &table,
                        &panel.columns,
                        viewed,
                        flash,
                        animate,
                        chrome,
                        shared,
                        cell,
                        row_h,
                        cols,
                        &mut view.sel,
                    );
                    clicked = drew.clicked;
                    view.hover = drew.hover;
                    if let Some(ident) = drew.fold {
                        if !view.folded.remove(&ident) {
                            view.folded.insert(ident);
                        }
                        ui.ctx().request_repaint();
                    }
                    view.clicks = table.lines.iter().map(|l| l.click.clone()).collect();
                });
            outer.advance_cursor_after_rect(inner.min_rect());
        });
    (clicked, header_line)
}

/// The pane's frame: the session's own background, not the rail's lifted
/// panel colour, so the pane reads as part of the terminal above it. egui's
/// separator line (on by default) keeps the boundary between the two.
fn pane_frame(bg: Color32) -> egui::Frame {
    // No margin at the sides: the table is placed on the terminal's grid
    // ([`INSET`]), and the scroll bar sits at the right edge.
    egui::Frame::NONE
        .fill(bg)
        .inner_margin(egui::Margin::symmetric(0, 5))
}

/// The pane's outer height for `rows` rows of `row_h`: the rows, egui's
/// `gap` between each two of them, and the frame's `margin`. Leave out the
/// gaps and the rows overflow by one gap a row — about one row in seven —
/// and the scroll area's fade dims the last row shown.
fn pane_height(rows: usize, row_h: f32, gap: f32, margin: f32) -> f32 {
    let n = rows as f32;
    n * row_h + (n - 1.0).max(0.0) * gap + margin
}

/// Keep the pane sized to its rows as they come and go, until the user
/// drags it: egui remembers a panel's height and only reads `default_size`
/// the first time, so a pane opened with three rows would stay three rows
/// tall. `fit` is the height the pane last sized itself to; a stored
/// height that differs from it is the user's, and from then on theirs.
fn follow_rows(ctx: &egui::Context, id: egui::Id, fit: &mut Option<f32>, want: f32) {
    let stored = egui::containers::panel::PanelState::load(ctx, id).map(|s| s.size().y);
    match (stored, *fit) {
        // First sight: default_size applies.
        (None, _) => *fit = Some(want),
        // Still ours; resize to the rows if they changed.
        (Some(h), Some(f)) if (h - f).abs() <= 1.0 => {
            if (want - f).abs() > 0.5 {
                ctx.data_mut(|d| d.remove::<egui::containers::panel::PanelState>(id));
                *fit = Some(want);
            }
        }
        // Dragged: the user's height stands.
        _ => *fit = None,
    }
}

/// A row's background tint, from the pointer alone: a deeper one while it is
/// pressed, a light one while hovered, none otherwise. There is no third
/// input — a click leaves nothing behind to highlight.
/// How strongly the row of the worker on view is lit: past a hover's, so
/// it reads as the selection even under the pointer.
const VIEWED_TINT: f32 = 0.26;

/// How wide a newly done row's breathing frame is, in points.
const FLASH_WIDTH: f32 = 1.5;
/// How far out past the row's left edge the frame sits, in points: clear of
/// the row's first glyph, and within the scroll area's clip margin, so it is
/// not cut. The right edge is the row's own, which ends clear of its last
/// glyph and at the clip.
const FLASH_OUT: f32 = 3.0;

fn row_tint(hovered: bool, pressed: bool) -> Option<f32> {
    if pressed {
        Some(0.22)
    } else if hovered {
        Some(0.10)
    } else {
        None
    }
}

/// The scrollbar's full lane, in points, whether or not it is showing.
fn bar_lane(ui: &Ui) -> f32 {
    let bar = &ui.spacing().scroll;
    bar.bar_inner_margin + bar.bar_width + bar.bar_outer_margin
}

/// How many cells in from the pane's left edge the table starts: as many
/// as Claude Code leaves at the left of its status line, which it draws
/// two columns into the terminal right above the pane.
const INSET: usize = session_use::CLAUDE_MARGIN / 2;

/// How many character columns the table lays out in, from the pane's width
/// *outside* the scroll area, which is the terminal's: the pane spans the
/// terminal's area, and is drawn on its grid ([`INSET`] whole cells in).
/// The table is as wide as the status line Claude Code draws over it
/// (the terminal's columns less [`session_use::CLAUDE_MARGIN`]), so its
/// last column, the use column, ends where the status line does. The bar's
/// lane is always left free, so it never paints over a cell and the
/// columns never move when it appears; on the usual sizes it fits in the
/// two cells right of the table anyway.
fn table_cols(width: f32, cell_w: f32, ppp: f32, bar_lane: f32) -> usize {
    let cw = cell_w.max(0.1);
    // The terminal counts its columns in whole device pixels
    // (`TermView::show`); counted the same way, the two never disagree.
    let cell_px = (cw * ppp).round().max(1.0);
    let terminal = (width * ppp / cell_px).floor() as usize;
    let under_line = terminal.saturating_sub(session_use::CLAUDE_MARGIN);
    let room = ((width - INSET as f32 * cw - bar_lane) / cw)
        .floor()
        .max(0.0) as usize;
    under_line.min(room).max(40)
}

/// Where each column of a row starts or ends, in characters, for a table
/// laid out in `cols` columns. A column switched off
/// (`[management_panel.columns]`) takes no width: the rest close up, each a
/// [`GAP`] from the one before, and TASK takes what is left.
struct Cols {
    on: PaneColumns,
    idw: usize,
    taskw: usize,
    x_task: usize,
    x_el_end: usize,
    x_eta_end: usize,
    x_now: usize,
    x_tok_end: usize,
    /// The use column, the last: a [`usage_cell`] wide while any row is Running
    /// or Done, not there at all while every row is Next up. It ends on
    /// the table's last column, so it ends where the status line above
    /// does ([`table_cols`]).
    usew: usize,
    x_use: usize,
    /// The table nests rows ([`Table::nested`]).
    nested: bool,
}

impl Cols {
    /// Every column on.
    #[cfg(test)]
    fn new(table: &Table, cols: usize) -> Self {
        Self::shown(table, cols, &PaneColumns::default())
    }

    fn shown(table: &Table, cols: usize, on: &PaneColumns) -> Self {
        let idw = if on.id {
            table
                .lines
                .iter()
                .map(|l| l.id.chars().count())
                .max()
                .unwrap_or(0)
        } else {
            0
        };
        // Every Running and Done row has a cell, all of one width
        // ([`usage_cell`]), a Next up row none.
        let usew = if on.usage {
            table
                .lines
                .iter()
                .map(|l| l.usage.chars().count())
                .max()
                .unwrap_or(0)
        } else {
            0
        };
        // The fixed columns right of TASK, each with the gap before it.
        let fixed_right: usize = [
            (on.elapsed, EL_W),
            (on.eta, ETA_W),
            (on.now, NOW_W),
            (on.tokens, TOK_W),
            (usew > 0, usew),
        ]
        .iter()
        .filter(|(shown, _)| *shown)
        .map(|(_, w)| w + GAP)
        .sum();
        let stage_cols = if on.stage { STAGE_W + GAP } else { 0 };
        // TASK is the id and the title; with the title off, just the id.
        // However narrow the pane, it holds the id and, on every row, its
        // nesting, a few cells of title and a folded row's short note
        // (`+3`), so nothing spills into ELAPSED: the table
        // runs off the right instead.
        let taskw = if on.title {
            let id_room = if idw > 0 { idw + 1 } else { 0 };
            let least = table
                .lines
                .iter()
                .map(|l| {
                    let note = l.fold.as_ref().map(Fold::short_note).unwrap_or_default();
                    let note = if note.is_empty() {
                        0
                    } else {
                        note.chars().count() + 2
                    };
                    nest_head(l, table.nested).chars().count() + note + TITLE_KEEP
                })
                .max()
                .unwrap_or(0);
            cols.saturating_sub(stage_cols + fixed_right)
                .max(id_room + MIN_TITLE.max(least))
        } else {
            idw
        };
        // Lay the shown columns out left to right; `place` gives a
        // column's start and moves past it.
        let mut x = 0;
        let mut any = false;
        let mut place = |shown: bool, w: usize| -> usize {
            if !shown {
                return x;
            }
            let at = if any { x + GAP } else { 0 };
            any = true;
            x = at + w;
            at
        };
        place(on.stage, STAGE_W);
        let x_task = place(taskw > 0, taskw);
        let x_el_end = place(on.elapsed, EL_W) + EL_W;
        let x_eta_end = place(on.eta, ETA_W) + ETA_W;
        let x_now = place(on.now, NOW_W);
        let x_tok_end = place(on.tokens, TOK_W) + TOK_W;
        // The use column ends on the table's last column even when TASK
        // does not stretch to fill (the title off), never closer than a
        // gap to what is before it.
        let x_use = place(usew > 0, usew).max(cols.saturating_sub(usew));
        Cols {
            on: *on,
            idw,
            taskw,
            x_task,
            x_el_end,
            x_eta_end,
            x_now,
            x_tok_end,
            usew,
            x_use,
            nested: table.nested,
        }
    }

    /// The cells TASK leaves the title, past the id; `None` with the
    /// title off.
    fn title_w(&self) -> Option<usize> {
        if !self.on.title {
            None
        } else if self.idw > 0 {
            Some(self.taskw.saturating_sub(self.idw + 1))
        } else {
            Some(self.taskw)
        }
    }

    /// Where a row's title starts, in columns from the row's left: past
    /// the id. `None` with the title off.
    fn title_x(&self) -> Option<usize> {
        self.title_w()?;
        Some(self.x_task + if self.idw > 0 { self.idw + 1 } else { 0 })
    }

    /// The column of a row's fold toggle ([`Fold::glyph`]), when it has
    /// one and the title is shown.
    fn fold_x(&self, line: &Line) -> Option<usize> {
        line.fold.as_ref()?;
        Some(self.title_x()? + NEST_W * line.depth)
    }

    /// A click in `line`'s cell `col` is on its fold toggle — the glyph or
    /// the space after it: the fold it toggles ([`Fold::ident`]).
    fn fold_hit(&self, line: &Line, col: usize) -> Option<String> {
        let x = self.fold_x(line)?;
        (col == x || col == x + 1).then(|| line.fold.as_ref().map(|f| f.ident.clone()))?
    }

    /// A row's cells as drawn: each one's text and the column it starts at.
    /// Always the same seven, in this order ([`ETA_SEG`], [`LEASE_SEG`]); a
    /// column switched off is an empty cell.
    fn segments(&self, line: &Line) -> Vec<(usize, String)> {
        let on = &self.on;
        let idw = self.idw;
        // The id is never cut; the title fits in what is left
        // ([`fit_title`]).
        let title = |w: usize| fit_title(line, self.nested, w);
        let task = match self.title_w() {
            None => line.id.clone(),
            Some(w) if idw > 0 => format!("{:<idw$} {}", line.id, title(w)),
            Some(w) => title(w),
        };
        let cell = |shown: bool, at: usize, text: String| {
            if shown {
                (at, text)
            } else {
                (0, String::new())
            }
        };
        // A row with no tokens (Next up) lends NOW the TOKENS column:
        // `queued for 3G behind demo#12` is longer than NOW.
        let now_w = if line.tokens.is_empty() && on.tokens {
            NOW_W + GAP + TOK_W
        } else {
            NOW_W
        };
        vec![
            cell(on.stage, 0, stage_word(line.stage).to_string()),
            cell(self.taskw > 0, self.x_task, task),
            // Right-aligned: the text ends at the column's last cell.
            cell(
                on.elapsed,
                right_at(self.x_el_end, &line.elapsed),
                line.elapsed.clone(),
            ),
            cell(
                on.eta,
                right_at(self.x_eta_end, eta_cell(line)),
                eta_cell(line).into(),
            ),
            cell(on.now, self.x_now, cut(&line.now, now_w)),
            cell(
                on.tokens,
                right_at(self.x_tok_end, &line.tokens),
                line.tokens.clone(),
            ),
            cell(self.usew > 0, self.x_use, cut(&line.usage, self.usew)),
        ]
    }
}

/// How far each level of nesting moves a title in.
const NEST_W: usize = 3;

/// The least of a title kept beside a folded row's note before the note
/// shortens: `Work …`.
const TITLE_KEEP: usize = 6;

/// A row's title cell, `w` cells wide: its nesting, the
/// title, and a folded row's note, never wider than `w`. The nesting is
/// never cut ([`Cols::shown`] leaves it room). As the cell narrows the
/// title gives way first, down to [`TITLE_KEEP`]; then the note shortens
/// (`+3, 2 running` to `+3`); then it drops and the title takes it all.
fn fit_title(line: &Line, nested: bool, w: usize) -> String {
    let head = nest_head(line, nested);
    let hw = head.chars().count();
    let keep = line.title.chars().count().min(TITLE_KEEP);
    let notes = line
        .fold
        .as_ref()
        .filter(|f| f.folded)
        .map(|f| [f.note(), f.short_note()])
        .unwrap_or_default();
    for note in notes.iter().filter(|n| !n.is_empty()) {
        let tail = format!("  {note}");
        let used = hw + tail.chars().count();
        if used + keep <= w {
            return format!("{head}{}{tail}", cut(&line.title, w - used));
        }
    }
    cut(
        &format!("{head}{}", cut(&line.title, w.saturating_sub(hw))),
        w,
    )
}

/// What a title is drawn behind: its level's indent, then its
/// fold toggle, or `└` for a nested row with nothing under it. A row of its
/// own with nothing under it has none — but leaves the toggle's room while
/// another row has one (`nested`), so the titles stay aligned.
fn nest_head(line: &Line, nested: bool) -> String {
    let indent = " ".repeat(NEST_W * line.depth);
    match (&line.fold, line.depth) {
        (Some(f), _) => format!("{indent}{} ", f.glyph()),
        // The toggle's room: the glyph and the space after it.
        (None, 0) if nested => "  ".into(),
        (None, 0) => String::new(),
        (None, _) => format!("{indent}└ "),
    }
}

/// The ETA cell's text: the row's own, or [`NO_ETA`] for a Running row
/// nobody estimated.
fn eta_cell(line: &Line) -> &str {
    if line.no_eta { NO_ETA } else { &line.eta }
}

/// A row's text as it reads on screen, one character a cell: what a
/// selection over it copies.
fn compose(segments: &[(usize, String)]) -> String {
    let mut buf: Vec<char> = Vec::new();
    for (at, s) in segments {
        for (i, ch) in s.chars().enumerate() {
            let k = at + i;
            if buf.len() <= k {
                buf.resize(k + 1, ' ');
            }
            buf[k] = ch;
        }
    }
    let s: String = buf.into_iter().collect();
    s.trim_end().to_string()
}

// ---------------------------------------------------------- selection ----

/// A drag over the pane's text, in `(row, column)` cells —
/// rows count the table's lines, then its footer. The terminal's own model:
/// a stream from where the drag began to where the pointer is, copied when
/// the drag ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: (usize, usize),
    pub head: (usize, usize),
}

impl Selection {
    fn ordered(&self) -> ((usize, usize), (usize, usize)) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// The columns selected on `row`, a row `len` characters long; `None`
    /// when none are.
    fn span(&self, row: usize, len: usize) -> Option<(usize, usize)> {
        let ((r0, c0), (r1, c1)) = self.ordered();
        if row < r0 || row > r1 {
            return None;
        }
        let a = if row == r0 { c0.min(len) } else { 0 };
        let b = if row == r1 { c1.min(len) } else { len };
        (a < b).then_some((a, b))
    }
}

/// The text `sel` covers in `rows`, line by line, each line's trailing
/// blanks dropped: plain text, as it pastes.
pub fn selected_text(rows: &[String], sel: &Selection) -> String {
    let ((r0, _), (r1, _)) = sel.ordered();
    let mut out: Vec<String> = Vec::new();
    for (r, row) in rows.iter().enumerate().take(r1 + 1).skip(r0) {
        let len = row.chars().count();
        let piece: String = match sel.span(r, len) {
            Some((a, b)) => row.chars().skip(a).take(b - a).collect(),
            None => String::new(),
        };
        out.push(piece.trim_end().to_string());
    }
    out.join("\n")
}

/// What the pointer did to the rows this frame.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct RowsInput {
    /// A plain click: the row it landed on.
    clicked: Option<usize>,
    /// The cell a plain click landed in, counted from the rows' left.
    click_col: Option<usize>,
    /// The row under the pointer, while it is not dragging.
    hovered: Option<usize>,
    /// The pointer is held on a row, not (yet) dragging.
    pressed: bool,
    dragging: bool,
    /// Text put on the clipboard this frame.
    copied: Option<String>,
}

/// The cell under `p`, for rows at `rects` in cells `cw` wide: a point
/// above the first row or below the last is on that row, and a column
/// is the cell boundary nearest to it.
fn cell_at(rects: &[egui::Rect], cw: f32, p: egui::Pos2) -> Option<(usize, usize)> {
    let first = rects.first()?;
    let row = rects
        .iter()
        .position(|r| p.y < r.max.y)
        .unwrap_or(rects.len() - 1);
    let col = ((p.x - first.left()) / cw.max(1.0)).round().max(0.0) as usize;
    Some((row, col))
}

/// The pane's pointer handling over rows at `rects` reading `texts`: a
/// click (press and release in place) opens a row, a drag selects text and
/// copies it as it ends, the terminal's copy-on-select; `Ctrl+Shift+C` copies
/// the selection again. Only a drag selects, and a drag is never a click.
fn rows_input(
    ui: &mut Ui,
    id: egui::Id,
    rects: &[egui::Rect],
    texts: &[String],
    cw: f32,
    sel: &mut Option<Selection>,
) -> Option<(RowsInput, egui::Response)> {
    let mut out = RowsInput::default();
    let area = rects.iter().copied().reduce(|a, b| a.union(b))?;
    // Any new press ends the last selection: it becomes a click, or a new
    // drag, or it is somewhere else entirely — as in the terminal.
    if ui.input(|i| i.pointer.primary_pressed()) {
        *sel = None;
    }
    let resp = ui.interact(area, id, Sense::click_and_drag());
    let at = |p: Option<egui::Pos2>| p.and_then(|p| cell_at(rects, cw, p));
    if resp.drag_started_by(egui::PointerButton::Primary) {
        let origin = ui.input(|i| i.pointer.press_origin());
        if let (Some(a), Some(h)) = (at(origin), at(resp.interact_pointer_pos())) {
            *sel = Some(Selection { anchor: a, head: h });
        }
    } else if resp.dragged_by(egui::PointerButton::Primary)
        && let (Some(s), Some(h)) = (sel.as_mut(), at(resp.interact_pointer_pos()))
    {
        s.head = h;
    }
    out.dragging = resp.dragged();
    let mut copy = false;
    if resp.drag_stopped_by(egui::PointerButton::Primary) {
        copy = true;
    }
    // Ctrl+Shift+C (Cmd+C on macOS) with a selection here is the pane's,
    // taken before the terminal can read it; plain Ctrl+C stays an
    // interrupt, as it is in the terminal.
    if sel.is_some() {
        let chord = ui.input_mut(|i| {
            let shift = i.modifiers.shift;
            let mut hit = false;
            i.events.retain(|e| {
                let ours = matches!(e, egui::Event::Copy | egui::Event::Cut)
                    && giverny_term::input::clipboard_chord(
                        matches!(e, egui::Event::Cut),
                        shift,
                        cfg!(target_os = "macos"),
                    ) == giverny_term::input::ClipboardChord::CopySelection;
                hit |= ours;
                !ours
            });
            hit
        });
        copy |= chord;
    }
    if copy && let Some(s) = sel.as_ref() {
        let text = selected_text(texts, s);
        if !text.is_empty() {
            ui.ctx().copy_text(text.clone());
            out.copied = Some(text);
        }
    }
    if resp.clicked() {
        *sel = None;
        let p = resp.interact_pointer_pos();
        out.clicked = at(p).map(|(r, _)| r);
        out.click_col = p
            .zip(rects.first())
            .map(|(p, first)| ((p.x - first.left()) / cw.max(1.0)).floor().max(0.0) as usize);
    }
    if !out.dragging {
        out.hovered = resp
            .hover_pos()
            .filter(|p| area.contains(*p))
            .and_then(|p| at(Some(p)))
            .map(|(r, _)| r);
        out.pressed = resp.is_pointer_button_down_on();
    }
    if out.dragging {
        ui.ctx().set_cursor_icon(CursorIcon::Text);
    }
    Some((out, resp))
}

#[allow(clippy::too_many_arguments)]
fn draw_table(
    ui: &mut Ui,
    id: egui::Id,
    table: &Table,
    shown: &PaneColumns,
    viewed: Option<Shown<'_>>,
    flash: Option<(&HashSet<String>, f32)>,
    animate: bool,
    chrome: &Chrome,
    shared: &mut RenderShared,
    cell: egui::Vec2,
    row_h: f32,
    cols: usize,
    sel: &mut Option<Selection>,
) -> Drew {
    let cw = cell.x.max(1.0);
    let layout = Cols::shown(table, cols, shown);
    let mut segs: Vec<Vec<(usize, String)>> =
        table.lines.iter().map(|l| layout.segments(l)).collect();
    if let Some(footer) = &table.footer {
        segs.push(vec![(0, cut(footer, cols))]);
    }
    let texts: Vec<String> = segs.iter().map(|s| compose(s)).collect();
    let rects: Vec<egui::Rect> = segs
        .iter()
        .map(|_| {
            ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), Sense::hover())
                .0
        })
        .collect();
    let Some((input, resp)) = rows_input(ui, id, &rects, &texts, cw, sel) else {
        return Drew::default();
    };
    let n = table.lines.len();
    let hovered_line = input.hovered.filter(|&r| r < n);
    if hovered_line.is_some() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    // Every row's hover reads its whole title and why it is where it is.
    // Every row shares the one response, so the
    // tooltip is anchored to the hovered row's rect: it sits under that
    // row, and moving to another row keeps it open (same id, the pointer
    // still inside) with the new text. egui keeps it closed while a drag
    // selects or just after a click.
    if let Some(i) = hovered_line {
        let mut row = resp.clone();
        row.rect = rects[i];
        row.interact_rect = rects[i];
        row.on_hover_text(table.lines[i].tip());
    }
    let sel_fill = ui.visuals().selection.bg_fill;
    let p = ui.painter().clone();

    for (i, rect) in rects.iter().copied().enumerate() {
        let line = table.lines.get(i);
        let color = line.map_or(chrome.dim, |l| stage_color(l.stage));
        if let Some(line) = line {
            // The worker whose view the terminal above shows:
            // held lit, with a bar at its left edge, as a selection.
            let on_view = viewed.is_some_and(|v| v.is(&line.click));
            if on_view {
                p.rect_filled(rect, 2.0, color.gamma_multiply(VIEWED_TINT));
                // In the pane's left margin, clear of the text.
                let bar = egui::Rect::from_min_size(
                    rect.min - egui::vec2(6.0, 0.0),
                    egui::vec2(3.0, rect.height()),
                );
                p.rect_filled(bar, 1.0, color);
            }
            let here = hovered_line == Some(i);
            if let Some(alpha) = row_tint(here, here && input.pressed) {
                p.rect_filled(rect, 2.0, color.gamma_multiply(alpha));
            }
            // Landed since the last look: its frame breathes with the
            // others'. Painted only, over the row and a hair
            // out to its left so it clears the first glyph: nothing moves.
            // Kept moving only while the row is on screen.
            if let Some((rows, glow)) = flash
                && line.stage == Stage::Done
                && rows.contains(&done_ident(&line.click))
            {
                p.rect_stroke(
                    rect.with_min_x(rect.min.x - FLASH_OUT),
                    2.0,
                    egui::Stroke::new(FLASH_WIDTH, color.gamma_multiply(glow)),
                    egui::StrokeKind::Inside,
                );
                if animate {
                    crate::rail::keep_animating(ui, rect);
                }
            }
        }
        // The selected text, behind the glyphs.
        if let Some(s) = sel.as_ref()
            && let Some((a, b)) = s.span(i, texts[i].chars().count())
        {
            let r = egui::Rect::from_min_max(
                egui::pos2(rect.left() + a as f32 * cw, rect.top()),
                egui::pos2(rect.left() + b as f32 * cw, rect.bottom()),
            );
            p.rect_filled(r, 0.0, sel_fill);
        }
        let top = rect.center().y - cell.y / 2.0;
        for (k, (at, s)) in segs[i].iter().enumerate() {
            let at = egui::pos2(rect.left() + *at as f32 * cw, top);
            let missing = k == ETA_SEG && line.is_some_and(|l| l.no_eta);
            // The use cell takes its row's colour, but a run killed by the
            // memory cap is flagged in amber, as a missing ETA is in dim.
            let oom = k == LEASE_SEG && line.is_some_and(|l| l.oom);
            // So is a Running row that nothing runs.
            let flag = k == NOW_SEG && line.is_some_and(|l| l.flag);
            let ink = if missing {
                chrome.dim
            } else if oom || flag {
                chrome.amber
            } else {
                color
            };
            if k == LEASE_SEG
                && let Some(line) = line
            {
                // A Running row's figures each by their weight on the
                // machine.
                for (col, run, ink) in usage_inks(s, line.usage_load, ink, chrome) {
                    shared.paint_text(&p, at + egui::vec2(col as f32 * cw, 0.0), &run, ink);
                }
                continue;
            }
            shared.paint_text(&p, at, s, ink);
        }
    }
    // A click on a row's fold toggle, or the space after it, folds or
    // unfolds what hangs under it and opens nothing.
    let clicked_line = input.clicked.and_then(|i| table.lines.get(i));
    let fold = clicked_line.and_then(|l| layout.fold_hit(l, input.click_col?));
    let clicked = clicked_line
        .filter(|_| fold.is_none())
        .map(|l| l.click.clone());
    let hover = input
        .hovered
        .map(|i| table.lines.get(i).map(|l| l.click.clone()));
    Drew {
        clicked,
        hover,
        fold,
    }
}

/// What [`draw_table`] saw this frame.
#[derive(Default)]
struct Drew {
    /// The row a click opens.
    clicked: Option<RowClick>,
    /// The pointer over the rows: `Some(None)` on the pane but on no row.
    hover: Option<Option<RowClick>>,
    /// A click on a row's fold toggle: that row's [`Fold::ident`].
    fold: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use giverny_claude::subagents::LiveSnapshot;

    const T0: u64 = 1_790_000_000_000; // an epoch-ms "now"

    fn feed(json: &str) -> Feed {
        feed::parse(json.as_bytes()).expect("test feed parses")
    }

    fn live(json: &str) -> Vec<SubagentRow> {
        let mut t = Tracker::new(None);
        t.apply_live(&LiveSnapshot::parse(json), T0);
        t.rows().to_vec()
    }

    #[test]
    fn countdown_goes_through_zero() {
        assert_eq!(countdown(20 * 60), "~20m");
        assert_eq!(countdown(63 * 60), "~1h3m");
        assert_eq!(countdown(-12 * 60), "-~12m");
        assert_eq!(countdown(-10), "~0m");
    }

    #[test]
    fn tokens_read_like_claude_codes_own() {
        assert_eq!(fmt_tokens(842), "842");
        assert_eq!(fmt_tokens(13_500), "13.5k");
        assert_eq!(fmt_tokens(124_800), "124.8k");
        assert_eq!(fmt_tokens(999_950), "1M");
        assert_eq!(fmt_tokens(1_234_567), "1.2M");
        assert_eq!(fmt_tokens(2_000), "2k");
    }

    /// Live rows from `(id, status, start offset s, parent)`: each named
    /// `Work <id>`, its parent as its meta would give it.
    fn family(rows: &[(&str, &str, u64, Option<&str>)]) -> Vec<SubagentRow> {
        let tasks: Vec<String> = rows
            .iter()
            .map(|(id, status, at, _)| {
                format!(
                    r#"{{"id":"{id}","status":"{status}","description":"Work {id}",
                       "startTime":{},"tokenCount":1000}}"#,
                    T0 - 600_000 + at * 1000
                )
            })
            .collect();
        let mut out = live(&format!(
            r#"{{"session_id":"s","tasks":[{}]}}"#,
            tasks.join(",")
        ));
        for r in &mut out {
            let (.., parent) = rows.iter().find(|(id, ..)| *id == r.id).unwrap();
            r.parent_id = parent.map(str::to_string);
        }
        out
    }

    fn titles(t: &Table) -> Vec<(String, usize, Stage)> {
        t.lines
            .iter()
            .map(|l| (l.title.clone(), l.depth, l.stage))
            .collect()
    }

    fn drawn_titles(t: &Table) -> Vec<String> {
        let cols = Cols::new(t, 160);
        t.lines
            .iter()
            .map(|l| cols.segments(l)[1].1.trim_end().to_string())
            .collect()
    }

    #[test]
    fn an_agent_a_worker_spawned_hangs_under_its_row() {
        let rows = family(&[
            ("w1", "running", 0, None),
            ("w2", "running", 10, None),
            ("c1", "running", 20, Some("w1")),
            ("c2", "completed", 30, Some("w1")),
        ]);
        let t = build(None, &rows, T0);
        assert_eq!(
            titles(&t),
            [
                ("Work w1".into(), 0, Stage::Running),
                ("Work c1".into(), 1, Stage::Running),
                ("Work c2".into(), 1, Stage::Done),
                ("Work w2".into(), 0, Stage::Running),
            ]
        );
        let fold = t.lines[0].fold.as_ref().expect("a toggle on the worker");
        assert_eq!((fold.under, fold.running, fold.folded), (2, 1, false));
        assert!(t.nested);
        // Each with its own state, time and tokens; the worker's count is
        // its own, not its agents' too.
        let c2 = &t.lines[2];
        assert_eq!(c2.stage, Stage::Done);
        assert_eq!(c2.tokens, "1k");
        assert_eq!(t.lines[0].tokens, "1k");
        assert_eq!(t.lines[1].elapsed, "9:40");
        // Indented under the toggle; a row of its own keeps the toggle's
        // room.
        assert_eq!(
            drawn_titles(&t),
            ["▾ Work w1", "   └ Work c1", "   └ Work c2", "  Work w2"]
        );
        assert!(t.lines[1].tip().contains("spawned by Work w1"));
        assert!(
            t.lines[0]
                .tip()
                .contains("2 agents of its own under it, 1 running")
        );
        // A click on it is any worker's: its own id and transcript.
        assert_eq!(t.lines[1].click.agent_id.as_deref(), Some("c1"));
        assert_eq!(t.lines[2].click.stage, Stage::Done);
    }

    #[test]
    fn the_feed_gives_ids_titles_etas_and_landings() {
        let rows = live(
            r#"{"session_id":"s","tasks":[{"id":"a1","status":"running",
                "startTime":1789999400000,"tokenCount":1000}]}"#,
        );
        let f = feed(
            r#"{"version":1,"session":"s","rows":[
              {"key":"acme#1","stage":"running","title":"FEATURE: one","agent_id":"a1",
               "started":1789999400000,"eta_s":1800},
              {"key":"acme#2","stage":"planned","title":"FEATURE: two","eta_s":3780},
              {"key":"acme#3","stage":"done","title":"BUG: three","started":1789990000000,
               "ended":1789993900000,"eta_s":3600,"landing":"Review — alice","tokens":5000}
            ]}"#,
        );
        let t = build(Some(&f), &rows, T0);
        let ids: Vec<&str> = t.lines.iter().map(|l| l.id.as_str()).collect();
        assert_eq!(ids, ["acme#1", "acme#2", "acme#3"]);
        // Running: 10m in, 30m estimate → 20m left.
        assert_eq!(t.lines[0].elapsed, "10:00");
        assert_eq!(t.lines[0].eta, "~20m");
        // Planned: its estimate, nothing else.
        assert_eq!(t.lines[1].eta, "~1h3m");
        assert_eq!(t.lines[1].elapsed, "");
        assert_eq!(t.lines[1].tokens, "");
        // Done: took 65m against 60m → five minutes late.
        assert_eq!(t.lines[2].elapsed, "1:05:00");
        assert_eq!(t.lines[2].eta, "(+5m)");
        assert_eq!(t.lines[2].now, "Review — alice");
    }

    // ------------------------------------------- one worker, tasks in turn ----

    // ------------------------------------------------- resources ----

    // ------------------------------------------------------ held clocks ----

    // --------------------------------------------------- selectable text ----
}
