//! The left rail: categories and tabs.

use eframe::egui::{
    self, Align2, Color32, CursorIcon, FontId, Pos2, Rect, Sense, Stroke, TextEdit, Ui, Vec2,
};
use giverny_core::tabs::{CategoryId, TabId};

use crate::claude_watch::{ClaudeState, ClaudeWatch, Freshness};
use crate::{Action, App, RenameTarget, category_color};

const ROW_H: f32 = 40.0;
const HEADER_H: f32 = 26.0;
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
// Chrome colours come from the active theme (see `chrome`), reached through
// `app.chrome`. Only geometry is constant here.

struct RowData {
    id: TabId,
    title: String,
    sub: String,
    active: bool,
    exited: bool,
    color: Color32,
    claude: ClaudeState,
}

struct CatData {
    id: CategoryId,
    name: String,
    color: Color32,
    collapsed: bool,
    count: usize,
    busy: usize,
    needs: usize,
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
                    let ct = app.claude.tabs.get(&t.id);
                    if let Some(account) = ct.and_then(|c| c.account.as_deref()) {
                        sub = if sub.is_empty() {
                            format!("@{account}")
                        } else {
                            format!("{sub} · @{account}")
                        };
                    }
                    RowData {
                        id: t.id,
                        title: t.display_title(&app.cfg.titles),
                        sub,
                        active: app.ws.active == Some(t.id),
                        exited: t.exited,
                        color,
                        claude: ct.map(|c| c.state).unwrap_or_default(),
                    }
                })
                .collect::<Vec<_>>();
            let busy = rows
                .iter()
                .filter(|r| r.claude == ClaudeState::Busy)
                .count();
            let needs = rows
                .iter()
                .filter(|r| r.claude == ClaudeState::NeedsYou)
                .count();
            CatData {
                id: c.id,
                name: c.name.clone(),
                color,
                collapsed: c.collapsed,
                count: rows.len(),
                busy,
                needs,
                rows,
            }
        })
        .collect();

    let dim = app.chrome.dim;
    let fg = app.chrome.fg;

    // Bottom section first (panel-inside-panel): hooks banner + usage meters.
    egui::Panel::bottom("rail-bottom")
        .resizable(false)
        .show_separator_line(true)
        .show(ui, |ui| {
            update_banner(app, ui, &mut actions);
            hooks_banner(app, ui, &mut actions);
            usage_panel(app, ui, dim, fg, &mut actions);
        });

    // Drag-and-drop bookkeeping: where would a drop land right now?
    let pointer = ui.input(|i| i.pointer.interact_pos());
    let mut drop_target: Option<(CategoryId, usize, f32)> = None;

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.add_space(6.0);
            for cat in &cats {
                let header = category_header(app, ui, cat, dim, &mut actions);
                // Dropping on a header (or an empty category's band) appends
                // to that category.
                if let Some(p) = pointer
                    && app.dragging.is_some()
                    && header.contains(p)
                {
                    drop_target = Some((cat.id, cat.rows.len(), header.max.y));
                }
                if !cat.collapsed {
                    for (index, row) in cat.rows.iter().enumerate() {
                        let rect = tab_row(app, ui, row, fg, dim, &mut actions);
                        // Above a row's midpoint drops before it, below after.
                        if let Some(p) = pointer
                            && app.dragging.is_some()
                            && p.x >= rect.min.x
                            && p.x <= rect.max.x
                            && p.y >= rect.min.y - 2.0
                            && p.y <= rect.max.y + 2.0
                        {
                            let after = p.y > rect.center().y;
                            let y = if after { rect.max.y } else { rect.min.y };
                            drop_target = Some((cat.id, index + usize::from(after), y));
                        }
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

    // Drop indicator + commit on release.
    if let Some(dragged) = app.dragging {
        if let Some((_, _, y)) = drop_target {
            let x0 = ui.min_rect().min.x + 6.0;
            let x1 = ui.min_rect().max.x - 6.0;
            ui.painter()
                .hline(x0..=x1, y, egui::Stroke::new(2.0, app.chrome.accent));
        }
        ui.output_mut(|o| o.cursor_icon = CursorIcon::Grabbing);
        if ui.input(|i| i.pointer.any_released()) {
            if let Some((cat, index, _)) = drop_target {
                actions.push(Action::ReorderTab(dragged, cat, index));
            }
            app.dragging = None;
        }
    }

    actions
}

fn category_header(
    app: &mut App,
    ui: &mut Ui,
    cat: &CatData,
    dim: Color32,
    actions: &mut Vec<Action>,
) -> Rect {
    let c = app.chrome;
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
        let te = ui.put(
            edit_rect,
            TextEdit::singleline(buf).font(FontId::monospace(12.0)),
        );
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
            actions.push(Action::CommitRename(
                RenameTarget::Category(cat.id),
                Some(value),
            ));
        }
        return rect;
    }

    // Right-click: category management.
    resp.context_menu(|ui| {
        if ui.button("rename").clicked() {
            actions.push(Action::StartRename(RenameTarget::Category(cat.id)));
            ui.close();
        }
        if ui.button("new tab here").clicked() {
            actions.push(Action::NewTab {
                category: cat.id,
                cwd: None,
            });
            ui.close();
        }
        ui.menu_button("account", |ui| {
            if ui.button("(inherit)").clicked() {
                actions.push(Action::SetCategoryProfile(cat.id, None));
                ui.close();
            }
            for p in &app.claude.profiles {
                if ui.button(format!("@{}", p.name)).clicked() {
                    actions.push(Action::SetCategoryProfile(
                        cat.id,
                        Some(p.config_dir.clone()),
                    ));
                    ui.close();
                }
            }
        });
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
    p.circle_filled(
        Pos2::new(rect.min.x + 24.0, rect.center().y),
        4.0,
        cat.color,
    );
    p.text(
        Pos2::new(rect.min.x + 34.0, rect.center().y),
        Align2::LEFT_CENTER,
        cat.name.to_uppercase(),
        FontId::monospace(11.5),
        cat.color,
    );
    // Right-aligned summary: attention flags, working count, tab count.
    let mut badge_x = rect.max.x - 26.0;
    p.text(
        Pos2::new(badge_x, rect.center().y),
        Align2::RIGHT_CENTER,
        format!("{}", cat.count),
        FontId::monospace(10.0),
        dim,
    );
    badge_x -= 20.0;
    if cat.busy > 0 {
        let time = ui.input(|i| i.time);
        let glyph = SPINNER[(time * 10.0) as usize % SPINNER.len()];
        p.text(
            Pos2::new(badge_x, rect.center().y),
            Align2::RIGHT_CENTER,
            format!("{}{glyph}", cat.busy),
            FontId::monospace(10.0),
            cat.color,
        );
        badge_x -= 26.0;
    }
    if cat.needs > 0 {
        p.text(
            Pos2::new(badge_x, rect.center().y),
            Align2::RIGHT_CENTER,
            format!("{}⚑", cat.needs),
            FontId::monospace(10.0),
            c.amber,
        );
    }

    // "+" new-tab zone at the right edge.
    let plus_rect = Rect::from_min_size(
        Pos2::new(rect.max.x - 22.0, rect.min.y + 3.0),
        Vec2::splat(20.0),
    );
    let plus = ui.interact(
        plus_rect,
        ui.id().with(("cat-plus", cat.id.0)),
        Sense::click(),
    );
    p.text(
        plus_rect.center(),
        Align2::CENTER_CENTER,
        "+",
        FontId::monospace(13.0),
        if plus.hovered() { cat.color } else { dim },
    );
    if plus.clicked() {
        actions.push(Action::NewTab {
            category: cat.id,
            cwd: None,
        });
    } else if resp.clicked() {
        actions.push(Action::ToggleCollapse(cat.id));
    }
    if resp.hovered() {
        ui.output_mut(|o| o.cursor_icon = CursorIcon::PointingHand);
    }
    rect
}

fn tab_row(
    app: &mut App,
    ui: &mut Ui,
    row: &RowData,
    fg: Color32,
    dim: Color32,
    actions: &mut Vec<Action>,
) -> Rect {
    let c = app.chrome;
    let width = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, ROW_H), Sense::click_and_drag());
    let p = ui.painter_at(rect);
    // Geometry-based hover: `resp.hovered()` flickers when the close button
    // overlays the row in the hit-test stack.
    let hovered = ui.rect_contains_pointer(rect);

    if row.active {
        p.rect_filled(
            rect.shrink2(Vec2::new(4.0, 1.0)),
            4.0,
            row.color.gamma_multiply(0.16),
        );
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

    // Status glyph: Claude state wins over the plain shell dot.
    let dot = Pos2::new(rect.min.x + 18.0, rect.min.y + 13.0);
    let time = ui.input(|i| i.time);
    match row.claude {
        ClaudeState::Busy => {
            let glyph = SPINNER[(time * 10.0) as usize % SPINNER.len()];
            p.text(
                dot,
                Align2::CENTER_CENTER,
                glyph,
                FontId::monospace(13.0),
                row.color,
            );
        }
        ClaudeState::NeedsYou => {
            let pulse = ((time * 4.0).sin() * 0.35 + 0.65).clamp(0.0, 1.0);
            p.text(
                dot,
                Align2::CENTER_CENTER,
                "⚑",
                FontId::monospace(13.0),
                c.amber.gamma_multiply(pulse as f32),
            );
        }
        ClaudeState::DoneUnseen => {
            p.text(
                dot,
                Align2::CENTER_CENTER,
                "✓",
                FontId::monospace(12.0),
                c.accent,
            );
        }
        ClaudeState::Idle => {
            p.text(
                dot,
                Align2::CENTER_CENTER,
                "✳",
                FontId::monospace(11.0),
                dim,
            );
        }
        ClaudeState::None => {
            if row.exited {
                p.circle_stroke(dot, 3.5, Stroke::new(1.2, dim));
            } else {
                p.circle_filled(dot, 3.5, Color32::from_rgb(0x7b, 0xa2, 0x5a));
            }
        }
    }

    // Inline rename?
    if let Some((RenameTarget::Tab(id), buf)) = &mut app.rename
        && *id == row.id
    {
        let edit_rect = Rect::from_min_size(
            Pos2::new(rect.min.x + 28.0, rect.min.y + 3.0),
            Vec2::new(width - 40.0, 20.0),
        );
        let te = ui.put(
            edit_rect,
            TextEdit::singleline(buf).font(FontId::monospace(12.0)),
        );
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
        return rect;
    }

    // Title (char-budget truncation; the rail is monospace).
    let char_budget = ((width - 52.0) / 7.2).max(4.0) as usize;
    p.text(
        Pos2::new(rect.min.x + 28.0, rect.min.y + 13.0),
        Align2::LEFT_CENTER,
        truncate_chars(&row.title, char_budget),
        FontId::monospace(12.5),
        if row.active {
            fg
        } else {
            fg.gamma_multiply(0.8)
        },
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
        let close_rect = Rect::from_min_size(
            Pos2::new(rect.max.x - 24.0, rect.min.y + 4.0),
            Vec2::splat(18.0),
        );
        let close = ui.interact(
            close_rect,
            ui.id().with(("tab-close", row.id.0)),
            Sense::click(),
        );
        p.text(
            close_rect.center(),
            Align2::CENTER_CENTER,
            "×",
            FontId::monospace(13.0),
            if close.hovered() {
                Color32::from_rgb(0xd9, 0x7f, 0x70)
            } else {
                dim
            },
        );
        if close.clicked() {
            actions.push(Action::CloseTab(row.id));
            return rect;
        }
    }

    // Right-click: tab management.
    resp.context_menu(|ui| {
        if ui.button("rename").clicked() {
            actions.push(Action::StartRename(RenameTarget::Tab(row.id)));
            ui.close();
        }
        if ui.button("sessions…").clicked() {
            actions.push(Action::OpenSessions(row.id));
            ui.close();
        }
        ui.menu_button("move to", |ui| {
            for (id, name) in &app
                .ws
                .categories
                .iter()
                .map(|c| (c.id, c.name.clone()))
                .collect::<Vec<_>>()
            {
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

    if resp.drag_started() {
        app.dragging = Some(row.id);
    }
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
    // The dragged row dims in place; the drop indicator shows where it lands.
    if app.dragging == Some(row.id) {
        p.rect_filled(
            rect.shrink2(Vec2::new(4.0, 1.0)),
            4.0,
            Color32::from_rgba_unmultiplied(0, 0, 0, 90),
        );
    }
    rect
}

/// Offer the new release, if the daily check found one.
fn update_banner(app: &App, ui: &mut Ui, actions: &mut Vec<Action>) {
    let c = app.chrome;
    let Some(available) = &app.update else { return };
    if app.update_dismissed {
        return;
    }
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new(format!("▲ v{} available", available.version))
                .font(FontId::monospace(10.0))
                .color(c.accent),
        )
        .on_hover_text(available.url.clone());
        if ui
            .small_button("update")
            .on_hover_text(
                "opens a tab and runs the official install command,\nso you see exactly what runs",
            )
            .clicked()
        {
            actions.push(Action::RunUpdate);
        }
        if ui.small_button("×").clicked() {
            actions.push(Action::DismissUpdate);
        }
    });
}

fn hooks_banner(app: &App, ui: &mut Ui, actions: &mut Vec<Action>) {
    let c = app.chrome;
    // Installed, but every running session predates it — claude reads
    // settings at startup, so none of them will report anything.
    if app.claude.hooks_installed && app.stale_sessions {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new("⟳ restart claude for live states")
                    .font(FontId::monospace(10.0))
                    .color(c.amber),
            )
            .on_hover_text(
                "hooks and the usage statusline load when a claude session starts.\n\
                 every running session began before they were installed —\n\
                 exit and re-run claude in a tab to activate them.",
            );
        });
    }
    if app.claude.hooks_installed && !app.claude.relay_listening() {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new("⚠ hook relay socket failed — states degraded")
                    .font(FontId::monospace(10.0))
                    .color(c.poppy),
            );
        });
    }
    if app.claude.hooks_installed || app.hooks_banner_dismissed {
        return;
    }
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new("⚑ live Claude states need hooks")
                .font(FontId::monospace(10.0))
                .color(c.amber),
        );
        if ui.small_button("install").on_hover_text(format!(
            "adds `giverny relay` to {} event(s) plus a compact statusline for live usage,\nin each profile's settings.json (existing hooks and any statusline of your own\nare preserved; a .giverny-bak backup is written)",
            giverny_claude::hooks::RELAY_EVENTS.len()
        )).clicked() {
            actions.push(Action::InstallHooks);
        }
        if ui.small_button("×").clicked() {
            actions.push(Action::DismissHooksBanner);
        }
    });
}

fn usage_panel(app: &App, ui: &mut Ui, dim: Color32, fg: Color32, actions: &mut Vec<Action>) {
    let c = app.chrome;
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new("ACCOUNTS")
                .font(FontId::monospace(9.5))
                .color(dim),
        );
        let spinning = app.claude.refresh_in_flight();
        let label = if spinning {
            let t = ui.input(|i| i.time);
            SPINNER[(t * 10.0) as usize % SPINNER.len()].to_string()
        } else {
            "⟳".to_string()
        };
        if ui
            .add(egui::Button::new(
                egui::RichText::new(label)
                    .font(FontId::monospace(10.0))
                    .color(if spinning { c.accent } else { dim }),
            ))
            .on_hover_text(
                "refresh usage now\n(asks Claude Code to update its own cache;\nGiverny makes no API call)",
            )
            .clicked()
        {
            actions.push(Action::RefreshUsage);
        }
        if spinning {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(120));
        }
        // The way in that does not require knowing a chord.
        if ui
            .add(egui::Button::new(
                egui::RichText::new("⚙")
                    .font(FontId::monospace(11.0))
                    .color(dim),
            ))
            .on_hover_text("settings  (Ctrl+,)")
            .clicked()
        {
            actions.push(Action::ToggleSettings);
        }
    });
    ui.horizontal(|ui| {
        ui.add_space(6.0);
        let live = app.claude.hooks_installed && app.claude.relay_listening();
        ui.label(
            egui::RichText::new(if live { "● claude states live" } else { "○ states degraded" })
                .font(FontId::monospace(9.5))
                .color(if live { c.accent } else { c.poppy }),
        )
        .on_hover_text(if live {
            "hooks installed and the relay is connected\n(restart a claude session for its hooks to load)"
        } else {
            "install hooks below, or run `giverny doctor` in a tab"
        });
        // Only surfaced when off — it is on by default wherever hooks are.
        if !app.claude.statusline_on()
            && ui
                .small_button("enable live usage")
                .on_hover_text(
                    "adds a compact statusline to claude that pushes usage to Giverny\n\
                     (official rate_limits field — no API calls)",
                )
                .clicked()
        {
            actions.push(Action::ToggleStatusline(true));
        }
    });
    if app.claude.accounts.is_empty() {
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new("no claude profiles found")
                    .font(FontId::monospace(10.0))
                    .color(dim),
            );
        });
        ui.add_space(6.0);
        return;
    }
    let now = jiff::Timestamp::now();
    for acc in &app.claude.accounts {
        ui.add_space(3.0);
        ui.horizontal(|ui| {
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(format!("@{}", acc.profile.name))
                    .font(FontId::monospace(10.5))
                    .color(fg),
            );
            // Say where the numbers came from and how old they are — a
            // cache age shown next to live-pushed bars reads as stale.
            let human = |m: i64| {
                if m >= 1440 {
                    format!("{}d", m / 1440)
                } else if m >= 120 {
                    format!("{}h", m / 60)
                } else {
                    format!("{m}m")
                }
            };
            match ClaudeWatch::freshness(acc, now) {
                Freshness::Live(m) => {
                    ui.label(
                        egui::RichText::new(if m < 2 {
                            "· live".to_string()
                        } else {
                            format!("· live {}", human(m))
                        })
                        .font(FontId::monospace(9.0))
                        .color(c.accent),
                    )
                    .on_hover_text("pushed by claude's statusline");
                }
                Freshness::Cache(m) if m > 30 => {
                    let resp = ui.label(
                        egui::RichText::new(format!("{} old", human(m)))
                            .font(FontId::monospace(9.0))
                            .color(dim),
                    );
                    if acc.statusline_on {
                        resp.on_hover_text(
                            "from claude's on-disk cache.\nlive updates start when a claude \
                             session is restarted\n(settings load at session start)",
                        );
                    } else {
                        resp.on_hover_text("from claude's on-disk cache");
                    }
                }
                _ => {}
            }
        });
        match &acc.usage {
            Some(u) if !u.limits.is_empty() => {
                for limit in &u.limits {
                    let (pct, live) = ClaudeWatch::display_percent(acc, limit, now);
                    usage_bar(ui, limit, pct, live, now, dim, fg, c);
                }
            }
            _ => {
                ui.horizontal(|ui| {
                    ui.add_space(12.0);
                    ui.label(
                        egui::RichText::new("no usage data yet")
                            .font(FontId::monospace(9.5))
                            .color(dim),
                    );
                });
            }
        }
    }
    ui.add_space(6.0);
}

#[allow(clippy::too_many_arguments)]
fn usage_bar(
    ui: &mut Ui,
    limit: &giverny_claude::usage::LimitEntry,
    pct: f64,
    live: bool,
    now: jiff::Timestamp,
    dim: Color32,
    fg: Color32,
    c: crate::chrome::Chrome,
) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 15.0), Sense::hover());
    let p = ui.painter_at(rect);
    let color = if limit.critical() || pct >= 95.0 {
        c.poppy
    } else if pct >= 80.0 {
        c.amber
    } else {
        c.dim
    };

    // Label.
    p.text(
        Pos2::new(rect.min.x + 12.0, rect.center().y),
        Align2::LEFT_CENTER,
        truncate_chars(&limit.label(), 6),
        FontId::monospace(9.5),
        if limit.is_active { fg } else { dim },
    );
    // Track + fill.
    let track = Rect::from_min_max(
        Pos2::new(rect.min.x + 58.0, rect.center().y - 3.0),
        Pos2::new(rect.max.x - 74.0, rect.center().y + 3.0),
    );
    if track.width() > 10.0 {
        p.rect_filled(
            track,
            3.0,
            Color32::from_rgba_unmultiplied(255, 255, 255, 14),
        );
        let mut fill = track;
        fill.set_right(track.min.x + track.width() * (pct as f32 / 100.0));
        if fill.width() > 0.5 {
            p.rect_filled(fill, 3.0, color);
        }
    }
    // Numbers. A leading dot marks a value pushed live by the statusline.
    let mut right = format!("{}{:>3.0}%", if live { "·" } else { " " }, pct);
    if let Some(cd) = limit.reset_countdown(now) {
        right = format!("{right} {cd:>6}");
    }
    p.text(
        Pos2::new(rect.max.x - 6.0, rect.center().y),
        Align2::RIGHT_CENTER,
        right,
        FontId::monospace(9.0),
        if limit.critical() { c.poppy } else { dim },
    );
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}
