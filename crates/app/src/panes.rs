//! A split row on screen: its terminals laid out by the group's tree, a
//! header over each, and the dividers between them.
//!
//! The geometry lives in `giverny_core::panes`; this is the egui side of it.

use eframe::egui::{self, Align2, Color32, CursorIcon, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use giverny_core::panes::{Area, Axis, Direction};
use giverny_core::tabs::TabId;

use crate::{Action, App};

/// Space between two terminals, which is also the divider you drag.
pub const GAP: f32 = 3.0;
/// A split terminal's header: its state, its title, its buttons.
pub const HEADER_H: f32 = 22.0;
/// How far either side of a divider still grabs it.
const GRAB: f32 = 3.0;

/// One terminal's place on screen.
pub struct Pane {
    pub tab: TabId,
    /// Over the terminal, when it shares the row with others.
    pub header: Option<Rect>,
    pub body: Rect,
}

fn to_area(r: Rect) -> Area {
    Area::new(r.min.x, r.min.y, r.width(), r.height())
}

fn to_rect(a: Area) -> Rect {
    Rect::from_min_size(Pos2::new(a.x, a.y), Vec2::new(a.w, a.h))
}

/// Where each terminal on screen goes inside `rect`.
pub fn layout(app: &App, rect: Rect) -> Vec<Pane> {
    let Some(active) = app.ws.active else {
        return Vec::new();
    };
    let Some(group) = app.ws.group_of(active) else {
        return vec![Pane {
            tab: active,
            header: None,
            body: rect,
        }];
    };
    let areas = if group.zoomed {
        vec![(active, to_area(rect))]
    } else {
        group.tree.layout(to_area(rect), GAP)
    };
    areas
        .into_iter()
        .map(|(tab, a)| {
            let r = to_rect(a);
            let (header, body) = r.split_top_bottom_at_y(r.min.y + HEADER_H);
            Pane {
                tab,
                header: Some(header),
                body,
            }
        })
        .collect()
}

/// Paint and handle the dividers of the active row inside `rect`: drag to
/// resize, double-click to share the space evenly again.
pub fn dividers(app: &mut App, ui: &mut egui::Ui, rect: Rect) {
    let Some(active) = app.ws.active else { return };
    let Some(group) = app.ws.group_of(active) else {
        return;
    };
    if group.zoomed {
        return;
    }
    let divs = group.tree.dividers(to_area(rect), GAP);
    let gid = group.id;
    for d in divs {
        let area = to_rect(d.area);
        let grab = match d.axis {
            Axis::Row => area.expand2(Vec2::new(GRAB, 0.0)),
            Axis::Column => area.expand2(Vec2::new(0.0, GRAB)),
        };
        let id = ui.id().with(("pane-divider", gid.0, &d.path));
        let resp = ui.interact(grab, id, Sense::click_and_drag());
        let icon = match d.axis {
            Axis::Row => CursorIcon::ResizeHorizontal,
            Axis::Column => CursorIcon::ResizeVertical,
        };
        let live = resp.hovered() || resp.dragged();
        if live {
            ui.output_mut(|o| o.cursor_icon = icon);
        }
        let color = if live {
            app.chrome.accent
        } else {
            app.chrome.dim.gamma_multiply(0.35)
        };
        ui.painter().rect_filled(area, 0.0, color);
        if resp.double_clicked() {
            if let Some(g) = app.ws.group_mut(gid) {
                g.tree.equalize();
            }
            app.state_dirty = true;
        } else if resp.dragged()
            && let Some(p) = ui.input(|i| i.pointer.interact_pos())
        {
            let ratio = match d.axis {
                Axis::Row => (p.x - d.parent.x - GAP / 2.0) / (d.parent.w - GAP).max(1.0),
                Axis::Column => (p.y - d.parent.y - GAP / 2.0) / (d.parent.h - GAP).max(1.0),
            };
            if let Some(g) = app.ws.group_mut(gid) {
                g.tree.set_ratio(&d.path, ratio);
            }
            app.state_dirty = true;
        }
    }
}

/// A split terminal's header: its mark, its title, and close. The focused
/// one carries the category's colour along its bottom, where a single
/// terminal has its strip; the others are quieter.
pub fn header(
    app: &App,
    ui: &mut egui::Ui,
    rect: Rect,
    tab: TabId,
    focused: bool,
    actions: &mut Vec<Action>,
) {
    let Some(t) = app.ws.tab(tab) else { return };
    let c = app.chrome;
    let accent = app
        .ws
        .category(t.category)
        .map(|cat| c.category(cat.color_index))
        .unwrap_or(c.dim);
    let zoomed = app.ws.group_of(tab).is_some_and(|g| g.zoomed);
    let count = app.ws.row_tabs(tab).len();

    let resp = ui.interact(
        rect,
        ui.id().with(("pane-header", tab.0)),
        Sense::click_and_drag(),
    );
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 0.0, c.panel);
    let line = Rect::from_min_max(Pos2::new(rect.min.x, rect.max.y - 2.0), rect.max);
    if focused {
        p.rect_filled(line, 0.0, accent);
    } else {
        p.rect_filled(line, 0.0, accent.gamma_multiply(0.25));
    }

    let dot = Pos2::new(rect.min.x + 12.0, rect.center().y - 1.0);
    crate::rail::pane_mark(app, ui, &p, rect, dot, tab);

    let mut title = t.display_title(&app.cfg.titles);
    if zoomed {
        title = format!("{title}   ·   zoomed, {count} terminals (Ctrl+Shift+Z)");
    }
    let fg = if focused {
        c.fg
    } else {
        c.fg.gamma_multiply(0.6)
    };
    let budget = ((rect.width() - 60.0) / 7.0).max(4.0) as usize;
    p.text(
        Pos2::new(rect.min.x + 24.0, rect.center().y - 1.0),
        Align2::LEFT_CENTER,
        crate::rail::fit(&title, budget),
        FontId::monospace(11.5),
        fg,
    );

    // Close, at the right; always there, as in the rail on hover.
    let close_rect = Rect::from_center_size(
        Pos2::new(rect.max.x - 12.0, rect.center().y - 1.0),
        Vec2::splat(16.0),
    );
    let close = ui.interact(
        close_rect,
        ui.id().with(("pane-close", tab.0)),
        Sense::click(),
    );
    p.text(
        close_rect.center(),
        Align2::CENTER_CENTER,
        "×",
        FontId::monospace(12.5),
        if close.hovered() {
            Color32::from_rgb(0xd9, 0x7f, 0x70)
        } else {
            c.dim
        },
    );
    if close.clicked() {
        actions.push(Action::CloseTab(tab));
        return;
    }

    if resp.clicked() && !focused {
        actions.push(Action::Select(tab));
    }
    // Grab a header to move its terminal: onto another one's edge to sit
    // there, onto its middle to trade places, onto the rail to stand alone.
    if resp.drag_started() {
        app_drag(ui, tab);
    }
    if resp.hovered() && !resp.dragged() {
        ui.output_mut(|o| o.cursor_icon = CursorIcon::Grab);
    }
    if resp.double_clicked() {
        actions.push(Action::ToggleZoom(tab));
    }
    resp.context_menu(|ui| menu(ui, tab, zoomed, actions));
}

/// Start dragging one terminal; picked up by the app after the frame
/// ([`take_drag`]), since a header only borrows the app.
fn app_drag(ui: &egui::Ui, tab: TabId) {
    ui.ctx()
        .data_mut(|d| d.insert_temp(egui::Id::new("pane-drag-start"), Some(tab.0)));
}

/// A header drag that started this frame, if one did.
pub fn take_drag(ctx: &egui::Context) -> Option<TabId> {
    ctx.data_mut(|d| d.remove_temp::<Option<u64>>(egui::Id::new("pane-drag-start")))
        .flatten()
        .map(TabId)
}

/// What a terminal in a split can do, from its header's right-click menu.
fn menu(ui: &mut egui::Ui, tab: TabId, zoomed: bool, actions: &mut Vec<Action>) {
    let mut item = |ui: &mut egui::Ui, label: &str, keys: &str, action: Action| {
        if ui
            .add(egui::Button::new(label).shortcut_text(keys))
            .clicked()
        {
            actions.push(action);
            ui.close();
        }
    };
    item(
        ui,
        "split right",
        "Ctrl+Shift+D",
        Action::Split {
            from: tab,
            axis: Axis::Row,
            before: false,
        },
    );
    item(
        ui,
        "split down",
        "Ctrl+Shift+S",
        Action::Split {
            from: tab,
            axis: Axis::Column,
            before: false,
        },
    );
    item(
        ui,
        if zoomed { "unzoom" } else { "zoom" },
        "Ctrl+Shift+Z",
        Action::ToggleZoom(tab),
    );
    item(ui, "move to its own row", "", Action::Detach(tab));
    ui.separator();
    item(ui, "close", "Ctrl+Shift+W", Action::CloseTab(tab));
}

/// Where a drop on a terminal lands.
pub enum Zone {
    /// Beside it, in the half nearest the pointer.
    Edge {
        axis: Axis,
        before: bool,
        half: Rect,
    },
    /// In its place, the two trading: only for a terminal of the same split.
    Center(Rect),
}

/// Where a drop at `pointer` on terminal `body` would land. With `swap`,
/// the middle third trades places instead of splitting.
pub fn drop_zone(body: Rect, pointer: Pos2, swap: bool) -> Zone {
    let rel = (pointer - body.min) / body.size();
    if swap && (1.0 / 3.0..2.0 / 3.0).contains(&rel.x) && (1.0 / 3.0..2.0 / 3.0).contains(&rel.y) {
        return Zone::Center(body.shrink2(body.size() / 6.0));
    }
    // Distance to each edge, as a share of the terminal: the nearest wins.
    let edges = [
        (rel.x, Axis::Row, true),
        (1.0 - rel.x, Axis::Row, false),
        (rel.y, Axis::Column, true),
        (1.0 - rel.y, Axis::Column, false),
    ];
    let (_, axis, before) = edges
        .into_iter()
        .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
        .unwrap();
    let half = match (axis, before) {
        (Axis::Row, true) => body.split_left_right_at_fraction(0.5).0,
        (Axis::Row, false) => body.split_left_right_at_fraction(0.5).1,
        (Axis::Column, true) => body.split_top_bottom_at_fraction(0.5).0,
        (Axis::Column, false) => body.split_top_bottom_at_fraction(0.5).1,
    };
    Zone::Edge { axis, before, half }
}

/// Paint the half a drop would land in.
pub fn paint_drop(ui: &egui::Ui, half: Rect, accent: Color32) {
    let p = ui.ctx().layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("pane-drop"),
    ));
    p.rect_filled(half.shrink(2.0), 4.0, accent.gamma_multiply(0.18));
    p.rect_stroke(
        half.shrink(2.0),
        4.0,
        Stroke::new(1.5, accent),
        egui::StrokeKind::Inside,
    );
}

/// The terminal next to the active one in `dir`, as last laid out.
pub fn neighbour(app: &App, dir: Direction) -> Option<TabId> {
    giverny_core::panes::neighbour(&app.pane_areas, app.ws.active?, dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_drop_lands_in_the_half_nearest_the_pointer() {
        let body = Rect::from_min_size(Pos2::ZERO, Vec2::new(200.0, 100.0));
        let Zone::Edge { axis, before, half } = drop_zone(body, Pos2::new(190.0, 50.0), true)
        else {
            panic!()
        };
        assert_eq!((axis, before), (Axis::Row, false));
        assert_eq!(half.min.x, 100.0);
        let Zone::Edge { axis, before, .. } = drop_zone(body, Pos2::new(100.0, 5.0), true) else {
            panic!()
        };
        assert_eq!((axis, before), (Axis::Column, true));
        assert!(matches!(
            drop_zone(body, Pos2::new(100.0, 50.0), true),
            Zone::Center(_)
        ));
        assert!(
            matches!(
                drop_zone(body, Pos2::new(100.0, 50.0), false),
                Zone::Edge { .. }
            ),
            "a row from the rail only ever splits"
        );
    }
}
