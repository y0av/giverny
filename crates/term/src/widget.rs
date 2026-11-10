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
use crate::render::bidi::RowOrder;
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
    pub(crate) fonts: FontSet,
    pub(crate) atlas: Atlas,
    pub theme: Theme,
    /// Font size in logical points.
    pub font_size: f32,
    /// `window.opacity`, as far as the window can show it: 1.0 wherever
    /// the window was not made see-through.
    pub opacity: f32,
    metrics: Option<(u32, CellMetrics)>,
    pub(crate) generation: u32,
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

    pub(crate) fn metrics_for(&mut self, px: f32) -> CellMetrics {
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

    /// One terminal cell at the current font size, in logical points.
    pub fn cell_size(&mut self, pixels_per_point: f32) -> Vec2 {
        let m = self.metrics_for(self.font_size * pixels_per_point);
        Vec2::new(m.cell_w as f32, m.cell_h as f32) / pixels_per_point
    }

    /// Paint one line of `text` in the terminal's own glyphs — same face,
    /// size, hinting and cell grid as the session — with cell 0's top-left
    /// at `top_left` (snapped to the pixel grid). One cell per `char`.
    pub fn paint_text(
        &mut self,
        painter: &egui::Painter,
        top_left: Pos2,
        text: &str,
        color: Color32,
    ) {
        let ctx = painter.ctx().clone();
        let ppp = ctx.pixels_per_point();
        let metrics = self.metrics_for(self.font_size * ppp);
        let origin_px = Vec2::new((top_left.x * ppp).round(), (top_left.y * ppp).round());
        let meshes = mesh::text_line(
            &ctx,
            &self.fonts,
            &mut self.atlas,
            metrics,
            origin_px,
            ppp,
            text,
            color,
        );
        for m in meshes {
            painter.add(Shape::Mesh(Arc::new(m)));
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
    /// Buttons whose press landed on something drawn over the grid (an
    /// overlay in a layer above it) and so was not reported; their releases
    /// are not either.
    swallowed_press: Buttons,
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
    /// Paint the default background as the theme's worker background
    /// ([`Theme::worker_bg`]): the app sets it while the tab shows a worker.
    pub worker_bg: bool,
    /// Reads the screen's text (row per line) into [`RowMarks`] whenever the
    /// grid is redrawn: rows the app wants painted over. `None`: none.
    pub marks_for: Option<fn(&str) -> RowMarks>,
    marks: RowMarks,
    /// The row button's label, longest first: the first that fits the
    /// row's blank cells is drawn, and none when none fits.
    pub button_labels: &'static [&'static str],
    /// The row button's fill; its text is drawn in the background colour.
    pub button_fill: Color32,
    /// Set by [`TabView::show`] for a frame in which the row button was
    /// clicked, or Esc pressed for it ([`RowMarks::escape`]).
    pub button_pressed: bool,
    /// Keep painting the last frame drawn, whatever the program writes
    /// meanwhile: the app sets it while it walks Claude Code
    /// from one view to another, so the steps between never reach the
    /// screen and the view changes in one frame. Input still goes through.
    pub hold: bool,
    /// The frame on screen was held: the next frame is drawn afresh.
    was_held: bool,
    /// Watches for a paste chord that found no text.
    paste_chord: input::PasteChord,
    /// Set by [`TabView::show`] for a frame in which a paste chord found no
    /// text in the clipboard while the terminal had the keyboard: the app
    /// looks for files or an image there and pastes them.
    pub paste_image: bool,
}

/// What the app paints over a tab's grid (the management panel
/// standing in for Claude Code's agent strip). Rows are screen rows of the
/// live screen, and nothing is painted while the view is scrolled back.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RowMarks {
    /// Rows drawn as blank background.
    pub hidden: Vec<u16>,
    /// `(row, used)`: a button on `row`, on its blank cells from column
    /// `used` on: at the row's right end, or at [`Self::button_left`].
    pub button: Option<(u16, u16)>,
    /// The button's label starts at this column instead of the button
    /// sitting at the row's right end (where the strip's
    /// `main` was, under the terminal's other text).
    pub button_left: Option<u16>,
    /// The button ends before this column instead of at the row's end
    /// (text further right on the row, which it must not cover).
    pub button_end: Option<u16>,
    /// Esc presses the button instead of reaching the program.
    pub escape: bool,
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
            swallowed_press: Buttons::default(),
            wheel_through: Vec::new(),
            last_blink: true,
            blink_from: None,
            search: None,
            hover_target: None,
            hints: None,
            textures: std::collections::HashMap::new(),
            worker_bg: false,
            marks_for: None,
            marks: RowMarks::default(),
            button_labels: &[],
            button_fill: Color32::from_rgb(0x8a, 0xb4, 0xf8),
            button_pressed: false,
            hold: false,
            was_held: false,
            paste_chord: input::PasteChord::default(),
            paste_image: false,
        }
    }
}

/// A set of mouse buttons.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Buttons(u8);

impl Buttons {
    fn bit(button: PointerButton) -> u8 {
        1 << button as u8
    }

    fn insert(&mut self, button: PointerButton) {
        self.0 |= Self::bit(button);
    }

    /// Take `button` out of the set; whether it was in.
    fn take(&mut self, button: PointerButton) -> bool {
        let had = self.0 & Self::bit(button) != 0;
        self.0 &= !Self::bit(button);
        had
    }

    /// Is this press or release one the program must not see? A press on
    /// something drawn over the grid is not, and nor is that button's
    /// release. Another button's release is: a right click on the grid while
    /// left is held on the bar still lets go of right.
    fn swallows(&mut self, button: PointerButton, pressed: bool, covered: bool) -> bool {
        if pressed {
            if covered {
                self.insert(button);
            }
            covered
        } else {
            self.take(button)
        }
    }
}

/// Which swallowed presses are still owed their release, after a frame?
/// Only while mouse reporting is on and the release has not come: a release
/// on a frame that did not report (Shift held, the pointer over a link) is
/// the one it was owed, and with mouse mode off there is nothing left to
/// hold back. Kept past that, the flag swallowed the release of the next
/// click on the grid instead, and the program saw a button held down.
fn still_swallowed(
    swallowed: Buttons,
    reported: bool,
    mouse_mode: bool,
    released: Buttons,
) -> Buttons {
    if !mouse_mode {
        Buttons::default()
    } else if reported {
        swallowed
    } else {
        Buttons(swallowed.0 & !released.0)
    }
}

struct CachedFrame {
    origin_px: Vec2,
    generation: u32,
    bg: Color32,
    meshes: Vec<Arc<Mesh>>,
    /// Rows drawn right to left, as last painted: what the pointer is over.
    orders: Vec<(u16, RowOrder)>,
}

impl TabView {
    /// Whether the cursor is blinking at egui time `now`: a held picture
    /// ([`TabView::hold`]) would stop it mid-blink, so a hold nothing asked
    /// for is not started while it does.
    pub fn cursor_blinking(&self, now: f64) -> bool {
        self.blink_from.is_some_and(|from| now - from < BLINK_FOR)
    }

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

        // The row button, where the last redraw found its row. Registered
        // after the grid's response, so it is on top of it and a click on
        // it goes to it alone.
        self.button_pressed = false;
        let live = session.term.lock().grid().display_offset() == 0;
        if !live {
            self.marks = RowMarks::default();
        } else if self.was_held
            && !self.hold
            && let Some(marks_for) = self.marks_for
        {
            // The first frame after a hold: its marks now, so the button
            // comes with the picture rather than a frame after it.
            self.marks = marks_for(&session.screen_text());
        }
        let button = row_button(&self.marks, self.button_labels, rect, ppp, metrics, cols);
        let button_resp = button.map(|(r, _)| {
            ui.interact(r, response.id.with("row-button"), Sense::click())
                .on_hover_cursor(CursorIcon::PointingHand)
        });
        if button_resp.as_ref().is_some_and(|r| r.clicked()) {
            self.button_pressed = true;
        }

        self.handle_search(ui, session, rect, ppp, metrics);
        self.handle_hints(ui, session, rect, ppp, metrics);
        self.handle_click_targets(ui, session, &response, rect, ppp, metrics);
        // Ctrl over a path or URL makes the pointer ours: we open what is
        // under it, so the click must not also reach the program. Claude Code
        // opens hyperlinks itself (`onHyperlinkClick` → `xdg-open`), and a
        // forwarded click opened the same link a second time.
        let pointer_is_ours =
            self.hover_target.is_some() || button_resp.as_ref().is_some_and(|r| r.hovered());
        self.handle_keyboard(ui, shared, session, &response, mode);
        if pointer_is_ours {
            // Neither reported nor treated as a selection drag.
        } else if mouse_reporting {
            self.handle_mouse_reporting(ui, session, rect, ppp, metrics, mode);
        } else {
            self.handle_selection(ui, session, &response, rect, ppp, metrics);
        }
        let released = ui.input(|i| {
            let mut released = Buttons::default();
            for e in &i.events {
                if let EguiEvent::PointerButton {
                    button,
                    pressed: false,
                    ..
                } = e
                {
                    released.insert(*button);
                }
            }
            released
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
        let bg = if self.worker_bg {
            shared.theme.worker_bg()
        } else {
            shared.theme.bg
        };
        // Held: the last frame stays, with its background and marks, as
        // long as it still fits where it is drawn. The program's output
        // stays dirty, so the first frame after is drawn from it.
        let held = self.hold
            && self
                .cached
                .as_ref()
                .is_some_and(|c| c.origin_px == origin_px && c.generation == shared.generation);
        let bg = match (&self.cached, held) {
            (Some(c), true) => c.bg,
            _ => bg,
        };
        let released = std::mem::replace(&mut self.was_held, held) && !held;
        let needs_rebuild = !held
            && (session.take_dirty()
                || released
                || self.last_blink != cursor_visible
                || self.cached.as_ref().is_none_or(|c| {
                    c.origin_px != origin_px || c.generation != shared.generation || c.bg != bg
                }));
        if !held {
            self.last_blink = cursor_visible;
        }
        if needs_rebuild {
            // A worker's tab resolves the default background to its tint,
            // so it is elided and filled below like any default cell.
            let tinted = (bg != shared.theme.bg).then(|| Theme {
                bg,
                ..shared.theme.clone()
            });
            let metrics = shared.metrics_for(shared.font_size * ppp);
            let theme = tinted.as_ref().unwrap_or(&shared.theme);
            if let (Some(marks_for), true) = (self.marks_for, live) {
                self.marks = marks_for(&session.screen_text());
            } else if self.marks_for.is_none() {
                self.marks = RowMarks::default();
            }
            let snapshot = {
                let term = session.term.lock();
                Snapshot::capture(&term, theme)
            };
            let mut params = BuildParams {
                ctx: ui.ctx(),
                fonts: &shared.fonts,
                atlas: &mut shared.atlas,
                metrics,
                theme,
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
                bg,
                meshes,
                orders: snapshot.orders,
            });
        }

        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, see_through(bg, shared.opacity));
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
                painter.rect_filled(*band, 0.0, bg);
            }
        }
        if let Some(cached) = &self.cached {
            for mesh in &cached.meshes {
                if !mesh.vertices.is_empty() {
                    painter.add(Shape::Mesh(mesh.clone()));
                }
            }
        }

        // The rows the app blanks, and its row button, over the grid.
        let row_band = |row: u16| {
            Rect::from_min_size(
                Pos2::new(
                    rect.min.x,
                    rect.min.y + (row as f32 * metrics.cell_h as f32) / ppp,
                ),
                Vec2::new(rect.width(), metrics.cell_h as f32 / ppp),
            )
        };
        for &row in &self.marks.hidden {
            if i32::from(row) < rows_now(rect, ppp, metrics) {
                painter.rect_filled(row_band(row), 0.0, bg);
            }
        }
        if let (Some((r, label)), Some(resp)) = (button, &button_resp) {
            paint_row_button(&painter, shared, r, label, self.button_fill, bg, resp);
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
        let orders = self.cached.as_ref().map_or(&[][..], |c| &c.orders[..]);
        if let Some(hints) = &self.hints {
            for (label, row, item) in &hints.items {
                let (col, len) = drawn_span(orders, *row, item.col, item.len);
                let x = rect.min.x + (col as f32 * metrics.cell_w as f32) / ppp;
                let y = rect.min.y + (*row as f32 * metrics.cell_h as f32) / ppp;
                let w = (len as f32 * metrics.cell_w as f32) / ppp;
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
            let (col, len) = drawn_span(orders, *line, *col, *len);
            let x = rect.min.x + (col as f32 * metrics.cell_w as f32) / ppp;
            let y = rect.min.y + ((*line + 1) as f32 * metrics.cell_h as f32 - 2.0) / ppp;
            let underline = Rect::from_min_size(
                Pos2::new(x, y),
                Vec2::new((len as f32 * metrics.cell_w as f32) / ppp, 1.0),
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
        // Watched whether focused or not, so a V pressed while a text field
        // had the keyboard is still known to be pressed when it comes up.
        let paste_chord = &mut self.paste_chord;
        // Every event is fed, not just up to the first hit: the tracker has
        // to see them all.
        let empty_paste = ui.input(|i| {
            i.events
                .iter()
                .filter(|ev| paste_chord.on_event(ev))
                .count()
                > 0
        });
        self.paste_image = empty_paste && response.has_focus();
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
                        // Esc is the row button's while it asks for it.
                        if *key == Key::Escape && modifiers.is_none() && self.marks.escape {
                            self.button_pressed = true;
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
                    EguiEvent::Paste(s) => {
                        let text = input::copied_files_text();
                        bytes.extend(input::encode_paste(text.as_deref().unwrap_or(s), mode));
                    }
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
        let orders = self.cached.as_ref().map_or(&[][..], |c| &c.orders[..]);
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
                        if self.swallowed_press.swallows(*button, *pressed, covered) {
                            continue;
                        }
                        let Some(code) = button_code(*button) else {
                            continue;
                        };
                        let (col, line, _) = grid_cell(orders, cell_at(rect, ppp, m, *pos));
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
                        let (col, line, _) = grid_cell(orders, cell_at(rect, ppp, m, *pos));
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
        let orders = self.cached.as_ref().map_or(&[][..], |c| &c.orders[..]);

        let select_at = |ty: SelectionType, copy: bool| {
            let mut term = session.term.lock();
            let offset = term.grid().display_offset();
            let (col, vp_line, side) = grid_cell(orders, cell_at(rect, ppp, m, pos));
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
            let (col, vp_line, side) = grid_cell(orders, cell_at(rect, ppp, m, pos));
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
            let orders = self.cached.as_ref().map_or(&[][..], |c| &c.orders[..]);
            let (col, line, _) = grid_cell(orders, cell_at(rect, ppp, m, pos));
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
        let orders = self.cached.as_ref().map_or(&[][..], |c| &c.orders[..]);
        let (col, line, _) = grid_cell(orders, cell_at(rect, ppp, m, pos));

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

        let cwd = session.proc_cwd().unwrap_or_else(|| PathBuf::from("/"));
        let Some((target, start, len)) = session.click_target_at(line, col, &cwd) else {
            return;
        };
        self.hover_target = Some((target.clone(), line, start, len));

        ui.output_mut(|o| o.cursor_icon = CursorIcon::PointingHand);
        if response.clicked() {
            target.open();
        }
    }
}

/// The row button's rect and label, when `marks` ask for one and a label
/// fits the row's blank cells with a cell to spare each side: the first of
/// `labels` (longest first) that fits. Shared by a tab's terminal and a
/// [`crate::page::Page`], so the two draw the one button.
pub(crate) fn row_button(
    marks: &RowMarks,
    labels: &'static [&'static str],
    rect: Rect,
    ppp: f32,
    m: CellMetrics,
    cols: u16,
) -> Option<(Rect, &'static str)> {
    let (row, used) = marks.button?;
    if i32::from(row) >= rows_now(rect, ppp, m) {
        return None;
    }
    // One cell of padding inside each end; at the right, the end one
    // cell in; at the left, the label on the column asked for.
    let end = marks.button_end.map_or(cols, |e| e.min(cols));
    let (free, left) = match marks.button_left {
        Some(at) => {
            let start = at.saturating_sub(1).max(used);
            (end.saturating_sub(start), Some(start))
        }
        None => (end.saturating_sub(used), None),
    };
    let label = labels
        .iter()
        .find(|l| (l.chars().count() as u16) + 4 <= free)?;
    let width = label.chars().count() as u16 + 2;
    let start = left.unwrap_or(end - 1 - width);
    let (cw, ch) = (m.cell_w as f32 / ppp, m.cell_h as f32 / ppp);
    let r = Rect::from_min_size(
        Pos2::new(rect.min.x + start as f32 * cw, rect.min.y + row as f32 * ch),
        Vec2::new(width as f32 * cw, ch),
    );
    Some((r, *label))
}

/// Paint the row button at `r`: `fill`, a shade lighter while the pointer
/// is on it (`resp`), its label in the grid's background colour `bg`.
pub(crate) fn paint_row_button(
    painter: &egui::Painter,
    shared: &mut RenderShared,
    r: Rect,
    label: &str,
    fill: Color32,
    bg: Color32,
    resp: &Response,
) {
    let hot = resp.hovered() || resp.is_pointer_button_down_on();
    let fill = if hot { fill } else { fill.gamma_multiply(0.85) };
    let cw_pt = shared.cell_size(painter.ctx().pixels_per_point()).x;
    painter.rect_filled(r, 3.0, bg);
    painter.rect_filled(r.shrink2(Vec2::new(0.0, 1.0)), 4.0, fill);
    let text_at = Pos2::new(r.min.x + cw_pt, r.min.y);
    shared.paint_text(painter, text_at, label, bg);
}

/// Visible row count for the current geometry.
pub(crate) fn rows_now(rect: Rect, ppp: f32, m: CellMetrics) -> i32 {
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

/// A drawn cell → the grid cell it shows. On a row drawn right to left the
/// column under the pointer is not the cell's column, and the left half of
/// a right-to-left letter is the end of it, not the start.
fn grid_cell(orders: &[(u16, RowOrder)], (col, line, side): (u16, u16, Side)) -> (u16, u16, Side) {
    let Some((_, order)) = orders.iter().find(|(l, _)| *l == line) else {
        return (col, line, side);
    };
    let Some(&logical) = order.to_logical.get(col as usize) else {
        return (col, line, side);
    };
    let side = match (order.rtl.get(logical as usize), side) {
        (Some(true), Side::Left) => Side::Right,
        (Some(true), Side::Right) => Side::Left,
        _ => side,
    };
    (logical, line, side)
}

/// The drawn columns a run of grid cells covers, as its left edge and width.
/// A path in a Hebrew line is one left-to-right run, so it stays in one
/// piece on screen; it is just somewhere else.
fn drawn_span(orders: &[(u16, RowOrder)], line: u16, start: u16, len: u16) -> (u16, u16) {
    let Some((_, order)) = orders.iter().find(|(l, _)| *l == line) else {
        return (start, len);
    };
    let drawn = (start..start + len).filter_map(|c| order.to_visual.get(c as usize).copied());
    match (drawn.clone().min(), drawn.max()) {
        (Some(lo), Some(hi)) => (lo, hi - lo + 1),
        _ => (start, len),
    }
}

/// Pointer position → 0-based viewport cell + cell side.
pub(crate) fn cell_at(rect: Rect, ppp: f32, m: CellMetrics, pos: Pos2) -> (u16, u16, Side) {
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
    use super::{Buttons, still_swallowed};
    use egui::PointerButton::{Primary as Left, Secondary as Right};

    fn of(buttons: &[egui::PointerButton]) -> Buttons {
        let mut set = Buttons::default();
        for &b in buttons {
            set.insert(b);
        }
        set
    }

    #[test]
    fn a_swallowed_press_is_owed_one_release_and_no_more() {
        let none = Buttons::default();
        let left = of(&[Left]);
        // Held, reported frames: still owed until its release comes, which
        // the reporting itself takes.
        assert_eq!(still_swallowed(left, true, true, none), left);
        assert_eq!(still_swallowed(left, true, true, left), left);
        // Released on a frame that did not report (Shift held): that was it.
        assert_eq!(still_swallowed(left, false, true, left), none);
        // Not reported and not released yet: still owed.
        assert_eq!(still_swallowed(left, false, true, none), left);
        // Another button let go of on such a frame: left is still owed.
        assert_eq!(still_swallowed(left, false, true, of(&[Right])), left);
        // Mouse mode off: nothing to hold back any more.
        assert_eq!(still_swallowed(left, true, false, none), none);
        assert_eq!(still_swallowed(left, false, false, none), none);
        // Nothing swallowed stays nothing.
        assert_eq!(still_swallowed(none, false, true, left), none);
    }

    /// Left held on the bar, then a right click on the grid: the right
    /// release is reported, and the left one still is not.
    #[test]
    fn a_swallowed_press_owes_only_its_own_button() {
        let mut swallowed = Buttons::default();
        assert!(swallowed.swallows(Left, true, true));
        assert!(!swallowed.swallows(Right, true, false));
        assert!(
            !swallowed.swallows(Right, false, false),
            "right is reported"
        );
        assert!(swallowed.swallows(Left, false, false), "left is not");
        assert_eq!(swallowed, Buttons::default());
        // A release with nothing swallowed is reported.
        assert!(!swallowed.swallows(Left, false, true));
    }
}

#[cfg(test)]
mod rtl_pointer_tests {
    use super::{Side, drawn_span, grid_cell};

    #[test]
    fn a_click_on_a_hebrew_row_lands_on_the_letter_drawn_there() {
        use crate::render::bidi::{Dir, order_row};
        // "ab שלום" padded to 10, drawn right to left: the word at the right
        // edge, read leftwards, and "ab" to its left.
        let row: Vec<char> = "ab שלום   ".chars().collect();
        let orders = vec![(3u16, order_row(&row, Dir::Ltr))];
        let drawn_at = |logical: usize| orders[0].1.to_visual[logical];
        // Left to right: "ab " then the word turned round.
        assert_eq!(drawn_at(0), 0);
        assert_eq!(drawn_at(3), 6, "ש is drawn last of the word");
        let (col, line, side) = grid_cell(&orders, (6, 3, Side::Left));
        assert_eq!((col, line), (3, 3));
        assert_eq!(
            side,
            Side::Right,
            "the left half of an RTL letter is its end"
        );
        // Rows with no order are left alone.
        assert_eq!(grid_cell(&orders, (6, 4, Side::Left)), (6, 4, Side::Left));
        // A run of cells reads back as the columns it is drawn over.
        assert_eq!(drawn_span(&orders, 3, 3, 4), (3, 4));
        assert_eq!(drawn_span(&orders, 3, 0, 2), (0, 2));
    }
}
