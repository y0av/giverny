//! A keyboard walk through the tabs: Ctrl+Tab (recency order) or
//! Ctrl+PageUp/PageDown (rail order), with Ctrl held between the presses.
//!
//! A restored tab starts lazily, when it first becomes the active one. A walk
//! makes every tab it passes over active for a moment, so tapping through the
//! list used to start each tab on the way — a shell, and a `claude --resume`
//! for a Claude tab, for every tab merely looked at. While the walk is held a
//! tab that has not started is only shown, from its saved scrollback; the one
//! the walk ends on starts when Ctrl comes up.

use crate::chrome::Chrome;
use egui::{Align2, FontId, Rect, Vec2};
use giverny_core::tabs::TabId;

/// How many of the saved scrollback's last lines a preview keeps: more than
/// any pane shows, few enough to lay out each frame.
const PREVIEW_LINES: usize = 300;

/// Whether the active tab's process is started now. A tab that is already
/// running, or whose process exited (it waits for "respawn"), is left alone;
/// one that has not started waits while a keyboard walk is held, since the
/// walk may only be passing over it.
pub fn starts_now(started: bool, exited: bool, walking: bool) -> bool {
    !started && !exited && !walking
}

/// Whether a walk is still held: Ctrl is down and the window has the
/// keyboard. Losing the keyboard ends it as Alt+Tab's ends: no key-up
/// reaches a window that is not focused, so waiting for one could leave the
/// tab the walk stopped on unstarted while it is the one on screen.
pub fn still_held(ctrl: bool, focused: bool) -> bool {
    ctrl && focused
}

/// What a walk's preview says of the tab it passes over.
pub const WALKING: &str = "not started · starts when Ctrl is released";

/// What a tab stopped while on screen says.
pub const STOPPED: &str = "stopped · click to start";

/// What a tab the walk is passing over shows in place of its terminal: the
/// tail of its saved scrollback, as text. Read once per tab per walk.
pub struct Preview {
    pub tab: TabId,
    lines: Vec<String>,
}

impl Preview {
    /// The tab's saved scrollback (`state::load_snapshot`'s dump), or `None`
    /// for a tab with nothing saved.
    pub fn new(tab: TabId, dump: Option<&str>) -> Self {
        Self {
            tab,
            lines: dump.map(|d| tail(d, PREVIEW_LINES)).unwrap_or_default(),
        }
    }

    /// Paint it over `rect` as the terminal would show it once it starts —
    /// from the top, or its last lines when it is longer than the pane —
    /// under a line saying why it is not live: `caption`, [`WALKING`] for a
    /// walk's.
    pub fn show(
        &self,
        ui: &mut egui::Ui,
        rect: Rect,
        chrome: &Chrome,
        font_size: f32,
        caption: &str,
    ) {
        let painter = ui.painter_at(rect);
        let pad = Vec2::splat(8.0);
        let caption_font = FontId::proportional(12.0);
        let band_h = 22.0;
        let body = Rect::from_min_max(rect.min + Vec2::new(0.0, band_h), rect.max);
        if !self.lines.is_empty() {
            let galley = painter.layout_no_wrap(
                self.lines.join("\n"),
                FontId::monospace(font_size),
                chrome.dim,
            );
            // From the top while it fits; past that the newest lines stay in
            // view and the oldest are clipped above.
            let at = egui::pos2(
                body.min.x + pad.x,
                (body.max.y - pad.y - galley.size().y).min(body.min.y + pad.y),
            );
            painter.with_clip_rect(body).galley(at, galley, chrome.dim);
        }
        painter.rect_filled(
            Rect::from_min_size(rect.min, Vec2::new(rect.width(), band_h)),
            0.0,
            chrome.panel,
        );
        painter.text(
            rect.min + Vec2::new(pad.x, band_h / 2.0),
            Align2::LEFT_CENTER,
            if self.lines.is_empty() {
                format!("{caption} · nothing saved")
            } else {
                caption.to_string()
            },
            caption_font,
            chrome.dim,
        );
    }

    #[cfg(test)]
    fn lines(&self) -> &[String] {
        &self.lines
    }
}

/// The last `n` lines of an ANSI dump as plain text. A saved dump carries
/// only SGR styling (`Session::snapshot_ansi`), but any escape is dropped —
/// CSI, OSC, and the two-byte kind — so a stray one cannot show as garbage.
fn tail(dump: &str, n: usize) -> Vec<String> {
    let mut plain = String::with_capacity(dump.len());
    let mut chars = dump.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => match chars.next() {
                // CSI: parameters, then one final byte in @..~.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC: up to BEL or ST (ESC \).
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' {
                            chars.next_if_eq(&'\\');
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\r' => {}
            c if c.is_control() && c != '\n' && c != '\t' => {}
            c => plain.push(c),
        }
    }
    let lines: Vec<&str> = plain.trim_end().lines().collect();
    let from = lines.len().saturating_sub(n);
    lines[from..].iter().map(|l| l.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rule: a walk held over a tab that has not started does not start
    /// it; the walk ending (Ctrl up, or the window losing the keyboard) does,
    /// and only for the tab it ended on — the one still active.
    #[test]
    fn a_held_walk_starts_nothing_and_its_end_starts_the_tab_it_is_on() {
        // Passing over a restored tab with Ctrl down.
        assert!(!starts_now(false, false, true));
        // Ctrl up: the tab on screen starts.
        assert!(starts_now(false, false, false));
        // A running tab is never started again, walk or not.
        assert!(!starts_now(true, false, false));
        assert!(!starts_now(true, false, true));
        // An exited one waits for "respawn", walk or not.
        assert!(!starts_now(false, true, false));
        assert!(!starts_now(false, true, true));
    }

    /// The walk is held only while Ctrl is down in a focused window: a
    /// window that lost the keyboard gets no key-up, so it counts as
    /// released rather than leaving the active tab unstarted.
    #[test]
    fn a_walk_is_held_only_with_ctrl_down_in_a_focused_window() {
        assert!(still_held(true, true));
        assert!(!still_held(false, true));
        assert!(!still_held(true, false));
        assert!(!still_held(false, false));
    }

    /// A preview is the dump's text without its styling, its last lines.
    #[test]
    fn a_preview_is_the_plain_tail_of_the_saved_scrollback() {
        let dump =
            "\x1b[1m\x1b[38;5;2mone\x1b[0m\r\n\x1b]0;title\x07two\r\nthree\x1b]8;;x\x1b\\\r\n\r\n";
        assert_eq!(tail(dump, 10), ["one", "two", "three"]);
        assert_eq!(tail(dump, 2), ["two", "three"]);
        assert!(tail("", 5).is_empty());
        let preview = Preview::new(TabId(7), None);
        assert!(preview.lines().is_empty());
        assert_eq!(preview.tab, TabId(7));
    }
}
