//! The user's last prompt, pinned over the top of a Claude tab's terminal
//! once it has scrolled out of view.
//!
//! A long answer scrolls the question that started it off the screen, and
//! Claude Code has no way to keep it in view. One line here, cut to fit, and
//! the whole prompt below it on a click. While the prompt is still on screen
//! there is nothing to pin, and no bar. The terminal never moves for it:
//! the bar and the full prompt both float over the grid, so neither showing
//! them nor hiding them resizes the PTY and makes Claude redraw.

use eframe::egui::{self, Color32, FontId, Rect, Sense, Stroke, Vec2};
use giverny_core::tabs::TabId;

use crate::chrome::{Chrome, mix};

/// The most of a prompt the one line ever lays out. It is cut to the width
/// anyway; this only spares laying out a pasted log to show its first words.
const LINE_CHARS: usize = 400;

/// A prompt as one line: line breaks and runs of whitespace become single
/// spaces, and anything past `max` characters becomes an ellipsis.
pub fn one_line(prompt: &str, max: usize) -> String {
    one_line_cut(prompt, max).0
}

/// [`one_line`], and whether it had to cut.
fn one_line_cut(prompt: &str, max: usize) -> (String, bool) {
    let mut out = String::new();
    let mut count = 0;
    for word in prompt.split_whitespace() {
        if count > 0 {
            if count == max {
                return (cut(out), true);
            }
            out.push(' ');
            count += 1;
        }
        for ch in word.chars() {
            if count == max {
                return (cut(out), true);
            }
            out.push(ch);
            count += 1;
        }
    }
    (out, false)
}

fn cut(mut text: String) -> String {
    text.truncate(text.trim_end().len());
    text.push('…');
    text
}

/// How much of the prompt's first line has to be found on a row to call the
/// prompt on screen: enough to tell it from another, short enough to fit on
/// the first row of a wrapped one.
const MATCH_CHARS: usize = 40;
/// A wrapped row can end early, at a word; this much of a prompt's start on
/// it still counts.
const MATCH_MIN: usize = 8;
/// How far up the scrollback the prompt of the turn in view is looked for.
/// A turn's answer is rarely longer; past it, the latest prompt is shown.
pub const SEARCH_ABOVE: usize = 5000;

/// The characters a sent prompt's row starts with: Claude Code's `❯`, and
/// the `>` of older versions.
pub const PROMPT_MARKS: [char; 2] = ['❯', '>'];

/// The start of a prompt's first line that is not blank, whitespace
/// collapsed as [`one_line`] does, at most `max` characters of it. Read only
/// as far as that: a pasted log one line long is not copied whole to
/// compare a row's worth of it.
fn first_line_start(prompt: &str, max: usize) -> Vec<char> {
    let mut out = Vec::new();
    let mut space = false;
    for ch in prompt.trim_start().chars() {
        if ch == '\n' || out.len() == max {
            break;
        }
        if ch.is_whitespace() {
            space = true;
            continue;
        }
        if std::mem::take(&mut space) {
            out.push(' ');
            if out.len() == max {
                break;
            }
        }
        out.push(ch);
    }
    out
}

/// How much of this prompt a `❯` row, with the `❯` taken off and its
/// whitespace collapsed (`got`), agrees with: the length of their common
/// start, or `None` when the row is not this prompt.
///
/// The row holds the start of the prompt's first line, its whitespace as the
/// terminal laid it out, maybe cut short: by a wrap at a word, or with an
/// ellipsis where Claude Code pins it on the top row. So all of the row must
/// be the prompt's start, and at least [`MATCH_MIN`] of it (or all of a
/// shorter prompt). Past [`MATCH_CHARS`] the two may part: what a terminal
/// does to wide characters or tabs is not this check's business.
fn agreement(prompt: &str, got: &[char]) -> Option<usize> {
    // One more than the row, to tell a prompt that ends with the row from
    // one that goes on past it: no more of it can ever match.
    let want = first_line_start(prompt, got.len() + 1);
    if want.is_empty() {
        return None;
    }
    let common = want.iter().zip(got).take_while(|(a, b)| a == b).count();
    let whole_row = common == got.len();
    (common >= MATCH_MIN.min(want.len()) && common > 0 && (whole_row || common >= MATCH_CHARS))
        .then_some(common)
}

/// Which of `history` (oldest first) a row shows as a sent prompt, if any:
/// the one that agrees with most of it, and of two that agree as far, the
/// more recent.
pub fn prompt_of_row(history: &[String], row: &str) -> Option<usize> {
    let row = row.trim();
    let rest = PROMPT_MARKS.iter().find_map(|&m| row.strip_prefix(m))?;
    let got: Vec<char> = one_line(rest.trim().trim_end_matches('…'), usize::MAX)
        .chars()
        .collect();
    let mut best: Option<(usize, usize)> = None;
    for (i, prompt) in history.iter().enumerate() {
        if let Some(n) = agreement(prompt, &got)
            && best.is_none_or(|(_, m)| n >= m)
        {
            best = Some((i, n));
        }
    }
    best.map(|(i, _)| i)
}

/// The rows below the top one that could be a sent prompt: not the input
/// box (unshaded, right under a rule), and starting with `❯` (or `>`), each
/// with whether it is shaded.
fn prompt_rows(rows: &[(String, bool)]) -> impl Iterator<Item = (&str, bool)> {
    let mut under_rule = false;
    rows.iter().skip(1).filter_map(move |(text, shaded)| {
        let row = text.trim();
        let in_input = under_rule && !shaded;
        under_rule = !row.is_empty() && row.chars().all(|c| c == '─');
        (!in_input && (row.starts_with('❯') || row.starts_with('>'))).then_some((row, *shaded))
    })
}

/// Is a prompt the user sent in view? `rows` are the terminal's visible rows,
/// top to bottom, each with whether its first cell is shaded.
///
/// Claude Code shows a sent prompt as `❯ <prompt>` on a shaded row, wrapped
/// over as many rows as it takes, so the first row holds the start of its
/// first line. A shaded `❯` row is a sent prompt whoever's it is; an unshaded
/// one counts when it is one of `history`. The input box at the bottom
/// starts with `❯` too, right under a rule and unshaded: whatever is being
/// typed there is not a prompt sent.
///
/// The top row does not count: it is the one the bar covers.
pub fn prompt_in_view(history: &[String], rows: &[(String, bool)]) -> bool {
    prompt_rows(rows).any(|(row, shaded)| {
        (shaded && row.starts_with('❯')) || prompt_of_row(history, row).is_some()
    })
}

/// The prompt to pin, as an index into `history` (oldest first): the one
/// whose turn the top of the view is in. `None` when there is none to pin,
/// or when it is in view and so needs no bar.
///
/// Claude Code in fullscreen scrolls itself, the terminal holds only what is
/// on screen, and scrolled back Claude pins the turn's prompt on the top row:
/// that row names it, and the bar stands in for it there (Claude's row is a
/// grey of its own), unless the prompt's own row is in view below. Otherwise
/// a sent prompt in view means no bar; with none, `above` searches the
/// terminal's own scrollback for the nearest prompt row, given the matcher.
/// Failing that, it is the latest prompt.
pub fn owner(
    history: &[String],
    rows: &[(String, bool)],
    above: impl FnOnce(&dyn Fn(&str, bool) -> Option<usize>) -> Option<usize>,
) -> Option<usize> {
    if history.is_empty() {
        return None;
    }
    if let Some(pinned) = rows
        .first()
        .and_then(|(top, _)| prompt_of_row(history, top))
    {
        let own_row_in_view =
            prompt_rows(rows).any(|(row, _)| prompt_of_row(history, row) == Some(pinned));
        return (!own_row_in_view).then_some(pinned);
    }
    if prompt_in_view(history, rows) {
        return None;
    }
    Some(above(&|row, _| prompt_of_row(history, row)).unwrap_or(history.len() - 1))
}

/// [`owner`] for one tab, worked out again only when what it reads has
/// changed: the terminal's rows (`seq`, [`TermSession::content_seq`]) or the
/// prompts. Every frame of an idle Claude tab asked it, and each time read
/// the screen into strings, and with no prompt row on screen walked up to
/// [`SEARCH_ABOVE`] rows of scrollback, holding the lock the terminal's
/// output parser needs.
///
/// [`TermSession::content_seq`]: giverny_term::session::TermSession::content_seq
#[derive(Default)]
pub struct Owner {
    seen: Option<(u64, Vec<String>)>,
    owner: Option<usize>,
}

impl Owner {
    pub fn get(
        &mut self,
        seq: u64,
        history: &[String],
        work: impl FnOnce() -> Option<usize>,
    ) -> Option<usize> {
        // Compared, not copied: a copy is only taken when they differ.
        match &mut self.seen {
            Some((s, h)) if h.as_slice() == history => {
                if *s == seq {
                    return self.owner;
                }
                *s = seq;
            }
            seen => *seen = Some((seq, history.to_vec())),
        }
        self.owner = work();
        self.owner
    }
}

/// The bar's text size, in points: the terminal's, unless that would not
/// fit in a row `row` points high: egui lays a line of its monospace font
/// out about 1.17 times its size, and the terminal's cell is sized by its own
/// font's metrics.
pub fn text_size(terminal: f32, row: f32) -> f32 {
    terminal.min(row / 1.2).max(1.0)
}

fn open_id(tab: TabId) -> egui::Id {
    egui::Id::new(("giverny-prompt-bar", tab.0))
}

/// The layer the closed bar is drawn in, over the grid. The wheel goes
/// through it to the terminal; the open prompt, in a layer of its own,
/// scrolls itself.
pub fn layer(tab: TabId) -> egui::LayerId {
    egui::LayerId::new(egui::Order::Middle, open_id(tab).with("bar"))
}

/// The bar is not shown: the full prompt it opens goes with it, and the bar
/// comes back closed.
pub fn hide(ctx: &egui::Context, tab: TabId) {
    ctx.data_mut(|d| d.remove::<bool>(open_id(tab)));
}

/// Is there more to the prompt than the bar shows? Only then does a click
/// open it: a prompt of one line that fits has nothing more to show.
/// `cut` is whether the line shown was cut short, by the width or the cap.
pub fn expandable(prompt: &str, cut: bool) -> bool {
    cut || prompt.lines().filter(|l| !l.trim().is_empty()).count() > 1
}

/// The bar's one colour: the same closed or open, hovered or not, at the
/// bottom or scrolled back. Opaque, so the grid scrolling under it never
/// shows through.
pub fn fill(chrome: &Chrome) -> Color32 {
    mix(chrome.panel, chrome.fg, 0.10)
}

/// Draw the bar for `tab` over the top row of the terminal at `over`, `row`
/// points high, its text the size of the terminal's (`text`, in points).
/// Returns true when it was clicked, so the caller can hand the keyboard back
/// to the terminal.
///
/// Exactly the top row: at any font size or zoom, it covers that row and
/// none of the one below.
///
/// Over the grid rather than above it: the bar comes and goes as the prompt
/// scrolls in and out of view, and a bar that took a row of the layout would
/// resize the terminal each time, and make Claude redraw. The row it covers
/// is never the prompt's: the bar is only up while the prompt is off screen.
///
/// Open, the bar *becomes* the whole prompt, one panel from the same top
/// edge: the line it showed is the panel's first, not a second copy above it.
pub fn show(
    ctx: &egui::Context,
    chrome: &Chrome,
    over: Rect,
    row: f32,
    text: f32,
    tab: TabId,
    prompt: &str,
) -> bool {
    let open_id = open_id(tab);
    let mut open = ctx.data(|d| d.get_temp::<bool>(open_id).unwrap_or(false));
    let height = row;
    let fill = fill(chrome);
    let rule = mix(chrome.panel, chrome.fg, 0.25);
    let font = FontId::monospace(text_size(text, row));
    // Where the one line's text starts, and the room the marker keeps. The
    // room is kept whether or not there is a marker, so a prompt that fits
    // is decided at the same width either way.
    const LEFT: f32 = 10.0;
    const RIGHT: f32 = 26.0;

    // The one line, laid out at this width: whether it was cut is what
    // says there is more to see, and a resize can change the answer.
    let (line, capped) = one_line_cut(prompt, LINE_CHARS);
    let mut job = egui::text::LayoutJob::single_section(
        line,
        egui::TextFormat::simple(font.clone(), chrome.fg),
    );
    job.wrap = egui::text::TextWrapping {
        max_width: (over.width() - LEFT - RIGHT).max(0.0),
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('…'),
    };
    let galley = ctx.fonts_mut(|f| f.layout_job(job));
    let line_height = galley.size().y;
    let more = expandable(prompt, capped || galley.elided);
    if !more {
        open = false;
    }

    let shown = egui::Area::new(open_id.with("bar"))
        .order(if open {
            egui::Order::Foreground
        } else {
            egui::Order::Middle
        })
        .fixed_pos(over.min)
        .constrain(false)
        // egui fades an area in each time it reappears, and this one
        // reappears every time the prompt scrolls out of view: mid-scroll the
        // bar was half see-through, a different colour each frame.
        .fade_in(false)
        .show(ctx, |ui| {
            let rect = if open {
                // The prompt in full, its first line where the bar's was.
                let max_height = (over.height() * 0.6).max(60.0);
                let pad = ((height - line_height) / 2.0).round().clamp(0.0, 8.0) as i8;
                egui::Frame::new()
                    .fill(fill)
                    .inner_margin(egui::Margin {
                        left: LEFT as i8,
                        right: RIGHT as i8,
                        top: pad,
                        bottom: pad.max(6),
                    })
                    .show(ui, |ui| {
                        ui.set_width(over.width() - LEFT - RIGHT);
                        egui::ScrollArea::vertical()
                            .max_height(max_height)
                            .show(ui, |ui| {
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(prompt)
                                            .font(font.clone())
                                            .color(chrome.fg),
                                    )
                                    .wrap()
                                    .selectable(false),
                                );
                            });
                    })
                    .response
                    .rect
            } else {
                let (rect, _) =
                    ui.allocate_exact_size(Vec2::new(over.width(), height), Sense::hover());
                let p = ui.painter_at(rect);
                p.rect_filled(rect, 0.0, fill);
                let y = rect.center().y - galley.size().y / 2.0;
                p.galley(egui::pos2(rect.min.x + LEFT, y), galley, chrome.fg);
                rect
            };
            let p = ui.painter();
            // A hairline under it keeps it apart from the row below.
            p.hline(rect.x_range(), rect.max.y - 0.5, Stroke::new(1.0, rule));
            // Clicks only, anywhere on it: a focusable bar would take the
            // keyboard from the terminal under it. Sensed even with nothing to
            // open, so the click hands the keyboard back to the terminal; it
            // just does nothing else, and does not look like it would.
            let response = ui.interact(rect, open_id.with("click"), Sense::CLICK);
            if !more {
                return response;
            }
            p.text(
                egui::pos2(rect.max.x - 10.0, rect.min.y + height / 2.0),
                egui::Align2::RIGHT_CENTER,
                if open { "▴" } else { "▾" },
                font.clone(),
                chrome.dim,
            );
            response.on_hover_cursor(egui::CursorIcon::PointingHand)
        });
    let rect = shown.response.rect;
    let clicked = shown.inner.clicked();
    if clicked && more {
        open = !open;
    } else if open {
        // A click anywhere else puts it away, as a menu would.
        let elsewhere = ctx.input(|i| i.pointer.any_click())
            && ctx
                .input(|i| i.pointer.interact_pos())
                .is_some_and(|pos| !rect.contains(pos));
        if elsewhere {
            open = false;
        }
    }
    ctx.data_mut(|d| d.insert_temp(open_id, open));
    clicked
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prompt_becomes_one_line() {
        assert_eq!(
            one_line("  fix the build\n\nthen  run\tthe tests \n", 100),
            "fix the build then run the tests"
        );
    }

    /// Is this one prompt on screen: [`prompt_in_view`] for a history of one.
    fn on_screen(prompt: &str, rows: &[(String, bool)]) -> bool {
        prompt_in_view(&[prompt.to_string()], rows)
    }

    fn screen(rows: &[(&str, bool)]) -> Vec<(String, bool)> {
        rows.iter().map(|(t, s)| (format!("{t:<60}"), *s)).collect()
    }

    const INPUT_BOX: [(&str, bool); 3] = [
        ("────────────────────────────────────────", false),
        ("❯ ", false),
        ("────────────────────────────────────────", false),
    ];

    #[test]
    fn a_prompt_on_screen_needs_no_bar() {
        let prompt = "fix the build\nthen run the tests";
        let mut rows = vec![
            ("● earlier output", false),
            ("❯ fix the build", true),
            ("  then run the tests", true),
            ("", false),
            ("● Done.", false),
        ];
        rows.extend(INPUT_BOX);
        assert!(on_screen(prompt, &screen(&rows)));
    }

    #[test]
    fn a_prompt_scrolled_off_wants_the_bar() {
        let mut rows = vec![("27 twenty-seven", false), ("28 twenty-eight", false)];
        rows.extend(INPUT_BOX);
        assert!(!on_screen("List the numbers 1 to 60", &screen(&rows)));
        // Another prompt sent in view: the view is in its turn, not ours.
        let mut rows = vec![("", false), ("❯ count down from 5", true)];
        rows.extend(INPUT_BOX);
        assert!(on_screen("List the numbers 1 to 60", &screen(&rows)));
        // A menu's `❯` (unshaded, not a prompt) is not a prompt in view.
        let rows = screen(&[("", false), ("❯ 1. Yes", false), ("  2. No", false)]);
        assert!(!on_screen("List the numbers 1 to 60", &rows));
    }

    #[test]
    fn a_wrapped_prompt_is_found_by_its_first_row() {
        let prompt = "List the numbers 1 to 60, one per line, each followed by its English name.";
        // Wrapped at the terminal's width, mid-sentence.
        let rows = screen(&[
            ("● earlier", false),
            ("❯ List the numbers 1 to 60, one per line, each", true),
            ("  followed by its English name.", true),
        ]);
        assert!(on_screen(prompt, &rows));
        // Wrapped early, at a word, in a narrow terminal.
        let rows = screen(&[
            ("", false),
            ("❯ List the numbers 1 to", true),
            ("  60, one per", true),
        ]);
        assert!(on_screen(prompt, &rows));
        // Only its tail on screen: the start scrolled off.
        let rows = screen(&[("  followed by its English name.", true)]);
        assert!(!on_screen(prompt, &rows));
    }

    #[test]
    fn the_top_row_is_under_the_bar() {
        // Scrolled back in Claude Code: it pins the turn's prompt on the top
        // row itself. The bar covers that row, in its own colour.
        let mut rows = vec![("❯ List the numbers 1 to 80", true), ("20 400", false)];
        rows.extend(INPUT_BOX);
        assert!(!on_screen("List the numbers 1 to 80", &screen(&rows)));
        // One row lower, it is the prompt itself, in view.
        let mut rows = vec![
            ("❯ an older prompt", true),
            ("❯ List the numbers 1 to 80", true),
        ];
        rows.extend(INPUT_BOX);
        assert!(on_screen("List the numbers 1 to 80", &screen(&rows)));
    }

    fn history(prompts: &[&str]) -> Vec<String> {
        prompts.iter().map(|p| p.to_string()).collect()
    }

    const TURNS: [&str; 3] = [
        "List the numbers 1 to 60 with their English names",
        "Now the squares of 1 to 80, one per line\nNo other text.",
        "And the cubes of 1 to 70",
    ];

    /// No search above: the terminal has no scrollback (Claude fullscreen).
    fn nothing_above(_: &dyn Fn(&str, bool) -> Option<usize>) -> Option<usize> {
        None
    }

    #[test]
    fn claude_pinning_a_turn_names_its_prompt() {
        let h = history(&TURNS);
        // Scrolled back into turn 2: Claude pins its prompt on the top row,
        // cut to the width.
        let rows = screen(&[
            ("❯ Now the squares of 1 to 80, one per l…", true),
            ("20 400", false),
            ("21 441", false),
        ]);
        assert_eq!(owner(&h, &rows, nothing_above), Some(1));
        // Into turn 1's.
        let rows = screen(&[
            ("❯ List the numbers 1 to 60 with", true),
            ("7 seven", false),
        ]);
        assert_eq!(owner(&h, &rows, nothing_above), Some(0));
    }

    #[test]
    fn scrollback_is_searched_for_the_nearest_prompt_above() {
        let h = history(&TURNS);
        let rows = screen(&[("31 961", false), ("32 1024", false)]);
        // Nearest first: turn 2's answer, then its prompt, then older turns.
        let above = screen(&[
            ("30 900", false),
            ("1 1", false),
            ("❯ Now the squares of 1 to 80, one per line", true),
            ("● sixty", false),
            ("❯ List the numbers 1 to 60 with their English names", true),
        ]);
        let search = |m: &dyn Fn(&str, bool) -> Option<usize>| {
            above.iter().find_map(|(text, shaded)| m(text, *shaded))
        };
        assert_eq!(owner(&h, &rows, search), Some(1));
    }

    #[test]
    fn with_nothing_to_go_by_it_is_the_latest_prompt() {
        let h = history(&TURNS);
        let rows = screen(&[("31 29791", false), ("32 32768", false)]);
        assert_eq!(owner(&h, &rows, nothing_above), Some(2));
        // A pinned row that is no known prompt does not decide it.
        let rows = screen(&[("❯ something else entirely, typed", true), ("x", false)]);
        assert_eq!(owner(&h, &rows, nothing_above), Some(2));
        assert_eq!(owner(&[], &rows, nothing_above), None);
    }

    #[test]
    fn a_prompt_in_view_means_no_bar() {
        let h = history(&TURNS);
        // At the bottom, the latest prompt's own row in view.
        let rows = screen(&[("27 729", false), ("❯ And the cubes of 1 to 70", true)]);
        assert_eq!(owner(&h, &rows, nothing_above), None);
        // Pinned by Claude, and its own row just under the pin.
        let rows = screen(&[
            ("❯ Now the squares of 1 to 80, one per l…", true),
            ("❯ Now the squares of 1 to 80, one per line", true),
            ("1 1", false),
        ]);
        assert_eq!(owner(&h, &rows, nothing_above), None);
    }

    #[test]
    fn the_next_turns_prompt_in_view_does_not_hide_the_pinned_one() {
        // Near the end of turn 2's answer, turn 3's prompt in view below:
        // the top is still turn 2's, so the bar names it (over Claude's own
        // grey pin).
        let h = history(&TURNS);
        let rows = screen(&[
            ("❯ Now the squares of 1 to 80, one per l…", true),
            ("79 6241", false),
            ("80 6400", false),
            ("❯ And the cubes of 1 to 70", true),
        ]);
        assert_eq!(owner(&h, &rows, nothing_above), Some(1));
    }

    #[test]
    fn prompts_that_start_alike_are_told_apart_by_the_rest_of_the_row() {
        // Seen live: two prompts alike for their first 40 characters.
        let h = history(&[
            "List the numbers 1 to 70, one per line, each followed by its Roman numeral.",
            "List the numbers 1 to 70, one per line, each followed by its cube. No other text.",
        ]);
        let pinned =
            "❯ List the numbers 1 to 70, one per line, each followed by its Roman numeral.";
        assert_eq!(prompt_of_row(&h, pinned), Some(0));
        let cut = "❯ List the numbers 1 to 70, one per line, each followed by its cu…";
        assert_eq!(prompt_of_row(&h, cut), Some(1));
        // Only the common start on screen: the more recent.
        assert_eq!(
            prompt_of_row(&h, "❯ List the numbers 1 to 70, one"),
            Some(1)
        );
    }

    #[test]
    fn two_prompts_starting_alike_go_to_the_latest() {
        let h = history(&[
            "fix the build please, then run the tests",
            "x",
            "fix the build please, then lint",
        ]);
        assert_eq!(prompt_of_row(&h, "❯ fix the build please, then"), Some(2));
        assert_eq!(
            prompt_of_row(&h, "❯ fix the build please, then lint"),
            Some(2)
        );
        assert_eq!(
            prompt_of_row(&h, "❯ fix the build please, then run"),
            Some(0)
        );
        assert_eq!(prompt_of_row(&h, "fix the build"), None, "no ❯, no prompt");
    }

    #[test]
    fn the_same_text_in_the_input_box_is_not_the_prompt() {
        let rows = screen(&[
            ("● output", false),
            ("────────────────────────────────────────", false),
            ("❯ fix the build", false),
            ("────────────────────────────────────────", false),
        ]);
        assert!(!on_screen("fix the build", &rows));
        // A sent prompt right under a rule is still shaded, and still counts.
        let rows = screen(&[
            ("● output", false),
            ("────────────────────────────────────────", false),
            ("❯ fix the build", true),
        ]);
        assert!(on_screen("fix the build", &rows));
    }

    /// One opaque colour, in every theme: nothing under the bar shows
    /// through it as the grid scrolls.
    #[test]
    fn the_bar_is_one_opaque_colour() {
        use giverny_term::render::theme::Theme;
        for name in Theme::NAMES {
            let chrome = Chrome::from_theme(&Theme::by_name(name));
            assert_eq!(fill(&chrome).a(), 255, "{name}");
            assert_ne!(fill(&chrome), chrome.panel, "{name}: apart from the panel");
        }
    }

    #[test]
    fn only_a_prompt_with_more_to_show_opens() {
        assert!(!expandable("fix the build", false), "fits: nothing to open");
        assert!(
            !expandable("  fix the build \n\n", false),
            "blank lines are not more"
        );
        assert!(expandable("fix the build", true), "cut to the width");
        assert!(
            expandable("fix the build\nthen test", false),
            "a second line"
        );
        assert_eq!(one_line_cut("abcdefg", 6), ("abcdef…".to_string(), true));
        assert_eq!(one_line_cut("abc", 6), ("abc".to_string(), false));
    }

    #[test]
    fn cjk_and_emoji_prompts_are_found_by_their_rows() {
        // The rows as the terminal reads them now: one character per
        // double-width cell, its spacer left out.
        let h = history(&[
            "日本語で長い答えを書いてください。各段落に見出しを付けて。",
            "👋 hi, write me a long story 🎉",
            "And the cubes of 1 to 70",
        ]);
        assert_eq!(
            prompt_of_row(
                &h,
                "❯ 日本語で長い答えを書いてください。各段落に見出しを付けて。"
            ),
            Some(0)
        );
        // Wrapped, and pinned by Claude with an ellipsis.
        assert_eq!(prompt_of_row(&h, "❯ 日本語で長い答えを書いて"), Some(0));
        assert_eq!(prompt_of_row(&h, "❯ 日本語で長い答え…"), Some(0));
        assert_eq!(
            prompt_of_row(&h, "❯ 👋 hi, write me a long story 🎉"),
            Some(1)
        );
        // Read with the spacers, as it was: no longer the prompt.
        assert_eq!(prompt_of_row(&h, "❯ 日 本 語 で 長 い 答 え"), None);
        let rows = screen(&[("第三段落の続き", false), ("もう少し", false)]);
        let above = screen(&[
            ("段落", false),
            ("❯ 日本語で長い答えを書いてください。", true),
        ]);
        let search = |m: &dyn Fn(&str, bool) -> Option<usize>| {
            above.iter().find_map(|(text, shaded)| m(text, *shaded))
        };
        assert_eq!(owner(&h, &rows, search), Some(0));
    }

    #[test]
    fn only_a_rows_worth_of_a_prompt_is_read() {
        assert_eq!(
            first_line_start("\n\n  fix   the\tbuild  \nthen test", 100),
            "fix the build".chars().collect::<Vec<_>>()
        );
        assert_eq!(
            first_line_start("fix   the build", 5),
            "fix t".chars().collect::<Vec<_>>()
        );
        assert_eq!(
            first_line_start("fix   the build", 4),
            "fix ".chars().collect::<Vec<_>>()
        );
        assert!(first_line_start("  \n ", 10).is_empty());
        // A pasted log on one line, a megabyte long: matched by its start.
        let log = format!("cargo build failed: {}", "x".repeat(1 << 20));
        let h = history(&[&log]);
        assert_eq!(prompt_of_row(&h, "❯ cargo build failed: xxxxxxxx"), Some(0));
        assert_eq!(prompt_of_row(&h, "❯ cargo build passed"), None);
    }

    #[test]
    fn the_owner_is_worked_out_again_only_when_something_changed() {
        let mut cache = Owner::default();
        let h = history(&TURNS);
        let runs = std::cell::Cell::new(0);
        let work = |answer| {
            runs.set(runs.get() + 1);
            answer
        };
        assert_eq!(cache.get(1, &h, || work(Some(1))), Some(1));
        assert_eq!(cache.get(1, &h, || work(Some(0))), Some(1), "kept");
        assert_eq!(runs.get(), 1);
        // The terminal changed.
        assert_eq!(cache.get(2, &h, || work(Some(0))), Some(0));
        // A prompt came.
        let more = history(&[TURNS[0], TURNS[1], TURNS[2], "and squares"]);
        assert_eq!(cache.get(2, &more, || work(Some(3))), Some(3));
        // The same number of prompts, another session's.
        let other = history(&["a", "b", "c", "d"]);
        assert_eq!(cache.get(2, &other, || work(None)), None);
        assert_eq!(cache.get(2, &other, || work(Some(2))), None, "kept");
        assert_eq!(runs.get(), 4);
    }

    #[test]
    fn the_bar_text_fits_the_row() {
        // The terminal's size, where its row has the room.
        assert_eq!(text_size(13.0, 17.0), 13.0);
        assert_eq!(text_size(24.0, 31.0), 24.0);
        // Smaller where a line of it would be taller than the row.
        assert!(text_size(8.0, 8.0) * 1.17 <= 8.0);
    }

    #[test]
    fn a_long_prompt_is_cut_with_an_ellipsis() {
        assert_eq!(one_line("abcdef", 6), "abcdef", "exactly max is not cut");
        assert_eq!(one_line("abcdefg", 6), "abcdef…");
        assert_eq!(one_line("abc def", 3), "abc…", "cut at the space");
        assert_eq!(
            one_line("abc def", 4),
            "abc…",
            "no space before the ellipsis"
        );
        assert_eq!(one_line("אבג דהו", 5), "אבג ד…", "characters, not bytes");
        assert_eq!(one_line("", 5), "");
    }
}
