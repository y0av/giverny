//! A management-panel row's worker, shown in the tab as Claude Code shows a
//! worker's own view.
//!
//! A click on a row opens the page (`row_open`). It takes the tab over: it
//! fills the terminal's area in the terminal's own grid, glyphs and worker
//! background ([`giverny_term::page::Page`]), under a header naming the
//! worker (`view_header`), with a "back" button under it. Esc, the button,
//! or a second click on the row goes back to the terminal.
//!
//! What is drawn is what Claude Code draws for a worker (as captured from
//! Claude Code 2.1.308): its banner's rows (here blank, under the header),
//! the worker's prompt on its grey band and each entry under it
//! ([`transcript`]), then the prompt box, its rules in the worker's colour
//! with its name on the top one, dimmed and `read-only`. No key reaches the
//! shell under the page, and nothing typed goes anywhere.
//!
//! A Running worker is followed as its transcript grows; a Done one opens at
//! its end, on its report.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Key, Rect};
use giverny_claude::transcript::{self, Row, Tone};
use giverny_core::tabs::TabId;
use giverny_term::page::Page;
use giverny_term::widget::{RenderShared, RowMarks};

use crate::management_panel::RowClick;
use crate::{Action, App};

/// How often a live transcript is looked at.
const POLL: Duration = Duration::from_millis(300);

/// Rows Claude Code's view starts its entries under: its banner's three,
/// then two blank (Claude Code 2.1.308).
const BANNER_ROWS: usize = 5;

/// The column the "back" button is drawn from.
const BUTTON_COL: u16 = 2;

/// The worker's colour on its prompt box's rules and name tag: Claude
/// Code's for a general-purpose worker, as captured.
const WORKER_RGB: (u8, u8, u8) = (8, 145, 178);

const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

pub struct WorkerPage {
    /// The tab it is drawn in, in place of the terminal.
    pub tab: TabId,
    /// The management-panel row it shows.
    pub row: Option<RowClick>,
    /// The worker's name: on its prompt box, and on the header when the
    /// pane has no row for it.
    pub name: String,
    pub content: Content,
    /// The header's `×` was clicked: it stays hidden for this page.
    pub header_closed: bool,
    /// The header's height last frame, in points: the rows under it are
    /// the banner's.
    pub header_h: f32,
    page: Page,
    /// Bumped whenever the text changes.
    generation: u64,
    /// What the page was last laid out for: (generation, cols, rows, pad).
    laid: Option<(u64, u16, u16, usize)>,
    /// Open at the top rather than at the end.
    at_top: bool,
    /// The line of the text the button is on.
    button_line: usize,
}

pub enum Content {
    Text(String),
    Transcript(TranscriptView),
}

/// A worker's transcript as the page shows it.
pub struct TranscriptView {
    pub path: PathBuf,
    /// Follow it as it grows.
    pub live: bool,
    tail: transcript::Tail,
    render: transcript::Render,
    rows: Vec<Row>,
    /// Why the file could not be read, when it could not.
    error: Option<String>,
    polled: Instant,
}

/// What a frame of the page did.
pub struct Drawn {
    /// The "back" button was clicked.
    pub back: bool,
    /// Where the page is: the terminal's area.
    pub rect: Rect,
}

impl WorkerPage {
    /// Plain text: why there is nothing else to show.
    pub fn text(tab: TabId, name: String, text: String) -> Self {
        let mut page = Self::with(tab, name, Content::Text(text));
        page.at_top = true;
        page
    }

    /// A worker's transcript, read now; `live` keeps following it.
    pub fn transcript(tab: TabId, name: String, path: PathBuf, live: bool) -> Self {
        let mut view = TranscriptView {
            path,
            live,
            tail: transcript::Tail::default(),
            render: transcript::Render::default(),
            rows: Vec::new(),
            error: None,
            polled: Instant::now(),
        };
        view.poll();
        Self::with(tab, name, Content::Transcript(view))
    }

    fn with(tab: TabId, name: String, content: Content) -> Self {
        WorkerPage {
            tab,
            row: None,
            name,
            content,
            header_closed: false,
            header_h: 0.0,
            page: Page::new(),
            generation: 0,
            laid: None,
            at_top: false,
            button_line: 0,
        }
    }

    /// The page shows this row.
    pub fn of_row(mut self, row: RowClick) -> Self {
        self.row = Some(row);
        self
    }

    /// What the page draws between the banner and the prompt box.
    fn entries(&self) -> Vec<Row> {
        match &self.content {
            Content::Text(t) => t
                .lines()
                .map(|l| Row::one(Tone::Plain, format!("  {l}")))
                .collect(),
            Content::Transcript(v) => v.lines_for_view(),
        }
    }

    /// The page's text for a grid `cols` wide with `pad` rows under the
    /// header: its lines, and the line the button goes on.
    fn lines(&self, cols: u16, pad: usize) -> (Vec<String>, usize) {
        let cols = usize::from(cols).max(8);
        let mut out = vec![String::new(); pad];
        let mut wrapped: Vec<Row> = Vec::new();
        for row in self.entries() {
            for mut r in transcript::wrap(&row, cols - 1) {
                if r.is_prompt() {
                    // The band runs the grid's width, as Claude Code's does.
                    let n = r.plain().chars().count();
                    if n < cols {
                        r.spans.push(transcript::Span {
                            tone: Tone::Prompt,
                            text: " ".repeat(cols - n),
                        });
                    }
                }
                wrapped.push(r);
            }
        }
        let style = transcript::Style { color: true };
        out.extend(transcript::lay_out(&wrapped, true, style));
        out.push(String::new());
        out.extend(prompt_box(cols, &self.name));
        out.push(String::new());
        // The button's row: blank, the button drawn over it.
        out.push(String::new());
        let button = out.len() - 1;
        (out, button)
    }

    /// Draw the page over the rest of `ui`, in the tab's terminal's place.
    /// `labels` (longest first) and `fill` are its "back" button's.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        shared: &mut RenderShared,
        labels: &'static [&'static str],
        fill: Color32,
    ) -> Drawn {
        if let Content::Transcript(v) = &mut self.content
            && v.live
        {
            if v.polled.elapsed() >= POLL && v.poll() {
                self.generation += 1;
            }
            ui.ctx().request_repaint_after(POLL);
        }
        let rect = ui.available_rect_before_wrap();
        let ppp = ui.ctx().pixels_per_point();
        let (cols, rows) = Page::grid(shared, rect, ppp);
        let cell_h = shared.cell_size(ppp).y.max(1.0);
        // The rows under the header, at least the banner's.
        let under = if self.header_h > 0.0 {
            (self.header_h / cell_h).ceil() as usize + 1
        } else {
            0
        };
        let pad = BANNER_ROWS.max(under);
        let key = (self.generation, cols, rows, pad);
        if self.laid != Some(key) {
            let (lines, button) = self.lines(cols, pad);
            self.page.set(lines, cols, rows);
            if self.laid.is_none() && self.at_top {
                self.page.to_top();
            }
            self.button_line = button;
            self.laid = Some(key);
        }
        let marks = RowMarks {
            button: self.page.screen_row(self.button_line).map(|r| (r, 0)),
            button_left: Some(BUTTON_COL),
        };
        let bg = shared.theme.worker_bg();
        let shown = self.page.show(ui, shared, bg, (&marks, labels, fill));
        Drawn {
            back: shown.button_pressed,
            rect: shown.response.rect,
        }
    }

    /// The page's keys, taken before the terminal under it sees them: the
    /// scroll keys scroll, as in Claude Code's view, and nothing typed
    /// reaches the shell. Esc goes back. Ctrl and Alt chords pass through
    /// to Giverny's own shortcuts. Returns whether Esc was pressed.
    fn keys(&mut self, ctx: &egui::Context) -> bool {
        let mut back = false;
        let page = i32::from(self.page.rows()).saturating_sub(2).max(1);
        let mut moves: Vec<i32> = Vec::new();
        let mut ends: Vec<bool> = Vec::new();
        ctx.input_mut(|i| {
            i.events.retain(|e| match e {
                egui::Event::Key {
                    key,
                    pressed,
                    modifiers,
                    ..
                } if !(modifiers.ctrl
                    || modifiers.alt
                    || modifiers.command
                    || modifiers.mac_cmd) =>
                {
                    if *pressed {
                        match key {
                            Key::Escape => back = true,
                            Key::ArrowUp => moves.push(1),
                            Key::ArrowDown => moves.push(-1),
                            Key::PageUp => moves.push(page),
                            Key::PageDown | Key::Space => moves.push(-page),
                            Key::Home => ends.push(true),
                            Key::End => ends.push(false),
                            _ => {}
                        }
                    }
                    false
                }
                egui::Event::Text(_) | egui::Event::Paste(_) => false,
                _ => true,
            });
        });
        for n in moves {
            self.page.scroll(n);
        }
        for top in ends {
            if top {
                self.page.to_top();
            } else {
                self.page.to_end();
            }
        }
        back
    }
}

/// The page's keys (called from `App::shortcuts`), while the active tab
/// shows one.
pub fn keys(app: &mut App, ctx: &egui::Context) -> Vec<Action> {
    let active = app.ws.active;
    let Some(page) = app.page.as_mut().filter(|p| Some(p.tab) == active) else {
        return Vec::new();
    };
    if page.keys(ctx) {
        vec![Action::ClosePage(page.tab)]
    } else {
        Vec::new()
    }
}

/// Claude Code's prompt box under a worker's view, dimmed: nothing can be
/// typed to this worker here. Its rules are in the worker's colour, the
/// worker's `name` on the top one's right, as Claude Code puts it.
fn prompt_box(cols: usize, name: &str) -> Vec<String> {
    let (r, g, b) = WORKER_RGB;
    let rule = format!("\x1b[38;2;{r};{g};{b}m");
    let tag = format!("\x1b[48;2;{r};{g};{b}m\x1b[38;2;0;0;0m");
    let max = cols.saturating_sub(6).max(1);
    let name: String = if name.chars().count() > max {
        let kept: String = name.chars().take(max - 1).collect();
        format!("{kept}…")
    } else {
        name.to_string()
    };
    let label = format!(" {name} ");
    let left = cols.saturating_sub(label.chars().count() + 1);
    vec![
        format!(
            "{DIM}{rule}{}{RESET}{DIM}{tag}{label}{RESET}{DIM}{rule}─{RESET}",
            "─".repeat(left)
        ),
        format!("{DIM}❯ read-only{RESET}"),
        format!("{DIM}{rule}{}{RESET}", "─".repeat(cols)),
    ]
}

impl TranscriptView {
    /// Read what was appended. True when the rows changed.
    fn poll(&mut self) -> bool {
        self.polled = Instant::now();
        match self.tail.read(&self.path) {
            Ok(update) => {
                let was_error = self.error.take().is_some();
                if update.restarted {
                    self.rows.clear();
                    self.render = transcript::Render::default();
                }
                let before = self.rows.len();
                for line in &update.lines {
                    transcript::push(&mut self.rows, self.render.line(line));
                }
                was_error || update.restarted || self.rows.len() != before
            }
            Err(err) => {
                let msg = format!("Could not read {}: {err}", self.path.display());
                let changed = self.error.as_deref() != Some(msg.as_str());
                self.error = Some(msg);
                changed
            }
        }
    }

    /// The rows, and for a finished transcript its unterminated last line.
    fn lines_for_view(&self) -> Vec<Row> {
        let mut rows = self.rows.clone();
        if !self.live
            && let Some(rest) = self.tail.rest()
        {
            transcript::push(&mut rows, self.render.clone().line(rest));
        }
        if let Some(err) = &self.error {
            let mut row = Row::one(Tone::Error, err.clone());
            row.starts = true;
            rows.push(row);
        } else if rows.is_empty() {
            rows.push(Row::one(
                Tone::Dim,
                if self.live {
                    "waiting for the worker's first turn…"
                } else {
                    "(the transcript is empty)"
                },
            ));
        }
        rows
    }
}
