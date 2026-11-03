//! A line from the worker page's prompt box, put at the manager's prompt.
//!
//! Claude Code gives no way to type to a subagent from outside: the one way
//! in is its parent's `SendMessage`. So a line typed on a worker's page goes
//! to the tab's own Claude Code, the manager, as a plain line naming the
//! worker ([`crate::row_open::Ask`]), and the manager passes it on. It is
//! pasted at the manager's prompt, so the manager reads it as the user's own
//! typing. Nothing is read off the screen.
//!
//! * **When.** Only while the hooks say Claude Code runs in the tab, and the
//!   tab's terminal has bracketed paste on. The session's registry entry
//!   (`sessions/<pid>.json`) is read at send time: `waiting` means a
//!   permission, a question or a plan approval is up, and the Enter would
//!   answer it, so the line is refused ([`refusal`]).
//! * **What.** The text without control characters ([`clean`]), split into
//!   bracketed pastes short enough that Claude Code folds none of them into
//!   `[Pasted text]` ([`chunks`]), all in one write, then `\r` ([`encode`]).
//! * **Confirm.** The send counts once the session's `UserPromptSubmit`
//!   carries the text ([`Pending`]). A compaction holds the prompt until it
//!   ends, so while one runs the wait goes on.

use std::time::{Duration, Instant};

use giverny_core::tabs::TabId;

use crate::management_panel::RowClick;

/// The longest paste Claude Code takes as typed, in UTF-16 units (its
/// string `length`): one longer is folded into `[Pasted text #N]` and
/// reaches the model wrapped in `<pasted_content>` (Claude Code 2.1.317).
pub const PASTE_UNITS: usize = 800;

/// The most line breaks a paste can hold and still be taken as typed.
pub const PASTE_BREAKS: usize = 2;

/// How long a line has to show up in a `UserPromptSubmit`. It took 30 to
/// 110 ms, idle or mid-turn alike.
pub const CONFIRM: Duration = Duration::from_secs(5);

/// The longest a compaction is waited out.
pub const COMPACT_MAX: Duration = Duration::from_secs(10 * 60);

/// `text` as it may go inside a bracketed paste: no ESC (the one byte that
/// could end the paste early), no other C0 control but the line break, no
/// DEL, no C1. Every line break is `\n`, a tab is four spaces (as Claude Code
/// shows one), and the ends are trimmed.
pub fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\n' => out.push('\n'),
            '\t' => out.push_str("    "),
            '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}' => {}
            c => out.push(c),
        }
    }
    out.trim().to_string()
}

/// `text` (already [`clean`]) in pieces of at most [`PASTE_UNITS`] UTF-16
/// units and [`PASTE_BREAKS`] line breaks, never split inside a character. A
/// break that would be one too many starts the next piece.
pub fn chunks(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut start, mut units, mut breaks) = (0, 0, 0);
    for (i, c) in text.char_indices() {
        let n = c.len_utf16();
        let brk = c == '\n';
        if i > start && (units + n > PASTE_UNITS || (brk && breaks == PASTE_BREAKS)) {
            out.push(&text[start..i]);
            (start, units, breaks) = (i, 0, 0);
        }
        units += n;
        breaks += usize::from(brk);
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// The bytes for the tab's terminal: each piece of `line` as its own
/// bracketed paste, back to back, then the Enter that submits them as one
/// prompt.
pub fn encode(line: &str) -> Vec<u8> {
    let line = clean(line);
    let mut out = Vec::with_capacity(line.len() + 16);
    for piece in chunks(&line) {
        out.extend_from_slice(b"\x1b[200~");
        out.extend(piece.replace('\n', "\r").into_bytes());
        out.extend_from_slice(b"\x1b[201~");
    }
    out.push(b'\r');
    out
}

/// Whether a submitted prompt is the line: the same words in the same order.
/// Claude Code turns a paste's breaks into `\n` and its tabs into spaces, and
/// may wrap a long paste in `<pasted_content>`, so only the words are
/// compared, the tags taken off.
pub fn same(prompt: &str, line: &str) -> bool {
    let prompt = giverny_claude::registry::unwrap_pastes(prompt);
    prompt.split_whitespace().eq(line.split_whitespace())
}

/// What the app knows about the tab when the box's Enter is pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Facts {
    /// Giverny's hooks report on this tab's session.
    pub hooked: bool,
    /// The hooks say Claude Code runs in the tab.
    pub running: bool,
    /// The session's registry entry, read now: `Some(waiting)`, or `None`
    /// when it has no live one.
    pub registry: Option<bool>,
    /// The tab's terminal has bracketed paste on.
    pub bracketed: bool,
    /// An earlier line is still being confirmed.
    pub in_flight: bool,
}

/// Why the line can't be sent now, if it can't.
pub fn refusal(f: &Facts) -> Option<&'static str> {
    if f.in_flight {
        return Some("Not sent: the last line is still on its way to the manager.");
    }
    if !f.hooked {
        return Some(
            "Not sent: Giverny's hooks don't report on this tab's Claude Code, so it can't \
             tell when a line is safe to send or whether it arrived.",
        );
    }
    let Some(waiting) = f.registry.filter(|_| f.running) else {
        return Some("Not sent: no Claude Code is running in this tab.");
    };
    if waiting {
        return Some("Not sent: the manager is waiting on a question in its tab.");
    }
    if !f.bracketed {
        return Some("Not sent: the tab's terminal isn't taking pastes.");
    }
    None
}

/// A line written to the manager's prompt, not yet seen to arrive.
#[derive(Debug, Clone)]
pub struct Pending {
    pub tab: TabId,
    /// The page's row: its result is shown only on its page.
    pub row: Option<RowClick>,
    /// What was sent.
    line: String,
    /// How many `UserPromptSubmit`s the tab had heard when it was sent.
    pub submits: u64,
    /// When it was sent.
    sent: Instant,
    /// When the wait for its `UserPromptSubmit` began: the send, or the end
    /// of a compaction it was held behind.
    since: Instant,
}

/// Where a [`Pending`] line is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Not seen yet; the wait goes on.
    Sending,
    /// Claude Code is compacting: the line waits at its prompt until that
    /// ends.
    Compacting,
    /// The session's `UserPromptSubmit` carried it.
    Delivered,
    /// Nothing carried it in time.
    NotSent,
}

impl Pending {
    pub fn new(tab: TabId, row: Option<RowClick>, line: &str, submits: u64, now: Instant) -> Self {
        Pending {
            tab,
            row,
            line: clean(line),
            submits,
            sent: now,
            since: now,
        }
    }

    /// One look: `prompts` are the tab's prompts submitted since the send,
    /// `compacting` whether a compaction runs in it now.
    pub fn step<'a>(
        &mut self,
        now: Instant,
        mut prompts: impl Iterator<Item = &'a str>,
        compacting: bool,
    ) -> Step {
        if prompts.any(|p| same(p, &self.line)) {
            return Step::Delivered;
        }
        if compacting {
            self.since = now;
            if now.duration_since(self.sent) > COMPACT_MAX {
                return Step::NotSent;
            }
            return Step::Compacting;
        }
        if now.duration_since(self.since) > CONFIRM {
            return Step::NotSent;
        }
        Step::Sending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units(s: &str) -> usize {
        s.encode_utf16().count()
    }

    fn breaks(s: &str) -> usize {
        s.matches('\n').count()
    }

    #[test]
    fn clean_strips_controls_and_keeps_the_rest() {
        assert_eq!(
            clean("a\x1b[201~b\x03c\x04d\x15e\x7ff\u{85}g\u{9b}h"),
            "a[201~bcdefgh"
        );
        assert_eq!(clean("one\r\ntwo\rthree\nfour"), "one\ntwo\nthree\nfour");
        assert_eq!(clean("a\tb"), "a    b");
        assert_eq!(clean("ü ï 😀 中"), "ü ï 😀 中");
        assert_eq!(clean("\x00\x08\x0b\x0c\x0e\x1a\x1c\x1f"), "");
        assert_eq!(clean("  \n hi \n\n"), "hi");
    }

    #[test]
    fn a_short_line_is_one_paste() {
        assert_eq!(chunks("hello"), vec!["hello"]);
        assert_eq!(chunks("a\nb\nc"), vec!["a\nb\nc"], "two breaks fit");
        assert!(chunks("").is_empty());
        let full = "x".repeat(PASTE_UNITS);
        assert_eq!(chunks(&full), vec![full.as_str()], "exactly 800 fits");
    }

    #[test]
    fn a_third_break_starts_the_next_paste() {
        assert_eq!(chunks("a\nb\nc\nd"), vec!["a\nb\nc", "\nd"]);
        // Thirteen lines: six pastes, as measured.
        let text: Vec<String> = (1..=13).map(|i| format!("line {i}")).collect();
        let text = text.join("\n");
        let parts = chunks(&text);
        assert_eq!(parts.len(), 6);
        assert_eq!(parts.concat(), text);
        assert!(parts.iter().all(|p| breaks(p) <= PASTE_BREAKS));
    }

    #[test]
    fn a_long_line_is_cut_at_800_units() {
        let text = "y".repeat(2001);
        let parts = chunks(&text);
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![800, 800, 401]
        );
    }

    #[test]
    fn units_are_utf16_and_no_character_is_split() {
        // An emoji is two units: 400 of them fill a paste.
        let text = "😀".repeat(401);
        let parts = chunks(&text);
        assert_eq!(parts.len(), 2);
        assert_eq!(units(parts[0]), 800);
        assert_eq!(parts[1], "😀");
        // One unit short of room for the next emoji: it goes to the next.
        let text = format!("{}😀", "a".repeat(799));
        let parts = chunks(&text);
        assert_eq!(parts, vec!["a".repeat(799).as_str(), "😀"]);
        // Every piece of a mixed text keeps to both limits.
        let text = "ab中😀\n".repeat(300);
        let parts = chunks(&text);
        assert_eq!(parts.concat(), text);
        assert!(
            parts
                .iter()
                .all(|p| units(p) <= PASTE_UNITS && breaks(p) <= PASTE_BREAKS)
        );
    }

    #[test]
    fn the_write_is_pastes_then_one_enter() {
        assert_eq!(
            encode("hi\x1b[201~"),
            b"\x1b[200~hi[201~\x1b[201~\r".to_vec()
        );
        let out = String::from_utf8(encode("a\nb\nc\nd")).unwrap();
        assert_eq!(out, "\x1b[200~a\rb\rc\x1b[201~\x1b[200~\rd\x1b[201~\r");
        // The only ESCs are the brackets', the only Enter outside them the
        // last byte.
        let out = encode(&"z\n".repeat(40));
        assert_eq!(out.last(), Some(&b'\r'));
        assert_eq!(
            out.iter().filter(|b| **b == 0x1b).count(),
            2 * chunks(&clean(&"z\n".repeat(40))).len()
        );
    }

    #[test]
    fn a_prompt_matches_by_its_words() {
        let line = clean("Send this to acme#1's worker (agent a1): go\n\tnow");
        assert!(same(
            "Send this to acme#1's worker (agent a1): go\n    now",
            &line
        ));
        assert!(same(
            "\n\n<pasted_content id=\"5897\">\nSend this to acme#1's worker (agent a1): go\nnow\n</pasted_content id=\"5897\">",
            &line
        ));
        assert!(!same("Send this to acme#1's worker (agent a1): go", &line));
    }

    fn facts() -> Facts {
        Facts {
            hooked: true,
            running: true,
            registry: Some(false),
            bracketed: true,
            in_flight: false,
        }
    }

    #[test]
    fn the_gate() {
        assert_eq!(refusal(&facts()), None);
        let waiting = Facts {
            registry: Some(true),
            ..facts()
        };
        assert_eq!(
            refusal(&waiting),
            Some("Not sent: the manager is waiting on a question in its tab.")
        );
        for f in [
            Facts {
                running: false,
                ..facts()
            },
            Facts {
                registry: None,
                ..facts()
            },
        ] {
            assert_eq!(
                refusal(&f),
                Some("Not sent: no Claude Code is running in this tab.")
            );
        }
        assert!(
            refusal(&Facts {
                hooked: false,
                ..facts()
            })
            .is_some_and(|w| w.contains("hooks"))
        );
        assert!(
            refusal(&Facts {
                bracketed: false,
                ..facts()
            })
            .is_some_and(|w| w.contains("pastes"))
        );
        assert!(
            refusal(&Facts {
                in_flight: true,
                ..facts()
            })
            .is_some_and(|w| w.contains("still on its way"))
        );
    }

    #[test]
    fn delivered_when_its_prompt_arrives() {
        let t0 = Instant::now();
        let mut p = Pending::new(TabId(1), None, "Start acme#2 now: go", 3, t0);
        assert_eq!(p.step(t0, std::iter::empty(), false), Step::Sending);
        let ms = |n| t0 + Duration::from_millis(n);
        assert_eq!(
            p.step(ms(100), ["something else"].into_iter(), false),
            Step::Sending
        );
        assert_eq!(
            p.step(ms(200), ["Start acme#2 now: go"].into_iter(), false),
            Step::Delivered
        );
    }

    #[test]
    fn not_sent_after_a_few_quiet_seconds() {
        let t0 = Instant::now();
        let mut p = Pending::new(TabId(1), None, "hi", 0, t0);
        assert_eq!(
            p.step(t0 + CONFIRM, std::iter::empty(), false),
            Step::Sending
        );
        assert_eq!(
            p.step(
                t0 + CONFIRM + Duration::from_millis(1),
                std::iter::empty(),
                false
            ),
            Step::NotSent
        );
    }

    #[test]
    fn a_compaction_holds_the_wait_until_it_ends() {
        let t0 = Instant::now();
        let s = |n| t0 + Duration::from_secs(n);
        let mut p = Pending::new(TabId(1), None, "hi", 0, t0);
        assert_eq!(p.step(s(1), std::iter::empty(), true), Step::Compacting);
        assert_eq!(p.step(s(30), std::iter::empty(), true), Step::Compacting);
        // Over: the few seconds start again from its end.
        assert_eq!(p.step(s(33), std::iter::empty(), false), Step::Sending);
        assert_eq!(p.step(s(34), ["hi"].into_iter(), false), Step::Delivered);
        let mut p = Pending::new(TabId(1), None, "hi", 0, t0);
        p.step(s(30), std::iter::empty(), true);
        assert_eq!(p.step(s(36), std::iter::empty(), false), Step::NotSent);
        // One that never ends is given up on.
        let mut p = Pending::new(TabId(1), None, "hi", 0, t0);
        assert_eq!(
            p.step(
                t0 + COMPACT_MAX + Duration::from_secs(1),
                std::iter::empty(),
                true
            ),
            Step::NotSent
        );
    }
}
