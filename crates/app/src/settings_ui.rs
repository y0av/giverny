//! The settings screen (`Ctrl+,`).
//!
//! An overlay over the terminal pane, with the rail left visible — settings
//! that change the rail (theme, titles, colours) can be watched taking effect.
//! Rows are generated from `giverny_core::settings::SETTINGS`, so an option
//! declared there appears here without any code, and one that is not declared
//! cannot appear at all.
//!
//! Every row shows its TOML key under the label. That is deliberate: the
//! screen teaches the file, so the next edit can be made over SSH or dropped
//! into dotfiles.

use eframe::egui::{self, Color32, FontId, Key, Modifiers, RichText};
use giverny_core::settings::{self, Kind, Section, SettingDef, Value};

use crate::{Action, App};

const DIM: Color32 = Color32::from_rgb(0x6b, 0x78, 0x80);
const KEY_COLOR: Color32 = Color32::from_rgb(0x7d, 0x8a, 0x94);
const ACCENT: Color32 = Color32::from_rgb(0x5f, 0xa3, 0xa3);
const MOD_DOT: Color32 = Color32::from_rgb(0xd9, 0xb5, 0x5f);

pub struct SettingsState {
    pub section: Section,
    pub search: String,
    pub search_focus: bool,
    /// Key of the row being text-edited, with its in-progress buffer. Text and
    /// number rows commit on Enter or focus loss, not on every keystroke — a
    /// half-typed "1" in a "10000" field must not be written to disk.
    pub editing: Option<(String, String)>,
}

impl Default for SettingsState {
    fn default() -> Self {
        SettingsState {
            section: Section::Appearance,
            search: String::new(),
            search_focus: true,
            editing: None,
        }
    }
}

/// Rows to show: everything in the current section, or — while searching —
/// every match across all sections, since search *is* the navigation.
fn visible(state: &SettingsState) -> Vec<&'static SettingDef> {
    let needle = state.search.trim().to_lowercase();
    if needle.is_empty() {
        return settings::in_section(state.section).collect();
    }
    settings::SETTINGS
        .iter()
        .filter(|d| {
            let hay = format!("{} {} {} {}", d.label, d.key, d.doc, d.section.title());
            hay.to_lowercase().contains(&needle)
        })
        .collect()
}

/// Drawn in place of the terminal pane, so the rail stays put. The terminal
/// keeps running behind it — this hides a view, it does not pause anything.
pub fn settings_ui(app: &mut App, ui: &mut egui::Ui) -> Vec<Action> {
    let mut actions = Vec::new();
    let Some(mut state) = app.settings.take() else {
        return actions;
    };
    let ctx = ui.ctx().clone();

    let mut close = false;
    ctx.input_mut(|i| {
        if i.consume_key(Modifiers::NONE, Key::Escape) {
            // Esc leaves the field first, the screen second: the TUI rule is
            // that Esc always goes back exactly one step.
            if state.editing.is_some() {
                state.editing = None;
            } else if !state.search.is_empty() {
                state.search.clear();
            } else {
                close = true;
            }
        }
    });

    let cfg = app.cfg.clone();
    let rows = visible(&state);
    // Suggestions for the restore list, from what tabs have actually run.
    let allowed: Vec<String> = cfg.behavior.restore_apps.clone();
    let suggestions = restore_suggestions(app, &allowed);

    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(14, 10))
        .show(ui, |ui| {
            header(ui, &mut state, &mut close);
            ui.add_space(6.0);
            ui.separator();
            ui.add_space(6.0);

            let footer_h = 22.0;
            let body_h = (ui.available_height() - footer_h).max(80.0);
            ui.horizontal_top(|ui| {
                ui.set_height(body_h);
                sections(ui, &mut state, &cfg);
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(10.0);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // The scroll area sits inside a horizontal layout, so
                        // without this its rows would run left-to-right.
                        ui.vertical(|ui| {
                            body(app, ui, &mut state, &cfg, &rows, &suggestions, &mut actions);
                        });
                    });
            });
            ui.separator();
            footer(ui, &mut actions, &mut close);
        });

    if !close {
        app.settings = Some(state);
    }
    actions
}

fn header(ui: &mut egui::Ui, state: &mut SettingsState, close: &mut bool) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("settings")
                .font(FontId::monospace(13.0))
                .color(ACCENT),
        );
        ui.add_space(12.0);
        let search = ui.add(
            egui::TextEdit::singleline(&mut state.search)
                .hint_text("search")
                .desired_width(260.0)
                .font(FontId::monospace(12.0)),
        );
        if state.search_focus {
            search.request_focus();
            state.search_focus = false;
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .button(RichText::new("esc ✕").font(FontId::monospace(11.0)))
                .clicked()
            {
                *close = true;
            }
        });
    });
}

fn sections(ui: &mut egui::Ui, state: &mut SettingsState, cfg: &giverny_core::config::Config) {
    ui.vertical(|ui| {
        ui.set_width(120.0);
        for section in Section::ALL {
            // A dot marks a section holding something you changed.
            let touched = settings::in_section(*section).any(|d| !settings::is_default(cfg, d));
            let selected = state.section == *section && state.search.is_empty();
            let label = format!("{:<13}{}", section.title(), if touched { "•" } else { " " });
            if ui
                .selectable_label(
                    selected,
                    RichText::new(label)
                        .font(FontId::monospace(12.0))
                        .color(if selected { ACCENT } else { Color32::GRAY }),
                )
                .clicked()
            {
                state.section = *section;
                state.search.clear();
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn body(
    app: &App,
    ui: &mut egui::Ui,
    state: &mut SettingsState,
    cfg: &giverny_core::config::Config,
    rows: &[&'static SettingDef],
    suggestions: &[String],
    actions: &mut Vec<Action>,
) {
    // Sections with no options of their own still have something to say.
    if state.search.is_empty() {
        match state.section {
            Section::Keys => return keys_section(ui),
            Section::About => return about_section(app, ui, actions),
            _ => {}
        }
    }

    if rows.is_empty() {
        ui.label(
            RichText::new("nothing here yet")
                .font(FontId::monospace(11.0))
                .color(DIM),
        );
        return;
    }

    // Said once per section rather than per row: allowing a program means
    // Giverny runs it unattended when a tab restores.
    if state.search.is_empty() && state.section == Section::Restore {
        ui.label(
            RichText::new(
                "Listed programs are started again by a restored tab, unattended. \
                 Everything else is remembered but never re-run.",
            )
            .font(FontId::monospace(10.0))
            .color(DIM),
        );
        ui.add_space(8.0);
    }

    for def in rows {
        row(ui, state, cfg, def, suggestions, actions);
        ui.add_space(10.0);
    }
}

fn row(
    ui: &mut egui::Ui,
    state: &mut SettingsState,
    cfg: &giverny_core::config::Config,
    def: &'static SettingDef,
    suggestions: &[String],
    actions: &mut Vec<Action>,
) {
    let Some(value) = settings::current(cfg, def) else {
        return;
    };
    let modified = value != def.default_value();

    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.set_width(230.0);
            ui.label(RichText::new(def.label).font(FontId::monospace(12.5)));
            // The key, so the screen teaches the file.
            ui.label(
                RichText::new(def.key)
                    .font(FontId::monospace(10.0))
                    .color(KEY_COLOR),
            );
        });

        ui.vertical(|ui| {
            widget(ui, state, def, &value, suggestions, actions);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(def.doc)
                        .font(FontId::monospace(10.0))
                        .color(DIM),
                );
                if modified {
                    ui.label(
                        RichText::new("●")
                            .font(FontId::monospace(9.0))
                            .color(MOD_DOT),
                    )
                    .on_hover_text("changed from the default");
                    if ui
                        .small_button(RichText::new("↺").font(FontId::monospace(10.0)))
                        .on_hover_text("reset to default")
                        .clicked()
                    {
                        actions.push(Action::SetSetting(def.key.into(), def.default_value()));
                        state.editing = None;
                    }
                }
            });
        });
    });
}

fn widget(
    ui: &mut egui::Ui,
    state: &mut SettingsState,
    def: &'static SettingDef,
    value: &Value,
    suggestions: &[String],
    actions: &mut Vec<Action>,
) {
    match &def.kind {
        Kind::Bool { .. } => {
            let on = value.as_bool().unwrap_or(false);
            // `[ on ]`, not a switch: this is a terminal.
            let text = if on { "[ on  ]" } else { "[ off ]" };
            if ui
                .button(
                    RichText::new(text)
                        .font(FontId::monospace(12.0))
                        .color(if on { ACCENT } else { DIM }),
                )
                .clicked()
            {
                actions.push(Action::SetSetting(def.key.into(), Value::Bool(!on)));
            }
        }
        Kind::Choice { options, .. } => {
            let current = value.as_str().unwrap_or_default().to_string();
            ui.horizontal(|ui| {
                for opt in *options {
                    let selected = current == *opt;
                    if ui
                        .selectable_label(
                            selected,
                            RichText::new(*opt)
                                .font(FontId::monospace(12.0))
                                .color(if selected { ACCENT } else { Color32::GRAY }),
                        )
                        .clicked()
                        && !selected
                    {
                        actions.push(Action::SetSetting(
                            def.key.into(),
                            Value::Text((*opt).into()),
                        ));
                    }
                }
            });
        }
        Kind::Float { min, max, .. } => {
            let mut v = value.as_f64().unwrap_or_default();
            if ui
                .add(
                    egui::DragValue::new(&mut v)
                        .speed(0.25)
                        .range(*min..=*max)
                        .fixed_decimals(1),
                )
                .changed()
            {
                actions.push(Action::SetSetting(def.key.into(), Value::Float(v)));
            }
        }
        Kind::Int { min, max, .. } => {
            let mut v = value.as_i64().unwrap_or_default();
            if ui
                .add(egui::DragValue::new(&mut v).speed(10.0).range(*min..=*max))
                .changed()
            {
                actions.push(Action::SetSetting(def.key.into(), Value::Int(v)));
            }
        }
        Kind::Text { placeholder, .. } => {
            let stored = value.as_str().unwrap_or_default().to_string();
            let editing = state.editing.as_ref().is_some_and(|(k, _)| k == def.key);
            let mut buf = match (&state.editing, editing) {
                (Some((_, b)), true) => b.clone(),
                _ => stored.clone(),
            };
            let resp = ui.add(
                egui::TextEdit::singleline(&mut buf)
                    .hint_text(*placeholder)
                    .desired_width(220.0)
                    .font(FontId::monospace(12.0)),
            );
            if resp.changed() {
                state.editing = Some((def.key.into(), buf.clone()));
            }
            // Commit on Enter or when the field loses focus — never per
            // keystroke, which would write a config file per character.
            let done = resp.lost_focus() || ui.input(|i| i.key_pressed(Key::Enter));
            if done && editing && buf != stored {
                actions.push(Action::SetSetting(def.key.into(), Value::Text(buf)));
                state.editing = None;
            }
        }
        Kind::StringList { .. } => list_widget(ui, state, def, value, suggestions, actions),
    }
}

/// The restore-apps editor (and any other list): remove per row, add by typing,
/// plus one-click suggestions taken from what tabs have actually been running.
fn list_widget(
    ui: &mut egui::Ui,
    state: &mut SettingsState,
    def: &'static SettingDef,
    value: &Value,
    suggestions: &[String],
    actions: &mut Vec<Action>,
) {
    let items: Vec<String> = value.as_list().unwrap_or_default().to_vec();
    let add_key = format!("{}::add", def.key);

    ui.vertical(|ui| {
        ui.set_width(300.0);
        // Say how many there are: the list scrolls, and a clipped 26-entry
        // list otherwise looks like a 4-entry one.
        if !items.is_empty() {
            ui.label(
                RichText::new(format!("{} programs", items.len()))
                    .font(FontId::monospace(10.0))
                    .color(DIM),
            );
        }
        egui::ScrollArea::vertical()
            .max_height(210.0)
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
            .id_salt(def.key)
            .show(ui, |ui| {
                for item in &items {
                    ui.horizontal(|ui| {
                        if ui
                            .small_button(RichText::new("✕").font(FontId::monospace(10.0)))
                            .on_hover_text("remove")
                            .clicked()
                        {
                            let rest: Vec<String> =
                                items.iter().filter(|i| *i != item).cloned().collect();
                            actions.push(Action::SetSetting(def.key.into(), Value::List(rest)));
                        }
                        ui.label(RichText::new(item).font(FontId::monospace(12.0)));
                    });
                }
                if items.is_empty() {
                    ui.label(
                        RichText::new("(empty)")
                            .font(FontId::monospace(11.0))
                            .color(DIM),
                    );
                }
            });

        ui.horizontal(|ui| {
            let editing = state.editing.as_ref().is_some_and(|(k, _)| *k == add_key);
            let mut buf = match (&state.editing, editing) {
                (Some((_, b)), true) => b.clone(),
                _ => String::new(),
            };
            let resp = ui.add(
                egui::TextEdit::singleline(&mut buf)
                    .hint_text("add…")
                    .desired_width(160.0)
                    .font(FontId::monospace(12.0)),
            );
            if resp.changed() {
                state.editing = Some((add_key.clone(), buf.clone()));
            }
            let submit = resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            let clicked = ui
                .small_button(RichText::new("+").font(FontId::monospace(12.0)))
                .clicked();
            if (submit || clicked) && !buf.trim().is_empty() {
                let mut next = items.clone();
                let entry = buf.trim().to_string();
                if !next.contains(&entry) {
                    next.push(entry);
                    actions.push(Action::SetSetting(def.key.into(), Value::List(next)));
                }
                state.editing = None;
            }
        });

        // No typing required for the common case: the programs your tabs have
        // actually been running, one click to allow.
        if !suggestions.is_empty() && matches!(def.key, "behavior.restore_apps") {
            ui.add_space(4.0);
            ui.label(
                RichText::new("seen in your tabs")
                    .font(FontId::monospace(10.0))
                    .color(DIM),
            );
            ui.horizontal_wrapped(|ui| {
                for program in suggestions {
                    if ui
                        .small_button(
                            RichText::new(format!("+ {program}")).font(FontId::monospace(11.0)),
                        )
                        .clicked()
                    {
                        let mut next = items.clone();
                        next.push(program.clone());
                        actions.push(Action::SetSetting(def.key.into(), Value::List(next)));
                    }
                }
            });
        }
    });
}

/// Programs seen running in tabs that are not on the list yet — one click to
/// allow. The data is already there: every tab records its foreground command
/// so restore can bring it back.
pub fn restore_suggestions(app: &App, allowed: &[String]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for tab in &app.ws.tabs {
        let Some(cmd) = tab.foreground.as_deref() else {
            continue;
        };
        let program = giverny_core::procs::program_name(cmd);
        if program.is_empty()
            || allowed.iter().any(|a| a == program)
            || seen.iter().any(|s| s == program)
        {
            continue;
        }
        seen.push(program.to_string());
    }
    seen
}

fn keys_section(ui: &mut egui::Ui) {
    ui.label(
        RichText::new("F1 shows this without leaving what you are doing.")
            .font(FontId::monospace(10.5))
            .color(DIM),
    );
    ui.add_space(8.0);
    crate::keymap::table_ui(ui, "");
}

fn about_section(app: &App, ui: &mut egui::Ui, actions: &mut Vec<Action>) {
    let line = |ui: &mut egui::Ui, k: &str, v: String| {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("{k:<10}"))
                    .font(FontId::monospace(11.5))
                    .color(DIM),
            );
            ui.label(RichText::new(v).font(FontId::monospace(11.5)));
        });
    };
    line(ui, "version", crate::update::CURRENT.to_string());
    line(
        ui,
        "config",
        giverny_core::config::config_path(app.paths.base())
            .display()
            .to_string(),
    );
    line(ui, "state", app.paths.state_file().display().to_string());
    ui.add_space(10.0);
    if ui
        .button(RichText::new("open config.toml in a tab").font(FontId::monospace(11.5)))
        .clicked()
    {
        actions.push(Action::EditConfig);
    }
}

fn footer(ui: &mut egui::Ui, actions: &mut Vec<Action>, close: &mut bool) {
    ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("/ search")
                    .font(FontId::monospace(10.5))
                    .color(DIM),
            );
            ui.label(RichText::new("·").color(DIM));
            if ui
                .link(
                    RichText::new("⇧⏎ edit config.toml")
                        .font(FontId::monospace(10.5))
                        .color(DIM),
                )
                .clicked()
            {
                actions.push(Action::EditConfig);
            }
            ui.label(RichText::new("·").color(DIM));
            if ui
                .link(
                    RichText::new("esc back")
                        .font(FontId::monospace(10.5))
                        .color(DIM),
                )
                .clicked()
            {
                *close = true;
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use giverny_term::render::theme::Theme;

    #[test]
    fn every_theme_the_screen_offers_is_real_and_distinct() {
        // The choice list lives in giverny-core, which cannot see the themes;
        // this crate sees both, so this is where they are checked against
        // each other. A name that falls through `by_name` silently becomes
        // monet-dark, which looks like the picker doing nothing.
        let offered = match settings::by_key("theme.name").map(|d| &d.kind) {
            Some(Kind::Choice { options, .. }) => *options,
            _ => panic!("theme.name is not a choice"),
        };
        assert_eq!(
            offered,
            Theme::NAMES,
            "offered themes differ from the built-ins"
        );
        for name in offered.iter().filter(|n| **n != "monet-dark") {
            assert_ne!(
                Theme::by_name(name).bg,
                Theme::monet_dark().bg,
                "{name} is offered but not implemented"
            );
        }
    }

    #[test]
    fn search_finds_options_from_any_section() {
        let mut state = SettingsState {
            section: Section::Appearance,
            ..Default::default()
        };
        // A restore option, found from the appearance section.
        state.search = "btop".into();
        assert!(
            visible(&state).is_empty(),
            "search is over labels, not values"
        );
        state.search = "restart".into();
        let hits = visible(&state);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].key, "behavior.restore_apps");
        // Searching by TOML key works too — that is half the point of showing it.
        state.search = "titles.strip".into();
        assert_eq!(visible(&state).len(), 1);
    }
}
