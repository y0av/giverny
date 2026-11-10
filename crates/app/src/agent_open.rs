//! What a click on a management-panel row opens.
//!
//! The pane hands the app a [`RowClick`]; this module decides, without
//! touching the app, what that click means — so the decision is tested here
//! and `main.rs::apply` only carries it out:
//!
//! * A **Running** worker opens straight into its view in the parent's
//!   Claude Code: Giverny types the keys that
//!   put Claude Code in that subagent's interactive view, driven by
//!   [`Walk`], which reads the parent's screen after every key rather than
//!   typing blind. With the management panel on, the relay is asked to show
//!   Claude Code's agent strip while the walk runs (`hooks::show_strip`),
//!   since the strip is the only keyboard path to a worker's view; `/tasks`
//!   is never used. The tab's picture is held from the click until the
//!   view is final ([`Settle`]), so the steps between are never drawn, and
//!   a one-column [`Nudge`] of the pty's width brings the relay's run
//!   forward so the hold is short. Only when that cannot start does the
//!   row open Giverny's page of the worker below.
//! * A Running row whose worker has **finished** but is not landed yet
//!   ([`RowClick::finished`]) is not walked into: Claude Code has dropped
//!   that worker from its list, so a walk could only wait out [`DEADLINE`]
//!   and say it may have left. It opens as a Done row does.
//! * **Done** workers (and a Running one that cannot be opened) open
//!   Giverny's page of the worker in the tab (`worker_page`):
//!   it takes the tab as the running view does, under the same header and
//!   "back to manager" button, and draws the worker's transcript as Claude
//!   Code draws a worker's own view (`giverny_claude::transcript`;
//!   `giverny transcript` renders it the same way): a Running one live,
//!   following it as it grows; a Done one opened at its end, on the final
//!   report. Nothing is typed into Claude Code.
//! * While a tab shows a worker's view, Esc and the terminal's "back to
//!   manager" button walk it back to the main view ([`Walk::home`]),
//!   and the strip's own `main` row is not drawn
//!   ([`row_marks`]).
//! * Claude Code keeps no view of a worker whose turn ended (it leaves the
//!   agent strip and `/tasks`, and its transcript is not a resumable
//!   session): the one way back in is the parent's `SendMessage`. So the
//!   page of such a worker (a finished, asleep or Done row's) has a prompt
//!   box, and a line typed there goes to the manager's prompt as one plain
//!   line naming the worker ([`Ask`], put there by [`Forward`]); the
//!   manager passes it on. Nothing is typed but what the user typed: the
//!   old **Revive**, which typed a canned line for them, is gone.
//! * **Planned** shows the row's brief on the same page, drawn as the
//!   worker's prompt; without one, its task, title and note as that prompt,
//!   and how a dispatcher adds a brief. Its box asks the manager to start
//!   the task now, the line typed there added to its brief.
//! * A Running row that names no worker but has a feed `open` command runs
//!   it in a new tab — unless it resumes a conversation something is already
//!   running, which two claudes on one transcript would interleave.
//!
//! A row with none of these still answers the click, with a page saying
//! what is missing, rather than doing nothing.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use giverny_claude::feed::Stage;

use crate::management_panel::RowClick;

/// The largest brief read; past it the page shows the head and says so.
/// A safety net, not a fold: no brief comes near it.
pub const BRIEF_MAX: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Show a worker's transcript on its page; `live` follows it.
    Watch {
        title: String,
        transcript: PathBuf,
        live: bool,
    },
    /// Run the feed's `open` command in a new tab titled `title`.
    Run { title: String, command: String },
    /// Show text on the page.
    Show { title: String, body: Body },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    /// A brief on disk, read when the page opens, and shown as the
    /// worker's prompt.
    File(PathBuf),
    /// A Next up row with no brief: what it has (task, title, note) as the
    /// prompt, then how a dispatcher adds a brief.
    Unbriefed {
        prompt: String,
        hint: String,
    },
    Text(String),
}

/// What a click on the row can open besides the page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Offer {
    Nothing,
    /// A Running worker: open it in the parent's Claude Code (`Attach`).
    OpenInClaude {
        agent_id: String,
    },
}

/// The row's title for a tab or page: `acme#158 · name`, whichever exist.
pub fn title_of(click: &RowClick) -> String {
    match (click.key.is_empty(), click.name.is_empty()) {
        (false, false) if click.key != click.name => format!("{} · {}", click.key, click.name),
        (false, _) => click.key.clone(),
        (true, false) => click.name.clone(),
        (true, true) => click
            .agent_id
            .clone()
            .unwrap_or_else(|| "agent".to_string()),
    }
}

/// What a tab's status line is told while its Claude Code shows a worker's
/// view: enough for the line to be that worker's alone — its
/// task name (the management-panel row's key, `g477`; never the
/// description), its agent id and its transcript, for its model and tokens.
/// `agent` is the worker the view's label names in the tab's tracker; the
/// row's own agent id wins over its. `None` on the manager's own view
/// (`label` none), where the line is the manager's session's. The tab keeps
/// its own title either way: it is still the manager's place.
///
/// `label` is read off the screen ([`viewing_worker`], [`prompt_box`]), so
/// it follows the view however it was left: the pane's back button, a
/// second click on the row, Esc or ← typed by hand, the strip's `main`.
pub fn viewed(
    label: Option<&str>,
    row: Option<&RowClick>,
    agent: Option<&giverny_claude::subagents::SubagentRow>,
) -> Option<giverny_claude::hooks::Viewing> {
    let label = label?;
    let given = |s: &Option<String>| s.clone().filter(|s| !s.trim().is_empty());
    Some(giverny_claude::hooks::Viewing {
        task: row.map(|r| r.key.clone()).filter(|k| !k.trim().is_empty()),
        agent_id: row
            .and_then(|r| given(&r.agent_id))
            .or_else(|| agent.map(|a| a.id.clone())),
        transcript: row
            .and_then(|r| r.transcript.clone())
            .or_else(|| agent.and_then(|a| a.transcript.clone())),
        label: agent
            .and_then(|a| given(&a.name))
            .or_else(|| Some(label.trim().to_string())),
    })
}

fn agent_id(click: &RowClick) -> Option<&str> {
    click.agent_id.as_deref().filter(|id| !id.is_empty())
}

/// Decide what a click does.
pub fn plan(click: &RowClick) -> Plan {
    let title = title_of(click);
    let open = click
        .open
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty());
    match click.stage {
        Stage::Running | Stage::Done => {
            // A finished worker's transcript is final: opened at its end,
            // on the report, as a Done row's is. An asleep one's is not: it
            // goes on when the worker wakes, so it is followed.
            let live = click.stage == Stage::Running && (!click.finished || click.asleep);
            if let Some(t) = &click.transcript {
                return Plan::Watch {
                    title,
                    transcript: t.clone(),
                    live,
                };
            }
            let id = agent_id(click);
            if live
                && id.is_none()
                && let Some(cmd) = open
            {
                return Plan::Run {
                    title,
                    command: cmd.to_string(),
                };
            }
            let mut why = match (id, live) {
                (Some(id), true) => format!(
                    "No transcript for worker {id} yet — Claude Code writes it once the \
                     worker's first turn lands. Click again in a moment."
                ),
                (Some(id), false) => {
                    format!("No transcript was found for worker {id}, so there is nothing to show.")
                }
                (None, _) if open.is_some() => {
                    "This row names no worker, so there is no transcript to show.".to_string()
                }
                (None, _) => "This row names no worker and no `open` command, so there is \
                              nothing to open."
                    .to_string(),
            };
            if let Some(cmd) = open.filter(|_| !live) {
                why.push_str(&format!("\n\nThe row's open command:\n  {cmd}"));
            }
            Plan::Show {
                title,
                body: Body::Text(note_then(click, &why)),
            }
        }
        Stage::Planned => {
            if let Some(b) = &click.brief {
                return Plan::Show {
                    title,
                    body: Body::File(b.clone()),
                };
            }
            let (prompt, hint) = planned_without_brief(click);
            Plan::Show {
                title,
                body: Body::Unbriefed { prompt, hint },
            }
        }
    }
}

/// A Next up row with no brief still says what it has: its
/// task, its title and its note (the prompt it has so far), then how a
/// dispatcher gives it a brief.
fn planned_without_brief(click: &RowClick) -> (String, String) {
    let some = |s: &str| (!s.trim().is_empty()).then(|| s.trim().to_string());
    let mut parts: Vec<String> = Vec::new();
    if let Some(k) = some(&click.key) {
        parts.push(format!("Task: {k}"));
    }
    if let Some(t) = some(&click.name).filter(|t| *t != click.key.trim()) {
        parts.push(t);
    }
    if let Some(n) = click.note.as_deref().and_then(some) {
        parts.push(n);
    }
    let hint = "No brief was given for this row. The dispatcher adds one with \
         `giverny-manage plan <task> --eta <min> --brief FILE` (or `start … --brief FILE`)."
        .to_string();
    (parts.join("\n\n"), hint)
}

/// What this click can open in Claude Code. A finished worker is no longer
/// in Claude Code's list, so there is nothing to open it in.
pub fn offer(click: &RowClick) -> Offer {
    match (click.stage, agent_id(click)) {
        (Stage::Running, Some(id)) if !click.finished => Offer::OpenInClaude {
            agent_id: id.to_string(),
        },
        _ => Offer::Nothing,
    }
}

/// The conversation a Done row's `open` command resumes, if it does: a
/// click on it goes to the tab running that conversation rather than
/// showing its page.
pub fn done_resumes(click: &RowClick) -> Option<String> {
    if click.stage != Stage::Done {
        return None;
    }
    resumed_session(click.open.as_deref()?)
}

fn note_then(click: &RowClick, text: &str) -> String {
    match click.note.as_deref().filter(|n| !n.trim().is_empty()) {
        Some(n) => format!("{n}\n\n{text}"),
        None => text.to_string(),
    }
}

/// Read a brief for the page: the file, capped, or a line saying why not.
pub fn read_brief(path: &Path) -> String {
    match std::fs::read(path) {
        Ok(bytes) if bytes.len() > BRIEF_MAX => {
            let head = String::from_utf8_lossy(&bytes[..BRIEF_MAX]);
            format!(
                "{head}\n\n… {} more bytes — open {} to read the rest.",
                bytes.len() - BRIEF_MAX,
                path.display()
            )
        }
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(err) => format!("Could not read the brief at {}: {err}", path.display()),
    }
}

/// The conversation an `open` command resumes, if it resumes one:
/// the session id after `--resume`/`-r`, or `--resume=<id>`.
pub fn resumed_session(command: &str) -> Option<String> {
    let is_sid = |s: &str| s.len() == 36 && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    let unquote = |s: &str| s.trim_matches(|c| c == '\'' || c == '"').to_string();
    let words: Vec<&str> = command.split_whitespace().collect();
    for (i, w) in words.iter().enumerate() {
        if let Some(v) = w.strip_prefix("--resume=") {
            let v = unquote(v);
            if is_sid(&v) {
                return Some(v);
            }
        }
        if (*w == "--resume" || *w == "-r")
            && let Some(next) = words.get(i + 1)
        {
            let v = unquote(next);
            if is_sid(&v) {
                return Some(v);
            }
        }
    }
    None
}

// ------------------------------------------------------------ attach ----

/// Claude Code's key path to one subagent's interactive view: the one place
/// that depends on Claude Code's keybindings and screen, so a Claude Code
/// update that moves them is fixed here.
///
/// Established on Claude Code 2.1.281 with `"tui": "fullscreen"` by driving
/// a real `claude` in tmux with background subagents. Nothing is typed:
/// only arrow keys and
/// Enter, so nothing is left in the prompt and no dialog flickers open.
///
/// 1. Under the prompt, Claude Code draws its agent strip: `● main`, then a
///    row per agent, `◯ <agent type>  <description>  <time> · ↓ <tokens>`
///    (the filled dot is the view on screen). ↓ at the prompt moves the
///    focus down out of the prompt box — through the draft's lines, then
///    the `N shells` pill when background shells exist — into the strip,
///    which then shows `❯` on its selected row and an `↑/↓ to select` or
///    `Enter to view` hint. A one-line draft stays where it is.
/// 2. ↑/↓ step the `❯` along the strip onto the worker, read off the
///    screen one key at a time; its label is the Agent call's description.
/// 3. Enter opens that agent's view. The strip keeps the focus, and
///    anything typed next goes to the view's prompt — which messages the
///    worker. A draft in the main prompt comes along into it.
///
/// Every key is its own write. Never sent: Esc (at the prompt it cancels
/// the turn) and `x` (stops the selected agent).
pub mod cc_keys {
    use super::Keystroke;

    /// Moves the focus out of the prompt, and the strip's selection.
    pub const NEXT: Keystroke = Keystroke::Down;
    pub const PREVIOUS: Keystroke = Keystroke::Up;
    /// Opens the selected strip row's view.
    pub const VIEW: Keystroke = Keystroke::Enter;
    /// The strip's hint rows while it has the focus.
    pub const STRIP_HINTS: &[&str] = &["↑/↓ to select", "Enter to view"];

    /// The `/tasks` dialog's hint row in list mode, and the part only it
    /// has (the strip's hint also says "↑/↓ to select"). The dialog is not
    /// used any more; it is recognised so an attach never types into it.
    pub const LIST_HINT: &str = "↑/↓ to select";
    pub const LIST_HINT_CLOSE: &str = "Esc to close";
    /// The hint row of an agent's detail card.
    pub const DETAIL_HINT: &str = "← to go back";
    /// Between the agent type and its description on a detail card.
    pub const DETAIL_TITLE_SEP: &str = " › ";
    /// The prompt's first column.
    pub const PROMPT: char = '❯';
    /// The dialog's top border.
    pub const DIALOG_TOP: char = '▔';
    /// The footer strip's first row is the main session: `● main` while it
    /// is the one on screen, `◯ main` (`( ) main` without Unicode) while a
    /// subagent's or teammate's view is. Read off Claude Code 2.1.281's
    /// strip row (`[❯ |  ]<● or ◯> main`, then `↑ N more` right-aligned).
    pub const MAIN_UNVIEWED: &[&str] = &["◯ main", "( ) main"];
}

/// Whether Claude Code on this screen shows a worker's view rather than the
/// main session — entered by an attach, by `/tasks`, or by
/// hand, and left with the strip's `main` or ←. Read from the screen alone,
/// so it is right however the view was reached.
///
/// A strip row is the marker and nothing looser: the whole row is the
/// unviewed `main`, save the pointer before it and a `↑ N more` after it,
/// so a transcript line that merely mentions `◯ main` does not count.
pub fn viewing_worker(screen: &str) -> bool {
    screen.lines().any(|row| main_row(row) == Some(false))
}

/// A strip's `main` row: `Some(true)` while main is the view on screen
/// (`● main`), `Some(false)` while a worker's is (`◯ main`), `None` for any
/// other row. The whole row is the dot and `main`, save the pointer before
/// it and a `↑ N more` after it.
fn main_row(row: &str) -> Option<bool> {
    let row = row.trim();
    let row = row
        .strip_prefix(cc_keys::PROMPT)
        .map_or(row, str::trim_start);
    let rest = if let Some(r) = row.strip_prefix("( )").or_else(|| row.strip_prefix("(*)")) {
        r
    } else {
        let mut chars = row.chars();
        let dot = chars.next()?;
        if dot.is_alphanumeric() {
            return None;
        }
        chars.as_str()
    };
    let rest = rest.strip_prefix(" main")?;
    if !(rest.is_empty() || (rest.starts_with("  ") && rest.trim_start().starts_with('↑'))) {
        return None;
    }
    Some(!cc_keys::MAIN_UNVIEWED.iter().any(|m| row.starts_with(m)))
}

/// One key Giverny types into the parent's Claude Code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keystroke {
    Enter,
    Up,
    Down,
}

/// One item of the Background dialog's list, or a row of the agent strip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub label: String,
    pub selected: bool,
    /// The agent id a strip row is tagged with, while the relay tags them
    /// (`hooks::tag_strip_row`).
    pub id: Option<String>,
}

/// What the parent's screen shows, as far as the attach cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum View {
    /// The prompt box; `draft` when it holds typed text.
    Prompt { draft: bool },
    /// The agent strip under the prompt, with the focus (a `❯` row).
    Strip(Vec<Item>),
    /// The `/tasks` dialog's list.
    List(Vec<Item>),
    /// An agent's detail card, titled with its description.
    Detail(String),
    /// Anything else: a permission question, another dialog, a shell.
    Other,
}

pub(crate) fn is_rule(line: &str) -> bool {
    line.trim_start().starts_with("───")
}

/// Read the parent's screen. `screen` is its text; `undimmed` the same with
/// dim cells blanked, which is how a draft is told from the placeholder.
pub fn read_view(screen: &str, undimmed: &str) -> View {
    use cc_keys::*;
    let rows: Vec<&str> = screen.lines().collect();

    let hint = |needle: &str, also: Option<&str>| {
        rows.iter()
            .rposition(|r| r.contains(needle) && also.is_none_or(|a| r.contains(a)))
    };
    let top_above = |at: usize| {
        rows[..at]
            .iter()
            .rposition(|r| r.trim_start().starts_with(DIALOG_TOP))
    };
    if let Some(h) = hint(LIST_HINT, Some(LIST_HINT_CLOSE))
        && let Some(top) = top_above(h)
    {
        let items = rows[top + 1..h]
            .iter()
            .filter_map(|r| parse_item(r))
            .collect();
        return View::List(items);
    }
    if let Some(h) = hint(DETAIL_HINT, None)
        && let Some(top) = top_above(h)
        && let Some(title) = rows[top + 1..h]
            .iter()
            .find_map(|r| r.split_once(DETAIL_TITLE_SEP).map(|(_, t)| t.trim()))
    {
        return View::Detail(title.to_string());
    }
    if let Some(items) = read_strip(&rows) {
        return View::Strip(items);
    }
    match prompt_box(screen, undimmed) {
        Some(b) => View::Prompt { draft: b.draft },
        None => View::Other,
    }
}

/// Claude Code's prompt box, as far as a send cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptBox {
    /// The text on the box's top rule: a worker's view carries the worker's
    /// description there (`─── eta worker ─`); the main view none.
    pub label: Option<String>,
    /// It holds typed text (not the dim placeholder).
    pub draft: bool,
    /// Its rows on screen, from the `❯` row to the one above its bottom
    /// rule.
    pub rows: std::ops::Range<usize>,
}

/// Read the prompt box off the screen, wherever the focus is: a rule,
/// `❯ …` in the first column, maybe more lines of draft, a rule. The last
/// one on screen; the transcript above has `❯ ` lines too, but never right
/// under a rule. `undimmed` tells a draft from the placeholder.
pub fn prompt_box(screen: &str, undimmed: &str) -> Option<PromptBox> {
    let rows: Vec<&str> = screen.lines().collect();
    let bright: Vec<&str> = undimmed.lines().collect();
    for i in (1..rows.len()).rev() {
        if !rows[i].starts_with(cc_keys::PROMPT) || !is_rule(rows[i - 1]) {
            continue;
        }
        let Some(end) = (i + 1..rows.len().min(i + 40)).find(|&j| is_rule(rows[j])) else {
            continue;
        };
        let typed = |j: usize| {
            let line = bright.get(j).copied().unwrap_or("");
            let line = if j == i {
                line.trim_start().trim_start_matches(cc_keys::PROMPT)
            } else {
                line
            };
            !line.trim().is_empty()
        };
        let label = rows[i - 1].trim().trim_matches('─').trim();
        return Some(PromptBox {
            label: (!label.is_empty()).then(|| label.to_string()),
            draft: (i..end).any(typed),
            rows: i..end,
        });
    }
    None
}

/// Whether Claude Code's agent strip is drawn under the prompt: its `main`
/// row is on screen. With the management panel on, Giverny's relay hides the
/// strip until asked to show it.
pub fn strip_shown(screen: &str) -> bool {
    screen.lines().any(|row| main_row(row).is_some())
}

/// `   ❯ <label> (running) · Opus 5.5` → the label, and whether it is
/// selected. Section headings (`Local agents (3)`) and counts are not items.
fn parse_item(row: &str) -> Option<Item> {
    let row = row.trim();
    let (selected, rest) = match row.strip_prefix(cc_keys::PROMPT) {
        Some(r) => (true, r.trim_start()),
        None => (false, row),
    };
    // The last ` (<status>)` whose status is a word, followed by the end or
    // ` · <model>`.
    let mut at = rest.len();
    while let Some(open) = rest[..at].rfind(" (") {
        let after = &rest[open + 2..];
        if let Some(close) = after.find(')') {
            let status = &after[..close];
            let tail = &after[close + 1..];
            if !status.is_empty()
                && status.chars().all(|c| c.is_ascii_lowercase() || c == ' ')
                && (tail.is_empty() || tail.starts_with(" · "))
            {
                let label = rest[..open].trim();
                return (!label.is_empty()).then(|| Item {
                    label: label.to_string(),
                    selected,
                    id: None,
                });
            }
        }
        at = open;
    }
    None
}

/// The agent strip, when it has the focus: the rows under its hint row,
/// one of them selected. `None` when no strip row is selected.
fn read_strip(rows: &[&str]) -> Option<Vec<Item>> {
    let h = rows.iter().rposition(|r| {
        cc_keys::STRIP_HINTS.iter().any(|hint| r.contains(hint))
            && !r.contains(cc_keys::LIST_HINT_CLOSE)
    })?;
    let items: Vec<Item> = rows[h + 1..]
        .iter()
        .map(|r| r.trim())
        // Inside a worker's view a blank row sits under the hint.
        .skip_while(|r| r.is_empty())
        .take_while(|r| !r.is_empty())
        .filter_map(parse_strip_item)
        .collect();
    items.iter().any(|it| it.selected).then_some(items)
}

/// `❯ ◯ general-purpose  eta worker     8s · ↓ 27.2k tokens` → `eta worker`
/// (selected); `● main` → `main`; a row the relay tagged,
/// `◯ [a95d7f3452a3597df] Reading main.rs`, → `Reading main.rs` with that
/// id. `↑ N more` rows are not items.
fn parse_strip_item(row: &str) -> Option<Item> {
    let (selected, rest) = match row.strip_prefix(cc_keys::PROMPT) {
        Some(r) => (true, r.trim_start()),
        None => (false, row),
    };
    // The dot: one symbol (`●`, `◯`, a spinner), or `( )` / `(*)` without
    // Unicode.
    let rest = if rest.len() >= 3 && rest.starts_with('(') && rest[2..].starts_with(')') {
        &rest[3..]
    } else {
        let mut chars = rest.chars();
        let dot = chars.next()?;
        if dot.is_alphanumeric() || dot == '↑' || dot == '↓' {
            return None;
        }
        chars.as_str()
    };
    if !rest.starts_with(' ') {
        return None;
    }
    let rest = rest.trim();
    if let Some((id, label)) = giverny_claude::hooks::strip_row_tag(rest) {
        return Some(Item {
            label: label.to_string(),
            selected,
            id: Some(id.to_string()),
        });
    }
    // Columns are separated by runs of spaces: type, description, stats.
    let cols: Vec<&str> = rest
        .split("  ")
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .collect();
    let label = match cols.as_slice() {
        [] => return None,
        [only] => *only,
        [_, desc, ..] => *desc,
    };
    Some(Item {
        label: label.to_string(),
        selected,
        id: None,
    })
}

/// Rows of the prompt box on screen (its draft's lines), else 1.
fn prompt_rows(screen: &str) -> usize {
    let rows: Vec<&str> = screen.lines().collect();
    for i in (1..rows.len()).rev() {
        if !rows[i].starts_with(cc_keys::PROMPT) || !is_rule(rows[i - 1]) {
            continue;
        }
        if let Some(end) = (i + 1..rows.len().min(i + 40)).find(|&j| is_rule(rows[j])) {
            return end - i;
        }
    }
    1
}

/// Whether a label on Claude Code's screen names this description: equal
/// but for spacing, or cut short with `…`.
pub fn label_matches(label: &str, description: &str) -> bool {
    let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let (label, description) = (norm(label), norm(description));
    if label == description {
        return true;
    }
    match label.strip_suffix('…') {
        Some(head) => !head.trim().is_empty() && description.starts_with(head.trim_end()),
        None => false,
    }
}

/// The bytes for one keystroke, `encode` being the terminal's key encoder
/// (`giverny_term::input::encode_key` in the tab's current mode), so an arrow
/// goes out as the running program asked for it.
pub fn keystroke_bytes(
    key: Keystroke,
    encode: impl Fn(egui::Key, egui::Modifiers) -> Option<Vec<u8>>,
) -> Vec<u8> {
    use egui::{Key, Modifiers};
    let special = |k: Key, m: Modifiers| encode(k, m).unwrap_or_default();
    match key {
        Keystroke::Enter => special(Key::Enter, Modifiers::NONE),
        Keystroke::Up => special(Key::ArrowUp, Modifiers::NONE),
        Keystroke::Down => special(Key::ArrowDown, Modifiers::NONE),
    }
}

/// Why an attach stopped short of the worker's view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stuck {
    /// Neither the prompt nor the dialog is on screen.
    NoPrompt,
    /// The dialog opened but does not list the worker.
    NotListed,
    /// More than one running agent carries this description.
    Ambiguous,
    /// The screen never got where the keys should have taken it.
    TimedOut,
    /// Claude Code's agent strip never came up under the prompt.
    NoStrip,
    /// Enter opened a view, but another worker's (its prompt's rule says
    /// which).
    WrongView(String),
    /// The manager's prompt holds a draft of the user's: a line forwarded
    /// to it is not typed into that.
    Draft,
    /// The forwarded line never showed in the manager's prompt.
    NotTyped,
    /// The forwarded line is in the manager's prompt, but Enter did not
    /// send it.
    NotSent,
}

impl Stuck {
    /// What the page tells the user.
    pub fn explain(&self, description: &str) -> String {
        match self {
            Stuck::NoPrompt => "This means typing into this tab's Claude Code, and its prompt \
                                is not on screen (a permission question, or another dialog, is \
                                up). Answer or close it, then try again."
                .to_string(),
            Stuck::NotListed => format!(
                "The agent list under Claude Code's prompt has no agent called \
                 \u{201c}{description}\u{201d}. It may have finished and left the list; its \
                 row still shows the transcript."
            ),
            Stuck::Ambiguous => format!(
                "More than one agent is called \u{201c}{description}\u{201d}, so Giverny \
                 cannot tell which is which. The agent list under Claude Code's prompt is \
                 left selected: pick it with ↑/↓ and press Enter."
            ),
            Stuck::TimedOut => "Claude Code did not answer the keys in time, so Giverny \
                                stopped. Try again, or press ↓ at this tab's prompt until the \
                                agent list is selected, then Enter on the worker."
                .to_string(),
            Stuck::NoStrip => format!(
                "Claude Code's agent list never came up under this tab's prompt, so there \
                 was no way to \u{201c}{description}\u{201d}. It may have just finished."
            ),
            Stuck::WrongView(other) => format!(
                "Giverny opened the view of \u{201c}{other}\u{201d} instead of \
                 \u{201c}{description}\u{201d}: Claude Code's agent list names busy agents by \
                 what they are doing, and two looked alike. Press Esc to go back to the \
                 manager."
            ),
            Stuck::Draft => "Not sent: the manager's prompt holds a draft of yours, and this \
                             line would have been typed into it. Send or clear that draft, \
                             then press Enter here again."
                .to_string(),
            Stuck::NotTyped => "Not sent: the line never showed in the manager's prompt. Look \
                                at the manager's tab, then press Enter here again."
                .to_string(),
            Stuck::NotSent => "The line is in the manager's prompt, but Enter did not send it. \
                               Go back to the manager and press Enter there."
                .to_string(),
        }
    }
}

/// What to do this frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tick {
    /// Write this key to the parent's pty.
    Send(Keystroke),
    /// Write this key so many times (more than one), in one write: Claude
    /// Code reads each of the keys in it (2.1.312: three `←` and a letter
    /// in one write land as four keys), so a known number of steps along
    /// the strip costs one redraw, not one per step.
    Repeat(Keystroke, u8),
    /// Nothing yet; look again next frame.
    Wait,
    /// The foreground key is sent: the worker's view is open.
    Done,
    Stuck(Stuck),
}

/// `key` pressed `n` times (at least once), as one write.
fn press(key: Keystroke, n: usize) -> Tick {
    match u8::try_from(n.max(1)).unwrap_or(u8::MAX) {
        1 => Tick::Send(key),
        n => Tick::Repeat(key, n),
    }
}

/// The least gap between two writes of keys. Every key after the first
/// waits for the screen to show the last one's effect anyway; this only
/// keeps two writes out of one frame.
pub const KEY_GAP: Duration = Duration::from_millis(16);
/// How long a key's effect is waited for before the screen is read afresh.
pub const SETTLE: Duration = Duration::from_millis(700);
/// How long a worker's view must hold still, prompt drawn but cursor not in
/// it, before the walk takes the footer's shells pill to have the focus.
/// Claude Code draws the frame and puts its cursor back in one write, so
/// this only has to outlast a frame split across reads; the full [`SETTLE`]
/// here cost every open ~0.7 s whenever a background shell was running.
/// The same calm as a final screen's ([`SETTLE_CALM`]).
pub const FOCUS_CALM: Duration = Duration::from_millis(60);
/// The whole attach, start to finish.
pub const DEADLINE: Duration = Duration::from_secs(5);

/// Walks the key path into a parent's Claude Code, reading its screen after
/// every step. Pure but for the clock and screen it is handed, so it is
/// driven the same way in tests as in the app.
#[derive(Debug, Clone)]
pub struct Attach {
    pub description: String,
    /// Other labels the strip may show the worker by: Claude Code swaps the
    /// description for the worker's live activity label once it is working.
    aliases: Vec<String>,
    /// With no row by any of its names, take the strip's only agent row:
    /// for a caller that checks the view it lands on ([`Walk`]).
    only_agent: bool,
    /// The worker's agent id: on a strip whose rows carry ids (the relay
    /// tags them while a walk asks for the strip) the row is
    /// found by it alone, whatever Claude Code labels it.
    id: Option<String>,
    /// ↓ has been sent out of the prompt.
    opened: bool,
    /// Enter on the worker has gone.
    finishing: bool,
    last_key: Option<Instant>,
    /// After a key, wait until the view differs from this one, or the
    /// instant passes.
    settle: Option<(Instant, View)>,
    /// The strip row the last steps were to bring the selection to: a
    /// screen with it short of there is one the keys have not all reached
    /// yet, and is waited out like an unchanged one.
    expect: Option<usize>,
    deadline: Instant,
}

impl Attach {
    pub fn new(description: impl Into<String>, now: Instant) -> Attach {
        Attach {
            description: description.into(),
            aliases: Vec::new(),
            only_agent: false,
            id: None,
            opened: false,
            finishing: false,
            last_key: None,
            settle: None,
            expect: None,
            deadline: now + DEADLINE,
        }
    }

    /// An attach that also takes a strip row labelled with one of
    /// `aliases` (empty ones are dropped) for the worker.
    pub fn with_aliases(mut self, aliases: impl IntoIterator<Item = String>) -> Attach {
        self.aliases = aliases
            .into_iter()
            .filter(|a| !a.trim().is_empty())
            .collect();
        self
    }

    /// With no row by any name, go to the strip's only agent row, if it
    /// has exactly one. The caller must check the view it opens.
    pub fn or_only_agent(mut self) -> Attach {
        self.only_agent = true;
        self
    }

    /// Find the row by agent id `id` when the strip's rows carry ids.
    pub fn with_id(mut self, id: Option<String>) -> Attach {
        self.id = id.filter(|i| !i.is_empty());
        self
    }

    fn names(&self, label: &str) -> bool {
        label_matches(label, &self.description)
            || self.aliases.iter().any(|a| label_matches(label, a))
    }

    /// `key`, `n` times in one write; the next look waits for the view
    /// to change from `view`.
    fn push(&mut self, key: Keystroke, n: usize, view: View, now: Instant) -> Tick {
        self.expect = None;
        self.last_key = Some(now);
        self.settle = Some((now + SETTLE, view));
        press(key, n)
    }

    /// One frame: `screen` and `undimmed` are the parent's screen now.
    pub fn tick(&mut self, now: Instant, screen: &str, undimmed: &str) -> Tick {
        if self.finishing {
            return Tick::Done;
        }
        if self.last_key.is_some_and(|t| now < t + KEY_GAP) {
            return Tick::Wait;
        }
        if now >= self.deadline {
            return Tick::Stuck(Stuck::TimedOut);
        }
        let view = read_view(screen, undimmed);
        // ↓ went out and the prompt is still all there is: the keys are
        // still landing, or — once they have had time to — there is no
        // strip to reach, so no agent is listed. However the prompt box
        // redraws meanwhile (the placeholder, the footer) it is waited out.
        if self.opened && matches!(view, View::Prompt { .. }) {
            return match &self.settle {
                Some((until, _)) if now < *until => Tick::Wait,
                _ => Tick::Stuck(Stuck::NotListed),
            };
        }
        if let Some((until, before)) = &self.settle {
            let short = match (&view, self.expect) {
                (View::Strip(items), Some(row)) => {
                    items.iter().position(|it| it.selected) != Some(row)
                }
                _ => false,
            };
            if (view == *before || short) && now < *until {
                return Tick::Wait;
            }
            self.settle = None;
        }
        match &view {
            View::Strip(items) => {
                // Rows tagged with ids: the id alone says which is the
                // worker's. A busy worker's label is Claude
                // Code's summary of what it is doing, never its description.
                let by_id = self
                    .id
                    .as_deref()
                    .filter(|_| items.iter().any(|it| it.id.is_some()));
                let hits: Vec<usize> = items
                    .iter()
                    .enumerate()
                    .filter(|(_, it)| match by_id {
                        Some(id) => it.id.as_deref() == Some(id),
                        None => self.names(&it.label),
                    })
                    .map(|(i, _)| i)
                    .collect();
                // The strip's first row is `main`; the rest are agents.
                let only = (by_id.is_none()
                    && self.only_agent
                    && items.len() == 2
                    && items[0].label == "main")
                    .then_some(1);
                let target = match (hits.as_slice(), only) {
                    ([], Some(one)) => one,
                    ([], None) => return Tick::Stuck(Stuck::NotListed),
                    ([one], _) => *one,
                    _ => return Tick::Stuck(Stuck::Ambiguous),
                };
                let Some(at) = items.iter().position(|it| it.selected) else {
                    return Tick::Wait;
                };
                // Every step to the row in one write; Enter only once the
                // screen shows the selection on it.
                let steps = target.abs_diff(at);
                let key = match target.cmp(&at) {
                    std::cmp::Ordering::Equal => {
                        self.finishing = true;
                        cc_keys::VIEW
                    }
                    std::cmp::Ordering::Greater => cc_keys::NEXT,
                    std::cmp::Ordering::Less => cc_keys::PREVIOUS,
                };
                let tick = self.push(key, steps.max(1), view, now);
                if steps > 0 {
                    self.expect = Some(target);
                }
                tick
            }
            View::Prompt { .. } => {
                self.opened = true;
                // Down through the draft's lines, past the shells pill,
                // into the strip. A Down too many only moves the strip's
                // selection, which the next look corrects.
                let downs = prompt_rows(screen) + 1;
                self.push(cc_keys::NEXT, downs, view, now)
            }
            View::List(_) | View::Detail(_) | View::Other => Tick::Stuck(Stuck::NoPrompt),
        }
    }
}

// -------------------------------------------------------------- walk ----

/// The agent rows of Claude Code's strip on this screen, focused or not:
/// the rows under its `main` row. Empty while the strip is hidden, and
/// while it shows `main` alone (a worker's view with the management panel on:
/// the relay hides every agent row, but Claude Code keeps `main` so the
/// way back is there).
pub fn strip_agents(screen: &str) -> Vec<Item> {
    let rows: Vec<&str> = screen.lines().collect();
    let Some(at) = rows.iter().rposition(|r| main_row(r).is_some()) else {
        return Vec::new();
    };
    rows[at + 1..]
        .iter()
        .map(|r| r.trim())
        .take_while(|r| !r.is_empty())
        .filter_map(parse_strip_item)
        .collect()
}

/// Whether Claude Code's prompt has the keyboard: the terminal cursor
/// (`cursor`, its screen row while it is shown) sits in the prompt box.
/// Claude Code shows the cursor only there; with the focus on the strip or
/// the footer's `N shells` pill it hides it (2.1.281), and
/// nothing else on screen tells the pill's focus from the prompt's.
pub fn prompt_focused(screen: &str, cursor: Option<usize>) -> bool {
    let Some(row) = cursor else {
        return false;
    };
    prompt_box(screen, screen).is_some_and(|p| p.rows.contains(&row))
}

/// What a [`Walk`] reads each frame.
#[derive(Debug, Clone, Copy)]
pub struct Look<'a> {
    /// The screen's text, row per line.
    pub screen: &'a str,
    /// The same with dim cells blanked (a draft told from the placeholder).
    pub undimmed: &'a str,
    /// The terminal cursor's screen row, while the cursor is shown.
    pub cursor: Option<usize>,
}

/// Where a [`Walk`] takes the tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Goal {
    /// A worker's view, found on the strip by its description or one of
    /// the other labels Claude Code may show it by (its live activity).
    Worker {
        description: String,
        aliases: Vec<String>,
        /// Its agent id, which the relay tags the strip's rows with while
        /// the strip is asked for: found by that first.
        agent_id: Option<String>,
    },
    /// The main session's view: the manager.
    Main,
}

/// How long the strip is waited for: with the management panel on, the relay
/// shows it only when asked (`hooks::show_strip`), and Claude Code runs the
/// relay about every five seconds.
pub const STRIP_WAIT: Duration = Duration::from_secs(8);
/// How long one step's effect is waited for on screen.
pub const STEP_WAIT: Duration = Duration::from_secs(3);
/// The whole walk, start to finish.
pub const WALK_DEADLINE: Duration = Duration::from_secs(20);
/// The most `↑`s sent to bring the focus back up into the prompt: one per
/// strip row above the selection, and one for the shells pill.
const MAX_UPS: u8 = 12;

#[derive(Debug, Clone)]
enum Phase {
    Start,
    /// Waiting for the strip's rows.
    Strip,
    Attach(Box<Attach>),
    /// Enter went on the row: waiting for the view it opens.
    Landing,
    /// On the view: the focus back into its prompt.
    Focus {
        pill_up: bool,
    },
}

/// What was on screen when the last key went.
type Seen = (View, Option<PromptBox>, Option<usize>);

/// What the focus walk waits to hold still: the view, the prompt box's
/// label and draft, and the cursor's row within the box — not where on
/// screen the box is, which moves when the relay hides the strip's rows
/// under it and says nothing about the focus.
type Held = (View, Option<(Option<String>, bool)>, Option<isize>);

fn held(seen: &Seen) -> Held {
    let (view, prompt, cursor) = seen;
    let at = |c: usize| {
        let top = prompt.as_ref().map_or(0, |p| p.rows.start);
        c as isize - top as isize
    };
    (
        view.clone(),
        prompt.as_ref().map(|p| (p.label.clone(), p.draft)),
        cursor.map(at),
    )
}

/// Moves a tab's Claude Code between the main view and a worker's view
/// through the agent strip: wait for the strip, step to
/// the row with [`Attach`], Enter, check the view that opened, then put the
/// focus back in that view's prompt — so what the user types next goes to it,
/// and an Enter never lands on the strip. Reads the screen before every
/// key and never types text; `/tasks` is never opened.
///
/// Pure but for the clock and screen it is handed, like [`Attach`].
#[derive(Debug, Clone)]
pub struct Walk {
    pub goal: Goal,
    phase: Phase,
    /// When the current phase began.
    since: Instant,
    /// The screen when the last key went, and when: the next look waits
    /// until it changes, or `SETTLE` passes.
    hold: Option<(Instant, Seen)>,
    /// The screen, and since when it has looked like that.
    calm: Option<(Instant, Held)>,
    ups: u8,
    /// When the last ↑s up the strip went, and the row they were to bring
    /// the selection to (`None`: out of the strip). A screen still short
    /// of that is waited out, up to [`SETTLE`]: Claude Code redraws the
    /// strip on its own (a busy worker's row), and an ↑ sent on such a
    /// screen for a row the last ones already took could go past the
    /// strip's top and recall the prompt's history.
    ups_to: Option<(Instant, Option<usize>)>,
    /// In the worker's view with the focus off the strip: its rows may go.
    off_strip: bool,
    deadline: Instant,
}

impl Walk {
    /// To worker `description` (and `aliases`, the strip's other names
    /// for it).
    pub fn open(description: impl Into<String>, aliases: Vec<String>, now: Instant) -> Walk {
        Walk::new(
            Goal::Worker {
                description: description.into(),
                aliases,
                agent_id: None,
            },
            now,
        )
    }

    /// Find the worker's strip row by its agent id `id` wherever the rows
    /// carry ids; its labels stay the way in where they do not.
    pub fn by_id(mut self, id: impl Into<String>) -> Walk {
        if let Goal::Worker { agent_id, .. } = &mut self.goal {
            *agent_id = Some(id.into()).filter(|i: &String| !i.is_empty());
        }
        self
    }

    /// Back to the main view.
    pub fn home(now: Instant) -> Walk {
        Walk::new(Goal::Main, now)
    }

    fn new(goal: Goal, now: Instant) -> Walk {
        Walk {
            goal,
            phase: Phase::Start,
            since: now,
            hold: None,
            calm: None,
            ups: 0,
            ups_to: None,
            off_strip: false,
            deadline: now + WALK_DEADLINE,
        }
    }

    /// The worker's description, or `main`.
    pub fn target(&self) -> &str {
        match &self.goal {
            Goal::Worker { description, .. } => description,
            Goal::Main => "main",
        }
    }

    /// Whether the walk needs Claude Code's strip drawn: into a worker's
    /// view until the focus is off the strip — its rows going away under a
    /// focused strip would move its selection. From there the
    /// relay can hide them again while the focus walk finishes, so its
    /// answer is not waited for only after it. Going home
    /// needs no help: a worker's view keeps `main` on the strip whatever
    /// the relay hides.
    pub fn wants_strip(&self) -> bool {
        matches!(self.goal, Goal::Worker { .. }) && !self.off_strip
    }

    /// The phase the walk is in, for the timing log.
    pub fn phase_name(&self) -> &'static str {
        match self.phase {
            Phase::Start => "start",
            Phase::Strip => "strip",
            Phase::Attach(_) => "attach",
            Phase::Landing => "landing",
            Phase::Focus { .. } => "focus",
        }
    }

    /// Whether the walk is still on its way to the goal's Enter: waiting
    /// for the strip, or stepping along it.
    pub fn entering(&self) -> bool {
        matches!(self.phase, Phase::Start | Phase::Strip | Phase::Attach(_))
    }

    fn go(&mut self, phase: Phase, now: Instant) {
        self.phase = phase;
        self.since = now;
    }

    fn key(&mut self, k: Keystroke, seen: Seen, now: Instant) -> Tick {
        self.keys(k, 1, seen, now)
    }

    /// `k`, `n` times in one write; the next look waits for the screen to
    /// change from `seen`.
    fn keys(&mut self, k: Keystroke, n: usize, seen: Seen, now: Instant) -> Tick {
        self.hold = Some((now, seen));
        press(k, n)
    }

    /// The view on screen is the goal's.
    fn arrived(&self, screen: &str, label: Option<&str>) -> bool {
        match &self.goal {
            Goal::Main => !viewing_worker(screen) && label.is_none(),
            Goal::Worker { description, .. } => {
                viewing_worker(screen) && label.is_some_and(|l| label_matches(l, description))
            }
        }
    }

    /// One frame.
    pub fn tick(&mut self, now: Instant, look: Look) -> Tick {
        let (screen, undimmed) = (look.screen, look.undimmed);
        let view = read_view(screen, undimmed);
        let prompt = prompt_box(screen, undimmed);
        let seen: Seen = (view.clone(), prompt.clone(), look.cursor);
        if let Some((at, before)) = &self.hold {
            if now < *at + KEY_GAP || (seen == *before && now < *at + SETTLE) {
                return Tick::Wait;
            }
            self.hold = None;
        }
        if now >= self.deadline {
            return Tick::Stuck(Stuck::TimedOut);
        }
        let waited = now.duration_since(self.since);
        let label = prompt.as_ref().and_then(|p| p.label.clone());
        match &mut self.phase {
            Phase::Start => {
                if prompt.is_none() || matches!(view, View::List(_) | View::Detail(_) | View::Other)
                {
                    return Tick::Stuck(Stuck::NoPrompt);
                }
                if self.arrived(screen, label.as_deref()) {
                    // Already there: only the focus to see to.
                    self.go(Phase::Focus { pill_up: false }, now);
                } else {
                    self.go(Phase::Strip, now);
                }
                self.tick(now, look)
            }
            Phase::Strip => {
                let ready = match &self.goal {
                    Goal::Main => strip_shown(screen),
                    Goal::Worker { .. } => !strip_agents(screen).is_empty(),
                };
                if !ready {
                    return if waited < STRIP_WAIT {
                        Tick::Wait
                    } else {
                        Tick::Stuck(Stuck::NoStrip)
                    };
                }
                let attach = match &self.goal {
                    Goal::Main => Attach::new("main", now),
                    Goal::Worker {
                        description,
                        aliases,
                        agent_id,
                    } => Attach::new(description.clone(), now)
                        .with_aliases(aliases.clone())
                        .with_id(agent_id.clone())
                        .or_only_agent(),
                };
                self.go(Phase::Attach(Box::new(attach)), now);
                self.tick(now, look)
            }
            Phase::Attach(a) => match a.tick(now, screen, undimmed) {
                Tick::Done => {
                    self.go(Phase::Landing, now);
                    Tick::Wait
                }
                other => other,
            },
            Phase::Landing => {
                if self.arrived(screen, label.as_deref()) {
                    self.go(Phase::Focus { pill_up: false }, now);
                    return self.tick(now, look);
                }
                if let (Goal::Worker { .. }, Some(other)) = (&self.goal, &label)
                    && viewing_worker(screen)
                    && waited >= SETTLE
                {
                    return Tick::Stuck(Stuck::WrongView(other.clone()));
                }
                if waited < STEP_WAIT {
                    Tick::Wait
                } else {
                    Tick::Stuck(Stuck::TimedOut)
                }
            }
            Phase::Focus { pill_up } => {
                let pill_up = *pill_up;
                // The screen held still: a key's effect has landed, cursor
                // and all.
                let still = held(&seen);
                let calm = match &self.calm {
                    Some((since, before)) if *before == still => now >= *since + FOCUS_CALM,
                    _ => {
                        self.calm = Some((now, still));
                        false
                    }
                };
                match &view {
                    // The strip still has the focus: ↑ walks the selection
                    // up it, and past its top out of it. It never changes
                    // the view — only Enter does. Every row above the
                    // selection goes in one write, up to `main`; the ↑ out
                    // of the strip goes on its own, once the screen shows
                    // the selection on `main`, so no ↑ is ever sent past
                    // the strip on a guess: two too many there would
                    // recall the prompt's history.
                    View::Strip(items) if self.ups < MAX_UPS => {
                        let at = items.iter().position(|it| it.selected).unwrap_or(0);
                        if let Some((sent, to)) = self.ups_to
                            && to.is_none_or(|to| at > to)
                            && now < sent + SETTLE
                        {
                            return Tick::Wait;
                        }
                        let n = at.clamp(1, usize::from(MAX_UPS - self.ups));
                        self.ups += n as u8;
                        self.ups_to = Some((now, at.checked_sub(n)));
                        self.keys(cc_keys::PREVIOUS, n, seen, now)
                    }
                    View::Prompt { .. } if !self.off_strip && viewing_worker(screen) => {
                        self.off_strip = true;
                        self.tick(now, look)
                    }
                    View::Prompt { .. } if prompt_focused(screen, look.cursor) => Tick::Done,
                    // The prompt is drawn but has not got the cursor, and
                    // the screen has settled: the shells pill has the focus,
                    // and one ↑ leaves it. Only one: in the prompt, ↑ would
                    // recall history.
                    View::Prompt { .. } if !pill_up && calm => {
                        self.phase = Phase::Focus { pill_up: true };
                        self.calm = None;
                        self.key(cc_keys::PREVIOUS, seen, now)
                    }
                    // A terminal that never shows the cursor: the view is
                    // right, which is what counts.
                    View::Prompt { .. } if pill_up && calm => Tick::Done,
                    View::Prompt { .. } => Tick::Wait,
                    _ if waited < STEP_WAIT => Tick::Wait,
                    _ => Tick::Stuck(Stuck::NoPrompt),
                }
            }
        }
    }
}

// ----------------------------------------------------------- forward ----

/// What a line typed in a worker page's box asks the manager to do.
/// Claude Code keeps no view of a worker whose turn ended,
/// so there is nothing to type to it; the one way back in is its
/// manager's `SendMessage`, and a Next up row's way in is its manager's
/// start. So the line goes to the manager, said in the user's words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ask {
    /// Wake a worker whose turn ended (finished, asleep or landed) with
    /// the user's line.
    Revive { task: String, agent_id: String },
    /// Start a Next up task now, the user's line added to its brief.
    Start { task: String },
}

impl Ask {
    /// The page's row's ask: a worker that ended its turn (a Running row's
    /// finished or asleep one, or a Done row's) is woken, a Next up row is
    /// started. `task` is the task's name as the page gives it. `None` for
    /// a row with neither: a worker still mid-turn, a row with no worker.
    pub fn of(click: &RowClick, task: &str) -> Option<Ask> {
        let task = task.trim().to_string();
        match click.stage {
            Stage::Planned => {
                let key = Some(click.key.trim())
                    .filter(|k| !k.is_empty())
                    .map_or(task, str::to_string);
                (!key.is_empty()).then_some(Ask::Start { task: key })
            }
            Stage::Running if !click.finished => None,
            Stage::Running | Stage::Done => Some(Ask::Revive {
                task,
                agent_id: agent_id(click)?.to_string(),
            }),
        }
    }

    /// The line the manager is given: one plain line in which the only
    /// words that are not the user's say whose they are and where they go.
    pub fn line(&self, text: &str) -> String {
        let text = one_line(text);
        match self {
            Ask::Revive { task, agent_id } if task.is_empty() => {
                format!("Send this to the worker (agent {agent_id}): {text}")
            }
            Ask::Revive { task, agent_id } => {
                format!("Send this to {task}'s worker (agent {agent_id}): {text}")
            }
            Ask::Start { task } => format!("Start {task} now, adding this to its brief: {text}"),
        }
    }

    /// The page's box's placeholder, drawn dim and never sent.
    pub fn placeholder(&self) -> String {
        match self {
            Ask::Revive { .. } => "Message this worker — its manager passes it on".to_string(),
            Ask::Start { .. } => "Start it now — its manager adds this to its brief".to_string(),
        }
    }
}

/// `text` as one line: a line break would submit the manager's prompt
/// half-way, so each run of whitespace is one space.
pub fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// What a [`Forward`] wants done this frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Put {
    /// Write this key to the manager's pty.
    Key(Keystroke),
    /// Write this key so many times, in one write ([`Tick::Repeat`]).
    Keys(Keystroke, u8),
    /// Write this text to the manager's pty, as typed.
    Text(String),
    Wait,
    /// The line is submitted: Claude Code took it, or queued it behind the
    /// manager's turn.
    Done,
    Stuck(Stuck),
}

/// The text of the prompt box on `screen`, every whitespace removed: a
/// long line wraps in the box, and the wrap is not the line's.
fn box_text(screen: &str, undimmed: &str) -> Option<String> {
    let b = prompt_box(screen, undimmed)?;
    let rows: Vec<&str> = undimmed.lines().collect();
    let text: String = rows
        .get(b.rows.clone())?
        .iter()
        .map(|r| r.trim_start().trim_start_matches(cc_keys::PROMPT))
        .collect();
    Some(text.chars().filter(|c| !c.is_whitespace()).collect())
}

/// How much of the line's start must show in the manager's prompt before
/// Enter goes: enough to know the text landed there and nowhere else.
const READ_BACK: usize = 24;

/// How long a line typed at the manager's prompt sits there, read back,
/// before its Enter goes: the 40 ms the line has always had, kept when the
/// walk's own gap between keys went down.
const TEXT_TO_ENTER: Duration = Duration::from_millis(40);

/// Claude Code's stand-in for a long paste in its prompt box: the text is
/// all there, drawn as `[Pasted text #1 +N lines]`.
const PASTED: &str = "[Pastedtext";

/// Puts one line at the manager's prompt and submits it, so
/// a line typed in a worker page reaches the session that can act on it.
/// It walks the tab back to the main view first if it is on a worker's
/// ([`Walk::home`], which leaves the keyboard in the main prompt), types
/// the line only into an empty prompt, reads it back off the screen, and
/// then presses Enter. A draft of the user's in the manager's prompt is
/// left alone: the line is refused ([`Stuck::Draft`]) rather than typed
/// into it. A busy manager takes the line as Claude Code takes any typed
/// while it works: queued for after its turn.
#[derive(Debug, Clone)]
pub struct Forward {
    pub line: String,
    phase: Step,
    since: Instant,
}

#[derive(Debug, Clone)]
enum Step {
    Home(Box<Walk>),
    Type,
    /// The text went: waiting to see it in the prompt.
    Typed,
    /// Enter went: waiting for the prompt to empty.
    Entered,
}

impl Forward {
    pub fn new(line: impl Into<String>, now: Instant) -> Forward {
        Forward {
            line: line.into(),
            phase: Step::Home(Box::new(Walk::home(now))),
            since: now,
        }
    }

    fn go(&mut self, phase: Step, now: Instant) {
        self.phase = phase;
        self.since = now;
    }

    /// One frame.
    pub fn tick(&mut self, now: Instant, look: Look) -> Put {
        let waited = now.duration_since(self.since);
        match &mut self.phase {
            Step::Home(walk) => match walk.tick(now, look) {
                Tick::Send(k) => Put::Key(k),
                Tick::Repeat(k, n) => Put::Keys(k, n),
                Tick::Wait => Put::Wait,
                Tick::Done => {
                    self.go(Step::Type, now);
                    self.tick(now, look)
                }
                Tick::Stuck(why) => Put::Stuck(why),
            },
            Step::Type => match read_view(look.screen, look.undimmed) {
                View::Prompt { draft: true } => Put::Stuck(Stuck::Draft),
                View::Prompt { draft: false } if !viewing_worker(look.screen) => {
                    self.go(Step::Typed, now);
                    Put::Text(self.line.clone())
                }
                _ => Put::Stuck(Stuck::NoPrompt),
            },
            Step::Typed => {
                let want: String = self
                    .line
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .take(READ_BACK)
                    .collect();
                let shown = box_text(look.screen, look.undimmed)
                    .is_some_and(|t| t.contains(&want) || t.contains(PASTED));
                if shown && waited >= TEXT_TO_ENTER {
                    self.go(Step::Entered, now);
                    return Put::Key(Keystroke::Enter);
                }
                if waited >= STEP_WAIT {
                    Put::Stuck(Stuck::NotTyped)
                } else {
                    Put::Wait
                }
            }
            Step::Entered => {
                let empty = matches!(
                    read_view(look.screen, look.undimmed),
                    View::Prompt { draft: false }
                );
                if empty {
                    Put::Done
                } else if waited >= STEP_WAIT {
                    Put::Stuck(Stuck::NotSent)
                } else {
                    Put::Wait
                }
            }
        }
    }
}

// ----------------------------------------------------------- settle ----

/// How long the final screen must hold before it is shown: Claude Code
/// writes a frame in more than one read, and half of one is not it.
pub const SETTLE_CALM: Duration = Duration::from_millis(60);
/// The longest a finished walk waits for its final screen.
pub const SETTLE_MAX: Duration = Duration::from_millis(2500);

/// After a [`Walk`] lands: waits for the screen to be the view's final
/// one, so the terminal can go from the view before to it in one frame.
/// Into a worker's view, final is that view: the strip's agent rows the
/// walk had drawn may still be under it, but they are not drawn
/// ([`row_marks`]), and the relay's answer that takes them away changes
/// nothing above them, so it is not waited for; back on
/// main, it is main's prompt.
#[derive(Debug, Clone)]
pub struct Settle {
    main: bool,
    since: Instant,
    good_since: Option<Instant>,
}

impl Settle {
    pub fn new(goal: &Goal, now: Instant) -> Settle {
        Settle {
            main: matches!(goal, Goal::Main),
            since: now,
            good_since: None,
        }
    }

    /// Whether `screen` is the view's final screen.
    pub fn is_final(&self, screen: &str) -> bool {
        if self.main {
            !viewing_worker(screen)
        } else {
            viewing_worker(screen)
        }
    }

    /// One frame: true once the final screen has held for [`SETTLE_CALM`],
    /// or [`SETTLE_MAX`] has gone by regardless.
    pub fn done(&mut self, now: Instant, screen: &str) -> bool {
        if now >= self.since + SETTLE_MAX {
            return true;
        }
        if !self.is_final(screen) {
            self.good_since = None;
            return false;
        }
        let since = *self.good_since.get_or_insert(now);
        now >= since + SETTLE_CALM
    }
}

/// How long the pty is left one column narrow: long enough for Claude Code
/// to take the new width in before the real one comes back.
pub const NUDGE_NARROW: Duration = Duration::from_millis(90);
/// How long after a nudge the next is sent, while still waited on. Claude
/// Code answers a nudge about 0.4 s after the width comes back; one sent
/// while its relay run is in flight throws that run's answer away, so the
/// next waits well past it.
pub const NUDGE_AGAIN: Duration = Duration::from_millis(800);
/// The longest a [`Nudge::ask_held`] keeps the pty narrow waiting for the
/// answer: long enough that it never cuts off a relay run in flight.
pub const NUDGE_HOLD: Duration = Duration::from_millis(1200);
/// The most nudges one wait sends; after that the relay's own tick.
const NUDGE_MAX: u8 = 5;
/// Claude Code runs the relay this long after the last width change
/// (`subagentStatusLine`'s 300 ms), less a little for the relay to start
/// and read the ask: a new ask made before then is answered by that run.
const CC_TICK_AFTER_RESIZE: Duration = Duration::from_millis(280);

/// What a [`Nudge`] wants done to the pty's width this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Width {
    Narrow,
    Restore,
}

/// Brings Claude Code's next `subagentStatusLine` run forward.
/// Claude Code runs it every five seconds, and 300 ms after
/// the terminal's width changes — so a width one column short for a moment
/// makes the relay's answer to a new strip ask land within about half a
/// second instead of up to five. The pty alone is narrowed, never the grid,
/// and only while the tab's picture is held, so nothing of it is seen.
///
/// Claude Code's timer starts over at every width change, so the answer
/// comes about 300 ms after the *last* one. [`Nudge::ask_held`] leaves the
/// pty narrow until the answer is on screen, which starts that timer at the
/// narrowing rather than at the restore; it is for a wait whose screens are
/// read but never shown as final (the strip on the way in), since Claude
/// Code draws them one column short until the width is back.
#[derive(Debug, Clone, Default)]
pub struct Nudge {
    narrowed: Option<Instant>,
    last: Option<Instant>,
    sent: u8,
    /// Keep the pty narrow until the answer is in ([`Nudge::ask_held`]).
    held: bool,
}

impl Nudge {
    /// Start over: a new ask for the relay to answer. When the last nudge
    /// came back so recently that Claude Code's run is still to come, that
    /// run answers it, and nudging now would only put it off: the next
    /// nudge waits its usual [`NUDGE_AGAIN`] after that one.
    pub fn ask(&mut self, now: Instant) {
        if self
            .last
            .is_some_and(|back| now >= back + CC_TICK_AFTER_RESIZE)
        {
            self.last = None;
        }
        self.sent = 0;
        self.held = false;
    }

    /// Start over, keeping the pty narrow until the answer is on screen
    /// (or [`NUDGE_HOLD`] passes).
    pub fn ask_held(&mut self) {
        self.last = None;
        self.sent = 0;
        self.held = true;
    }

    /// One frame. `waiting`: the relay's answer is still not on screen.
    pub fn tick(&mut self, now: Instant, waiting: bool) -> Option<Width> {
        if let Some(at) = self.narrowed {
            let back = if self.held {
                !waiting || now >= at + NUDGE_HOLD
            } else {
                now >= at + NUDGE_NARROW
            };
            if back {
                self.narrowed = None;
                self.last = Some(now);
                return Some(Width::Restore);
            }
            return None;
        }
        let due = self.last.is_none_or(|at| now >= at + NUDGE_AGAIN);
        if waiting && due && self.sent < NUDGE_MAX {
            self.sent += 1;
            self.narrowed = Some(now);
            return Some(Width::Narrow);
        }
        None
    }

    /// The pty is narrow right now and must be put back.
    pub fn is_narrow(&self) -> bool {
        self.narrowed.is_some()
    }

    /// Claude Code's next relay run is due with no more help: the width
    /// has gone back since the last [`Nudge::ask`], or went back so
    /// shortly before it that the run it set off reads the ask. Past this
    /// the ask is answered whether or not anyone keeps nudging.
    pub fn answer_due(&self) -> bool {
        self.narrowed.is_none() && self.last.is_some()
    }
}

// ------------------------------------------------------------ marks ----

/// What Giverny paints over a Claude Code tab's grid while the management panel
/// stands in for Claude Code's own agent strip:
///
/// * the strip's `main` row is not drawn — the pane shows which worker the
///   tab is on, and the way back is Giverny's button;
/// * while a worker's view is on screen, a "back to manager" button
///   sits where that `◯ main` was, left-aligned under the terminal's other
///   text; with no such row, on the status line under the prompt (the
///   row with the session's token counts), after its text and before the
///   session's use at its right edge;
/// * and Esc presses that button rather than reaching Claude Code, while
///   the prompt or the strip has the keyboard (not over a dialog, whose Esc
///   is its own).
pub fn row_marks(screen: &str) -> giverny_term::widget::RowMarks {
    let rows: Vec<&str> = screen.lines().collect();
    let as_row = |i: usize| u16::try_from(i).ok();
    let hidden = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| main_row(r).is_some())
        .filter_map(|(i, _)| as_row(i))
        .collect();
    let mut marks = giverny_term::widget::RowMarks {
        hidden,
        ..Default::default()
    };
    if !viewing_worker(screen) {
        return marks;
    }
    // Nor are the strip's agent rows under a worker's view: the relay
    // draws them only for a walk, which shows the view before its answer
    // takes them away again.
    if let Some(at) = rows.iter().rposition(|r| main_row(r).is_some()) {
        let agents = rows[at + 1..]
            .iter()
            .take_while(|r| !r.trim().is_empty())
            .count();
        marks
            .hidden
            .extend((at + 1..at + 1 + agents).filter_map(as_row));
    }
    marks.escape = matches!(
        read_view(screen, screen),
        View::Prompt { .. } | View::Strip(_)
    );
    // Where the strip's `◯ main` was: its dot's column, pointer skipped.
    if let Some(i) = rows.iter().rposition(|r| main_row(r) == Some(false)) {
        let row = rows[i];
        let lead = row.chars().take_while(|c| c.is_whitespace()).count();
        let rest = row.trim_start();
        let dot = match rest.strip_prefix(cc_keys::PROMPT) {
            Some(after) => {
                let gap = after.chars().take_while(|c| c.is_whitespace()).count();
                lead + 1 + gap
            }
            None => lead,
        };
        marks.button = as_row(i).map(|r| (r, 0));
        marks.button_left = as_row(dot);
        return marks;
    }
    let Some(prompt) = prompt_box(screen, screen) else {
        return marks;
    };
    // The status line: under the prompt's bottom rule, the row with the
    // token counts (Giverny's `… · session: … · subagents: … · total: …`);
    // else the first row there with anything on it.
    let below = prompt.rows.end + 1;
    let near = below..rows.len().min(below + 3);
    let status = near
        .clone()
        .find(|&i| rows[i].contains("total:") || rows[i].contains("session:"))
        .or_else(|| near.clone().find(|&i| !rows[i].trim().is_empty()));
    let Some(i) = status else {
        return marks;
    };
    // The session's use sits at the line's right edge, past a gap: the
    // button goes in the gap, between the two.
    let (used, end) = status_gap(rows[i]);
    marks.button = as_row(i).zip(as_row(used));
    marks.button_end = end.and_then(as_row);
    marks
}

/// Where a status line's left part ends, in columns, and where a
/// right-aligned part after a gap of three or more blanks starts (`None`
/// with no such part: the whole line is the left part).
fn status_gap(row: &str) -> (usize, Option<usize>) {
    let chars: Vec<char> = row.trim_end().chars().collect();
    let first = chars.iter().position(|c| !c.is_whitespace()).unwrap_or(0);
    let mut run = 0;
    for j in (first..chars.len()).rev() {
        if chars[j] == ' ' {
            run += 1;
        } else if run >= 3 {
            return (j + 1, Some(j + 1 + run));
        } else {
            run = 0;
        }
    }
    (row.chars().count(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------ attach ----
    //
    // The screens below are Claude Code 2.1.280 (fullscreen tui), captured
    // from a real `claude` in a pty while building the attach.

    const TRANSCRIPT: &str = "\
❯ Spawn three general-purpose subagents in parallel
● 2 background agents launched (↓ to manage)
   ├ eta worker
   └ theta worker
✻ Waiting for 3 background agents to finish
";

    fn prompt_screen(text: &str) -> String {
        format!(
            "{TRANSCRIPT}──────────────────────────────\n❯ {text}\n──────────────────────────────\n  \
             Haiku 4.5  ·  5h 20%  ·  wk 86%\n  ⏸ manual mode on · ← 2 agents\n"
        )
    }

    fn list_screen(selected: usize) -> String {
        let rows = [
            "sleep 300",
            "wait for background sleep 240 to finish",
            "iota worker",
            "theta worker",
            "eta worker",
        ];
        let mut s = format!(
            "{TRANSCRIPT}▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔\n   Background\n   2 active shells · 3 active agents\n     Shells (2)\n"
        );
        for (i, r) in rows.iter().enumerate() {
            if i == 2 {
                s.push_str("     Local agents (3)\n");
            }
            let mark = if i == selected { "   ❯ " } else { "     " };
            let tail = if i < 2 { "" } else { " · Opus 5.5" };
            s.push_str(&format!("{mark}{r} (running){tail}\n"));
        }
        s.push_str(
            "   ↑/↓ to select · Enter to view · f to foreground · x to stop · \
             ctrl+x ctrl+k to stop all agents · Esc to close\n",
        );
        s
    }

    const DETAIL: &str = "\
▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔
   general-purpose › theta worker
   58s · 24.7k tokens · 1 tool · Opus 5.5
   Progress
   › Bash(python3 -c 'import time; time.sleep(240)')
   ← to go back · Esc/Enter/Space to close · x to stop · f to foreground · ctrl+x ctrl+k to stop all agents
";

    #[test]
    fn reads_the_prompt_and_tells_a_draft_from_the_placeholder() {
        // The placeholder is dim: blank once dim cells are dropped.
        let shown = prompt_screen("Message @general-purpose…");
        let undimmed = prompt_screen("");
        assert_eq!(read_view(&shown, &undimmed), View::Prompt { draft: false });
        let draft = prompt_screen("half a thought");
        assert_eq!(read_view(&draft, &draft), View::Prompt { draft: true });
        // A draft on a second line counts too.
        let two = "──────\n❯ \n  second line\n──────\n";
        assert_eq!(read_view(two, two), View::Prompt { draft: true });
    }

    /// The footer strip under a worker's view, as the attach saw it (`─── <desc> ─`,
    /// the `Message @…` placeholder, `◯ main`), and the same strip on main.
    fn strip_screen(main: &str, pointer: bool) -> String {
        let p = if pointer { "❯ " } else { "  " };
        format!(
            "{TRANSCRIPT}─── theta worker ──────────────\n❯ Message @general-purpose…\n\
             ──────────────────────────────\n  {p}{main} main                 ↑ 1 more\n  \
             ◯ theta worker (running)\n"
        )
    }

    #[test]
    fn tells_a_worker_view_from_main() {
        assert!(viewing_worker(&strip_screen("◯", false)));
        assert!(viewing_worker(&strip_screen("◯", true)));
        assert!(viewing_worker(&strip_screen("( )", false)));
        assert!(!viewing_worker(&strip_screen("●", false)));
        assert!(!viewing_worker(&strip_screen("⏺", true)));
        // Main with the strip folded away, and the Background dialog.
        assert!(!viewing_worker(&prompt_screen("")));
        assert!(!viewing_worker(&list_screen(2)));
        assert!(!viewing_worker(DETAIL));
        // A transcript line that talks about the strip is not the strip.
        assert!(!viewing_worker(
            "● the worker view has a ◯ main strip to go back\n  ◯ main is how\n"
        ));
    }

    // Claude Code 2.1.281 (fullscreen tui), captured from a real `claude`
    // in tmux while testing the attach: two background agents, two
    // background shells.

    const STRIP_TOP: &str = "\
● Theta worker is now also running its sleep 400 in the background.
✻ Crunched for 18s · done 1:02 AM · 2 shells still running
──────────────────────────────────────────────────────────────────
";

    fn strip_rows(selected: Option<usize>, hint: &str) -> String {
        let rows = [
            "● main",
            "◯ general-purpose  eta worker                          8s · ↓ 27.2k tokens",
            "◯ general-purpose  theta worker                        8s · ↓ 27.2k tokens",
        ];
        let mut out = format!("  {hint}\n");
        for (i, r) in rows.iter().enumerate() {
            let lead = if Some(i) == selected { "❯ " } else { "  " };
            out.push_str(&format!("{lead}{r}\n"));
        }
        out
    }

    /// The main prompt with the strip under it, unfocused.
    fn at_prompt(draft: &str) -> String {
        format!(
            "{STRIP_TOP}❯ {draft}\n──────────────────────────────────────────────────────────────────\n  \
             Haiku 4.5  ·  5h 11%  ·  wk 94%  ·  session: 36.2k  ·  subagents: 53.8k  ·  total: 90k\n{}",
            strip_rows(None, "⏸ manual mode on · 2 shells · ← 2 agents")
        )
    }

    /// The strip with the focus on row `at`.
    fn in_strip(at: usize) -> String {
        let hint = if at == 0 {
            "↑/↓ to select"
        } else {
            "Enter to view · x to stop · ctrl+x ctrl+k to stop all agents"
        };
        format!(
            "{STRIP_TOP}❯ \n──────────────────────────────────────────────────────────────────\n  \
             Haiku 4.5  ·  5h 11%  ·  wk 94%  ·  session: 36.2k  ·  subagents: 53.8k  ·  total: 90k\n{}",
            strip_rows(Some(at), hint)
        )
    }

    #[test]
    fn reads_the_agent_strip() {
        assert_eq!(
            read_view(&at_prompt(""), &at_prompt("")),
            View::Prompt { draft: false }
        );
        let View::Strip(items) = read_view(&in_strip(1), &in_strip(1)) else {
            panic!("expected the strip");
        };
        let labels: Vec<(&str, bool)> = items
            .iter()
            .map(|i| (i.label.as_str(), i.selected))
            .collect();
        assert_eq!(
            labels,
            vec![
                ("main", false),
                ("eta worker", true),
                ("theta worker", false)
            ]
        );
        // Without Unicode, and with a `↑ N more` row.
        assert_eq!(
            parse_strip_item("❯ ( ) general-purpose  iota worker   3s"),
            Some(Item {
                label: "iota worker".into(),
                selected: true,
                id: None,
            })
        );
        assert_eq!(parse_strip_item("↑ 2 more"), None);
        assert_eq!(parse_strip_item("Haiku 4.5  ·  5h 11%"), None);
    }

    // ------------------------------------------------------- walk ----
    //
    // A fake Claude Code 2.1.281 that draws the screens seen in tmux during
    // the strip and back-to-main work (main view, a worker's view, the strip focused or
    // not, the shells pill focused, the management panel's relay hiding rows)
    // and answers keys the way that one did.
}
