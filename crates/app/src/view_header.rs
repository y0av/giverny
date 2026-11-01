//! The header over a worker's view.
//!
//! While a tab shows a worker — Claude Code's own view of a running one, or
//! Giverny's page of one whose turn has ended, a Done one or a Next up one
//! (`worker_page`) — a slim band at the top of the terminal says what that
//! worker is on: its task's name, then the facts of its management-panel
//! row (state, time against the estimate, what it is doing or why it waits,
//! tokens, what it holds and uses), and a Done row's Review line. The facts
//! are the row's, as the pane drew it this frame, so both change together,
//! and a running worker and a finished one read the same. It is drawn over
//! the terminal rather than beside it, so it never resizes the grid (a
//! resize would have Claude Code repaint under it). A small `×` closes it
//! for that view; the keyboard stays with the terminal throughout.

use egui::text::{LayoutJob, TextFormat};
use egui::{FontId, Rect, Sense, Ui};

use giverny_core::tabs::TabId;

use crate::chrome::Chrome;
use crate::management_panel::{Line, RowClick};

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
    /// The task's name alone: `g477`, never the
    /// description after it.
    pub name: String,
    pub facts: String,
    /// A Done row's Review line.
    pub review: Option<String>,
}

impl Head {
    /// The header of a management-panel row.
    pub fn of(line: &Line) -> Head {
        let facts = if line.click.facts.is_empty() {
            facts(line)
        } else {
            line.click.facts.join("  ·  ")
        };
        Head {
            name: task_name(line),
            facts,
            review: crate::overlays::review_line(line.stage, line.click.review.as_deref()),
        }
    }

    /// A header naming `title` alone: a page the pane has no row for.
    pub fn titled(title: &str) -> Head {
        Head {
            name: title.to_string(),
            facts: String::new(),
            review: None,
        }
    }
}

/// A row's task name: the feed's key (`g477`); for a worker no feed row
/// names, its description up to its first `:` (`g477: popup looks like
/// worker screen` is `g477`), else the whole description.
pub fn task_name(line: &Line) -> String {
    name_of(&line.click, &line.title, &line.id)
}

/// [`task_name`] of a row's click: `title` stands in for a click with no
/// name, `fallback` for a name that comes to nothing.
pub fn name_of(click: &RowClick, title: &str, fallback: &str) -> String {
    if !click.key.is_empty() {
        return click.key.clone();
    }
    let described = if click.name.trim().is_empty() {
        title.trim()
    } else {
        click.name.trim()
    };
    let name = described
        .split_once(':')
        .map_or(described, |(name, _)| name.trim());
    if name.is_empty() {
        fallback.to_string()
    } else {
        name.to_string()
    }
}

/// The facts line of a row with none of its own: its cells, labelled, the
/// empty ones left out.
pub fn facts(line: &Line) -> String {
    let mut out: Vec<String> = Vec::new();
    if !line.elapsed.is_empty() {
        out.push(format!("{} elapsed", line.elapsed));
    }
    if !line.eta.is_empty() {
        out.push(format!("ETA {}", line.eta));
    } else if line.no_eta {
        out.push(crate::management_panel::NO_ETA.to_string());
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

/// Draw the band over the top of `term` (the terminal's rect).
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
    if let Some(review) = &head.review {
        texts.push((
            line_job(&[
                ("Review  ", FACTS_SIZE, chrome.amber),
                (review, FACTS_SIZE, chrome.fg),
            ]),
            chrome.fg,
            "review",
        ));
    }
    let text_h: f32 = texts.iter().map(|(g, ..)| g.size().y).sum::<f32>()
        + GAP_Y * (texts.len().saturating_sub(1)) as f32;
    let height = (PAD_Y * 2.0 + text_h).min(term.height() * 0.5);
    let band = Rect::from_min_size(term.min, egui::vec2(term.width(), height));

    // Registered after the terminal, so a click on the band is the band's
    // and never reaches the program under it.
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
