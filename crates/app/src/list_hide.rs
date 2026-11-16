//! Claude Code's own agent list, kept off a tab whose management panel is
//! on: the pane stands in for it.
//!
//! Nothing inside Claude Code hides the list any more (2.1.323 removed the
//! `subagentStatusLine` setting that could), so Giverny does, in what it
//! draws. The grid keeps every row; only the picture leaves them out.
//!
//! Claude Code 2.1.325 draws the list as the last rows of its screen, under
//! the prompt's footer: a blank row, `● main`, then one row per worker. It
//! comes with the first worker and goes with the last:
//!
//! ```text
//!   ⏵⏵ auto mode on · 2 shells · ← 2 agents · ↓ to manage
//!
//!   ● main
//!   ◯ general-purpose  Alpha: sleep then reply             8s · ↓ 22.3k tokens
//!   ◯ general-purpose  Beta: sleep then reply             10s · ↓ 22.5k tokens
//! ```
//!
//! The footer's `← N agents` is not part of it, and stays.
//!
//! **Only while nobody is using it.** `↓` from the prompt walks into the
//! list: a `❯` points at its `main` or a worker's row, and Enter opens that
//! worker's view, where `main` reads `◯ main`. Then the list is drawn as
//! Claude Code draws it, so it can be used as ever; once the keyboard is
//! back at the prompt of the main view, it goes again.
//!
//! * **Fullscreen** (the alternate screen), the list pushes everything above
//!   it up by its height. Those rows are given to Claude Code past the
//!   bottom of the tab ([`tail_rows`], `giverny_term::tail`): it lays out the
//!   rest exactly as it would with no list, and the picture ends where the
//!   tab does.
//! * **Outside fullscreen** Claude Code draws on the main screen, whose
//!   scrollback a resize would rewrite (it reprints its transcript), so
//!   nothing is resized: the list's rows are drawn blank ([`hidden_rows`]).

/// The pointer Claude Code puts before the row that has the keyboard.
const POINTER: char = '❯';

/// One of the list's `main` rows: `Some(true)` while main's view is on
/// screen (`● main`), `Some(false)` while a worker's is (`◯ main`, `( ) main`
/// without Unicode), `None` for any other row. The whole row is the dot and
/// `main`, save the pointer before it and a `↑ N more` after it, so a line
/// of the transcript that merely mentions `● main` is not one.
fn main_row(row: &str) -> Option<bool> {
    let row = row.trim();
    let row = row.strip_prefix(POINTER).map_or(row, str::trim_start);
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
    Some(!(row.starts_with("◯ main") || row.starts_with("( ) main")))
}

/// The agent list on a screen: its rows, from the blank row above `main`
/// (when there is one) to its last worker, and whether it is left alone —
/// no `❯` on any of its rows and main's own view on screen.
struct List {
    start: usize,
    end: usize,
    idle: bool,
}

/// The last list on the screen, if any.
fn list(rows: &[&str]) -> Option<List> {
    let at = rows.iter().rposition(|r| main_row(r).is_some())?;
    let workers = rows[at + 1..]
        .iter()
        .take_while(|r| !r.trim().is_empty())
        .count();
    let end = at + 1 + workers;
    let pointed = rows[at..end]
        .iter()
        .any(|r| r.trim_start().starts_with(POINTER));
    let idle = !pointed && main_row(rows[at]) == Some(true);
    let start = if at > 0 && rows[at - 1].trim().is_empty() {
        at - 1
    } else {
        at
    };
    Some(List { start, end, idle })
}

/// How many rows at the bottom of a fullscreen Claude Code screen to keep
/// out of sight: the idle agent list, from the blank row above `● main`,
/// when it is the bottom of the screen. 0 with no list, one in use, or one
/// that does not reach the bottom (a grid just grown and not redrawn yet,
/// or some other program's screen).
pub fn tail_rows(screen: &str) -> u16 {
    let rows: Vec<&str> = screen.lines().collect();
    match list(&rows) {
        Some(l) if l.idle && l.end == rows.len() => {
            u16::try_from(rows.len() - l.start).unwrap_or(u16::MAX)
        }
        _ => 0,
    }
}

/// The rows to paint blank over the tab's own Claude Code: the idle agent
/// list's `● main` and worker rows. All that hides the list outside
/// fullscreen; in fullscreen those rows are past the picture already.
pub fn hidden_rows(screen: &str) -> Vec<u16> {
    let rows: Vec<&str> = screen.lines().collect();
    match list(&rows) {
        Some(l) if l.idle => (l.start..l.end)
            .filter(|&i| !rows[i].trim().is_empty())
            .filter_map(|i| u16::try_from(i).ok())
            .collect(),
        _ => Vec::new(),
    }
}

/// Whether a tab's foreground command (`Tab::foreground`, the shell's
/// newest child) is Claude Code itself: the tab's own session, not a
/// `claude` inside a multiplexer or a nested shell, whose list is left as
/// Claude Code draws it.
pub fn own_claude(foreground: Option<&str>) -> bool {
    let Some(command) = foreground else {
        return false;
    };
    let program = giverny_core::procs::program_name(command);
    if giverny_claude::lineage::is_claude_name(program) {
        return true;
    }
    // Claude Code run as a script: `node …/@anthropic-ai/claude-code/cli.js`.
    program == "node"
        && command
            .split_whitespace()
            .nth(1)
            .is_some_and(|s| s.contains("claude-code") && s.ends_with(".js"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULE: &str = "────────────────────────────────────────────────────────────";

    /// The bottom of Claude Code 2.1.325's fullscreen screen with two
    /// workers running, as captured in a 120-column terminal, and `list`
    /// under the footer.
    fn screen(footer: &str, list: &[&str]) -> String {
        let mut s = String::from("● spawned\n\n✻ Waiting for 2 background agents to finish\n\n");
        s.push_str(&format!("{RULE}\n❯ \n{RULE}\n"));
        s.push_str(footer);
        s.push('\n');
        for row in list {
            s.push_str(row);
            s.push('\n');
        }
        s
    }

    const FOOTER: &str = "  ⏵⏵ auto mode on · 2 shells · ← 2 agents · ↓ to manage";
    const ALPHA: &str =
        "  ◯ general-purpose  Alpha: sleep then reply                    8s · ↓ 22.3k tokens";
    const BETA: &str =
        "  ◯ general-purpose  Beta: sleep then reply                    10s · ↓ 22.5k tokens";

    fn row_of(s: &str, text: &str) -> u16 {
        s.lines().position(|l| l == text).unwrap() as u16
    }

    #[test]
    fn the_whole_idle_list_goes() {
        let s = screen(FOOTER, &["", "  ● main", ALPHA, BETA]);
        assert_eq!(tail_rows(&s), 4);
        let main = row_of(&s, "  ● main");
        assert_eq!(hidden_rows(&s), vec![main, main + 1, main + 2]);
        let s = screen(FOOTER, &["", "  ● main", BETA]);
        assert_eq!(tail_rows(&s), 3);
        // The first ↓ takes the footer's shells, not the list.
        let s = screen(
            "  ⏵⏵ auto mode on · 2 shells · esc to interrupt · Enter to view tasks",
            &["", "  ● main", ALPHA, BETA],
        );
        assert_eq!(tail_rows(&s), 4);
    }

    #[test]
    fn no_list_no_rows() {
        assert_eq!(tail_rows(&screen(FOOTER, &[])), 0);
        assert!(hidden_rows(&screen(FOOTER, &[])).is_empty());
        assert_eq!(tail_rows(""), 0);
        // A transcript line that mentions it is not the list.
        let s = screen(FOOTER, &["", "  ● main is where the prompt goes"]);
        assert_eq!(tail_rows(&s), 0);
    }

    #[test]
    fn a_list_in_use_stays() {
        // ↓ into the list: the pointer on `main`, then on a worker.
        let hint = "  ↑/↓ to select";
        let s = screen(hint, &["", "❯ ● main", ALPHA, BETA]);
        assert_eq!(tail_rows(&s), 0);
        assert!(hidden_rows(&s).is_empty());
        let alpha = ALPHA.replacen("  ◯", "❯ ◯", 1);
        let s = screen(hint, &["", "  ● main", &alpha, BETA]);
        assert_eq!(tail_rows(&s), 0);
        // A worker's view: `◯ main`, wherever the pointer is.
        let viewed = ALPHA.replacen("  ◯", "❯ ●", 1);
        let s = screen(hint, &["", "  ◯ main", &viewed, BETA]);
        assert_eq!(tail_rows(&s), 0);
        assert!(hidden_rows(&s).is_empty());
        let s = screen(
            hint,
            &["", "❯ ◯ main", &ALPHA.replacen("  ◯", "  ●", 1), BETA],
        );
        assert_eq!(tail_rows(&s), 0);
    }

    #[test]
    fn a_list_that_does_not_reach_the_bottom_is_not_taken_off_the_grid() {
        // A grid just grown, before Claude Code redrew it: blank rows under
        // the list.
        let s = screen(FOOTER, &["", "  ● main", ALPHA, BETA, "", ""]);
        assert_eq!(tail_rows(&s), 0);
    }

    #[test]
    fn outside_fullscreen_the_list_rows_are_drawn_blank() {
        // On the main screen the list sits under the footer, with the rows
        // below it empty while the screen is not full.
        let s = screen(FOOTER, &["", "  ● main", ALPHA, BETA, "", ""]);
        let main = row_of(&s, "  ● main");
        assert_eq!(hidden_rows(&s), vec![main, main + 1, main + 2]);
    }

    #[test]
    fn only_claude_itself_in_front_is_the_tabs_own_session() {
        assert!(own_claude(Some("claude --model haiku")));
        assert!(own_claude(Some(
            "/home/u/.nvm/versions/node/v22.22.1/bin/claude --resume 0db89496"
        )));
        assert!(own_claude(Some("claude.exe")));
        assert!(own_claude(Some(
            "node /usr/lib/node_modules/@anthropic-ai/claude-code/cli.js"
        )));
        assert!(!own_claude(None));
        assert!(!own_claude(Some("tmux new -s work")));
        assert!(!own_claude(Some("bash")));
        assert!(!own_claude(Some("vim claude.md")));
    }
}
