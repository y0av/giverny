//! Rows given to the program below the tab's picture, never drawn.
//!
//! A full-screen program lays its screen out to the terminal's height. When
//! it adds rows at the bottom of its screen that the app does not want shown
//! (Claude Code's own agent list, while Giverny's management panel stands in
//! for it), drawing them blank would still push everything above them up. Instead the program is told the terminal is that many rows
//! taller, so the rows above lay out exactly as they would without the
//! extra ones, and the picture is cut off where the tab ends.
//!
//! The rows the screen asks for change as the program draws (a worker
//! starts or ends), and each change is a resize the program answers with a
//! redraw. Between the two the grid is neither the old layout nor the new
//! one, so the picture is held ([`Step::Hold`]) from the moment the screen
//! asks for different rows until the program has drawn at the new height:
//! output after the resize, then a short calm (or [`ANSWER_BY`] of output
//! that never pauses), or [`GIVE_UP`] with no answer at all.
//!
//! Only the alternate screen gets these rows: a program there redraws all
//! of it on a resize, and it has no scrollback for a resize to move. A
//! program on the main screen (Claude Code outside fullscreen) is never
//! resized for them.

use std::time::{Duration, Instant};

/// Output that pauses this long after a resize is the program's redraw at
/// the new height.
pub const CALM: Duration = Duration::from_millis(30);
/// Output that never pauses (a spinner, a reply streaming) counts as the
/// redraw once it has gone on this long after the resize.
pub const ANSWER_BY: Duration = Duration::from_millis(120);
/// A program that writes nothing this long after a resize is not going to
/// redraw for it: the picture is released as it is.
pub const GIVE_UP: Duration = Duration::from_millis(1000);

/// What the widget does with this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Draw as usual.
    Draw,
    /// Keep painting the last picture: the program is redrawing.
    Hold,
    /// Tell the program the terminal is this many rows taller than the
    /// tab, and hold the picture until it has redrawn.
    Resize(u16),
}

#[derive(Debug, Clone, Copy)]
struct Pending {
    since: Instant,
    /// The program's output count at the resize ([`crate::proxy::SharedTermState::output_seq`]).
    output: u64,
    /// When output was first and last seen since the resize.
    first_output: Option<Instant>,
    last_output: Option<Instant>,
    /// The picture is held meanwhile: a resize of ours. One of the tab's
    /// own size ([`Tail::resized`]) is drawn as it comes, as any
    /// terminal's is; only the screen is not asked until it has settled.
    hold: bool,
}

impl Pending {
    fn new(now: Instant, output: u64, hold: bool) -> Pending {
        Pending {
            since: now,
            output,
            first_output: None,
            last_output: None,
            hold,
        }
    }
}

/// One tab's rows past the bottom of its picture.
#[derive(Debug, Clone, Default)]
pub struct Tail {
    rows: u16,
    pending: Option<Pending>,
}

impl Tail {
    /// Rows the program is given past the bottom of the picture.
    pub fn rows(&self) -> u16 {
        self.rows
    }

    /// Whether a resize is waiting for the program's redraw: the screen
    /// must be read every frame meanwhile.
    pub fn pending(&self) -> bool {
        self.pending.is_some()
    }

    /// The tab itself changed size (the window, or the management panel
    /// opening under it): the program redraws for that, and until it has,
    /// the grid is the old screen cut or padded to the new size, which says
    /// nothing about the rows it wants out of sight. Drawn as it comes.
    pub fn resized(&mut self, now: Instant, output: u64) {
        self.pending = Some(Pending::new(now, output, false));
    }

    /// One frame. `want` is how many rows the screen asks to keep out of
    /// sight, when it was read this frame (`None`: not read, nothing
    /// changed); `output` the program's output count.
    pub fn step(&mut self, now: Instant, want: Option<u16>, output: u64) -> Step {
        if let Some(p) = &mut self.pending {
            if output != p.output {
                p.output = output;
                p.first_output.get_or_insert(now);
                p.last_output = Some(now);
            }
            let answered = p.last_output.is_some_and(|last| {
                now.duration_since(last) >= CALM || now.duration_since(p.since) >= ANSWER_BY
            });
            let held = now.duration_since(p.since);
            if !answered && held < GIVE_UP {
                return if p.hold { Step::Hold } else { Step::Draw };
            }
            let p = self.pending.take().expect("pending");
            if p.hold {
                tracing::debug!(
                    "tail: {} rows, picture held {} ms{}",
                    self.rows,
                    held.as_millis(),
                    match p.first_output {
                        Some(first) => format!(
                            ", first output after {} ms",
                            first.duration_since(p.since).as_millis()
                        ),
                        None => " (no redraw)".to_string(),
                    }
                );
            }
            // A program that never answered is not asked again until it
            // writes: its screen is the old layout cut at the new height.
            if !answered {
                return Step::Draw;
            }
        }
        match want {
            Some(want) if want != self.rows => {
                tracing::debug!("tail: {} -> {want} rows", self.rows);
                self.rows = want;
                self.pending = Some(Pending::new(now, output, true));
                Step::Resize(want)
            }
            _ => Step::Draw,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn a_new_height_resizes_and_holds_until_the_program_has_redrawn() {
        let t0 = Instant::now();
        let mut tail = Tail::default();
        assert_eq!(tail.step(t0, Some(0), 0), Step::Draw);
        // A worker starts: the screen asks for five rows out of sight.
        assert_eq!(tail.step(t0, Some(5), 1), Step::Resize(5));
        assert_eq!(tail.rows(), 5);
        // The grid grown, nothing redrawn yet: its tail is blank, so the
        // screen asks for none, which a pending resize ignores.
        assert_eq!(tail.step(t0 + ms(10), Some(0), 1), Step::Hold);
        // The redraw lands, then the program is quiet.
        assert_eq!(tail.step(t0 + ms(20), Some(5), 2), Step::Hold);
        assert_eq!(tail.step(t0 + ms(40), Some(5), 2), Step::Hold);
        assert_eq!(tail.step(t0 + ms(60), Some(5), 2), Step::Draw);
        assert!(!tail.pending());
        // Unchanged, unread: nothing happens.
        assert_eq!(tail.step(t0 + ms(80), None, 3), Step::Draw);
        assert_eq!(tail.step(t0 + ms(90), Some(5), 4), Step::Draw);
    }

    #[test]
    fn workers_starting_together_resize_once_per_answer() {
        let t0 = Instant::now();
        let mut tail = Tail::default();
        assert_eq!(tail.step(t0, Some(3), 1), Step::Resize(3));
        // The second worker arrives while the first resize is answered:
        // asked again only once the program is calm, at the newest height.
        assert_eq!(tail.step(t0 + ms(15), Some(4), 2), Step::Hold);
        assert_eq!(tail.step(t0 + ms(25), Some(5), 3), Step::Hold);
        assert_eq!(tail.step(t0 + ms(60), Some(5), 3), Step::Resize(5));
        assert_eq!(tail.step(t0 + ms(70), Some(5), 4), Step::Hold);
        assert_eq!(tail.step(t0 + ms(110), Some(5), 4), Step::Draw);
        assert_eq!(tail.rows(), 5);
    }

    #[test]
    fn streaming_output_counts_as_the_answer_after_a_while() {
        let t0 = Instant::now();
        let mut tail = Tail::default();
        assert_eq!(tail.step(t0, Some(4), 0), Step::Resize(4));
        // Output every 16 ms, never calm.
        let mut seq = 0;
        let mut at = 0;
        let step = loop {
            at += 16;
            seq += 1;
            match tail.step(t0 + ms(at), Some(4), seq) {
                Step::Hold => continue,
                s => break s,
            }
        };
        assert_eq!(step, Step::Draw);
        assert!(ms(at) >= ANSWER_BY && ms(at) < ANSWER_BY + ms(20), "{at}");
    }

    #[test]
    fn a_silent_program_is_given_up_on_and_not_asked_again_until_it_writes() {
        let t0 = Instant::now();
        let mut tail = Tail::default();
        tail.step(t0, Some(4), 7);
        assert_eq!(tail.step(t0 + ms(500), Some(0), 7), Step::Hold);
        assert_eq!(tail.step(t0 + GIVE_UP, Some(0), 7), Step::Draw);
        assert_eq!(tail.rows(), 4);
        // Read again only on output; then it is asked for what it shows.
        assert_eq!(tail.step(t0 + GIVE_UP + ms(5), None, 7), Step::Draw);
        assert_eq!(tail.step(t0 + GIVE_UP + ms(9), Some(0), 8), Step::Resize(0));
    }

    #[test]
    fn the_tab_resized_is_drawn_and_its_screen_asked_only_once_redrawn() {
        let t0 = Instant::now();
        let mut tail = Tail::default();
        tail.step(t0, Some(3), 0);
        tail.step(t0 + ms(10), Some(3), 1);
        assert_eq!(tail.step(t0 + ms(50), Some(3), 1), Step::Draw);
        // The panel opens under the tab: the grid is cut shorter, the list
        // no longer at its bottom. Drawn, not held, and not taken for a
        // screen with no list.
        tail.resized(t0 + ms(100), 1);
        assert_eq!(tail.step(t0 + ms(101), Some(0), 1), Step::Draw);
        assert_eq!(tail.step(t0 + ms(110), Some(0), 2), Step::Draw);
        assert_eq!(tail.rows(), 3);
        // Redrawn at the new size, list at the bottom again: nothing to do.
        assert_eq!(tail.step(t0 + ms(150), Some(3), 2), Step::Draw);
        assert!(!tail.pending());
        assert_eq!(tail.rows(), 3);
    }

    #[test]
    fn the_last_worker_ending_gives_the_rows_back() {
        let t0 = Instant::now();
        let mut tail = Tail::default();
        tail.step(t0, Some(5), 0);
        tail.step(t0 + ms(10), Some(5), 1);
        assert_eq!(tail.step(t0 + ms(50), Some(5), 1), Step::Draw);
        // The list is gone: the program drew its screen without it, five
        // rows lower, and is told its real height again.
        assert_eq!(tail.step(t0 + ms(100), Some(0), 2), Step::Resize(0));
        assert_eq!(tail.rows(), 0);
        // The shrunk grid asks for nothing either, but the redraw is still
        // awaited before it is drawn.
        assert_eq!(tail.step(t0 + ms(105), Some(0), 2), Step::Hold);
        assert_eq!(tail.step(t0 + ms(120), Some(0), 3), Step::Hold);
        assert_eq!(tail.step(t0 + ms(160), Some(0), 3), Step::Draw);
    }
}
