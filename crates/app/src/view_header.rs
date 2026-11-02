//! The header over a worker page.
//!
//! While a tab shows a worker's page (`worker_page`), a slim band at the
//! top of it says what that worker is on: its name, then the facts of its
//! management-panel row (time so far, what it is doing or how it ended,
//! tokens). The facts are the row's, as the pane drew it this frame, so both
//! change together, and a running worker and a finished one read the same.
//! It is drawn over the page rather than beside it, so it never resizes the
//! grid. A small `×` closes it for that page.

use egui::text::{LayoutJob, TextFormat};
use egui::{FontId, Rect, Sense, Ui};

use giverny_core::tabs::TabId;

use crate::chrome::Chrome;
use crate::management_panel::Line;

/// Room kept free at the band's right end for its `×`.
const CLOSE_W: f32 = 22.0;
const PAD_X: f32 = 10.0;
const PAD_Y: f32 = 5.0;
const GAP_Y: f32 = 2.0;
const TITLE_SIZE: f32 = 14.0;
const FACTS_SIZE: f32 = 12.0;

/// What the header says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    /// The worker's name ([`name_of`]).
    pub name: String,
    pub facts: String,
}

impl Head {
    /// The header of a management-panel row.
    pub fn of(line: &Line) -> Head {
        Head {
            name: name_of(&line.click.name, &line.id),
            facts: facts(line),
        }
    }

    /// A header naming `name` alone: a page the pane has no row for.
    pub fn titled(name: &str) -> Head {
        Head {
            name: name.to_string(),
            facts: String::new(),
        }
    }
}

/// A worker's name for its header and prompt box: its name or description
/// up to the first `:` (`g477: popup looks like worker screen` is `g477`),
/// else the whole of it; `fallback` when that comes to nothing.
pub fn name_of(described: &str, fallback: &str) -> String {
    let described = described.trim();
    let name = described
        .split_once(':')
        .map_or(described, |(name, _)| name.trim());
    if name.is_empty() {
        fallback.to_string()
    } else {
        name.to_string()
    }
}

/// The facts line of a row: its cells, labelled, the empty ones left out.
pub fn facts(line: &Line) -> String {
    let mut out: Vec<String> = Vec::new();
    if !line.elapsed.is_empty() {
        out.push(format!("{} elapsed", line.elapsed));
    }
    if !line.now.is_empty() {
        out.push(line.now.clone());
    }
    if !line.tokens.is_empty() {
        out.push(format!("{} tokens", line.tokens));
    }
    out.join("  ·  ")
}

/// What a frame of the header did.
pub struct Shown {
    /// Its `×` was clicked.
    pub closed: bool,
    /// The band's height, in points.
    pub height: f32,
}

/// Draw the band over the top of `term` (the page's rect).
pub fn show(ui: &mut Ui, term: Rect, head: &Head, chrome: &Chrome, tab: TabId) -> Shown {
    let wrap = (term.width() - 2.0 * PAD_X - CLOSE_W).max(40.0);
    let line_job = |parts: &[(&str, f32, egui::Color32)]| {
        let mut job = LayoutJob::default();
        job.wrap.max_width = wrap;
        for (text, size, color) in parts {
            job.append(
                text,
                0.0,
                TextFormat::simple(FontId::proportional(*size), *color),
            );
        }
        ui.ctx().fonts_mut(|f| f.layout_job(job))
    };
    let mut texts = vec![(
        line_job(&[(&head.name, TITLE_SIZE, chrome.accent)]),
        chrome.accent,
        "title",
    )];
    if !head.facts.is_empty() {
        texts.push((
            line_job(&[(&head.facts, FACTS_SIZE, chrome.dim)]),
            chrome.dim,
            "facts",
        ));
    }
    let text_h: f32 = texts.iter().map(|(g, ..)| g.size().y).sum::<f32>()
        + GAP_Y * (texts.len().saturating_sub(1)) as f32;
    let height = (PAD_Y * 2.0 + text_h).min(term.height() * 0.5);
    let band = Rect::from_min_size(term.min, egui::vec2(term.width(), height));

    // Registered after the page, so a click on the band is the band's and
    // never reaches the page under it.
    let id = egui::Id::new(("view_header", tab));
    ui.interact(band, id, Sense::click());
    let close = Rect::from_min_size(
        egui::pos2(band.max.x - CLOSE_W - 4.0, band.min.y + 3.0),
        egui::vec2(CLOSE_W, CLOSE_W - 4.0),
    );
    let close_resp = ui
        .interact(close, id.with("close"), Sense::click())
        .on_hover_text("Hide for this view")
        .on_hover_cursor(egui::CursorIcon::PointingHand);

    let painter = ui.painter_at(band);
    painter.rect_filled(band, 0.0, chrome.panel);
    painter.hline(
        band.x_range(),
        band.max.y - 0.5,
        egui::Stroke::new(1.0, chrome.dim.gamma_multiply(0.4)),
    );
    // The text is selectable, as a label's is, and copied as a drag over
    // it ends; registered after the band, so a drag is the
    // text's.
    crate::overlays::copy_on_release(ui.ctx(), id, band);
    // Clipped to the band, as its painter is.
    let mut text_ui = ui.new_child(egui::UiBuilder::new().max_rect(band));
    text_ui.set_clip_rect(band);
    let mut at = egui::pos2(band.min.x + PAD_X, band.min.y + PAD_Y);
    for (galley, color, name) in texts {
        let h = galley.size().y;
        let rect = Rect::from_min_size(at, galley.size()).intersect(band);
        let resp = text_ui
            .interact(rect, id.with(name), Sense::click_and_drag())
            .on_hover_cursor(egui::CursorIcon::Text);
        egui::text_selection::LabelSelectionState::label_text_selection(
            &text_ui,
            &resp,
            at,
            galley,
            color,
            egui::Stroke::NONE,
        );
        at.y += h + GAP_Y;
    }
    if close_resp.hovered() {
        painter.rect_filled(close, 3.0, chrome.dim.gamma_multiply(0.25));
    }
    painter.text(
        close.center(),
        egui::Align2::CENTER_CENTER,
        "×",
        FontId::proportional(15.0),
        if close_resp.hovered() {
            chrome.fg
        } else {
            chrome.dim
        },
    );
    Shown {
        closed: close_resp.clicked(),
        height,
    }
}
