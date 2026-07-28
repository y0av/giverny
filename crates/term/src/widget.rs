//! The egui terminal widget: allocates the pane, handles keyboard/mouse
//! input, selection, scrolling and resize, and paints cached meshes.

use std::sync::Arc;

use egui::epaint::Mesh;
use egui::{
    CursorIcon, Event as EguiEvent, EventFilter, Key, Modifiers, PointerButton, Pos2, Rect,
    Response, Sense, Shape, Ui, Vec2,
};

use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::TermMode;

use crate::input::{self, MouseCode};
use crate::pty::GridSize;
use crate::render::atlas::Atlas;
use crate::render::mesh::{self, BuildParams, Snapshot};
use crate::render::metrics::{CellMetrics, FontSet};
use crate::render::theme::Theme;
use crate::session::TermSession;

pub struct TermView {
    fonts: FontSet,
    atlas: Atlas,
    pub theme: Theme,
    /// Font size in logical points.
    pub font_size: f32,
    metrics: Option<(u32, CellMetrics)>,
    scroll_accum: f32,
    cached: Option<CachedFrame>,
    had_focus: bool,
    last_motion_cell: Option<(u16, u16)>,
}

struct CachedFrame {
    origin_px: Vec2,
    meshes: Vec<Arc<Mesh>>,
}

impl TermView {
    pub fn new(theme: Theme, font_size: f32) -> anyhow::Result<Self> {
        Ok(Self {
            fonts: FontSet::load(None)?,
            atlas: Atlas::default(),
            theme,
            font_size,
            metrics: None,
            scroll_accum: 0.0,
            cached: None,
            had_focus: false,
            last_motion_cell: None,
        })
    }

    fn metrics_for(&mut self, px: f32) -> CellMetrics {
        let key = px.to_bits();
        if let Some((k, m)) = self.metrics
            && k == key
        {
            return m;
        }
        let font = self
            .fonts
            .primary()
            .as_font()
            .expect("primary font parses (validated at load)");
        let m = CellMetrics::compute(font, px);
        self.metrics = Some((key, m));
        m
    }

    pub fn show(&mut self, ui: &mut Ui, session: &mut TermSession) -> Response {
        let ppp = ui.ctx().pixels_per_point();
        let metrics = self.metrics_for(self.font_size * ppp);
        let ch_pt = metrics.cell_h as f32 / ppp;
        let cw_pt = metrics.cell_w as f32 / ppp;

        let avail = ui.available_size();
        let (rect, response) = ui.allocate_exact_size(avail, Sense::click_and_drag());
        if response.clicked() || response.drag_started() {
            response.request_focus();
        }
        response.clone().on_hover_cursor(CursorIcon::Text);

        // Resize the grid to fit.
        let cols = ((rect.width() * ppp / metrics.cell_w as f32).floor() as u16).max(2);
        let rows = ((rect.height() * ppp / metrics.cell_h as f32).floor() as u16).max(2);
        session.resize(GridSize {
            cols,
            rows,
            cell_width: metrics.cell_w as u16,
            cell_height: metrics.cell_h as u16,
        });

        let mode = session.mode();
        let shift_held = ui.input(|i| i.modifiers.shift);
        let mouse_reporting = mode.intersects(TermMode::MOUSE_MODE) && !shift_held;

        self.handle_keyboard(ui, session, &response, mode);
        if mouse_reporting {
            self.handle_mouse_reporting(ui, session, &response, rect, ppp, metrics, mode);
        } else {
            self.handle_selection(ui, session, &response, rect, ppp, metrics);
        }
        self.handle_wheel(ui, session, &response, rect, ppp, metrics, mode, mouse_reporting, ch_pt);
        self.handle_focus_reporting(session, &response, mode);

        // Paint.
        let origin_px = Vec2::new((rect.min.x * ppp).round(), (rect.min.y * ppp).round());
        let dirty = session.take_dirty();
        let needs_rebuild =
            dirty || self.cached.as_ref().is_none_or(|c| c.origin_px != origin_px);
        if needs_rebuild {
            let snapshot = {
                let term = session.term.lock();
                Snapshot::capture(&term, &self.theme)
            };
            let mut params = BuildParams {
                ctx: ui.ctx(),
                fonts: &self.fonts,
                atlas: &mut self.atlas,
                metrics,
                theme: &self.theme,
                origin_px,
                pixels_per_point: ppp,
            };
            let built = mesh::build(&snapshot, &mut params);
            let mut meshes = Vec::with_capacity(built.glyphs.len() + 2);
            meshes.push(Arc::new(built.bg));
            meshes.extend(built.glyphs.into_iter().map(Arc::new));
            meshes.push(Arc::new(built.decor));
            self.cached = Some(CachedFrame { origin_px, meshes });
        }

        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, self.theme.bg);
        if let Some(cached) = &self.cached {
            for mesh in &cached.meshes {
                if !mesh.vertices.is_empty() {
                    painter.add(Shape::Mesh(mesh.clone()));
                }
            }
        }

        // Let the platform position IME candidate windows near the pane.
        if response.has_focus() {
            let cursor_pos = Pos2::new(rect.min.x, rect.min.y);
            ui.ctx().output_mut(|o| {
                o.ime = Some(egui::output::IMEOutput {
                    rect: Rect::from_min_size(cursor_pos, Vec2::new(cw_pt, ch_pt)),
                    cursor_rect: Rect::from_min_size(cursor_pos, Vec2::new(1.0, ch_pt)),
                    should_interrupt_composition: false,
                });
            });
        }

        response
    }

    fn handle_keyboard(
        &mut self,
        ui: &mut Ui,
        session: &TermSession,
        response: &Response,
        mode: TermMode,
    ) {
        ui.memory_mut(|mem| {
            mem.set_focus_lock_filter(
                response.id,
                EventFilter {
                    tab: true,
                    escape: true,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                },
            )
        });
        if !response.has_focus() {
            return;
        }
        let mut bytes: Vec<u8> = Vec::new();
        let mut copied = false;
        ui.input(|i| {
            for ev in &i.events {
                match ev {
                    EguiEvent::Text(t) => {
                        if !i.modifiers.alt && !i.modifiers.ctrl {
                            bytes.extend(input::sanitize_text(t));
                        }
                    }
                    EguiEvent::Key { key, pressed: true, modifiers, .. } => {
                        // Terminal-standard clipboard chords first.
                        if modifiers.ctrl && modifiers.shift && *key == Key::C {
                            copied = true;
                            continue;
                        }
                        if let Some(seq) = input::encode_key(*key, *modifiers, mode) {
                            bytes.extend(seq);
                        }
                    }
                    EguiEvent::Paste(s) => bytes.extend(input::encode_paste(s, mode)),
                    _ => {}
                }
            }
        });
        if copied {
            let text = session.term.lock().selection_to_string();
            if let Some(text) = text
                && !text.is_empty()
            {
                ui.ctx().copy_text(text);
            }
        }
        if !bytes.is_empty() {
            session.scroll_to_bottom();
            session.write(bytes);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_mouse_reporting(
        &mut self,
        ui: &mut Ui,
        session: &TermSession,
        _response: &Response,
        rect: Rect,
        ppp: f32,
        m: CellMetrics,
        mode: TermMode,
    ) {
        let mut out: Vec<u8> = Vec::new();
        ui.input(|i| {
            for ev in &i.events {
                match ev {
                    EguiEvent::PointerButton { pos, button, pressed, modifiers } => {
                        if !rect.contains(*pos) && *pressed {
                            continue;
                        }
                        let Some(code) = button_code(*button) else { continue };
                        let (col, line, _) = cell_at(rect, ppp, m, *pos);
                        if let Some(seq) =
                            input::encode_mouse(code, col, line, *pressed, false, *modifiers, mode)
                        {
                            out.extend(seq);
                        }
                    }
                    EguiEvent::PointerMoved(pos) => {
                        if !rect.contains(*pos) {
                            continue;
                        }
                        let any_down = i.pointer.any_down();
                        let motion_wanted = mode.contains(TermMode::MOUSE_MOTION)
                            || (mode.contains(TermMode::MOUSE_DRAG) && any_down);
                        if !motion_wanted {
                            continue;
                        }
                        let (col, line, _) = cell_at(rect, ppp, m, *pos);
                        if self.last_motion_cell == Some((col, line)) {
                            continue;
                        }
                        self.last_motion_cell = Some((col, line));
                        let code = if i.pointer.button_down(PointerButton::Primary) {
                            MouseCode::Left
                        } else if i.pointer.button_down(PointerButton::Middle) {
                            MouseCode::Middle
                        } else if i.pointer.button_down(PointerButton::Secondary) {
                            MouseCode::Right
                        } else {
                            MouseCode::NoButton
                        };
                        if let Some(seq) = input::encode_mouse(
                            code,
                            col,
                            line,
                            true,
                            true,
                            i.modifiers,
                            mode,
                        ) {
                            out.extend(seq);
                        }
                    }
                    _ => {}
                }
            }
        });
        if !out.is_empty() {
            session.write(out);
        }
    }

    fn handle_selection(
        &mut self,
        ui: &mut Ui,
        session: &TermSession,
        response: &Response,
        rect: Rect,
        ppp: f32,
        m: CellMetrics,
    ) {
        let pointer = ui.input(|i| i.pointer.interact_pos());
        let Some(pos) = pointer else { return };

        let select_at = |ty: SelectionType, copy: bool| {
            let mut term = session.term.lock();
            let offset = term.grid().display_offset();
            let (col, vp_line, side) = cell_at(rect, ppp, m, pos);
            let point = Point::new(Line(vp_line as i32 - offset as i32), Column(col as usize));
            let mut sel = Selection::new(ty, point, side);
            if ty != SelectionType::Simple {
                sel.include_all();
            }
            term.selection = Some(sel);
            let text = if copy { term.selection_to_string() } else { None };
            drop(term);
            session.mark_dirty();
            if let Some(text) = text
                && !text.is_empty()
            {
                ui.ctx().copy_text(text);
            }
        };

        if response.triple_clicked() {
            select_at(SelectionType::Lines, true);
        } else if response.double_clicked() {
            select_at(SelectionType::Semantic, true);
        } else if response.drag_started_by(PointerButton::Primary) {
            select_at(SelectionType::Simple, false);
        } else if response.dragged_by(PointerButton::Primary) {
            let mut term = session.term.lock();
            let offset = term.grid().display_offset();
            let (col, vp_line, side) = cell_at(rect, ppp, m, pos);
            let point = Point::new(Line(vp_line as i32 - offset as i32), Column(col as usize));
            if let Some(sel) = &mut term.selection {
                sel.update(point, side);
            }
            drop(term);
            session.mark_dirty();
        } else if response.drag_stopped_by(PointerButton::Primary) {
            let text = session.term.lock().selection_to_string();
            if let Some(text) = text
                && !text.is_empty()
            {
                ui.ctx().copy_text(text);
            }
        } else if response.clicked() {
            let mut term = session.term.lock();
            if term.selection.take().is_some() {
                drop(term);
                session.mark_dirty();
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_wheel(
        &mut self,
        ui: &mut Ui,
        session: &TermSession,
        response: &Response,
        rect: Rect,
        ppp: f32,
        m: CellMetrics,
        mode: TermMode,
        mouse_reporting: bool,
        ch_pt: f32,
    ) {
        if !response.hovered() {
            return;
        }
        let dy = ui.input(|i| i.smooth_scroll_delta.y);
        if dy == 0.0 {
            return;
        }
        self.scroll_accum += dy / ch_pt;
        let lines = self.scroll_accum.trunc() as i32;
        if lines == 0 {
            return;
        }
        self.scroll_accum -= lines as f32;

        if mouse_reporting {
            let pos = ui.input(|i| i.pointer.hover_pos()).unwrap_or(rect.min);
            let (col, line, _) = cell_at(rect, ppp, m, pos);
            let code = if lines > 0 { MouseCode::WheelUp } else { MouseCode::WheelDown };
            let mods = ui.input(|i| i.modifiers);
            let mut out = Vec::new();
            for _ in 0..lines.unsigned_abs() {
                if let Some(seq) = input::encode_mouse(code, col, line, true, false, mods, mode) {
                    out.extend(seq);
                }
            }
            session.write(out);
        } else if mode.contains(TermMode::ALT_SCREEN) && mode.contains(TermMode::ALTERNATE_SCROLL) {
            let key = if lines > 0 { Key::ArrowUp } else { Key::ArrowDown };
            if let Some(seq) = input::encode_key(key, Modifiers::NONE, mode) {
                let mut all = Vec::new();
                for _ in 0..lines.unsigned_abs() {
                    all.extend_from_slice(&seq);
                }
                session.write(all);
            }
        } else {
            session.scroll_lines(lines);
        }
    }

    fn handle_focus_reporting(
        &mut self,
        session: &TermSession,
        response: &Response,
        mode: TermMode,
    ) {
        let has = response.has_focus();
        if has != self.had_focus {
            if mode.contains(TermMode::FOCUS_IN_OUT) {
                session.write(if has { b"\x1b[I".to_vec() } else { b"\x1b[O".to_vec() });
            }
            self.had_focus = has;
        }
    }
}

fn button_code(button: PointerButton) -> Option<MouseCode> {
    match button {
        PointerButton::Primary => Some(MouseCode::Left),
        PointerButton::Middle => Some(MouseCode::Middle),
        PointerButton::Secondary => Some(MouseCode::Right),
        _ => None,
    }
}

/// Pointer position → 0-based viewport cell + cell side.
fn cell_at(rect: Rect, ppp: f32, m: CellMetrics, pos: Pos2) -> (u16, u16, Side) {
    let x_px = ((pos.x - rect.min.x) * ppp).max(0.0);
    let y_px = ((pos.y - rect.min.y) * ppp).max(0.0);
    let col = (x_px / m.cell_w as f32).floor() as u16;
    let line = (y_px / m.cell_h as f32).floor() as u16;
    let within = x_px - col as f32 * m.cell_w as f32;
    let side = if within < m.cell_w as f32 / 2.0 { Side::Left } else { Side::Right };
    (col, line, side)
}
