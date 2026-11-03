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
//! with its name on the top one. No key reaches the shell under the page.
//!
//! A line typed in the box goes to the tab's manager, its own Claude Code,
//! which passes it on to the worker with `SendMessage`, or for a Next up row
//! starts the task with it ([`Ask`], `send_line`). The box takes only what
//! the user types, and a line goes only on their Enter. A page with nobody
//! to send to, or with `management_panel.prompt_box` off, keeps the box
//! dimmed and `read-only`.
//!
//! The manager's turn takes seconds, so a line sent to a worker shows on its
//! page at once, as its prompt will, dimmed, after the transcript's rows
//! ([`WorkerPage::echo`]). Each line that lands in the transcript takes the
//! place of the oldest such echo. The text is not compared, as the manager
//! may change it. Echoes live on the page, and go when it closes.
//!
//! A worker's transcript opens at its end, on its report once it has one,
//! and is followed as it grows, a Done worker's too: a line from the box can
//! wake it. A Next up row's brief is drawn as the prompt its worker will be
//! given, and opens at its top.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Key, Rect};
use giverny_claude::transcript::{self, Row, Tone};
use giverny_core::tabs::TabId;
use giverny_term::page::Page;
use giverny_term::widget::{RenderShared, RowMarks};

use crate::management_panel::RowClick;
use crate::row_open::Ask;
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
    /// The prompt box, when a line typed in it goes to the manager.
    input: Option<Input>,
}

/// The page's prompt box while it takes a line for the manager.
struct Input {
    ask: Ask,
    text: String,
    /// The cursor, in chars into `text`.
    cursor: usize,
    state: Sending,
}

/// Where the box's line is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sending {
    /// Nothing sent yet, or the last line went and another may follow.
    Idle,
    /// Written to the manager's prompt; waiting to hear it was submitted.
    Going,
    /// Waiting at the manager's prompt for a compaction to end.
    Compacting,
    /// The manager has it: what the box says under it.
    Sent(String),
    /// Not sent, and why: the line stays in the box.
    Refused(String),
}

impl Sending {
    fn busy(&self) -> bool {
        matches!(self, Sending::Going | Sending::Compacting)
    }
}

pub enum Content {
    Text(String),
    /// Rows drawn as a transcript is: a Next up row's brief, as the
    /// prompt its worker will be given.
    Rows(Vec<Row>),
    Transcript(TranscriptView),
}

/// A worker's transcript as the page shows it.
pub struct TranscriptView {
    pub path: PathBuf,
    /// The worker is at work, so an empty transcript has its first turn to
    /// come.
    pub live: bool,
    tail: transcript::Tail,
    render: transcript::Render,
    rows: Vec<Row>,
    /// Why the file could not be read, when it could not.
    error: Option<String>,
    polled: Instant,
    /// Lines sent to the worker and not in the transcript yet, oldest first.
    echoes: Vec<String>,
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

    /// Rows in the transcript's look, opened at the top: a brief as the
    /// prompt it will be.
    pub fn rows(tab: TabId, name: String, rows: Vec<Row>) -> Self {
        let mut page = Self::with(tab, name, Content::Rows(rows));
        page.at_top = true;
        page
    }

    /// A worker's transcript, read now and followed from here on; `live`
    /// when the worker is still at work.
    pub fn transcript(tab: TabId, name: String, path: PathBuf, live: bool) -> Self {
        let mut view = TranscriptView {
            path,
            live,
            tail: transcript::Tail::default(),
            render: transcript::Render::default(),
            rows: Vec::new(),
            error: None,
            polled: Instant::now(),
            echoes: Vec::new(),
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
            input: None,
        }
    }

    /// The page's box takes a line for the manager: `ask` says what the
    /// line asks of it. `None` keeps the box read-only.
    pub fn with_input(mut self, ask: Option<Ask>) -> Self {
        self.input = ask.map(|ask| Input {
            ask,
            text: String::new(),
            cursor: 0,
            state: Sending::Idle,
        });
        self
    }

    /// What a line typed in the box asks of the manager, if one can be.
    pub fn ask(&self) -> Option<&Ask> {
        self.input.as_ref().map(|i| &i.ask)
    }

    /// The box's line is on its way, went, or was refused: the box says so
    /// under it. A line the manager has leaves the box; any other stays.
    pub fn set_sending(&mut self, state: Sending) {
        let Some(input) = &mut self.input else {
            return;
        };
        if input.state == state {
            return;
        }
        if matches!(state, Sending::Sent(_)) {
            input.text.clear();
            input.cursor = 0;
        }
        input.state = state;
        self.generation += 1;
        self.page.to_end();
    }

    /// `text` just went to the manager: show it now, until it lands in the
    /// transcript. A page with no transcript has nowhere for it to land, and
    /// shows none.
    pub fn echo(&mut self, text: &str) {
        let Content::Transcript(v) = &mut self.content else {
            return;
        };
        // What is in the file by now came before this line.
        v.poll();
        v.echoes.push(text.to_string());
        self.generation += 1;
        self.page.to_end();
    }

    /// The line sent last never reached the manager: its echo goes. A send
    /// is refused while another is on its way, so the newest is this line's.
    pub fn drop_echo(&mut self) {
        if let Content::Transcript(v) = &mut self.content
            && v.echoes.pop().is_some()
        {
            self.generation += 1;
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
            Content::Rows(rows) => rows.clone(),
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
                if let Some(tone) = r.band() {
                    // The band runs the grid's width, as Claude Code's does.
                    let n = r.plain().chars().count();
                    if n < cols {
                        r.spans.push(transcript::Span {
                            tone,
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
        match &self.input {
            Some(input) => {
                out.extend(input_box(cols, &self.name, input));
                out.extend(status_lines(input, cols));
            }
            None => out.extend(prompt_box(cols, &self.name)),
        }
        out.push(String::new());
        // The button's row: blank, the button drawn over it.
        out.push(String::new());
        let button = out.len() - 1;
        (out, button)
    }

    /// Take in what the transcript has grown by. True for a page that shows
    /// one, to look again later.
    fn follow(&mut self) -> bool {
        let Content::Transcript(v) = &mut self.content else {
            return false;
        };
        if v.polled.elapsed() >= POLL && v.poll() {
            self.generation += 1;
        }
        true
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
        if self.follow() {
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
    /// to Giverny's own shortcuts. With a box that takes a line, typing
    /// goes into it, Shift+Enter breaks the line and Enter sends it; Esc
    /// then clears a line first, and Home/End move in it.
    fn keys(&mut self, ctx: &egui::Context) -> Keys {
        let mut out = Keys::default();
        let page = i32::from(self.page.rows()).saturating_sub(2).max(1);
        let mut moves: Vec<i32> = Vec::new();
        let mut ends: Vec<bool> = Vec::new();
        let mut edits: Vec<Edit> = Vec::new();
        let typing = self.input.as_ref().is_some_and(|i| !i.state.busy());
        let has_text = self.input.as_ref().is_some_and(|i| !i.text.is_empty());
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
                            Key::Escape if typing && has_text => edits.push(Edit::Clear),
                            Key::Escape => out.back = true,
                            Key::Enter if typing && modifiers.shift => {
                                edits.push(Edit::Insert("\n".into()));
                            }
                            Key::Enter if typing => edits.push(Edit::Enter),
                            Key::Backspace if typing => edits.push(Edit::Back),
                            Key::Delete if typing => edits.push(Edit::Delete),
                            Key::ArrowLeft if typing => edits.push(Edit::Left),
                            Key::ArrowRight if typing => edits.push(Edit::Right),
                            Key::Home if typing && has_text => edits.push(Edit::Start),
                            Key::End if typing && has_text => edits.push(Edit::End),
                            Key::ArrowUp => moves.push(1),
                            Key::ArrowDown => moves.push(-1),
                            Key::PageUp => moves.push(page),
                            Key::PageDown => moves.push(-page),
                            // A space is typed, not a scroll, in a box that
                            // takes a line.
                            Key::Space if !typing => moves.push(-page),
                            Key::Home => ends.push(true),
                            Key::End => ends.push(false),
                            _ => {}
                        }
                    }
                    false
                }
                egui::Event::Text(t) | egui::Event::Paste(t) => {
                    if typing {
                        edits.push(Edit::Insert(t.clone()));
                    }
                    false
                }
                _ => true,
            });
        });
        if let Some(input) = &mut self.input {
            let before = (input.text.clone(), input.cursor);
            for edit in edits {
                if let Some(line) = input.edit(edit) {
                    out.send = Some(line);
                }
            }
            if (input.text.clone(), input.cursor) != before || out.send.is_some() {
                // A new line: what became of the last one is old news.
                if matches!(input.state, Sending::Refused(_) | Sending::Sent(_)) {
                    input.state = Sending::Idle;
                }
                self.generation += 1;
                // The box is at the page's end: typing shows it.
                self.page.to_end();
            }
        }
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
        out
    }
}

/// What the page's keys did this frame.
#[derive(Debug, Default, PartialEq, Eq)]
struct Keys {
    /// Back to the terminal.
    back: bool,
    /// The box's line, sent with Enter.
    send: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Edit {
    Insert(String),
    Back,
    Delete,
    Left,
    Right,
    Start,
    End,
    Clear,
    Enter,
}

impl Input {
    /// One edit. Enter on a line with words in it hands the line over; it
    /// stays in the box, unchangeable, until the manager has it.
    fn edit(&mut self, edit: Edit) -> Option<String> {
        let len = self.text.chars().count();
        let at = |s: &str, n: usize| s.char_indices().nth(n).map_or(s.len(), |(i, _)| i);
        match edit {
            Edit::Insert(t) => {
                let t: String = t
                    .replace("\r\n", "\n")
                    .replace('\r', "\n")
                    .replace('\t', "    ")
                    .chars()
                    .filter(|c| *c == '\n' || !c.is_control())
                    .collect();
                let i = at(&self.text, self.cursor);
                self.text.insert_str(i, &t);
                self.cursor += t.chars().count();
            }
            Edit::Back if self.cursor > 0 => {
                let i = at(&self.text, self.cursor - 1);
                self.text.remove(i);
                self.cursor -= 1;
            }
            Edit::Delete if self.cursor < len => {
                let i = at(&self.text, self.cursor);
                self.text.remove(i);
            }
            Edit::Left => self.cursor = self.cursor.saturating_sub(1),
            Edit::Right => self.cursor = (self.cursor + 1).min(len),
            Edit::Start => self.cursor = 0,
            Edit::End => self.cursor = len,
            Edit::Clear => {
                self.text.clear();
                self.cursor = 0;
            }
            Edit::Enter if !self.text.trim().is_empty() => {
                self.state = Sending::Going;
                return Some(self.text.clone());
            }
            Edit::Back | Edit::Delete | Edit::Enter => {}
        }
        None
    }
}

/// The page's keys (called from `App::shortcuts`), while the active tab
/// shows one.
pub fn keys(app: &mut App, ctx: &egui::Context) -> Vec<Action> {
    let active = app.ws.active;
    let Some(page) = app.page.as_mut().filter(|p| Some(p.tab) == active) else {
        return Vec::new();
    };
    let keys = page.keys(ctx);
    let mut out = Vec::new();
    if let Some(text) = keys.send {
        out.push(Action::SendLine(page.tab, text));
    }
    if keys.back {
        out.push(Action::ClosePage(page.tab));
    }
    out
}

/// The prompt box while it takes a line for the manager: Claude Code's box
/// under a worker's view, bright, its rules in the worker's colour and its
/// name on the top one, the text typed so far with the cursor in it,
/// wrapped as Claude Code wraps a long line. Empty, it shows what a line
/// is for, dim. While the line is on its way it is drawn dim, with no
/// cursor.
fn input_box(cols: usize, name: &str, input: &Input) -> Vec<String> {
    const REV: &str = "\x1b[7m";
    let (r, g, b) = WORKER_RGB;
    let rule = format!("\x1b[38;2;{r};{g};{b}m");
    let top = prompt_box(cols, name)[0].replace(DIM, "");
    let bottom = format!("{rule}{}{RESET}", "─".repeat(cols));
    let busy = input.state.busy();
    let mut body: Vec<String> = Vec::new();
    if input.text.is_empty() {
        // The cursor sits on the placeholder's first cell, as Claude Code's
        // does.
        let hint: String = input
            .ask
            .placeholder()
            .chars()
            .take(cols.saturating_sub(3))
            .collect();
        let mut chars = hint.chars();
        let first = chars.next().unwrap_or(' ');
        let rest: String = chars.collect();
        body.push(format!("❯ {REV}{DIM}{first}{RESET}{DIM}{rest}{RESET}"));
    } else {
        let width = cols.saturating_sub(2).max(1);
        let cursor = (!busy).then_some(input.cursor);
        let lead = if busy { DIM } else { "" };
        for (n, row) in text_rows(&input.text, cursor, width)
            .into_iter()
            .enumerate()
        {
            let prompt = if n == 0 { "❯ " } else { "  " };
            body.push(format!("{prompt}{lead}{row}{RESET}"));
        }
    }
    let mut lines = vec![top];
    lines.extend(body);
    lines.push(bottom);
    lines
}

/// `text` in rows of at most `width` cells: broken at each line break and
/// wrapped where a line is longer. The char at `cursor`, or a cell past the
/// end of its line, is drawn reversed.
fn text_rows(text: &str, cursor: Option<usize>, width: usize) -> Vec<String> {
    const REV: &str = "\x1b[7m";
    const UNREV: &str = "\x1b[27m";
    let chars: Vec<char> = text.chars().collect();
    let mut rows = Vec::new();
    let mut row = String::new();
    let mut cells = 0;
    for i in 0..=chars.len() {
        let here = cursor == Some(i);
        match chars.get(i) {
            Some('\n') => {
                if here {
                    row.push_str(&format!("{REV} {UNREV}"));
                }
                rows.push(std::mem::take(&mut row));
                cells = 0;
            }
            Some(c) => {
                if cells == width {
                    rows.push(std::mem::take(&mut row));
                    cells = 0;
                }
                if here {
                    row.push_str(&format!("{REV}{c}{UNREV}"));
                } else {
                    row.push(*c);
                }
                cells += 1;
            }
            None => {
                if here {
                    if cells == width {
                        rows.push(std::mem::take(&mut row));
                    }
                    row.push_str(&format!("{REV} {UNREV}"));
                }
                rows.push(std::mem::take(&mut row));
            }
        }
    }
    rows
}

/// The rows under the box: where the line is, wrapped to the grid.
fn status_lines(input: &Input, cols: usize) -> Vec<String> {
    let (color, text) = match &input.state {
        Sending::Idle => return Vec::new(),
        Sending::Going => (DIM, "sending…"),
        Sending::Compacting => (DIM, "queued behind compaction"),
        Sending::Sent(what) => (DIM, what.as_str()),
        Sending::Refused(why) => ("\x1b[38;2;220;90;90m", why.as_str()),
    };
    let width = cols.saturating_sub(6).max(8);
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let n = line.chars().count();
        if n > 0 && n + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    lines.push(line);
    lines
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let lead = if i == 0 { "  ⎿  " } else { "     " };
            format!("{color}{lead}{l}{RESET}")
        })
        .collect()
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
                let sent = self.render.sent_lines();
                for line in &update.lines {
                    transcript::push(&mut self.rows, self.render.line(line));
                }
                let landed = self.render.sent_lines() - sent;
                self.echoes.drain(..landed.min(self.echoes.len()));
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

    /// The rows, for a finished transcript its unterminated last line, and
    /// the lines still on their way.
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
        for echo in &self.echoes {
            rows.extend(transcript::echo_rows(echo));
        }
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> Input {
        Input {
            ask: Ask::Start {
                task: "acme#2".into(),
            },
            text: String::new(),
            cursor: 0,
            state: Sending::Idle,
        }
    }

    fn plain(s: &str) -> String {
        let mut out = String::new();
        let mut esc = false;
        for c in s.chars() {
            match c {
                '\x1b' => esc = true,
                'm' if esc => esc = false,
                _ if esc => {}
                c => out.push(c),
            }
        }
        out
    }

    /// A worker's page opened once it was done: the line from the box and
    /// the reply it woke show up without opening the page again.
    #[test]
    fn a_done_page_follows_a_line_sent_from_it() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("giverny-page-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("agent-t.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"user","message":{"role":"user","content":"Count, then report."}}"#,
                "\n",
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Counted to 8."}]}}"#,
                "\n",
            ),
        )
        .unwrap();
        let mut page = WorkerPage::transcript(TabId(1), "counter".into(), path.clone(), false);
        let said = |page: &WorkerPage| -> Vec<String> {
            let rows = page.entries();
            rows.iter().map(Row::plain).collect()
        };
        assert_eq!(
            said(&page),
            vec!["❯ Count, then report.", "● Counted to 8."]
        );

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(
            file,
            r#"{{"type":"user","isMeta":true,"origin":{{"kind":"coordinator"}},"message":{{"role":"user","content":"The coordinator sent a message while you were working:\ntestingggggggggggggg\n\nAddress this before completing your current task."}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","message":{{"content":[{{"type":"text","text":"Got it."}}]}}}}"#
        )
        .unwrap();
        let generation = page.generation;
        let Content::Transcript(v) = &mut page.content else {
            panic!("a transcript page");
        };
        v.polled -= POLL;
        assert!(page.follow());
        assert_eq!(page.generation, generation + 1, "redrawn");
        assert_eq!(
            said(&page),
            vec![
                "❯ Count, then report.",
                "● Counted to 8.",
                "❯ testingggggggggggggg",
                "● Got it.",
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A Done worker's page over a scratch transcript, and a way to append to
    /// it and have the page look.
    struct Scratch {
        page: WorkerPage,
        path: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("giverny-echo-{}-{name}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("agent-t.jsonl");
            std::fs::write(
                &path,
                concat!(
                    r#"{"type":"user","message":{"role":"user","content":"Count."}}"#,
                    "\n"
                ),
            )
            .unwrap();
            let page = WorkerPage::transcript(TabId(1), "counter".into(), path.clone(), false)
                .with_input(Some(Ask::Send {
                    task: "acme#1".into(),
                    agent_id: "a1".into(),
                }));
            Scratch { page, path }
        }

        fn append(&mut self, record: &str) {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&self.path)
                .unwrap();
            writeln!(file, "{record}").unwrap();
        }

        /// A line the manager passed on lands.
        fn lands(&mut self, text: &str) {
            let record = serde_json::json!({
                "type": "attachment",
                "attachment": {
                    "type": "queued_command",
                    "prompt": text,
                    "origin": {"kind": "coordinator"},
                },
            });
            self.append(&record.to_string());
            self.look();
        }

        fn look(&mut self) {
            let Content::Transcript(v) = &mut self.page.content else {
                panic!("a transcript page");
            };
            v.polled -= POLL;
            assert!(self.page.follow());
        }

        /// What the page draws, the echoes marked.
        fn rows(&self) -> Vec<String> {
            self.page
                .entries()
                .iter()
                .map(|r| {
                    let mark = if r.band() == Some(Tone::Echo) {
                        "~"
                    } else {
                        ""
                    };
                    format!("{mark}{}", r.plain())
                })
                .collect()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            if let Some(dir) = self.path.parent() {
                std::fs::remove_dir_all(dir).ok();
            }
        }
    }

    #[test]
    fn a_line_shows_at_once_and_after_the_rows() {
        let mut s = Scratch::new("shows");
        let generation = s.page.generation;
        s.page.echo("go on");
        assert_ne!(s.page.generation, generation, "redrawn");
        assert_eq!(s.rows(), vec!["❯ Count.", "~❯ go on"]);
        // Whatever the worker does next goes above it.
        s.append(r#"{"type":"assistant","message":{"content":[{"type":"text","text":"One."}]}}"#);
        s.look();
        assert_eq!(s.rows(), vec!["❯ Count.", "● One.", "~❯ go on"]);
    }

    #[test]
    fn its_band_runs_the_grid_dim_as_a_prompt_does() {
        let mut s = Scratch::new("band");
        s.page.echo("go on");
        let (lines, _) = s.page.lines(40, 0);
        let echo = lines
            .iter()
            .find(|l| plain(l).starts_with("❯ go on"))
            .unwrap();
        assert!(echo.contains(DIM));
        assert_eq!(plain(echo).chars().count(), 40);
    }

    #[test]
    fn lines_land_first_in_first_out_with_no_duplicate() {
        let mut s = Scratch::new("fifo");
        s.page.echo("one");
        s.page.echo("two");
        assert_eq!(s.rows(), vec!["❯ Count.", "~❯ one", "~❯ two"]);
        s.lands("one");
        assert_eq!(s.rows(), vec!["❯ Count.", "❯ one", "~❯ two"]);
        s.lands("two");
        assert_eq!(s.rows(), vec!["❯ Count.", "❯ one", "❯ two"]);
        // Nothing to clear: nothing goes wrong.
        s.lands("three");
        assert_eq!(s.rows(), vec!["❯ Count.", "❯ one", "❯ two", "❯ three"]);
    }

    #[test]
    fn a_line_the_manager_changed_still_lands() {
        let mut s = Scratch::new("paraphrase");
        s.page.echo("plz count slower");
        s.lands("Please count more slowly.");
        assert_eq!(s.rows(), vec!["❯ Count.", "❯ Please count more slowly."]);
    }

    #[test]
    fn a_line_that_wakes_a_finished_worker_lands_too() {
        let mut s = Scratch::new("wakes");
        s.page.echo("hello");
        s.append(
            r#"{"type":"user","isMeta":true,"origin":{"kind":"coordinator"},"message":{"role":"user","content":"The coordinator sent a message while you were working:\nhello\n\nAddress this before completing your current task."}}"#,
        );
        s.look();
        assert_eq!(s.rows(), vec!["❯ Count.", "❯ hello"]);
    }

    #[test]
    fn what_landed_before_the_send_clears_nothing() {
        let mut s = Scratch::new("before");
        // In the file, not looked at yet, when the line goes.
        let record = r#"{"type":"attachment","attachment":{"type":"queued_command","prompt":"earlier","origin":{"kind":"coordinator"}}}"#;
        s.append(record);
        s.page.echo("now");
        s.look();
        assert_eq!(s.rows(), vec!["❯ Count.", "❯ earlier", "~❯ now"]);
    }

    #[test]
    fn other_records_clear_nothing() {
        let mut s = Scratch::new("others");
        s.page.echo("go on");
        s.append(r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"<system-reminder>x</system-reminder>"}}"#);
        s.append(r#"{"type":"assistant","message":{"content":[{"type":"text","text":"One."}]}}"#);
        s.look();
        assert_eq!(s.rows(), vec!["❯ Count.", "● One.", "~❯ go on"]);
    }

    #[test]
    fn not_sent_takes_the_echo_back_and_compaction_keeps_it() {
        let mut s = Scratch::new("notsent");
        s.page.echo("first");
        s.page.echo("second");
        s.page.set_sending(Sending::Compacting);
        assert_eq!(s.rows(), vec!["❯ Count.", "~❯ first", "~❯ second"]);
        s.page.drop_echo();
        s.page.set_sending(Sending::Refused("Not sent: x".into()));
        assert_eq!(s.rows(), vec!["❯ Count.", "~❯ first"], "the last one's");
        // A refusal alone isn't a line that was on its way.
        s.page.set_sending(Sending::Refused("Not sent: y".into()));
        assert_eq!(s.rows(), vec!["❯ Count.", "~❯ first"]);
    }

    #[test]
    fn a_page_with_no_transcript_shows_no_echo() {
        let brief = transcript::prompt_rows("The brief.", None);
        let ask = Ask::Start {
            task: "acme#2".into(),
        };
        let mut next_up = WorkerPage::rows(TabId(1), "acme#2".into(), brief).with_input(Some(ask));
        next_up.echo("add this");
        let said: Vec<String> = next_up.entries().iter().map(Row::plain).collect();
        assert_eq!(said, vec!["❯ The brief."]);

        let mut waiting = WorkerPage::text(TabId(1), "w".into(), "No transcript yet.".into());
        waiting.echo("hello");
        assert_eq!(waiting.entries().len(), 1);
        waiting.drop_echo();
    }

    #[test]
    fn the_box_edits_a_line_and_enter_hands_it_over() {
        let mut i = input();
        assert_eq!(i.edit(Edit::Enter), None, "an empty box sends nothing");
        i.edit(Edit::Insert("helo".into()));
        i.edit(Edit::Left);
        i.edit(Edit::Insert("l".into()));
        assert_eq!((i.text.as_str(), i.cursor), ("hello", 4));
        i.edit(Edit::End);
        i.edit(Edit::Insert("\r\nthere\tyou\x07".into()));
        assert_eq!(i.text, "hello\nthere    you");
        i.edit(Edit::Start);
        i.edit(Edit::Delete);
        assert_eq!(i.text, "ello\nthere    you");
        assert_eq!(i.edit(Edit::Enter), Some("ello\nthere    you".into()));
        assert_eq!(i.state, Sending::Going);
        assert_eq!(
            i.text, "ello\nthere    you",
            "kept until the manager has it"
        );
        i.edit(Edit::Clear);
        assert_eq!((i.text.as_str(), i.cursor), ("", 0));
    }

    #[test]
    fn the_text_wraps_and_breaks_with_the_cursor_in_it() {
        let rows: Vec<String> = text_rows("abcdef\ngh", Some(9), 4)
            .iter()
            .map(|r| plain(r))
            .collect();
        assert_eq!(rows, vec!["abcd", "ef", "gh "]);
        let rows: Vec<String> = text_rows("abcd", Some(4), 4)
            .iter()
            .map(|r| plain(r))
            .collect();
        assert_eq!(rows, vec!["abcd", " "], "a full row puts the cursor below");
        assert_eq!(text_rows("ab", None, 4), vec!["ab"]);
    }

    #[test]
    fn under_the_box_it_says_where_the_line_is() {
        let mut i = input();
        assert!(status_lines(&i, 80).is_empty());
        i.state = Sending::Going;
        assert_eq!(plain(&status_lines(&i, 80)[0]), "  ⎿  sending…");
        i.state = Sending::Compacting;
        assert_eq!(
            plain(&status_lines(&i, 80)[0]),
            "  ⎿  queued behind compaction"
        );
        i.state = Sending::Refused("Not sent: the manager is waiting on a question.".into());
        assert_eq!(status_lines(&i, 30).len(), 2, "wrapped to the grid");
        let empty = input_box(40, "acme#2", &i);
        assert!(plain(&empty[1]).starts_with("❯ Start this task now."));
    }
}
