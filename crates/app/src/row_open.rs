//! What a click on a management-panel row opens.
//!
//! The pane hands the app a [`RowClick`]; this module decides, without
//! touching the app, what that click means, and `main.rs::apply` only
//! carries it out.
//!
//! Every row opens Giverny's page of its worker in the tab (`worker_page`):
//! it takes the terminal's place, under a header naming the worker and with
//! a "back" button, and draws the worker's transcript as Claude Code draws a
//! worker's own view (`giverny_claude::transcript`). A **Running** worker's
//! page is live, following the transcript as it grows; a **Done** one's
//! opens at its end, on the final report. Nothing is typed into Claude Code.
//!
//! A row whose worker has no transcript yet still answers the click, with a
//! page saying so, rather than doing nothing.

use std::path::PathBuf;

use giverny_claude::subagents::Stage;

use crate::management_panel::RowClick;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Show a worker's transcript on its page; `live` follows it.
    Watch { transcript: PathBuf, live: bool },
    /// Show text on the page: why there is nothing to watch.
    Show(String),
}

/// Decide what a click does.
pub fn plan(click: &RowClick) -> Plan {
    let live = click.stage == Stage::Running;
    if let Some(t) = &click.transcript {
        return Plan::Watch {
            transcript: t.clone(),
            live,
        };
    }
    let id = &click.agent_id;
    Plan::Show(if live {
        format!(
            "No transcript for worker {id} yet — Claude Code writes it once the \
             worker's first turn lands. Click again in a moment."
        )
    } else {
        format!("No transcript was found for worker {id}, so there is nothing to show.")
    })
}
