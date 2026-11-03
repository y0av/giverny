//! What a click on a management-panel row opens.
//!
//! The pane hands the app a [`RowClick`]; this module decides, without
//! touching the app, what that click means, and `main.rs::apply` only
//! carries it out.
//!
//! * **Running** and **Done** rows open Giverny's page of their worker in
//!   the tab (`worker_page`): it takes the terminal's place, under a header
//!   naming the worker and with a "back" button, and draws the worker's
//!   transcript as Claude Code draws a worker's own view
//!   (`giverny_claude::transcript`). A Running worker's page is live,
//!   following the transcript as it grows; a Done one's, or one whose
//!   worker has finished ([`RowClick::finished`]), opens at its end, on the
//!   final report. Nothing is typed into Claude Code.
//! * A Running row that names no worker but has a feed `open` command runs
//!   it in a new tab, unless it resumes a conversation something is already
//!   running, which two claudes on one transcript would interleave. A Done
//!   row whose `open` resumes a conversation running in a Giverny tab goes
//!   to that tab ([`done_resumes`]).
//! * **Next up** shows the row's brief on the same page, drawn as the
//!   worker's prompt; without one, its task, title and note as that prompt.
//!
//! A row with none of these still answers the click, with a page saying
//! what is missing, rather than doing nothing.

use std::path::{Path, PathBuf};

use giverny_claude::feed::Stage;

use crate::management_panel::RowClick;

/// The largest brief read; past it the page shows the head and says so.
/// A safety net, not a fold: no brief comes near it.
pub const BRIEF_MAX: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Show a worker's transcript on its page; `live` follows it.
    Watch { transcript: PathBuf, live: bool },
    /// Run the feed's `open` command in a new tab titled `title`.
    Run { title: String, command: String },
    /// Show something on the page.
    Show(Body),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    /// A brief on disk, read when the page opens, and shown as the
    /// worker's prompt.
    File(PathBuf),
    /// A Next up row with no brief: what it has (task, title, note) as the
    /// prompt, then that it has no brief.
    Unbriefed { prompt: String, hint: String },
    /// Why there is nothing else to show.
    Text(String),
}

/// The row's title for a tab: `acme#158 · name`, whichever exist.
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

fn agent_id(click: &RowClick) -> Option<&str> {
    click.agent_id.as_deref().filter(|id| !id.is_empty())
}

/// Decide what a click does.
pub fn plan(click: &RowClick) -> Plan {
    let open = click
        .open
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty());
    match click.stage {
        Stage::Running | Stage::Done => {
            // A finished worker's transcript is final: opened at its end,
            // on the report, as a Done row's is.
            let live = click.stage == Stage::Running && !click.finished;
            if let Some(t) = &click.transcript {
                return Plan::Watch {
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
                    title: title_of(click),
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
            Plan::Show(Body::Text(note_then(click, &why)))
        }
        Stage::Planned => {
            if let Some(b) = &click.brief {
                return Plan::Show(Body::File(b.clone()));
            }
            let (prompt, hint) = planned_without_brief(click);
            Plan::Show(Body::Unbriefed { prompt, hint })
        }
    }
}

/// A Next up row with no brief still says what it has: its task, its title
/// and its note (the prompt it has so far), then that it has no brief.
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
    let hint = "No brief was given for this row: its feed row has no `brief`.".to_string();
    (parts.join("\n\n"), hint)
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
