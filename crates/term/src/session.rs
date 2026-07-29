//! `TermSession`: one tab's live terminal bundle — Term + io loop + channels.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
    sender: LoopSender,
    notifier: Notifier,
    dirty: Arc<AtomicBool>,
    size: GridSize,
    handle: Option<JoinHandle<(IoLoop<Pty, EventProxy>, State)>>,
}

impl TermSession {
    /// Spawn a tab. `preseed` is an ANSI dump (from [`Self::snapshot_ansi`])
    /// advanced into the terminal *before* the shell starts — restored
    /// scrollback appears above the fresh prompt, colors intact, and re-wraps
    /// naturally at the current width.
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
            parser.advance(
                &mut *guard,
                "\x1b[0m\x1b[2m── restored ──\x1b[0m\r\n\r\n".as_bytes(),
            );
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
            sender,
            notifier,
            dirty,
            size: cfg.size,
            handle: Some(handle),
        })
    }

    /// Write user input to the PTY.
    /// The user interacted with this session (typed, clicked) — automated
    /// injections (cwd fix, auto-resume) must stand down.
    pub fn note_user_input(&self) {
        self.user_input.store(true, Ordering::Release);
    }

    pub fn had_user_input(&self) -> bool {
        self.user_input.load(Ordering::Acquire)
    }

    /// Shell's live working directory via `/proc` (linux).
    pub fn proc_cwd(&self) -> Option<std::path::PathBuf> {
        #[cfg(target_os = "linux")]
        {
            let pid = self.child_pid?;
            std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
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
        self.dirty.swap(false, Ordering::AcqRel)
    }

    pub fn mark_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
    }

    /// Ask the io loop to stop and join it.
    pub fn shutdown(mut self) {
        let _ = self.sender.send(Msg::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
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
