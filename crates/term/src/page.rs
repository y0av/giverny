//! A page: lines of ANSI text in a terminal grid of their own, with no
//! program behind them, drawn exactly as a tab's terminal is.
//!
//! Giverny shows a worker whose turn has ended, or that has not started yet,
//! in the tab the way Claude Code shows a running one. Claude
//! Code has no view of such a worker to open, so Giverny writes the worker's
//! screen itself, and a page is where it goes: the same cell grid, glyphs,
//! colours and background as the tab's [`crate::widget::TabView`], the same
//! wheel and selection, and the same row button. Nothing typed reaches it.
//!
//! The caller lays its text out for the grid ([`Page::grid`]) and hands it
//! over a line at a time ([`Page::set`]); the page keeps the scroll where
//! the reader left it as the text grows.

use std::sync::Arc;

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::{Config, Term, test::TermSize};
use alacritty_terminal::vte::ansi::Processor;
use egui::epaint::Mesh;
use egui::{Color32, CursorIcon, PointerButton, Rect, Response, Sense, Shape, Ui, Vec2};

use crate::render::mesh::{self, BuildParams, Snapshot};
use crate::render::opacity::see_through;
use crate::render::theme::Theme;
use crate::widget::{RenderShared, RowMarks, cell_at, paint_row_button, row_button};

/// Lines a page keeps above its screen: any transcript Giverny shows.
const HISTORY: usize = 100_000;

/// The meshes drawn last, and what they were drawn for.
struct Cached {
    origin_px: Vec2,
    generation: u32,
    bg: Color32,
    meshes: Vec<Arc<Mesh>>,
}

pub struct Page {
    term: Term<VoidListener>,
    /// The lines in the grid, as set.
    lines: Vec<String>,
    /// `(cols, rows)` of the grid.
    size: (u16, u16),
    scroll_accum: f32,
    dirty: bool,
    cached: Option<Cached>,
}

/// What a frame of a page did.
pub struct Shown {
    pub response: Response,
    /// The row button was clicked.
    pub button_pressed: bool,
}

impl Default for Page {
    fn default() -> Self {
        Self::new()
    }
}

impl Page {
    pub fn new() -> Self {
        Page {
            term: new_term(2, 2),
            lines: Vec::new(),
            size: (2, 2),
            scroll_accum: 0.0,
            dirty: true,
            cached: None,
        }
    }

    /// The grid `rect` holds at the shared font size: `(cols, rows)`, as a
    /// tab's terminal sizes its own.
    pub fn grid(shared: &mut RenderShared, rect: Rect, ppp: f32) -> (u16, u16) {
        let m = shared.metrics_for(shared.font_size * ppp);
        let cols = ((rect.width() * ppp / m.cell_w as f32).floor() as u16).max(2);
        let rows = ((rect.height() * ppp / m.cell_h as f32).floor() as u16).max(2);
        (cols, rows)
    }

    /// Put `lines` (ANSI, each at most `cols` wide) in a `cols` × `rows`
    /// grid. Nothing happens when neither changed. The scroll stays where
    /// it was: at the end when it was there (so a growing text is
    /// followed), else on the same line.
    pub fn set(&mut self, lines: Vec<String>, cols: u16, rows: u16) {
        let size = (cols.max(2), rows.max(2));
        if lines == self.lines && size == self.size {
            return;
        }
        let from_top = {
            let g = self.term.grid();
            let offset = g.display_offset();
            (offset > 0).then(|| g.history_size().saturating_sub(offset))
        };
        let mut term = new_term(size.0, size.1);
        let mut parser: Processor = Processor::new();
        let text = lines.join("\r\n");
        parser.advance(&mut term, text.as_bytes());
        // A page has no cursor to show.
        parser.advance(&mut term, b"\x1b[?25l");
        if let Some(from_top) = from_top {
            let history = term.grid().history_size();
            let offset = history.saturating_sub(from_top).max(1).min(history);
            term.scroll_display(Scroll::Delta(offset as i32));
        }
        self.term = term;
        self.lines = lines;
        self.size = size;
        self.dirty = true;
    }

    /// Scroll by `lines`: up (back) when positive, as the wheel does.
    pub fn scroll(&mut self, lines: i32) {
        self.term.scroll_display(Scroll::Delta(lines));
        self.dirty = true;
    }

    pub fn to_top(&mut self) {
        self.term.scroll_display(Scroll::Top);
        self.dirty = true;
    }

    pub fn to_end(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
        self.dirty = true;
    }

    /// Rows on the screen.
    pub fn rows(&self) -> u16 {
        self.size.1
    }

    /// The screen row line `line` of the text is on, when it is on screen.
    pub fn screen_row(&self, line: usize) -> Option<u16> {
        let g = self.term.grid();
        let top = g.history_size() - g.display_offset();
        let row = line.checked_sub(top)?;
        u16::try_from(row).ok().filter(|&r| r < self.size.1)
    }

    /// Draw the page over the whole of `ui`'s remaining rect, its default
    /// background `bg`, and read the wheel and selection over it. `button`
    /// is the row button as a tab's terminal has it: its marks (rows on
    /// screen, from [`Self::screen_row`]), labels and fill.
    pub fn show(
        &mut self,
        ui: &mut Ui,
        shared: &mut RenderShared,
        bg: Color32,
        button: (&RowMarks, &'static [&'static str], Color32),
    ) -> Shown {
        let ppp = ui.ctx().pixels_per_point();
        let m = shared.metrics_for(shared.font_size * ppp);
        let ch_pt = m.cell_h as f32 / ppp;
        let avail = ui.available_size();
        let (rect, response) = ui.allocate_exact_size(avail, Sense::click_and_drag());

        let (marks, labels, fill) = button;
        let at = row_button(marks, labels, rect, ppp, m, self.size.0);
        let button_resp = at.map(|(r, _)| {
            ui.interact(r, response.id.with("row-button"), Sense::click())
                .on_hover_cursor(CursorIcon::PointingHand)
        });
        let button_pressed = button_resp.as_ref().is_some_and(|r| r.clicked());
        let on_button = button_resp.as_ref().is_some_and(|r| r.hovered());
        if !on_button {
            response.clone().on_hover_cursor(CursorIcon::Text);
            self.select(ui, &response, rect, ppp, m);
        }
        if response.hovered() || on_button {
            let dy = ui.input(|i| i.smooth_scroll_delta.y);
            if dy != 0.0 {
                self.scroll_accum += dy / ch_pt;
                let lines = self.scroll_accum.trunc() as i32;
                if lines != 0 {
                    self.scroll_accum -= lines as f32;
                    self.scroll(lines);
                }
            }
        }

        let origin_px = Vec2::new((rect.min.x * ppp).round(), (rect.min.y * ppp).round());
        let stale = self.cached.as_ref().is_none_or(|c| {
            c.origin_px != origin_px || c.generation != shared.generation || c.bg != bg
        });
        if self.dirty || stale {
            let tinted = (bg != shared.theme.bg).then(|| Theme {
                bg,
                ..shared.theme.clone()
            });
            let theme = tinted.as_ref().unwrap_or(&shared.theme);
            let snapshot = Snapshot::capture(&self.term, theme);
            let mut params = BuildParams {
                ctx: ui.ctx(),
                fonts: &shared.fonts,
                atlas: &mut shared.atlas,
                metrics: m,
                theme,
                origin_px,
                pixels_per_point: ppp,
                cursor_visible: false,
            };
            let built = mesh::build(&snapshot, &mut params);
            let mut meshes = Vec::with_capacity(built.glyphs.len() + 2);
            meshes.push(Arc::new(built.bg));
            meshes.extend(built.glyphs.into_iter().map(Arc::new));
            meshes.push(Arc::new(built.decor));
            self.cached = Some(Cached {
                origin_px,
                generation: shared.generation,
                bg,
                meshes,
            });
            self.dirty = false;
        }

        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, see_through(bg, shared.opacity));
        if let Some(c) = &self.cached {
            for mesh in &c.meshes {
                if !mesh.vertices.is_empty() {
                    painter.add(Shape::Mesh(mesh.clone()));
                }
            }
        }
        if let (Some((r, label)), Some(resp)) = (at, &button_resp) {
            paint_row_button(&painter, shared, r, label, fill, bg, resp);
        }
        Shown {
            response,
            button_pressed,
        }
    }

    /// The terminal's selection: a drag selects, a double click a word, a
    /// triple click a line, and what is selected is copied as the drag ends.
    fn select(
        &mut self,
        ui: &Ui,
        response: &Response,
        rect: Rect,
        ppp: f32,
        m: crate::render::metrics::CellMetrics,
    ) {
        let Some(pos) = ui.input(|i| i.pointer.interact_pos()) else {
            return;
        };
        let (col, row, side) = cell_at(rect, ppp, m, pos);
        let offset = self.term.grid().display_offset() as i32;
        let point = Point::new(Line(i32::from(row) - offset), Column(usize::from(col)));
        let mut copy = false;
        if response.triple_clicked() || response.double_clicked() {
            let ty = if response.triple_clicked() {
                SelectionType::Lines
            } else {
                SelectionType::Semantic
            };
            let mut sel = Selection::new(ty, point, side);
            sel.include_all();
            self.term.selection = Some(sel);
            copy = true;
        } else if response.drag_started_by(PointerButton::Primary) {
            self.term.selection = Some(Selection::new(SelectionType::Simple, point, side));
        } else if response.dragged_by(PointerButton::Primary) {
            if let Some(sel) = &mut self.term.selection {
                sel.update(point, side);
            }
        } else if response.drag_stopped_by(PointerButton::Primary) {
            copy = true;
        } else if response.clicked() {
            if self.term.selection.take().is_none() {
                return;
            }
        } else {
            return;
        }
        self.dirty = true;
        if copy
            && let Some(text) = self.term.selection_to_string()
            && !text.is_empty()
        {
            ui.ctx().copy_text(text);
        }
    }

    /// The text on screen, a line per row: what a test reads.
    pub fn screen_text(&self) -> String {
        let g = self.term.grid();
        let offset = g.display_offset() as i32;
        (0..g.screen_lines() as i32)
            .map(|r| {
                let row = &g[Line(r - offset)];
                let s: String = (0..g.columns()).map(|c| row[Column(c)].c).collect();
                s.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn new_term(cols: u16, rows: u16) -> Term<VoidListener> {
    let config = Config {
        scrolling_history: HISTORY,
        ..Config::default()
    };
    Term::new(
        config,
        &TermSize::new(usize::from(cols), usize::from(rows)),
        VoidListener,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("line {i}")).collect()
    }

    #[test]
    fn a_long_text_opens_at_its_end_and_scrolls_back() {
        let mut p = Page::new();
        p.set(numbered(12), 20, 5);
        assert_eq!(p.screen_text().lines().last(), Some("line 11"));
        assert_eq!(p.screen_row(11), Some(4));
        assert_eq!(p.screen_row(6), None);
        p.scroll(3);
        assert_eq!(p.screen_text().lines().next(), Some("line 4"));
        p.to_top();
        assert_eq!(p.screen_text().lines().next(), Some("line 0"));
        p.to_end();
        assert_eq!(p.screen_text().lines().last(), Some("line 11"));
    }
}
