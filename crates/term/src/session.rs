//! `TermSession`: one tab's live terminal bundle — Term + io loop + channels.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;

use alacritty_terminal::event::Notify;
use alacritty_terminal::grid::Scroll;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Osc52, Term, TermMode, test::TermSize};
use alacritty_terminal::tty::Pty;
use crossbeam_channel::Receiver;
use parking_lot::RwLock;

use crate::io_loop::{IoLoop, LoopSender, Msg, Notifier, State, WriteBack};
use crate::proxy::{EventProxy, SharedTermState, TabEvent};
use crate::pty::{self, GridSize, SpawnCfg};
use crate::render::theme::Theme;
use crate::tee::Tee;

pub struct TermSession {
    pub term: Arc<FairMutex<Term<EventProxy>>>,
    pub events: Receiver<TabEvent>,
    pub shared: Arc<SharedTermState>,
    /// Shell process id (unix), for `/proc/<pid>/cwd` fallback tracking.
    pub child_pid: Option<u32>,
    /// Set once the user has interacted with this session (typing, clicks) —
    /// automated injections must stand down after that.
    user_input: Arc<AtomicBool>,
    /// Counts user interactions so the app can detect "typed since last frame".
    input_seq: Arc<std::sync::atomic::AtomicU64>,
    sender: LoopSender,
    notifier: Notifier,
    dirty: Arc<AtomicBool>,
    /// Bumped each time `dirty` is taken: [`Self::content_seq`].
    content_seq: AtomicU64,
    size: GridSize,
    handle: Option<JoinHandle<(IoLoop<Pty, EventProxy>, State)>>,
}

impl TermSession {
    /// Spawn a tab. `preseed` is ANSI advanced into the terminal *before* the
    /// shell starts, verbatim: restored scrollback (from
    /// [`Self::snapshot_ansi`]) appears above the fresh prompt, colors intact,
    /// and re-wraps naturally at the current width; a welcome screen arrives
    /// the same way. What it says is the caller's business.
    pub fn spawn(
        cfg: &SpawnCfg,
        egui_ctx: egui::Context,
        theme: Theme,
        preseed: Option<&str>,
    ) -> anyhow::Result<Self> {
        let pty = pty::spawn(cfg, 0)?;
        #[cfg(unix)]
        let child_pid = Some(pty.child().id());
        #[cfg(not(unix))]
        let child_pid = None;

        let (tx, events) = crossbeam_channel::unbounded();
        let dirty = Arc::new(AtomicBool::new(true));
        let write_back = Arc::new(WriteBack::default());
        let shared = Arc::new(SharedTermState {
            graphics: parking_lot::Mutex::new(crate::graphics::Graphics::default()),
            theme: RwLock::new(theme),
            size: RwLock::new(cfg.size),
        });
        let proxy = EventProxy::new(
            tx,
            egui_ctx,
            dirty.clone(),
            write_back.clone(),
            shared.clone(),
        );

        let term_config = Config {
            scrolling_history: 10_000,
            kitty_keyboard: true,
            osc52: Osc52::OnlyCopy,
            ..Config::default()
        };
        let term = Arc::new(FairMutex::new(Term::new(
            term_config,
            &TermSize::new(cfg.size.cols as usize, cfg.size.rows as usize),
            proxy.clone(),
        )));

        if let Some(dump) = preseed
            && !dump.is_empty()
        {
            use alacritty_terminal::vte::ansi::Processor;
            let mut parser: Processor = Processor::new();
            let mut guard = term.lock();
            parser.advance(&mut *guard, dump.as_bytes());
        }

        let tee = Tee::new(cfg.nonce.clone(), local_hostname());
        let io = IoLoop::new(term.clone(), proxy, pty, tee, write_back.clone(), true)?;
        let sender = io.channel();
        let notifier = Notifier(sender.clone());
        let handle = io.spawn();

        Ok(TermSession {
            term,
            events,
            shared,
            child_pid,
            user_input: Arc::new(AtomicBool::new(false)),
            input_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            sender,
            notifier,
            dirty,
            content_seq: AtomicU64::new(0),
            size: cfg.size,
            handle: Some(handle),
        })
    }

    /// Write user input to the PTY.
    /// The user interacted with this session (typed, clicked) — automated
    /// injections (cwd fix, auto-resume) must stand down.
    pub fn note_user_input(&self) {
        self.user_input.store(true, Ordering::Release);
        self.input_seq.fetch_add(1, Ordering::Release);
    }

    pub fn had_user_input(&self) -> bool {
        self.user_input.load(Ordering::Acquire)
    }

    /// Monotonic count of user interactions; a change since the last frame
    /// means the user just typed here (used to clear attention markers).
    pub fn input_seq(&self) -> u64 {
        self.input_seq.load(Ordering::Acquire)
    }

    /// The shell's live working directory: `/proc` on Linux (cheap), and
    /// `sysinfo` elsewhere — this is what keeps tab paths and Claude resume
    /// targets correct as the user `cd`s around.
    pub fn proc_cwd(&self) -> Option<std::path::PathBuf> {
        let pid = self.child_pid?;
        #[cfg(target_os = "linux")]
        {
            std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
        }
        #[cfg(not(target_os = "linux"))]
        {
            use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
            let mut sys = System::new();
            sys.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
                true,
                ProcessRefreshKind::nothing().with_cwd(sysinfo::UpdateKind::Always),
            );
            sys.process(Pid::from_u32(pid))
                .and_then(|p| p.cwd().map(|c| c.to_path_buf()))
        }
    }

    pub fn write(&self, bytes: impl Into<std::borrow::Cow<'static, [u8]>>) {
        self.notifier.notify(bytes.into());
    }

    /// Current terminal mode (brief lock).
    pub fn mode(&self) -> TermMode {
        *self.term.lock().mode()
    }

    /// Resize grid + PTY when the geometry changed.
    pub fn resize(&mut self, size: GridSize) {
        if size == self.size || size.cols < 2 || size.rows < 2 {
            return;
        }
        self.size = size;
        *self.shared.size.write() = size;
        self.term
            .lock()
            .resize(TermSize::new(size.cols as usize, size.rows as usize));
        let _ = self.sender.send(Msg::Resize(size.into()));
        self.dirty.store(true, Ordering::Release);
    }

    pub fn size(&self) -> GridSize {
        self.size
    }

    /// Serialize scrollback + screen to an ANSI dump for restore-time
    /// pre-seeding: logical lines (wrapped rows joined so they re-wrap at any
    /// width), styles re-emitted as SGR. `None` while on the alt screen
    /// (vim/fullscreen apps shouldn't persist).
    pub fn snapshot_ansi(&self, max_rows: usize) -> Option<String> {
        use alacritty_terminal::grid::Dimensions;
        use alacritty_terminal::index::{Column, Line, Point};
        use alacritty_terminal::term::cell::Flags;

        let term = self.term.lock();
        if term.mode().contains(TermMode::ALT_SCREEN) {
            return None;
        }
        let grid = term.grid();
        let cols = grid.columns();
        let screen = grid.screen_lines() as i32;
        let history = (grid.total_lines() - grid.screen_lines()) as i32;

        // Last row worth saving: bottom-most screen row with content.
        let mut last = -1;
        for l in (0..screen).rev() {
            let has_content = (0..cols).any(|c| {
                let cell = &grid[Point::new(Line(l), Column(c))];
                cell.c != ' '
                    || cell.bg
                        != alacritty_terminal::vte::ansi::Color::Named(
                            alacritty_terminal::vte::ansi::NamedColor::Background,
                        )
            });
            if has_content {
                last = l;
                break;
            }
        }
        if last < 0 && history == 0 {
            return None;
        }

        let first = (-history).max(last - max_rows as i32 + 1).min(0);
        let mut out = String::with_capacity(64 * 1024);
        let mut style = SgrTracker::default();

        for l in first..=last {
            let line = Line(l);
            // Trim trailing cells that are blank in every respect.
            let mut end = 0;
            for c in (0..cols).rev() {
                let cell = &grid[Point::new(line, Column(c))];
                let blank = cell.c == ' '
                    && cell.bg
                        == alacritty_terminal::vte::ansi::Color::Named(
                            alacritty_terminal::vte::ansi::NamedColor::Background,
                        )
                    && !cell
                        .flags
                        .intersects(Flags::ALL_UNDERLINES | Flags::STRIKEOUT | Flags::INVERSE);
                if !blank {
                    end = c + 1;
                    break;
                }
            }
            for c in 0..end {
                let cell = &grid[Point::new(line, Column(c))];
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                style.emit_diff(&mut out, cell.fg, cell.bg, cell.flags);
                out.push(cell.c);
                if let Some(extra) = cell.zerowidth() {
                    out.extend(extra.iter());
                }
            }
            let wrapped = cols > 0
                && grid[Point::new(line, Column(cols - 1))]
                    .flags
                    .contains(Flags::WRAPLINE);
            if !wrapped {
                out.push_str("\x1b[0m\r\n");
                style = SgrTracker::default();
            }
        }
        out.push_str("\x1b[0m");
        Some(out)
    }

    /// One visible row as text (0 = top of the viewport).
    pub fn row_text(&self, row: u16) -> String {
        use alacritty_terminal::grid::Dimensions;
        use alacritty_terminal::index::{Column, Line, Point};
        let term = self.term.lock();
        let grid = term.grid();
        if row as usize >= grid.screen_lines() {
            return String::new();
        }
        (0..grid.columns())
            .map(|c| grid[Point::new(Line(row as i32), Column(c))].c)
            .collect()
    }

    /// The OSC 8 hyperlink at a screen cell, and the run of cells sharing it.
    ///
    /// A program that emits OSC 8 puts the URL in the escape sequence and only
    /// the label on screen — `click here`, or a shortened title. No amount of
    /// looking at the visible text finds it, so the cell metadata is the only
    /// place the link exists.
    pub fn hyperlink_at(&self, row: u16, col: u16) -> Option<(String, u16, u16)> {
        use alacritty_terminal::grid::Dimensions;
        use alacritty_terminal::index::{Column, Line, Point};
        let term = self.term.lock();
        let grid = term.grid();
        if row as usize >= grid.screen_lines() || col as usize >= grid.columns() {
            return None;
        }
        let at = |c: usize| grid[Point::new(Line(row as i32), Column(c))].hyperlink();
        let here = at(col as usize)?;
        let uri = here.uri().to_string();
        // Underline the whole link, not the character under the pointer. Cells
        // are one link when they carry the same id — two adjacent links with
        // the same target stay separate, which is what the id is for.
        let same = |c: usize| at(c).is_some_and(|h| h.id() == here.id());
        let mut start = col as usize;
        while start > 0 && same(start - 1) {
            start -= 1;
        }
        let mut end = col as usize;
        while end + 1 < grid.columns() && same(end + 1) {
            end += 1;
        }
        Some((uri, start as u16, (end - start + 1) as u16))
    }

    /// Visible screen contents as text (row-major, newline-separated).
    /// Diagnostics/tests; trims trailing spaces per row.
    pub fn screen_text(&self) -> String {
        use alacritty_terminal::grid::Dimensions;
        use alacritty_terminal::index::{Column, Line, Point};
        let term = self.term.lock();
        let grid = term.grid();
        let mut out = String::new();
        for line in 0..grid.screen_lines() {
            let mut row = String::new();
            for col in 0..grid.columns() {
                let point = Point::new(Line(line as i32), Column(col));
                row.push(grid[point].c);
            }
            out.push_str(row.trim_end());
            out.push('\n');
        }
        out
    }

    /// The rows on screen right now, top to bottom, scrollback position
    /// included: each row's text, and whether its first cell has a background
    /// of its own (Claude Code shades a sent prompt; its input box it does
    /// not).
    pub fn viewport_rows(&self) -> Vec<(String, bool)> {
        viewport_rows(self.term.lock().grid())
    }

    /// Walk the rows above the top of the view, nearest first, at most `max`
    /// of them, until `found` returns something. Each row comes as its text
    /// and whether its first cell is shaded, as in [`Self::viewport_rows`].
    ///
    /// Only rows whose first character that is not blank is one of `starts`
    /// are read and handed over: the walk holds the terminal's lock, which
    /// the output parser needs, and most rows are told apart by a cell or two.
    pub fn find_above<T>(
        &self,
        max: usize,
        starts: &[char],
        found: impl FnMut(&str, bool) -> Option<T>,
    ) -> Option<T> {
        find_above(self.term.lock().grid(), max, starts, found)
    }

    /// Counts the changes to what the terminal shows, as the widget takes
    /// them to draw: output, a scroll, a resize. While it stands still, so
    /// does every row on screen and in the scrollback.
    pub fn content_seq(&self) -> u64 {
        self.content_seq.load(Ordering::Acquire)
    }

    /// Snap the viewport back to the live (bottom) position.
    pub fn scroll_to_bottom(&self) {
        self.term.lock().scroll_display(Scroll::Bottom);
        self.dirty.store(true, Ordering::Release);
    }

    /// Scroll the viewport by whole lines (positive = towards history).
    pub fn scroll_lines(&self, lines: i32) {
        if lines != 0 {
            self.term.lock().scroll_display(Scroll::Delta(lines));
            self.dirty.store(true, Ordering::Release);
        }
    }

    /// True when terminal content changed since the last call (consumes flag).
    pub fn take_dirty(&self) -> bool {
        let dirty = self.dirty.swap(false, Ordering::AcqRel);
        if dirty {
            self.content_seq.fetch_add(1, Ordering::AcqRel);
        }
        dirty
    }

    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
    }

    /// Ask the io loop to stop and join it.
    pub fn shutdown(self) {
        self.shutdown_within(std::time::Duration::from_millis(500));
    }

    /// [`Session::shutdown`], waiting up to `wait` for the io thread, which
    /// drops the pty and with it waits for the shell to exit. True when it
    /// did: the shell is gone, and has written whatever it writes on SIGHUP.
    pub fn shutdown_within(mut self, wait: std::time::Duration) -> bool {
        let _ = self.sender.send(Msg::Shutdown);
        let Some(handle) = self.handle.take() else {
            return true;
        };
        // Waited for, but not indefinitely. The io thread almost always
        // returns at once; an io thread blocked writing to a pty whose child
        // has stopped reading never does, and `join` has no timeout — so one
        // wedged tab held the whole window open until someone force-killed
        // it, which is precisely the unclean shutdown the next launch
        // complains about. The process is on its way out; a thread that will
        // not come is left behind rather than waited on.
        let (done, waited) = std::sync::mpsc::channel();
        let watcher = std::thread::Builder::new()
            .name("giverny pty join".into())
            .spawn(move || {
                let _ = handle.join();
                let _ = done.send(());
            });
        watcher.is_ok() && waited.recv_timeout(wait).is_ok()
    }
}

type Grid = alacritty_terminal::grid::Grid<alacritty_terminal::term::cell::Cell>;

/// The cell at `col` of `line`, unless it is the spacer a double-width
/// character leaves after it (or before it, wrapped to the next row): that
/// is no character of its own, and read as one, `❯ 日本語` came out as
/// `❯ 日 本 語`.
fn char_at(grid: &Grid, line: alacritty_terminal::index::Line, col: usize) -> Option<char> {
    use alacritty_terminal::index::{Column, Point};
    use alacritty_terminal::term::cell::Flags;
    let cell = &grid[Point::new(line, Column(col))];
    (!cell
        .flags
        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER))
    .then_some(cell.c)
}

/// A row's text as it reads, into `out`.
fn row_text(grid: &Grid, line: alacritty_terminal::index::Line, out: &mut String) {
    use alacritty_terminal::grid::Dimensions;
    out.clear();
    out.extend((0..grid.columns()).filter_map(|c| char_at(grid, line, c)));
}

/// Does a row's first cell have a background of its own?
fn row_shaded(grid: &Grid, line: alacritty_terminal::index::Line) -> bool {
    use alacritty_terminal::index::{Column, Point};
    use alacritty_terminal::vte::ansi::{Color, NamedColor};
    grid[Point::new(line, Column(0))].bg != Color::Named(NamedColor::Background)
}

/// [`TermSession::viewport_rows`] of a grid.
fn viewport_rows(grid: &Grid) -> Vec<(String, bool)> {
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::Line;
    let offset = grid.display_offset() as i32;
    (0..grid.screen_lines() as i32)
        .map(|row| {
            let line = Line(row - offset);
            let mut text = String::with_capacity(grid.columns());
            row_text(grid, line, &mut text);
            (text, row_shaded(grid, line))
        })
        .collect()
}

/// [`TermSession::find_above`] in a grid.
fn find_above<T>(
    grid: &Grid,
    max: usize,
    starts: &[char],
    mut found: impl FnMut(&str, bool) -> Option<T>,
) -> Option<T> {
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::Line;
    let top = -(grid.display_offset() as i32);
    let oldest = -(grid.history_size() as i32);
    let mut text = String::with_capacity(grid.columns());
    for line in (oldest..top).rev().take(max).map(Line) {
        let first = (0..grid.columns())
            .filter_map(|c| char_at(grid, line, c))
            .find(|ch| !ch.is_whitespace() && *ch != '\0');
        if !first.is_some_and(|ch| starts.contains(&ch)) {
            continue;
        }
        row_text(grid, line, &mut text);
        if let Some(hit) = found(&text, row_shaded(grid, line)) {
            return Some(hit);
        }
    }
    None
}

/// Minimal SGR re-emitter for snapshot serialization: on any style change,
/// resets and re-applies the full attribute set (simple and always correct).
#[derive(Default)]
struct SgrTracker {
    current: Option<(
        alacritty_terminal::vte::ansi::Color,
        alacritty_terminal::vte::ansi::Color,
        alacritty_terminal::term::cell::Flags,
    )>,
}

impl SgrTracker {
    fn emit_diff(
        &mut self,
        out: &mut String,
        fg: alacritty_terminal::vte::ansi::Color,
        bg: alacritty_terminal::vte::ansi::Color,
        flags: alacritty_terminal::term::cell::Flags,
    ) {
        use alacritty_terminal::term::cell::Flags;
        let styled = flags
            & (Flags::BOLD
                | Flags::DIM
                | Flags::ITALIC
                | Flags::ALL_UNDERLINES
                | Flags::INVERSE
                | Flags::HIDDEN
                | Flags::STRIKEOUT);
        if self.current == Some((fg, bg, styled)) {
            return;
        }
        self.current = Some((fg, bg, styled));

        out.push_str("\x1b[0");
        if styled.contains(Flags::BOLD) {
            out.push_str(";1");
        }
        if styled.contains(Flags::DIM) {
            out.push_str(";2");
        }
        if styled.contains(Flags::ITALIC) {
            out.push_str(";3");
        }
        if styled.intersects(Flags::ALL_UNDERLINES) {
            out.push_str(";4");
        }
        if styled.contains(Flags::INVERSE) {
            out.push_str(";7");
        }
        if styled.contains(Flags::HIDDEN) {
            out.push_str(";8");
        }
        if styled.contains(Flags::STRIKEOUT) {
            out.push_str(";9");
        }
        push_color(out, fg, true);
        push_color(out, bg, false);
        out.push('m');
    }
}

fn push_color(out: &mut String, color: alacritty_terminal::vte::ansi::Color, is_fg: bool) {
    use alacritty_terminal::vte::ansi::{Color, NamedColor};
    use std::fmt::Write;
    let named_base = |n: NamedColor| -> Option<u8> {
        use NamedColor::*;
        Some(match n {
            Black | DimBlack => 0,
            Red | DimRed => 1,
            Green | DimGreen => 2,
            Yellow | DimYellow => 3,
            Blue | DimBlue => 4,
            Magenta | DimMagenta => 5,
            Cyan | DimCyan => 6,
            White | DimWhite => 7,
            BrightBlack => 8,
            BrightRed => 9,
            BrightGreen => 10,
            BrightYellow => 11,
            BrightBlue => 12,
            BrightMagenta => 13,
            BrightCyan => 14,
            BrightWhite => 15,
            _ => return None,
        })
    };
    match color {
        Color::Named(n) => match named_base(n) {
            Some(i) if i < 8 => {
                let _ = write!(out, ";{}", if is_fg { 30 + i } else { 40 + i });
            }
            Some(i) => {
                let _ = write!(out, ";{}", if is_fg { 82 + i } else { 92 + i });
            }
            None => {
                let _ = write!(out, ";{}", if is_fg { 39 } else { 49 });
            }
        },
        Color::Indexed(i) => {
            let _ = write!(out, ";{};5;{}", if is_fg { 38 } else { 48 }, i);
        }
        Color::Spec(rgb) => {
            let _ = write!(
                out,
                ";{};2;{};{};{}",
                if is_fg { 38 } else { 48 },
                rgb.r,
                rgb.g,
                rgb.b
            );
        }
    }
}

fn local_hostname() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(h) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
            let h = h.trim();
            if !h.is_empty() {
                return Some(h.to_string());
            }
        }
    }
    std::env::var("HOSTNAME").ok().filter(|h| !h.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::vte::ansi::Processor;

    fn term(cols: usize, rows: usize, bytes: &[u8]) -> Term<VoidListener> {
        let mut term = Term::new(
            Config {
                scrolling_history: 10_000,
                ..Config::default()
            },
            &TermSize::new(cols, rows),
            VoidListener,
        );
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, bytes);
        term
    }

    fn texts(term: &Term<VoidListener>) -> Vec<String> {
        viewport_rows(term.grid())
            .into_iter()
            .map(|(text, _)| text.trim_end().to_string())
            .collect()
    }

    #[test]
    fn a_double_width_character_reads_as_one() {
        let t = term(20, 4, "❯ 日本語\r\n❯ 👋 hi 🎉\r\n".as_bytes());
        assert_eq!(texts(&t)[..2], ["❯ 日本語", "❯ 👋 hi 🎉"]);
    }

    #[test]
    fn a_double_width_character_wrapped_early_leaves_no_gap() {
        // Five columns taken, and the sixth too narrow for 日: it goes on
        // the next row, and a spacer holds the sixth.
        let t = term(6, 4, "❯ abc日本\r\n".as_bytes());
        assert_eq!(texts(&t)[..2], ["❯ abc", "日本"]);
    }

    /// `n` numbered rows of answer, as if streamed.
    fn answer(n: usize) -> String {
        (0..n)
            .map(|i| format!("{i} the answer goes on and on, filling the row\r\n"))
            .collect()
    }

    #[test]
    fn the_nearest_prompt_above_is_found_and_only_prompt_rows_are_read() {
        let bytes = format!(
            "❯ 古い質問\r\n{}❯ 日本語で答えて\r\n{}",
            answer(30),
            answer(40)
        );
        let t = term(60, 10, bytes.as_bytes());
        let mut read = Vec::new();
        let hit = find_above(t.grid(), 5000, &['❯', '>'], |row, _| {
            read.push(row.trim_end().to_string());
            row.contains("日本語").then(|| row.trim_end().to_string())
        });
        assert_eq!(hit.as_deref(), Some("❯ 日本語で答えて"));
        assert_eq!(read, ["❯ 日本語で答えて"], "no answer row is read");
        // Past `max` rows up, it is not looked for.
        assert_eq!(
            find_above(t.grid(), 20, &['❯'], |row, _| Some(row.to_string())),
            None
        );
    }

    /// Before and after: the walk up a full scrollback with no prompt in
    /// it, as every frame of a streaming answer paid. Run with
    /// `cargo test -p giverny-term --release -- --ignored --nocapture`.
    #[test]
    #[ignore = "timing, not a check"]
    fn time_the_walk_up_the_scrollback() {
        use alacritty_terminal::grid::Dimensions;
        use alacritty_terminal::index::{Column, Line, Point};
        let t = term(120, 50, answer(10_000).as_bytes());
        let grid = t.grid();
        // What `prompt_bar::prompt_of_row` does first with each row.
        let matcher = |row: &str, _: bool| {
            let row = row.trim();
            row.strip_prefix('❯')
                .or_else(|| row.strip_prefix('>'))
                .map(|_| ())
        };
        let runs = 50;
        let before = std::time::Instant::now();
        for _ in 0..runs {
            // The walk as it was: every cell of every row, into a string.
            let top = -(grid.display_offset() as i32);
            let oldest = -(grid.history_size() as i32);
            let mut text = String::new();
            let mut hit = None;
            for line in (oldest..top).rev().take(5000).map(Line) {
                text.clear();
                text.extend((0..grid.columns()).map(|c| grid[Point::new(line, Column(c))].c));
                if let Some(h) = matcher(&text, false) {
                    hit = Some(h);
                    break;
                }
            }
            assert!(hit.is_none());
        }
        let before = before.elapsed() / runs;
        let after = std::time::Instant::now();
        for _ in 0..runs {
            assert!(find_above(grid, 5000, &['❯', '>'], matcher).is_none());
        }
        let after = after.elapsed() / runs;
        println!("5,000 rows of 120 columns: {before:?} before, {after:?} after");
    }
}
