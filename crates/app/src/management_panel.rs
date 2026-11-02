//! The management panel: a table under a tab's terminal listing that tab's
//! Claude Code subagents — Running and Done — one row each.
//!
//! Off by default (`claude.management_panel`); off, nothing here runs. On, it
//! is drawn as a resizable bottom panel inside the terminal's area, only in a
//! tab whose tracker holds a real subagent, running or done this session.
//!
//! The rows are a [`Tracker`]: Claude Code's own data (live list,
//! transcripts, notifications), nothing else. This module owns only the
//! look: the columns STAGE, TASK, ELAPSED, NOW and TOKENS, every one but TASK
//! a fixed width, no column headings, the whole row tinted by its stage, a
//! stopwatch ELAPSED ticking every second — a compact status table pinned
//! under Claude Code. The session's token total is not here: it is on the
//! status line.
//!
//! **The rows are not kept here.** [`show`] is handed the tab's tracker —
//! `ClaudeWatch::agents` (`management_live.rs`), fed by the relay, persisted,
//! emptied on `/clear` and refreshed once a second — and keeps only what the
//! pane itself needs per tab.
//!
//! **Clocks stop while nothing can run**: while the tab's account is out of
//! a usage limit ([`Limit`]), a Running row's ELAPSED stops and NOW says
//! `5h limit → 13:00`. The pane remembers each such span ([`Hold`]) and the
//! clock carries on from where it stopped once the limit resets. A worker
//! stopped on an API error stands still the same way, from its error until
//! its next turn. A Done row is measured history and never moves.
//!
//! **Nesting**: an agent a worker spawned is drawn under that worker's row,
//! indented a step per level, and goes where that row goes ([`nest`]); a
//! toggle at the head of the worker's title folds them away ([`Fold`]), kept
//! per tab while Giverny runs.
//!
//! **Newly done rows flash**: a look at a tab that wore the unseen-finish
//! tick breathes the frames of the rows that landed since the last look, a
//! few times, together ([`Views::flash_new_done`]).
//!
//! **Text is selectable**: a drag selects the pane's text the way the
//! terminal does, as a stream of cells across rows, copies it as plain text
//! line by line when the drag ends, and `Ctrl+Shift+C` copies it again. The
//! next press anywhere lets it go.
//!
//! **Clicks** produce a [`RowClick`], which the app receives as
//! `Action::AgentRowClicked`. What a click *does* is not decided here
//! (`row_open`). The row of the worker the tab shows is lit as the selection.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use eframe::egui::{self, Color32, CursorIcon, Sense, Ui};
use giverny_claude::subagents::{Outcome, Stage, SubagentRow, Tracker};
use giverny_core::config::{DoneRows, ManagementPanelConfig, PaneColumns};
use giverny_core::tabs::TabId;
use giverny_term::widget::RenderShared;

use crate::claude_watch::{ClaudeWatch, LiveUsage};

/// The stage tints, the 256-colour codes 38;5;32 and 38;5;28, picked to read
/// on dark and light themes alike.
pub const RUNNING: Color32 = Color32::from_rgb(0x00, 0x87, 0xd7);
pub const DONE: Color32 = Color32::from_rgb(0x00, 0x87, 0x00);

// Column widths, in characters.
const STAGE_W: usize = 7;
const EL_W: usize = 8;
const NOW_W: usize = 28;
const TOK_W: usize = 6;
const GAP: usize = 2;
const MIN_TITLE: usize = 12;

// --------------------------------------------------------- view state ----

/// What the pane itself keeps per tab. Never the rows — those are the
/// tracker's.
#[derive(Default)]
struct View {
    /// The height the pane last sized itself to, while the user has not
    /// dragged it; `None` once they have.
    fit: Option<f32>,
    /// Every usage-limit span this tab's clocks were held through.
    holds: Vec<Hold>,
    /// The text dragged over, until the next press.
    sel: Option<Selection>,
    /// The Done rows this tab's pane has shown, by agent id: what of it was
    /// seen, kept up every frame it is drawn. `None` until it is first
    /// drawn.
    seen_done: Option<HashSet<String>>,
    /// The tab was just looked at with its unseen-finish tick on
    /// ([`Views::flash_new_done`]): the next drawing flashes what landed
    /// since the last look.
    flash_due: bool,
    /// The newly done rows breathing now, if any.
    flash: Option<Flash>,
    /// The rows whose agents' own agents are folded away under them, by
    /// agent id: kept for as long as the tab is.
    folded: HashSet<String>,
}

/// The newly done rows' frames breathing together: which rows (by agent id)
/// and from when, on the animation clock.
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

/// The Done rows of `lines` that landed since the pane was last looked at:
/// those it never showed (`seen`). A pane not drawn since Giverny started
/// (`seen` is `None`) knows nothing of what was seen before, so only rows
/// that landed since then (`born_ms`) count as new.
fn newly_done(lines: &[Line], seen: Option<&HashSet<String>>, born_ms: u64) -> HashSet<String> {
    lines
        .iter()
        .filter(|l| l.stage == Stage::Done)
        .filter(|l| match seen {
            Some(seen) => !seen.contains(&l.agent_id),
            None => l.landed_ms.is_some_and(|e| e >= born_ms),
        })
        .map(|l| l.agent_id.clone())
        .collect()
}

/// Note every Done row of `lines` as seen: the pane is showing them.
fn note_seen(seen: &mut Option<HashSet<String>>, lines: &[Line]) {
    let seen = seen.get_or_insert_with(HashSet::new);
    for l in lines.iter().filter(|l| l.stage == Stage::Done) {
        seen.insert(l.agent_id.clone());
    }
}

/// Every tab's pane view state.
pub struct Views {
    tabs: HashMap<TabId, View>,
    /// When Giverny started (epoch ms): a pane not drawn since knows only
    /// that rows landed after it went unseen ([`newly_done`]).
    born_ms: u64,
}

impl Default for Views {
    fn default() -> Self {
        Views {
            tabs: HashMap::new(),
            born_ms: now_ms(),
        }
    }
}

impl Views {
    /// The user looked at `tab` with its unseen-finish tick on: when its
    /// pane is next drawn, the Done rows that landed since they last looked
    /// breathe their frames together a few times, then go still.
    pub fn flash_new_done(&mut self, tab: TabId) {
        self.tabs.entry(tab).or_default().flash_due = true;
    }

    /// Forget a closed tab.
    pub fn forget(&mut self, tab: TabId) {
        self.tabs.remove(&tab);
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

/// Milliseconds of `[start, upto]` that fall inside a hold — what comes off
/// a row's clock. Holds never overlap: one opens only once the last closed.
pub fn held_ms(holds: &[Hold], start: u64, upto: u64) -> u64 {
    holds
        .iter()
        .map(|h| {
            let a = h.since_ms.max(start);
            let b = h.until_ms.unwrap_or(upto).min(upto);
            b.saturating_sub(a)
        })
        .sum()
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
    type Pick = fn(&LiveUsage) -> Option<f64>;
    type Reset = fn(&LiveUsage) -> Option<jiff::Timestamp>;
    let meters: [(&str, &'static str, Pick, Reset); 2] = [
        ("session", "5h", |l| l.five_hour, |l| l.five_hour_resets),
        ("weekly_all", "7d", |l| l.seven_day, |l| l.seven_day_resets),
    ];
    let mut windows: Vec<(&'static str, f64, Option<jiff::Timestamp>)> = Vec::new();
    if let Some(panel) = account {
        for (kind, short, pick, reset) in meters {
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
    pub limit: Option<&'a Limit>,
    pub tz: jiff::tz::TimeZone,
}

#[cfg(test)]
impl Clock<'_> {
    fn plain() -> Clock<'static> {
        Clock {
            holds: &[],
            limit: None,
            tz: jiff::tz::TimeZone::system(),
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ------------------------------------------------------------- clicks ----

/// What a click on a row names: everything the action behind it
/// (`row_open`) could want, so deciding what to do never needs the pane
/// again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowClick {
    pub stage: Stage,
    /// The Claude Code subagent id.
    pub agent_id: String,
    /// The worker's name or description, for its page's header.
    pub name: String,
    /// The worker's `agent-<id>.jsonl`, once known.
    pub transcript: Option<PathBuf>,
}

impl RowClick {
    /// Whether `other` is a click on the same row: the same worker. The
    /// stage is left out: a row viewed while Running is still that row once
    /// Done.
    pub fn same_row(&self, other: &RowClick) -> bool {
        self.agent_id == other.agent_id
    }
}

// -------------------------------------------------------------- table ----

/// One drawn row, every cell already formatted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub stage: Stage,
    /// The subagent's id: `agent-<id>.jsonl`, `tasks[].id`.
    pub agent_id: String,
    /// The TASK column's first word: the agent's name, else its model.
    pub id: String,
    pub title: String,
    pub elapsed: String,
    pub now: String,
    pub tokens: String,
    /// When a Done row landed (epoch ms), when known; `None` on any other
    /// row.
    pub landed_ms: Option<u64>,
    /// The hover's lines under the title: where the row is and why, in
    /// words ([`running_why`], [`done_why`]).
    pub why: Vec<String>,
    /// How deep under another row it hangs: 0 for a row of its own, 1 for an
    /// agent a worker spawned, 2 for one that agent spawned. Its title is
    /// drawn indented a step per level.
    pub depth: usize,
    /// The row's agent spawned agents of its own, drawn under it: whether
    /// they are folded away and how many there are.
    pub fold: Option<Fold>,
    pub click: RowClick,
}

/// A row whose agent spawned agents of its own: the toggle on its row folds
/// them away under it or shows them again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fold {
    /// What the pane keeps the fold by: the agent's id.
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

/// The whole table.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Table {
    pub lines: Vec<Line>,
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

/// Format with no limit ever seen, every Done row shown and nothing folded.
/// Pure: `now_ms` in, text out.
#[cfg(test)]
pub fn build(live: &[SubagentRow], now_ms: u64) -> Table {
    build_with(
        live,
        now_ms,
        &Clock::plain(),
        DoneRows::All,
        &HashSet::new(),
    )
}

/// The table for `live` at `now_ms`, its clocks held through `clock`'s
/// usage-limit spans, showing only the Done rows `done` keeps
/// (`management_panel.done_rows`).
///
/// An agent another drawn agent spawned hangs under that agent's row
/// ([`nest`]), unless that row is in `folded`.
pub fn build_with(
    live: &[SubagentRow],
    now_ms: u64,
    clock: &Clock,
    done: DoneRows,
    folded: &HashSet<String>,
) -> Table {
    let nested = nest(live, done, folded);
    let lines: Vec<Line> = nested
        .iter()
        .map(|n| {
            let mut line = format_row(&live[n.at], now_ms, clock);
            line.depth = n.depth;
            line.fold = n.fold.clone();
            if let Some(f) = &n.fold {
                line.why.push(fold_why(f));
            }
            if let Some(p) = &n.parent {
                line.why.push(format!("spawned by {p}"));
            }
            line
        })
        .collect();
    Table {
        nested: lines.iter().any(|l| l.fold.is_some()),
        lines,
    }
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

/// The row each row hangs under, by place: the row of the agent that
/// spawned it ([`SubagentRow::parent_id`]). An agent whose spawner holds no
/// row stands on its own, and so does one caught in a loop of parents
/// (never seen; never trusted).
fn parent_rows(rows: &[SubagentRow]) -> Vec<Option<usize>> {
    let mut up: Vec<Option<usize>> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let p = r.parent_id.as_deref()?;
            rows.iter()
                .enumerate()
                .position(|(j, o)| j != i && o.id == p)
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

/// Place the rows as the pane draws them: each agent another agent spawned
/// right under its spawner's row, a level deeper, in the order the rows
/// came; its spawner's Running or Done row carries it wherever that row is.
/// A row in `folded` hides everything under it and says how much
/// ([`Fold`]).
///
/// `done` keeps the newest Done rows of their own (`Last(n)` counts only
/// those) and what hangs under them. What hung under a Done row that is
/// not kept goes with it, but an agent still running there stands on its
/// own instead: a running agent is never hidden by the Done setting.
/// `Hide` hides every Done row, nested or not.
fn nest(rows: &[SubagentRow], done: DoneRows, folded: &HashSet<String>) -> Vec<Nested> {
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
    fn under(i: usize, kids: &[Vec<usize>], rows: &[SubagentRow]) -> (usize, usize) {
        kids[i].iter().fold((0, 0), |(all, run), &k| {
            let (a, r) = under(k, kids, rows);
            let me = usize::from(rows[k].stage == Stage::Running);
            (all + 1 + a, run + me + r)
        })
    }
    fn place(
        i: usize,
        depth: usize,
        parent: Option<String>,
        kids: &[Vec<usize>],
        rows: &[SubagentRow],
        folded: &HashSet<String>,
        out: &mut Vec<Nested>,
    ) {
        let fold = (!kids[i].is_empty()).then(|| {
            let (under, running) = under(i, kids, rows);
            let ident = rows[i].id.clone();
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
                let name = Some(rows[i].display_name().to_string());
                place(k, depth + 1, name, kids, rows, folded, out);
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

fn format_row(l: &SubagentRow, now_ms: u64, clock: &Clock) -> Line {
    // A named agent shows its name and description; any other its model
    // and description.
    let (id, title) = match (&l.name, &l.description) {
        (Some(n), Some(d)) => (n.clone(), d.clone()),
        _ => (
            l.model.as_deref().and_then(short_model).unwrap_or_default(),
            l.display_name().to_string(),
        ),
    };
    let start = l.started_ms;
    // A worker stopped on an API error stands still from then, and its
    // error spans are off its clock.
    let stopped = l.stopped().filter(|_| l.stage == Stage::Running);
    // Where a Running row's clock stands now.
    let stop = match (l.stage, start) {
        (Stage::Running, Some(s)) => Some(match &stopped {
            Some((since, _)) => now_ms.min((*since).max(s)),
            None => now_ms,
        }),
        _ => None,
    };
    let elapsed = match (l.stage, start) {
        // The work so far: wall time up to where its clock stands, less
        // every span the account was out of its limit or the worker stood
        // on an error.
        (Stage::Running, Some(s)) => {
            let stop = stop.unwrap_or(now_ms);
            let from = s.min(stop);
            stopwatch(
                stop.saturating_sub(from)
                    .saturating_sub(held_ms(clock.holds, from, stop))
                    .saturating_sub(l.stopped_ms(from, stop))
                    / 1000,
            )
        }
        (Stage::Done, Some(s)) => match l.ended_ms {
            Some(e) => stopwatch(e.saturating_sub(s).saturating_sub(l.stopped_ms(s, e)) / 1000),
            None => String::new(),
        },
        _ => String::new(),
    };
    let limit = clock.limit.filter(|l| l.out_at(now_ms));
    let now = match l.stage {
        // The limit first, then a stop on an API error, then what it does.
        Stage::Running => match (limit, &stopped) {
            (Some(limit), _) => limit_note(limit, now_ms, &clock.tz),
            (None, Some((_, why))) => why.clone(),
            (None, None) => l.activity.clone().unwrap_or_default(),
        },
        Stage::Done => l.outcome.map(outcome_word).unwrap_or_default(),
    };
    let tokens = l.shown_tokens().map(fmt_tokens).unwrap_or_default();
    // The hover's lines under the title: where the row is and why, from the
    // cells above.
    let why = match l.stage {
        Stage::Running => {
            let mut who = format!("worker {}", l.display_name());
            if let Some(m) = l.model.as_deref().and_then(short_model) {
                who.push_str(&format!(" ({m})"));
            }
            let since = start.map(|s| clock_at(s, now_ms, &clock.tz));
            let state = match (limit, &stopped) {
                (Some(lim), _) => Some(format!(
                    "{} — out of usage; its clock is held",
                    limit_note(lim, now_ms, &clock.tz)
                )),
                (None, Some((_, why))) => Some(format!("{why} — its worker is not working")),
                (None, None) => None,
            };
            // What the worker is doing right now, whole: NOW cuts it to fit.
            let doing = l.activity.clone().filter(|a| !a.is_empty());
            doing
                .into_iter()
                .chain(running_why(&who, since.as_deref(), state))
                .collect()
        }
        Stage::Done => done_why(&now, &elapsed),
    };
    Line {
        stage: l.stage,
        agent_id: l.id.clone(),
        id,
        title,
        elapsed,
        now,
        tokens,
        landed_ms: l.ended_ms.filter(|_| l.stage == Stage::Done),
        why,
        depth: 0,
        fold: None,
        click: RowClick {
            stage: l.stage,
            agent_id: l.id.clone(),
            name: l.display_name().to_string(),
            transcript: l.transcript.clone(),
        },
    }
}

/// A Running row's hover: who works on it and since when, and what holds it
/// when something does (`state`).
fn running_why(who: &str, since: Option<&str>, state: Option<String>) -> Vec<String> {
    let mut head = format!("Running: {who}");
    if let Some(s) = since {
        head.push_str(&format!(" since {s}"));
    }
    std::iter::once(head).chain(state).collect()
}

/// A Done row's hover: how it ended (`now`, the row's NOW) and how long it
/// took.
fn done_why(now: &str, elapsed: &str) -> Vec<String> {
    let mut head = "Done".to_string();
    // An ending that says only done is not said twice.
    if !now.is_empty() && !now.eq_ignore_ascii_case("done") {
        head.push_str(&format!(": {now}"));
    }
    if !elapsed.is_empty() {
        head.push_str(&format!("; took {elapsed}"));
    }
    vec![head]
}

/// A model id in Claude Code's short names (`haiku`, `sonnet`, `opus`,
/// `fable`), without version or date; None for a name of no known family.
fn short_model(m: &str) -> Option<String> {
    let m = m.to_ascii_lowercase();
    ["haiku", "sonnet", "opus", "fable"]
        .into_iter()
        .find(|f| m.contains(f))
        .map(str::to_string)
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
        Stage::Done => "Done",
    }
}

fn stage_color(s: Stage) -> Color32 {
    match s {
        Stage::Running => RUNNING,
        Stage::Done => DONE,
    }
}

/// Start column that makes `s` end at column `end` (right alignment on the
/// cell grid).
fn right_at(end: usize, s: &str) -> usize {
    end.saturating_sub(s.chars().count())
}

/// Cut to `max` characters, the last one an ellipsis.
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
/// clicked this frame, and the row of `shown` as the pane drew it.
///
/// `limit` is the usage limit the tab's account is out on ([`limit_for`]):
/// while it is, the Running rows' clocks are held. `shown` is the row whose
/// worker page the tab shows: it is lit as the selection.
#[allow(clippy::too_many_arguments)]
pub fn show(
    views: &mut Views,
    tab: TabId,
    tracker: Option<&Tracker>,
    shown: Option<&RowClick>,
    limit: Option<Limit>,
    panel: &ManagementPanelConfig,
    shared: &mut RenderShared,
    ui: &mut Ui,
) -> (Option<RowClick>, Option<Line>) {
    // A look asks for one flash, at the drawing right after it: a pane with
    // nothing to show now does not flash later.
    let flash_due = views
        .tabs
        .get_mut(&tab)
        .is_some_and(|v| std::mem::take(&mut v.flash_due));
    // Only when subagents are involved.
    let Some(tracker) = tracker.filter(|t| !t.is_empty()) else {
        return (None, None);
    };
    let view = views.tabs.entry(tab).or_default();
    let now = now_ms();
    track(&mut view.holds, limit.as_ref(), now);
    let clock = Clock {
        holds: &view.holds,
        limit: limit.as_ref(),
        tz: jiff::tz::TimeZone::system(),
    };
    let table = build_with(tracker.rows(), now, &clock, panel.done(), &view.folded);
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
    let shown_line = shown.and_then(|s| table.lines.iter().find(|l| s.same_row(&l.click)).cloned());
    if table.any_running() {
        // ELAPSED is a stopwatch.
        ui.ctx().request_repaint_after(Duration::from_secs(1));
    }

    // The session's own cell: the pane is drawn in the terminal's glyphs
    // (face, size, hinting, grid), so it follows zoom and the font setting.
    let cell = shared.cell_size(ui.ctx().pixels_per_point());
    // A little air around each row; a table, not a wall of grid.
    let row_h = (cell.y * 1.2).round().max(cell.y);
    let frame = pane_frame(shared.theme.bg);
    let want = pane_height(
        table.lines.len(),
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
                        shown,
                        flash,
                        animate,
                        shared,
                        cell,
                        row_h,
                        cols,
                        &mut view.sel,
                    );
                    clicked = drew.clicked;
                    if let Some(ident) = drew.fold {
                        if !view.folded.remove(&ident) {
                            view.folded.insert(ident);
                        }
                        ui.ctx().request_repaint();
                    }
                });
            outer.advance_cursor_after_rect(inner.min_rect());
        });
    (clicked, shown_line)
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

/// How wide a newly done row's breathing frame is, in points.
const FLASH_WIDTH: f32 = 1.5;
/// How far out past the row's left edge the frame sits, in points: clear of
/// the row's first glyph, and within the scroll area's clip margin, so it is
/// not cut. The right edge is the row's own, which ends clear of its last
/// glyph and at the clip.
const FLASH_OUT: f32 = 3.0;

/// How strongly a hovered row is tinted.
const HOVER_TINT: f32 = 0.10;
/// How strongly a pressed row is tinted.
const PRESS_TINT: f32 = 0.22;
/// How strongly the row of the worker on view is lit: past a hover's, so
/// it reads as the selection even under the pointer.
const VIEWED_TINT: f32 = 0.26;

/// The scrollbar's full lane, in points, whether or not it is showing.
fn bar_lane(ui: &Ui) -> f32 {
    let bar = &ui.spacing().scroll;
    bar.bar_inner_margin + bar.bar_width + bar.bar_outer_margin
}

/// How many cells Claude Code leaves free across a status line: two at the
/// left, two at the right.
const CLAUDE_MARGIN: usize = 4;

/// How many cells in from the pane's left edge the table starts: as many
/// as Claude Code leaves at the left of its status line, which it draws
/// two columns into the terminal right above the pane.
const INSET: usize = CLAUDE_MARGIN / 2;

/// How many character columns the table lays out in, from the pane's width
/// *outside* the scroll area, which is the terminal's: the pane spans the
/// terminal's area, and is drawn on its grid ([`INSET`] whole cells in).
/// The table is as wide as the status line Claude Code draws over it (the
/// terminal's columns less [`CLAUDE_MARGIN`]), so its last column ends
/// where the status line does. The bar's lane is always left free, so it
/// never paints over a cell and the columns never move when it appears; on
/// the usual sizes it fits in the two cells right of the table anyway.
fn table_cols(width: f32, cell_w: f32, ppp: f32, bar_lane: f32) -> usize {
    let cw = cell_w.max(0.1);
    // The terminal counts its columns in whole device pixels
    // (`TermView::show`); counted the same way, the two never disagree.
    let cell_px = (cw * ppp).round().max(1.0);
    let terminal = (width * ppp / cell_px).floor() as usize;
    let under_line = terminal.saturating_sub(CLAUDE_MARGIN);
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
    x_now: usize,
    x_tok_end: usize,
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
        // The fixed columns right of TASK, each with the gap before it.
        let fixed_right: usize = [(on.elapsed, EL_W), (on.now, NOW_W), (on.tokens, TOK_W)]
            .iter()
            .filter(|(shown, _)| *shown)
            .map(|(_, w)| w + GAP)
            .sum();
        let stage_cols = if on.stage { STAGE_W + GAP } else { 0 };
        // TASK is the id and the title; with the title off, just the id.
        // However narrow the pane, it holds the id and, on every row, its
        // nesting, a few cells of title and a folded row's short note
        // (`+3`), so nothing spills into ELAPSED: the table runs off the
        // right instead.
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
        let x_now = place(on.now, NOW_W);
        let x_tok_end = place(on.tokens, TOK_W) + TOK_W;
        Cols {
            on: *on,
            idw,
            taskw,
            x_task,
            x_el_end,
            x_now,
            x_tok_end,
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
    /// Always the same five, in this order; a column switched off is an
    /// empty cell.
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
        vec![
            cell(on.stage, 0, stage_word(line.stage).to_string()),
            cell(self.taskw > 0, self.x_task, task),
            // Right-aligned: the text ends at the column's last cell.
            cell(
                on.elapsed,
                right_at(self.x_el_end, &line.elapsed),
                line.elapsed.clone(),
            ),
            cell(on.now, self.x_now, cut(&line.now, NOW_W)),
            cell(
                on.tokens,
                right_at(self.x_tok_end, &line.tokens),
                line.tokens.clone(),
            ),
        ]
    }
}

/// How far each level of nesting moves a title in.
const NEST_W: usize = 3;

/// The least of a title kept beside a folded row's note before the note
/// shortens: `Work …`.
const TITLE_KEEP: usize = 6;

/// A row's title cell, `w` cells wide: its nesting, the title, and a folded
/// row's note, never wider than `w`. The nesting is never cut
/// ([`Cols::shown`] leaves it room). As the cell narrows the title gives way
/// first, down to [`TITLE_KEEP`]; then the note shortens (`+3, 2 running`
/// to `+3`); then it drops and the title takes it all.
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

/// What a title is drawn behind: its level's indent, then its fold toggle,
/// or `└` for a nested row with nothing under it. A row of its own with
/// nothing under it has none — but leaves the toggle's room while another
/// row has one (`nested`), so the titles stay aligned.
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

/// A drag over the pane's text, in `(row, column)` cells. The terminal's own
/// model: a stream from where the drag began to where the pointer is,
/// copied when the drag ends.
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
    /// The pointer is down on the rows.
    pressed: bool,
    dragging: bool,
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

/// The pane's pointer handling over rows at `rects` reading `texts`: a drag
/// selects text and copies it as it ends, the terminal's copy-on-select;
/// `Ctrl+Shift+C` copies the selection again. A click (press and release in
/// place) is reported: it opens a row or flips its fold. A drag is never a
/// click.
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
    let mut copy = resp.drag_stopped_by(egui::PointerButton::Primary);
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
            ui.ctx().copy_text(text);
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
    out.pressed = resp.is_pointer_button_down_on();
    if !out.dragging {
        out.hovered = resp
            .hover_pos()
            .filter(|p| area.contains(*p))
            .and_then(|p| at(Some(p)))
            .map(|(r, _)| r);
    }
    if out.dragging {
        ui.ctx().set_cursor_icon(CursorIcon::Text);
    }
    Some((out, resp))
}

/// What [`draw_table`] saw this frame.
#[derive(Default)]
struct Drew {
    /// The row a click opens.
    clicked: Option<RowClick>,
    /// A click on a row's fold toggle: that row's [`Fold::ident`].
    fold: Option<String>,
}

/// Draw the rows, `viewed`'s lit; return the row a click opens, or the fold
/// a click on a row's toggle flips.
#[allow(clippy::too_many_arguments)]
fn draw_table(
    ui: &mut Ui,
    id: egui::Id,
    table: &Table,
    shown: &PaneColumns,
    viewed: Option<&RowClick>,
    flash: Option<(&HashSet<String>, f32)>,
    animate: bool,
    shared: &mut RenderShared,
    cell: egui::Vec2,
    row_h: f32,
    cols: usize,
    sel: &mut Option<Selection>,
) -> Drew {
    let cw = cell.x.max(1.0);
    let layout = Cols::shown(table, cols, shown);
    let segs: Vec<Vec<(usize, String)>> = table.lines.iter().map(|l| layout.segments(l)).collect();
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
    if input.hovered.is_some() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    // Every row's hover reads its whole title and why it is where it is.
    // Every row shares the one response, so the tooltip is anchored to the
    // hovered row's rect: it sits under that row, and moving to another row
    // keeps it open (same id, the pointer still inside) with the new text.
    // egui keeps it closed while a drag selects or just after a click.
    if let Some(i) = input.hovered {
        let mut row = resp.clone();
        row.rect = rects[i];
        row.interact_rect = rects[i];
        row.on_hover_text(table.lines[i].tip());
    }
    let sel_fill = ui.visuals().selection.bg_fill;
    let p = ui.painter().clone();

    for (i, rect) in rects.iter().copied().enumerate() {
        let line = &table.lines[i];
        let color = stage_color(line.stage);
        // The worker whose page the terminal's place shows: held lit, with
        // a bar at its left edge, as a selection.
        if viewed.is_some_and(|v| v.same_row(&line.click)) {
            p.rect_filled(rect, 2.0, color.gamma_multiply(VIEWED_TINT));
            // In the pane's left margin, clear of the text.
            let bar = egui::Rect::from_min_size(
                rect.min - egui::vec2(6.0, 0.0),
                egui::vec2(3.0, rect.height()),
            );
            p.rect_filled(bar, 1.0, color);
        }
        if input.hovered == Some(i) {
            let tint = if input.pressed {
                PRESS_TINT
            } else {
                HOVER_TINT
            };
            p.rect_filled(rect, 2.0, color.gamma_multiply(tint));
        }
        // Landed since the last look: its frame breathes with the others'.
        // Painted only, over the row and a hair out to its left so it clears
        // the first glyph: nothing moves. Kept moving only while the row is
        // on screen.
        if let Some((rows, glow)) = flash
            && line.stage == Stage::Done
            && rows.contains(&line.agent_id)
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
        for (at, s) in &segs[i] {
            let at = egui::pos2(rect.left() + *at as f32 * cw, top);
            shared.paint_text(&p, at, s, color);
        }
    }
    // A click on a row's fold toggle, or the space after it, folds or
    // unfolds what hangs under it and opens nothing.
    let clicked_line = input.clicked.and_then(|i| table.lines.get(i));
    let fold = clicked_line.and_then(|l| layout.fold_hit(l, input.click_col?));
    let clicked = clicked_line
        .filter(|_| fold.is_none())
        .map(|l| l.click.clone());
    Drew { clicked, fold }
}

#[cfg(test)]
mod tests {
    use super::*;
    use giverny_claude::subagents::LiveSnapshot;

    const T0: u64 = 1_790_000_000_000; // an epoch-ms "now"

    fn live(json: &str) -> Vec<SubagentRow> {
        let mut t = Tracker::new(None);
        t.apply_live(&LiveSnapshot::parse(json), T0);
        t.rows().to_vec()
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
        let t = build(&rows, T0);
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
        assert_eq!(t.lines[1].click.agent_id, "c1");
        assert_eq!(t.lines[2].click.stage, Stage::Done);
    }
}
