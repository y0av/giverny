//! The left rail: categories and tabs.

use eframe::egui::{
    self, Align2, Color32, CursorIcon, FontId, Pos2, Rect, Sense, Stroke, TextEdit, Ui, Vec2,
};
use giverny_core::tabs::{CategoryId, TabId};

use crate::{Action, App, RenameTarget, category_color};

const ROW_H: f32 = 40.0;
const HEADER_H: f32 = 26.0;

struct RowData {
    id: TabId,
    title: String,
    sub: String,
    active: bool,
    exited: bool,
    color: Color32,
}

struct CatData {
    id: CategoryId,
    name: String,
    color: Color32,
    collapsed: bool,
    count: usize,
    rows: Vec<RowData>,
}

pub fn show(app: &mut App, ui: &mut Ui) -> Vec<Action> {
    let mut actions = Vec::new();

    // Precollect display data so rendering never borrows the workspace.
    let cats: Vec<CatData> = app
        .ws
        .categories
        .iter()
        .map(|c| {
            let color = category_color(c.color_index);
            let rows = app
                .ws
                .tabs_in(c.id)
                .map(|t| {
                    let mut sub = t
                        .cwd
                        .as_deref()
                        .map(|p| giverny_core::short_path(p, 24))
                        .unwrap_or_default();
                    if let Some(branch) = &t.git_branch {
                        sub = if sub.is_empty() {
                            format!(" {branch}")
                        } else {
                            format!("{sub} ·  {branch}")
                        };
                    }
                    RowData {
                        id: t.id,
                        title: t.title().to_string(),
                        sub,
                        active: app.ws.active == Some(t.id),
                        exited: t.exited,
                        color,
                    }
                })
                .collect::<Vec<_>>();
            CatData {
                id: c.id,
                name: c.name.clone(),
                color,
                collapsed: c.collapsed,
                count: rows.len(),
                rows,
            }
        })
        .collect();

    let dim = Color32::from_rgb(0x6b, 0x78, 0x80);
    let fg = Color32::from_rgb(0xd7, 0xdd, 0xe2);

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.add_space(6.0);
        for cat in &cats {
            category_header(app, ui, cat, dim, &mut actions);
            if !cat.collapsed {
                for row in &cat.rows {
                    tab_row(app, ui, row, fg, dim, &mut actions);
                }
            }
            ui.add_space(6.0);
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            if ui
                .small_button("+ category")
                .on_hover_text("add a category")
                .clicked()
            {
                actions.push(Action::NewCategory);
            }
        });
        ui.add_space(8.0);
    });

    actions
}

fn category_header(
    app: &mut App,
    ui: &mut Ui,
    cat: &CatData,
    dim: Color32,
    actions: &mut Vec<Action>,
) {
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, HEADER_H), Sense::click());
    let p = ui.painter_at(rect);

    // Inline rename?
    if let Some((RenameTarget::Category(id), buf)) = &mut app.rename
        && *id == cat.id
    {
        let edit_rect = Rect::from_min_size(
            Pos2::new(rect.min.x + 22.0, rect.min.y + 2.0),
            Vec2::new(width - 30.0, HEADER_H - 4.0),
        );
        let te = ui.put(edit_rect, TextEdit::singleline(buf).font(FontId::monospace(12.0)));
        if app.rename_needs_focus {
            te.request_focus();
            app.rename_needs_focus = false;
        }
        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
        let escape = ui.input(|i| i.key_pressed(egui::Key::Escape));
        if escape {
            actions.push(Action::CommitRename(RenameTarget::Category(cat.id), None));
        } else if enter || te.lost_focus() {
            let value = buf.clone();
            actions.push(Action::CommitRename(RenameTarget::Category(cat.id), Some(value)));
        }
        return;
    }

    // Right-click: category management.
    resp.context_menu(|ui| {
        if ui.button("rename").clicked() {
            actions.push(Action::StartRename(RenameTarget::Category(cat.id)));
            ui.close();
        }
        if ui.button("new tab here").clicked() {
            actions.push(Action::NewTab { category: cat.id, cwd: None });
            ui.close();
        }
        ui.menu_button("color", |ui| {
            ui.horizontal(|ui| {
                for (i, c) in crate::CATEGORY_PALETTE.iter().enumerate() {
                    let btn = egui::Button::new("  ").fill(*c).corner_radius(3.0);
                    if ui.add_sized(Vec2::splat(18.0), btn).clicked() {
                        actions.push(Action::SetCategoryColor(cat.id, i));
                        ui.close();
                    }
                }
            });
        });
        ui.separator();
        if ui.button("delete category").clicked() {
            actions.push(Action::DeleteCategory(cat.id));
            ui.close();
        }
    });

    let tri = if cat.collapsed { "▸" } else { "▾" };
    p.text(
        Pos2::new(rect.min.x + 8.0, rect.center().y),
        Align2::LEFT_CENTER,
        tri,
        FontId::monospace(10.0),
        dim,
    );
    p.circle_filled(Pos2::new(rect.min.x + 24.0, rect.center().y), 4.0, cat.color);
    p.text(
        Pos2::new(rect.min.x + 34.0, rect.center().y),
        Align2::LEFT_CENTER,
        cat.name.to_uppercase(),
        FontId::monospace(11.5),
        cat.color,
    );
    p.text(
        Pos2::new(rect.max.x - 26.0, rect.center().y),
        Align2::RIGHT_CENTER,
        format!("{}", cat.count),
        FontId::monospace(10.0),
        dim,
    );

    // "+" new-tab zone at the right edge.
    let plus_rect =
        Rect::from_min_size(Pos2::new(rect.max.x - 22.0, rect.min.y + 3.0), Vec2::splat(20.0));
    let plus = ui.interact(plus_rect, ui.id().with(("cat-plus", cat.id.0)), Sense::click());
    p.text(
        plus_rect.center(),
        Align2::CENTER_CENTER,
        "+",
        FontId::monospace(13.0),
        if plus.hovered() { cat.color } else { dim },
    );
    if plus.clicked() {
        actions.push(Action::NewTab { category: cat.id, cwd: None });
    } else if resp.clicked() {
        actions.push(Action::ToggleCollapse(cat.id));
    }
    if resp.hovered() {
        ui.output_mut(|o| o.cursor_icon = CursorIcon::PointingHand);
    }
}

fn tab_row(
    app: &mut App,
    ui: &mut Ui,
    row: &RowData,
    fg: Color32,
    dim: Color32,
    actions: &mut Vec<Action>,
) {
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, ROW_H), Sense::click());
    let p = ui.painter_at(rect);
    // Geometry-based hover: `resp.hovered()` flickers when the close button
    // overlays the row in the hit-test stack.
    let hovered = ui.rect_contains_pointer(rect);

    if row.active {
        p.rect_filled(rect.shrink2(Vec2::new(4.0, 1.0)), 4.0, row.color.gamma_multiply(0.16));
        p.rect_filled(
            Rect::from_min_size(rect.min + Vec2::new(4.0, 1.0), Vec2::new(3.0, ROW_H - 2.0)),
            2.0,
            row.color,
        );
    } else if hovered {
        p.rect_filled(
            rect.shrink2(Vec2::new(4.0, 1.0)),
            4.0,
            Color32::from_rgba_unmultiplied(255, 255, 255, 8),
        );
    }

    // Status dot.
    let dot = Pos2::new(rect.min.x + 18.0, rect.min.y + 13.0);
    if row.exited {
        p.circle_stroke(dot, 3.5, Stroke::new(1.2, dim));
    } else {
        p.circle_filled(dot, 3.5, Color32::from_rgb(0x7b, 0xa2, 0x5a));
    }

    // Inline rename?
    if let Some((RenameTarget::Tab(id), buf)) = &mut app.rename
        && *id == row.id
    {
        let edit_rect = Rect::from_min_size(
            Pos2::new(rect.min.x + 28.0, rect.min.y + 3.0),
            Vec2::new(width - 40.0, 20.0),
        );
        let te = ui.put(edit_rect, TextEdit::singleline(buf).font(FontId::monospace(12.0)));
        if app.rename_needs_focus {
            te.request_focus();
            app.rename_needs_focus = false;
        }
        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
        let escape = ui.input(|i| i.key_pressed(egui::Key::Escape));
        if escape {
            actions.push(Action::CommitRename(RenameTarget::Tab(row.id), None));
        } else if enter || te.lost_focus() {
            let value = buf.clone();
            actions.push(Action::CommitRename(RenameTarget::Tab(row.id), Some(value)));
        }
        return;
    }

    // Title (char-budget truncation; the rail is monospace).
    let char_budget = ((width - 52.0) / 7.2).max(4.0) as usize;
    p.text(
        Pos2::new(rect.min.x + 28.0, rect.min.y + 13.0),
        Align2::LEFT_CENTER,
        truncate_chars(&row.title, char_budget),
        FontId::monospace(12.5),
        if row.active { fg } else { fg.gamma_multiply(0.8) },
    );
    if !row.sub.is_empty() {
        p.text(
            Pos2::new(rect.min.x + 28.0, rect.min.y + 29.0),
            Align2::LEFT_CENTER,
            truncate_chars(&row.sub, char_budget + 2),
            FontId::monospace(10.0),
            dim,
        );
    }

    // Close button on hover.
    if hovered {
        let close_rect =
            Rect::from_min_size(Pos2::new(rect.max.x - 24.0, rect.min.y + 4.0), Vec2::splat(18.0));
        let close = ui.interact(close_rect, ui.id().with(("tab-close", row.id.0)), Sense::click());
        p.text(
            close_rect.center(),
            Align2::CENTER_CENTER,
            "×",
            FontId::monospace(13.0),
            if close.hovered() { Color32::from_rgb(0xd9, 0x7f, 0x70) } else { dim },
        );
        if close.clicked() {
            actions.push(Action::CloseTab(row.id));
            return;
        }
    }

    // Right-click: tab management.
    resp.context_menu(|ui| {
        if ui.button("rename").clicked() {
            actions.push(Action::StartRename(RenameTarget::Tab(row.id)));
            ui.close();
        }
        ui.menu_button("move to", |ui| {
            for (id, name) in &app.ws.categories.iter().map(|c| (c.id, c.name.clone())).collect::<Vec<_>>() {
                if ui.button(name).clicked() {
                    actions.push(Action::MoveTab(row.id, *id));
                    ui.close();
                }
            }
        });
        ui.separator();
        if ui.button("close").clicked() {
            actions.push(Action::CloseTab(row.id));
            ui.close();
        }
    });

    if resp.double_clicked() {
        actions.push(Action::StartRename(RenameTarget::Tab(row.id)));
    } else if resp.clicked() {
        actions.push(Action::Select(row.id));
    } else if resp.middle_clicked() {
        actions.push(Action::CloseTab(row.id));
    }
    if hovered {
        ui.output_mut(|o| o.cursor_icon = CursorIcon::PointingHand);
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}
