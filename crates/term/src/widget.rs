//! The egui terminal widget.
//!
//! [`RenderShared`] holds resources common to every tab (fonts, glyph atlas,
//! theme, font size); [`TabView`] holds one tab's view state (mesh cache,
//! scroll accumulator, focus). A `generation` counter on the shared state
//! invalidates every tab's cache when fonts/theme change.

use std::path::PathBuf;
use std::sync::Arc;

use egui::epaint::Mesh;
use egui::{
    Color32, CursorIcon, Event as EguiEvent, EventFilter, Key, Modifiers, PointerButton, Pos2,
    Rect, Response, Sense, Shape, Ui, Vec2,
};

use alacritty_terminal::index::{Column, Direction, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::TermMode;

use crate::input::{self, MouseCode};
use crate::pty::GridSize;
use crate::render::atlas::Atlas;
use crate::render::mesh::{self, BuildParams, Snapshot};
use crate::render::metrics::{CellMetrics, FontSet};
use crate::render::opacity::see_through;
use crate::render::theme::Theme;
use crate::search::{ClickTarget, Search};
use crate::session::TermSession;

/// Default font size in logical points (`Ctrl+0` resets to this).
pub const DEFAULT_FONT_SIZE: f32 = 13.0;

/// Render resources shared by all tabs.
pub struct RenderShared {
    fonts: FontSet,
    atlas: Atlas,
    pub theme: Theme,
    /// Font size in logical points.
    pub font_size: f32,
    /// `window.opacity`, as far as the window can show it: 1.0 wherever
    /// the window was not made see-through.
    pub opacity: f32,
    metrics: Option<(u32, CellMetrics)>,
    generation: u32,
}

impl RenderShared {
    pub fn new(theme: Theme, font_size: f32) -> anyhow::Result<Self> {
        Self::with_family(theme, font_size, None)
    }

    /// `family`: preferred font family from config (`None` = auto-detect).
    pub fn with_family(theme: Theme, font_size: f32, family: Option<&str>) -> anyhow::Result<Self> {
        Ok(Self {
            fonts: FontSet::load(family)?,
            atlas: Atlas::default(),
            theme,
            font_size,
            opacity: 1.0,
            metrics: None,
            generation: 0,
        })
    }

    /// Swap the theme; invalidates cached meshes so colors take effect.
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
        self.generation = self.generation.wrapping_add(1);
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

    /// Install the terminal's fonts into egui so rail/UI text can render the
    /// same symbol set as the grid (spinners, flags, box drawing).
    pub fn install_ui_fonts(&self, ctx: &egui::Context) {
        use egui::FontFamily;
        use egui::epaint::text::{FontData, FontInsert, FontPriority, InsertFontFamily};
        for (i, (name, bytes)) in self.fonts.face_bytes().into_iter().enumerate() {
            // Primary face leads the monospace chain; the rest (symbol/emoji
            // fallbacks) go behind everything, in order, so they only fill
            // gaps. Inserting them all as Highest would reverse that.
            let mono_priority = if i == 0 {
                FontPriority::Highest
            } else {
                FontPriority::Lowest
            };
            ctx.add_font(FontInsert {
                name,
                data: FontData::from_owned(bytes),
                families: vec![
                    InsertFontFamily {
                        family: FontFamily::Monospace,
                        priority: mono_priority,
                    },
                    InsertFontFamily {
                        family: FontFamily::Proportional,
                        priority: FontPriority::Lowest,
                    },
                ],
            });
        }
    }

    /// Change the font size; invalidates every tab's cached meshes.
    pub fn set_font_size(&mut self, size: f32) {
        let size = size.clamp(7.0, 32.0);
        if size != self.font_size {
            self.font_size = size;
            self.metrics = None;
            self.generation = self.generation.wrapping_add(1);
        }
    }
}

/// Per-tab view state.
pub struct TabView {
    scroll_accum: f32,
    cached: Option<CachedFrame>,
    had_focus: bool,
    last_motion_cell: Option<(u16, u16)>,
    /// A press that landed on something drawn over the grid (an overlay in a
    /// layer above it) and so was not reported; its release is not either.
    swallowed_press: bool,
    /// Layers drawn over the grid that the wheel goes through, to scroll the
    /// terminal under them: [`Self::wheel_through`].
    wheel_through: Vec<egui::LayerId>,
    last_blink: bool,
    /// When the cursor blink (re)started: on focus, and on every keystroke,
    /// so the cursor is solid while typing. `None` while unfocused.
    blink_from: Option<f64>,
    /// Open scrollback search (`Ctrl+Shift+F`).
    pub search: Option<Search>,
    /// Click target under the pointer while Ctrl is held.
    hover_target: Option<(ClickTarget, u16, u16, u16)>,
    /// Keyboard hints (`Ctrl+Shift+E`): every URL and path on screen, labelled.
    hints: Option<Hints>,
    /// Uploaded image textures, keyed by graphics id.
    textures: std::collections::HashMap<u32, egui::TextureHandle>,
}

/// Labelled targets on the visible screen.
///
/// The point is reaching what is *printed* without the mouse — including
/// inside a full-screen program like Claude, where the shell's own completion
/// does not exist because the shell is not running.
struct Hints {
    /// `(label, row, target)`, in reading order.
    items: Vec<(char, u16, crate::search::RowTarget)>,
    /// More targets than labels — say so rather than silently dropping them.
    dropped: usize,
}

/// Blink cycles per second.
const BLINK_HZ: f64 = 1.4;
/// Share of each cycle the cursor is shown.
const BLINK_ON: f64 = 0.65;
/// Seconds without typing after which the cursor stops blinking and stays
/// shown, as kitty and VS Code do.
const BLINK_FOR: f64 = 15.0;

/// Whether the cursor is shown `since` seconds into a blink, and how long
/// until that changes (`None` once the blink has stopped).
fn cursor_blink(since: f64) -> (bool, Option<f64>) {
    if since >= BLINK_FOR {
        return (true, None);
    }
    let phase = (since * BLINK_HZ).fract();
    let (visible, to_edge) = if phase < BLINK_ON {
        (true, BLINK_ON - phase)
    } else {
        (false, 1.0 - phase)
    };
    // A hair past the edge, so the frame lands on the far side of it.
    let wait = (to_edge / BLINK_HZ).min(BLINK_FOR - since) + 0.005;
    (visible, Some(wait))
}

/// Input that restarts the blink: what a person typing produces.
fn is_typing(event: &egui::Event) -> bool {
    matches!(
        event,
        egui::Event::Key { pressed: true, .. } | egui::Event::Text(_) | egui::Event::Paste(_)
    )
}

/// Input that says somebody is using the window, so output answering it is
/// drawn without pacing.
fn is_attention(event: &egui::Event) -> bool {
    is_typing(event)
        || matches!(
            event,
            egui::Event::PointerButton { .. }
                | egui::Event::PointerMoved(_)
                | egui::Event::MouseWheel { .. }
                | egui::Event::Key { .. }
        )
}

/// Home row first: the labels should be reachable without looking.
const HINT_LABELS: &[u8] = b"asdfghjklqwertyuiopzxcvbnm";

impl Default for TabView {
    fn default() -> Self {
        Self {
            scroll_accum: 0.0,
            cached: None,
            had_focus: false,
            last_motion_cell: None,
            swallowed_press: false,
            wheel_through: Vec::new(),
            last_blink: true,
            blink_from: None,
            search: None,
            hover_target: None,
            hints: None,
            textures: std::collections::HashMap::new(),
        }
    }
}

/// Is a swallowed press still owed its release, after a frame? Only while
/// mouse reporting is on and its release has not come: a release on a frame
/// that did not report (Shift held, the pointer over a link) is the one it
/// was owed, and with mouse mode off there is nothing left to hold back.
/// Kept past that, the flag swallowed the release of the next click on the
/// grid instead, and the program saw a button held down.
fn still_swallowed(swallowed: bool, reported: bool, mouse_mode: bool, released: bool) -> bool {
    swallowed && mouse_mode && (reported || !released)
}

struct CachedFrame {
    origin_px: Vec2,
    generation: u32,
    meshes: Vec<Arc<Mesh>>,
}

impl TabView {
    /// Let the mouse wheel through these layers, drawn over the grid, to the
    /// terminal: over them it scrolls exactly as over the grid. A bar pinned
    /// to the top row covers the grid, but nothing of its own scrolls.
    pub fn wheel_through(&mut self, layers: impl IntoIterator<Item = egui::LayerId>) {
        self.wheel_through.clear();
        self.wheel_through.extend(layers);
    }

    pub fn show(
        &mut self,
        ui: &mut Ui,
        shared: &mut RenderShared,
        session: &mut TermSession,
    ) -> Response {
        let ppp = ui.ctx().pixels_per_point();
        let metrics = shared.metrics_for(shared.font_size * ppp);
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

        self.handle_search(ui, session, rect, ppp, metrics);
        self.handle_hints(ui, session, rect, ppp, metrics);
        self.handle_click_targets(ui, session, &response, rect, ppp, metrics);
        // Ctrl over a path or URL makes the pointer ours: we open what is
        // under it, so the click must not also reach the program. Claude Code
        // opens hyperlinks itself (`onHyperlinkClick` → `xdg-open`), and a
        // forwarded click opened the same link a second time.
        let pointer_is_ours = self.hover_target.is_some();
        self.handle_keyboard(ui, shared, session, &response, mode);
        if pointer_is_ours {
            // Neither reported nor treated as a selection drag.
        } else if mouse_reporting {
            self.handle_mouse_reporting(ui, session, rect, ppp, metrics, mode);
        } else {
            self.handle_selection(ui, session, &response, rect, ppp, metrics);
        }
        let released = ui.input(|i| {
            i.events
                .iter()
                .any(|e| matches!(e, EguiEvent::PointerButton { pressed: false, .. }))
        });
        self.swallowed_press = still_swallowed(
            self.swallowed_press,
            mouse_reporting && !pointer_is_ours,
            mode.intersects(TermMode::MOUSE_MODE),
            released,
        );
        self.handle_wheel(
            ui,
            session,
            &response,
            rect,
            ppp,
            metrics,
            mode,
            mouse_reporting,
            ch_pt,
        );
        self.handle_focus_reporting(session, &response, mode);

        // Cursor blink: focused tabs pulse gently; unfocused show steady.
        // A blink is a full repaint, so wake only at its on/off edges, and
        // stop after a while without typing: a window left focused and idle
        // would otherwise redraw forever (#35).
        let focused = response.has_focus();
        let now = ui.input(|i| i.time);
        // Output pacing (`pace`): is anyone using the window right now?
        let (window_focused, touched, predicted_dt) =
            ui.input(|i| (i.focused, i.events.iter().any(is_attention), i.predicted_dt));
        if touched {
            crate::pace::note_input();
        }
        crate::pace::note_frame(window_focused, predicted_dt);
        if !focused {
            self.blink_from = None;
        } else if self.blink_from.is_none() || ui.input(|i| i.events.iter().any(is_typing)) {
            self.blink_from = Some(now);
        }
        let (cursor_visible, next_edge) = match self.blink_from {
            Some(from) => cursor_blink(now - from),
            None => (true, None),
        };
        if let Some(wait) = next_edge {
            // Plus egui's predicted frame time, which it takes off every
            // delayed repaint: without it the frame lands just before the
            // edge and spins on it (`pace::LAND_PAST`, #43).
            let wait = wait + f64::from(predicted_dt.clamp(0.0, 0.2));
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_secs_f64(wait));
        }

        // Paint.
        let origin_px = Vec2::new((rect.min.x * ppp).round(), (rect.min.y * ppp).round());
        let dirty = session.take_dirty();
        let needs_rebuild = dirty
            || self.last_blink != cursor_visible
            || self
                .cached
                .as_ref()
                .is_none_or(|c| c.origin_px != origin_px || c.generation != shared.generation);
        self.last_blink = cursor_visible;
        if needs_rebuild {
            let snapshot = {
                let term = session.term.lock();
                Snapshot::capture(&term, &shared.theme)
            };
            let metrics = shared.metrics_for(shared.font_size * ppp);
            let mut params = BuildParams {
                ctx: ui.ctx(),
                fonts: &shared.fonts,
                atlas: &mut shared.atlas,
                metrics,
                theme: &shared.theme,
                origin_px,
                pixels_per_point: ppp,
                cursor_visible,
            };
            let built = mesh::build(&snapshot, &mut params);
            let mut meshes = Vec::with_capacity(built.glyphs.len() + 2);
            meshes.push(Arc::new(built.bg));
            meshes.extend(built.glyphs.into_iter().map(Arc::new));
            meshes.push(Arc::new(built.decor));
            self.cached = Some(CachedFrame {
                origin_px,
                generation: shared.generation,
                meshes,
            });
        }

        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, see_through(shared.theme.bg, shared.opacity));
        // Search match rows, as bands across the grid.
        let search_bands: Vec<Rect> = self
            .search
            .as_ref()
            .and_then(|search| {
                let term = session.term.lock();
                search.highlight_rows(&term)
            })
            .into_iter()
            .flatten()
            .filter(|&row| row >= 0 && row < rows_now(rect, ppp, metrics))
            .map(|row| {
                let y = rect.min.y + (row as f32 * metrics.cell_h as f32) / ppp;
                Rect::from_min_size(
                    Pos2::new(rect.min.x, y),
                    Vec2::new(rect.width(), metrics.cell_h as f32 / ppp),
                )
            })
            .collect();
        // A see-through window keeps a match as solid as a selection: the
        // highlight is a tint over the background, and over the desktop it
        // would be a tint over whatever the wallpaper is.
        if shared.opacity < 1.0 {
            for band in &search_bands {
                painter.rect_filled(*band, 0.0, shared.theme.bg);
            }
        }
        if let Some(cached) = &self.cached {
            for mesh in &cached.meshes {
                if !mesh.vertices.is_empty() {
                    painter.add(Shape::Mesh(mesh.clone()));
                }
            }
        }

        // Search match highlight + hovered link underline, over the grid.
        for band in search_bands {
            painter.rect_filled(band, 0.0, Color32::from_rgba_unmultiplied(217, 181, 95, 40));
        }
        // Images from the kitty graphics protocol, drawn over the grid at the
        // cells they were placed on, scrolling with the text.
        self.paint_images(ui, session, &painter, rect, ppp, metrics);

        // Painted here, after the grid mesh: anything drawn before it is
        // covered by the terminal's own background.
        if let Some(hints) = &self.hints {
            for (label, row, item) in &hints.items {
                let x = rect.min.x + (item.col as f32 * metrics.cell_w as f32) / ppp;
                let y = rect.min.y + (*row as f32 * metrics.cell_h as f32) / ppp;
                let w = (item.len as f32 * metrics.cell_w as f32) / ppp;
                let h = metrics.cell_h as f32 / ppp;
                painter.rect_filled(
                    Rect::from_min_size(Pos2::new(x, y), Vec2::new(w, h)),
                    2.0,
                    Color32::from_rgba_unmultiplied(0x5f, 0xa3, 0xa3, 70),
                );
                let chip =
                    Rect::from_min_size(Pos2::new(x, y), Vec2::new(metrics.cell_w as f32 / ppp, h));
                painter.rect_filled(chip, 2.0, Color32::from_rgb(0xd9, 0xb5, 0x5f));
                painter.text(
                    chip.center(),
                    egui::Align2::CENTER_CENTER,
                    label.to_string(),
                    egui::FontId::monospace(h * 0.72),
                    Color32::BLACK,
                );
            }
        }
        if let Some((_, line, col, len)) = &self.hover_target {
            let x = rect.min.x + (*col as f32 * metrics.cell_w as f32) / ppp;
            let y = rect.min.y + ((*line + 1) as f32 * metrics.cell_h as f32 - 2.0) / ppp;
            let underline = Rect::from_min_size(
                Pos2::new(x, y),
                Vec2::new((*len as f32 * metrics.cell_w as f32) / ppp, 1.0),
            );
            painter.rect_filled(underline, 0.0, shared.theme.ansi[12]);
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
        shared: &mut RenderShared,
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
        let mut zoom: i32 = 0;
        let mut zoom_reset = false;
        ui.input(|i| {
            for ev in &i.events {
                match ev {
                    EguiEvent::Text(t) => {
                        if !i.modifiers.alt && !i.modifiers.ctrl {
                            bytes.extend(input::sanitize_text(t));
                        }
                    }
                    EguiEvent::Key {
                        key,
                        pressed: true,
                        modifiers,
                        ..
                    } => {
                        // Terminal-standard chords first (never reach the shell).
                        // Reached on macOS, where `command` is Cmd so Ctrl+C
                        // and Ctrl+Shift+C arrive as keys. Elsewhere egui turns
                        // both into `Copy`, handled below.
                        if modifiers.ctrl && modifiers.shift && *key == Key::C {
                            copied = true;
                            continue;
                        }
                        if modifiers.ctrl && matches!(key, Key::Plus | Key::Equals) {
                            zoom += 1;
                            continue;
                        }
                        if modifiers.ctrl && *key == Key::Minus {
                            zoom -= 1;
                            continue;
                        }
                        if modifiers.ctrl && *key == Key::Num0 {
                            zoom_reset = true;
                            continue;
                        }
                        if let Some(seq) = input::encode_key(*key, *modifiers, mode) {
                            bytes.extend(seq);
                        }
                    }
                    // egui-winit swallows the platform clipboard chords: it
                    // pushes Copy/Cut and returns *without* emitting the key.
                    // On Linux and Windows `command` is Ctrl, so Ctrl+C never
                    // reached the shell — no interrupt, and no copy either,
                    // since the branch above could not fire. Shift is what
                    // separates the two: Ctrl+Shift+C copies, Ctrl+C signals.
                    EguiEvent::Copy | EguiEvent::Cut => {
                        let cut = matches!(ev, EguiEvent::Cut);
                        match input::clipboard_chord(
                            cut,
                            i.modifiers.shift,
                            cfg!(target_os = "macos"),
                        ) {
                            input::ClipboardChord::CopySelection => copied = true,
                            input::ClipboardChord::Signal(b) => bytes.push(b),
                        }
                    }
                    EguiEvent::Paste(s) => bytes.extend(input::encode_paste(s, mode)),
                    // Composed input — Hebrew, Arabic, CJK, anything going
                    // through the platform input method — arrives as an IME
                    // commit and never as `Text`, so it used to vanish
                    // entirely. Pre-edit is deliberately not forwarded: the
                    // composition is not final, and the line editor on the
                    // other end of the pty is the one that owns the line.
                    EguiEvent::Ime(egui::ImeEvent::Commit(t)) => {
                        bytes.extend(input::sanitize_text(t));
                    }
                    _ => {}
                }
            }
        });
        if zoom != 0 || zoom_reset {
            let new_size = if zoom_reset {
                DEFAULT_FONT_SIZE
            } else {
                shared.font_size + zoom as f32
            };
            shared.set_font_size(new_size);
            session.mark_dirty();
        }
        if copied {
            let text = session.term.lock().selection_to_string();
            if let Some(text) = text
                && !text.is_empty()
            {
                ui.ctx().copy_text(text);
            }
        }
        if !bytes.is_empty() {
            session.note_user_input();
            session.scroll_to_bottom();
            session.write(bytes);
        }
    }

    fn handle_mouse_reporting(
        &mut self,
        ui: &mut Ui,
        session: &TermSession,
        rect: Rect,
        ppp: f32,
        m: CellMetrics,
        mode: TermMode,
    ) {
        let mut out: Vec<u8> = Vec::new();
        // Something the app draws over the grid in a layer of its own (a bar
        // pinned to its top row) takes the pointer there: a click on it is
        // not a click in the program.
        // Asked once, before reading the events: the context is locked
        // while they are read.
        let own = ui.layer_id();
        let covered = ui
            .input(|i| i.pointer.latest_pos())
            .and_then(|pos| ui.ctx().layer_id_at(pos))
            .is_some_and(|layer| layer != own);
        ui.input(|i| {
            for ev in &i.events {
                match ev {
                    EguiEvent::PointerButton {
                        pos,
                        button,
                        pressed,
                        modifiers,
                    } => {
                        if !rect.contains(*pos) && *pressed {
                            continue;
                        }
                        if *pressed && covered {
                            self.swallowed_press = true;
                            continue;
                        }
                        if !*pressed && std::mem::take(&mut self.swallowed_press) {
                            continue;
                        }
                        let Some(code) = button_code(*button) else {
                            continue;
                        };
                        let (col, line, _) = cell_at(rect, ppp, m, *pos);
                        if let Some(seq) =
                            input::encode_mouse(code, col, line, *pressed, false, *modifiers, mode)
                        {
                            out.extend(seq);
                        }
                    }
                    EguiEvent::PointerMoved(pos) => {
                        if !rect.contains(*pos) || covered {
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
                        if let Some(seq) =
                            input::encode_mouse(code, col, line, true, true, i.modifiers, mode)
                        {
                            out.extend(seq);
                        }
                    }
                    _ => {}
                }
            }
        });
        if !out.is_empty() {
            session.note_user_input();
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
            let text = if copy {
                term.selection_to_string()
            } else {
                None
            };
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
        // Over a layer the wheel goes through, the grid is not hovered but
        // is under the pointer. The layer is asked outside `input`: the
        // context is locked while it reads.
        let through = !self.wheel_through.is_empty()
            && ui
                .input(|i| i.pointer.hover_pos())
                .filter(|pos| rect.contains(*pos))
                .and_then(|pos| ui.ctx().layer_id_at(pos))
                .is_some_and(|layer| self.wheel_through.contains(&layer));
        if !response.hovered() && !through {
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
            let code = if lines > 0 {
                MouseCode::WheelUp
            } else {
                MouseCode::WheelDown
            };
            let mods = ui.input(|i| i.modifiers);
            let mut out = Vec::new();
            for _ in 0..lines.unsigned_abs() {
                if let Some(seq) = input::encode_mouse(code, col, line, true, false, mods, mode) {
                    out.extend(seq);
                }
            }
            session.write(out);
        } else if mode.contains(TermMode::ALT_SCREEN) && mode.contains(TermMode::ALTERNATE_SCROLL) {
            let key = if lines > 0 {
                Key::ArrowUp
            } else {
                Key::ArrowDown
            };
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
                session.write(if has {
                    b"\x1b[I".to_vec()
                } else {
                    b"\x1b[O".to_vec()
                });
            }
            self.had_focus = has;
        }
    }
}

impl TabView {
    /// Search overlay: query box, next/prev, Esc to close.
    /// `Ctrl+Shift+E`: label every URL and path on screen; a letter opens it,
    /// Shift+letter types it at the prompt.
    fn handle_hints(
        &mut self,
        ui: &mut Ui,
        session: &TermSession,
        rect: Rect,
        ppp: f32,
        m: CellMetrics,
    ) {
        let toggle = ui.input_mut(|i| i.consume_key(Modifiers::CTRL | Modifiers::SHIFT, Key::E));
        if toggle {
            self.hints = match self.hints.take() {
                Some(_) => None,
                None => Some(Self::collect_hints(session, rect, ppp, m)),
            };
        }
        let Some(hints) = self.hints.take() else {
            return;
        };
        if hints.items.is_empty() {
            return;
        }

        // Read the choice before painting, so a hit closes in the same frame.
        let mut chosen: Option<(char, bool)> = None;
        let mut close = ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
        ui.input_mut(|i| {
            i.events.retain(|ev| {
                let egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } = ev
                else {
                    return true;
                };
                let Some(name) = key.name().chars().next().map(|c| c.to_ascii_lowercase()) else {
                    return true;
                };
                if key.name().len() == 1 && HINT_LABELS.contains(&(name as u8)) {
                    chosen = Some((name, modifiers.shift));
                    // Swallow it: the letter chose a hint, it is not input.
                    return false;
                }
                true
            });
        });

        if let Some((label, insert)) = chosen {
            if let Some((_, _, item)) = hints.items.iter().find(|(l, ..)| *l == label) {
                if insert {
                    // Type it where the cursor is — the whole point inside a
                    // program that is not a shell.
                    session.write(input::sanitize_text(&item.text));
                    session.note_user_input();
                } else {
                    item.target.open();
                }
            }
            close = true;
        }

        // The key bar: memory is not a prerequisite.
        let mut hint = format!(
            "{} targets · letter opens · shift+letter types · esc",
            hints.items.len()
        );
        if hints.dropped > 0 {
            hint.push_str(&format!(" · {} more not labelled", hints.dropped));
        }
        let bar = Rect::from_min_size(
            Pos2::new(rect.min.x + 8.0, rect.max.y - 30.0),
            Vec2::new(rect.width() - 16.0, 24.0),
        );
        ui.scope_builder(egui::UiBuilder::new().max_rect(bar), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.label(egui::RichText::new(hint).font(egui::FontId::monospace(11.0)));
            });
        });

        if !close {
            self.hints = Some(hints);
        }
    }

    /// Draw placed images.
    ///
    /// Textures are uploaded once per image and kept until the placement goes;
    /// a screenful of images must not re-upload every frame.
    fn paint_images(
        &mut self,
        ui: &Ui,
        session: &TermSession,
        painter: &egui::Painter,
        rect: Rect,
        ppp: f32,
        m: CellMetrics,
    ) {
        use alacritty_terminal::grid::Dimensions;
        let (history, display_offset, screen_rows) = {
            let term = session.term.lock();
            let grid = term.grid();
            (
                grid.history_size() as i64,
                grid.display_offset() as i64,
                grid.screen_lines() as i64,
            )
        };
        let mut gfx = session.shared.graphics.lock();
        if gfx.placements.is_empty() {
            self.textures.clear();
            return;
        }
        // Anything older than the retained scrollback can never be shown again.
        gfx.prune(-1);

        let placements: Vec<crate::graphics::Placement> = gfx.placements.clone();
        for p in placements {
            // Absolute row -> row on screen right now.
            let row = p.abs_row - (history - display_offset);
            if row < 0 || row >= screen_rows {
                continue;
            }
            let Some(image) = gfx.images.get(&p.image_id) else {
                continue;
            };
            let key = p.image_id;
            let texture = match self.textures.get(&key) {
                Some(t) => t.clone(),
                None => {
                    let color = egui::ColorImage::from_rgba_unmultiplied(
                        [image.width as usize, image.height as usize],
                        &image.rgba,
                    );
                    let t = ui.ctx().load_texture(
                        format!("giverny-img-{key}"),
                        color,
                        egui::TextureOptions::LINEAR,
                    );
                    self.textures.insert(key, t.clone());
                    t
                }
            };
            // Cell counts when the program gave them, natural size otherwise.
            let w = if p.cols > 0 {
                p.cols as f32 * m.cell_w as f32
            } else {
                image.width as f32
            };
            let h = if p.rows > 0 {
                p.rows as f32 * m.cell_h as f32
            } else {
                image.height as f32
            };
            let origin = Pos2::new(
                rect.min.x + (p.col as f32 * m.cell_w as f32) / ppp,
                rect.min.y + (row as f32 * m.cell_h as f32) / ppp,
            );
            let area = Rect::from_min_size(origin, Vec2::new(w / ppp, h / ppp));
            painter.image(
                texture.id(),
                area.intersect(rect),
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );
        }
        // Drop textures for images that have gone.
        self.textures.retain(|id, _| gfx.images.contains_key(id));
    }

    /// Scan the visible rows for things worth reaching.
    fn collect_hints(session: &TermSession, rect: Rect, ppp: f32, m: CellMetrics) -> Hints {
        let cwd = session.proc_cwd().unwrap_or_else(|| PathBuf::from("/"));
        let rows = rows_now(rect, ppp, m).max(0) as u16;
        let mut items = Vec::new();
        let mut dropped = 0usize;
        for row in 0..rows {
            let text = session.row_text(row);
            for target in crate::search::targets_in_row(&text, &cwd) {
                match HINT_LABELS.get(items.len()) {
                    Some(&label) => items.push((label as char, row, target)),
                    None => dropped += 1,
                }
            }
        }
        Hints { items, dropped }
    }

    fn handle_search(
        &mut self,
        ui: &mut Ui,
        session: &TermSession,
        rect: Rect,
        _ppp: f32,
        _m: CellMetrics,
    ) {
        // Open/close chords are consumed before the terminal sees them.
        let toggle = ui.input_mut(|i| i.consume_key(Modifiers::CTRL | Modifiers::SHIFT, Key::F));
        if toggle {
            self.search = match self.search.take() {
                Some(s) => {
                    let mut term = session.term.lock();
                    s.clear(&mut term);
                    term.selection = None;
                    session.mark_dirty();
                    None
                }
                None => Some(Search::default()),
            };
        }
        let Some(mut search) = self.search.take() else {
            return;
        };

        let mut close = false;
        let mut step: Option<Direction> = None;
        ui.input_mut(|i| {
            if i.consume_key(Modifiers::NONE, Key::Escape) {
                close = true;
            }
            if i.consume_key(Modifiers::NONE, Key::Enter) {
                step = Some(Direction::Left);
            }
            if i.consume_key(Modifiers::SHIFT, Key::Enter) {
                step = Some(Direction::Right);
            }
        });

        let bar = Rect::from_min_size(
            Pos2::new(rect.max.x - 330.0, rect.min.y + 6.0),
            Vec2::new(320.0, 28.0),
        );
        let mut query = search.query.clone();
        ui.scope_builder(egui::UiBuilder::new().max_rect(bar), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.horizontal(|ui| {
                    let te = ui.add(
                        egui::TextEdit::singleline(&mut query)
                            .hint_text("search scrollback")
                            .desired_width(180.0)
                            .font(egui::FontId::monospace(12.0)),
                    );
                    if search.needs_focus {
                        te.request_focus();
                        search.needs_focus = false;
                    }
                    if ui.small_button("↑").clicked() {
                        step = Some(Direction::Left);
                    }
                    if ui.small_button("↓").clicked() {
                        step = Some(Direction::Right);
                    }
                    if search.no_match {
                        ui.colored_label(Color32::from_rgb(0xd9, 0x7f, 0x70), "none");
                    }
                });
            });
        });

        if query != search.query {
            search.set_query(query);
            step = Some(Direction::Left);
        }
        if let Some(direction) = step
            && !search.query.is_empty()
        {
            let mut term = session.term.lock();
            search.find(&mut term, direction);
            if let Some(m) = &search.current {
                let mut sel = alacritty_terminal::selection::Selection::new(
                    SelectionType::Simple,
                    *m.start(),
                    Side::Left,
                );
                sel.update(*m.end(), Side::Right);
                term.selection = Some(sel);
            }
            drop(term);
            session.mark_dirty();
        }

        if close {
            let mut term = session.term.lock();
            search.clear(&mut term);
            term.selection = None;
            drop(term);
            session.mark_dirty();
        } else {
            self.search = Some(search);
        }
    }

    /// Ctrl+hover underlines paths/URLs; Ctrl+click opens them.
    fn handle_click_targets(
        &mut self,
        ui: &mut Ui,
        session: &TermSession,
        response: &Response,
        rect: Rect,
        ppp: f32,
        m: CellMetrics,
    ) {
        self.hover_target = None;
        let (ctrl, pos) = ui.input(|i| {
            (
                i.modifiers.ctrl || i.modifiers.command,
                i.pointer.hover_pos(),
            )
        });
        let Some(pos) = pos.filter(|p| ctrl && rect.contains(*p)) else {
            return;
        };
        let (col, line, _) = cell_at(rect, ppp, m, pos);

        // An OSC 8 hyperlink wins: its URL is in the escape sequence, so the
        // visible text says nothing about it. This is how Claude, `gh`, `ls
        // --hyperlink` and friends emit links.
        if let Some((uri, start, len)) = session.hyperlink_at(line, col) {
            let target = crate::search::ClickTarget::Url(uri);
            self.hover_target = Some((target.clone(), line, start, len));
            ui.output_mut(|o| o.cursor_icon = CursorIcon::PointingHand);
            if response.clicked() {
                target.open();
            }
            return;
        }

        let text = session.row_text(line);
        let cwd = session.proc_cwd().unwrap_or_else(|| PathBuf::from("/"));
        let Some(target) = crate::search::target_at(&text, col as usize, &cwd) else {
            return;
        };

        // Underline the whole token: walk out from the cursor over non-space.
        let chars: Vec<char> = text.chars().collect();
        let start = (0..=col as usize)
            .rev()
            .take_while(|&i| !chars[i].is_whitespace())
            .last()
            .unwrap_or(col as usize);
        let end = (col as usize..chars.len())
            .take_while(|&i| !chars[i].is_whitespace())
            .last()
            .unwrap_or(col as usize);
        self.hover_target = Some((target.clone(), line, start as u16, (end - start + 1) as u16));

        ui.output_mut(|o| o.cursor_icon = CursorIcon::PointingHand);
        if response.clicked() {
            target.open();
        }
    }
}

/// Visible row count for the current geometry.
fn rows_now(rect: Rect, ppp: f32, m: CellMetrics) -> i32 {
    (rect.height() * ppp / m.cell_h as f32).floor() as i32
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
    let side = if within < m.cell_w as f32 / 2.0 {
        Side::Left
    } else {
        Side::Right
    };
    (col, line, side)
}

#[cfg(test)]
mod blink_tests {
    use super::cursor_blink;

    #[test]
    fn the_blink_wakes_only_at_its_edges_and_then_stops() {
        let (shown, wait) = cursor_blink(0.0);
        assert!(shown);
        // On for 0.65 of a 1/1.4 s cycle: the first edge is ~0.46 s away.
        let wait = wait.unwrap();
        assert!((0.46..0.47).contains(&wait), "{wait}");
        let (shown, wait) = cursor_blink(0.5);
        assert!(!shown);
        assert!(wait.unwrap() < 0.25);
        // Solid, and no more wake-ups, once nobody has typed for a while.
        assert_eq!(cursor_blink(15.0), (true, None));
        assert_eq!(cursor_blink(600.0), (true, None));
        // The last wake-up before the stop lands on the stop.
        let (_, wait) = cursor_blink(14.9);
        assert!(wait.unwrap() <= 0.11);
    }
}

#[cfg(test)]
mod swallow_tests {
    use super::still_swallowed;

    #[test]
    fn a_swallowed_press_is_owed_one_release_and_no_more() {
        // Held, reported frames: still owed until its release comes, which
        // the reporting itself takes.
        assert!(still_swallowed(true, true, true, false));
        assert!(still_swallowed(true, true, true, true));
        // Released on a frame that did not report (Shift held): that was it.
        assert!(!still_swallowed(true, false, true, true));
        // Not reported and not released yet: still owed.
        assert!(still_swallowed(true, false, true, false));
        // Mouse mode off: nothing to hold back any more.
        assert!(!still_swallowed(true, true, false, false));
        assert!(!still_swallowed(true, false, false, false));
        // Nothing swallowed stays nothing.
        assert!(!still_swallowed(false, false, true, false));
    }
}
