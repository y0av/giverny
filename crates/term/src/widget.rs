//! The egui terminal widget: allocates the pane, handles input/scroll/resize,
//! and paints cached meshes.

use std::sync::Arc;

use egui::epaint::Mesh;
use egui::{
    CursorIcon, Event as EguiEvent, EventFilter, Pos2, Response, Sense, Shape, Ui, Vec2,
};

use alacritty_terminal::term::TermMode;

use crate::input;
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
        let (cw_pt, ch_pt) = (metrics.cell_w as f32 / ppp, metrics.cell_h as f32 / ppp);

        let avail = ui.available_size();
        let (rect, response) = ui.allocate_exact_size(avail, Sense::click_and_drag());
        if response.clicked() {
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

        // Keyboard input.
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
        if response.has_focus() {
            let mode = session.mode();
            let mut bytes: Vec<u8> = Vec::new();
            ui.input(|i| {
                for ev in &i.events {
                    match ev {
                        EguiEvent::Text(t) => {
                            if !i.modifiers.alt && !i.modifiers.ctrl {
                                bytes.extend(input::sanitize_text(t));
                            }
                        }
                        EguiEvent::Key { key, pressed: true, modifiers, .. } => {
                            if let Some(seq) = input::encode_key(*key, *modifiers, mode) {
                                bytes.extend(seq);
                            }
                        }
                        EguiEvent::Paste(s) => bytes.extend(input::encode_paste(s, mode)),
                        _ => {}
                    }
                }
            });
            if !bytes.is_empty() {
                session.scroll_to_bottom();
                session.write(bytes);
            }
        }

        // Wheel scrolling: viewport scrollback, or arrow keys on the alt screen.
        if response.hovered() {
            let dy = ui.input(|i| i.smooth_scroll_delta.y);
            if dy != 0.0 {
                self.scroll_accum += dy / ch_pt;
                let lines = self.scroll_accum.trunc() as i32;
                if lines != 0 {
                    self.scroll_accum -= lines as f32;
                    let mode = session.mode();
                    if mode.contains(TermMode::ALT_SCREEN)
                        && mode.contains(TermMode::ALTERNATE_SCROLL)
                    {
                        let key = if lines > 0 { egui::Key::ArrowUp } else { egui::Key::ArrowDown };
                        if let Some(seq) = input::encode_key(key, egui::Modifiers::NONE, mode) {
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
            }
        }

        // Paint.
        let origin_px = Vec2::new((rect.min.x * ppp).round(), (rect.min.y * ppp).round());
        let dirty = session.take_dirty();
        let needs_rebuild = dirty
            || self
                .cached
                .as_ref()
                .is_none_or(|c| c.origin_px != origin_px);
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

        // Let egui position IME candidate windows near the cursor.
        if response.has_focus() {
            let cursor_pos = Pos2::new(rect.min.x, rect.min.y);
            ui.ctx().output_mut(|o| {
                o.ime = Some(egui::output::IMEOutput {
                    rect: egui::Rect::from_min_size(cursor_pos, Vec2::new(cw_pt, ch_pt)),
                    cursor_rect: egui::Rect::from_min_size(cursor_pos, Vec2::new(1.0, ch_pt)),
                    should_interrupt_composition: false,
                });
            });
        }

        response
    }
}
