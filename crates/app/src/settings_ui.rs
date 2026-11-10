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

use std::sync::OnceLock;

use eframe::egui::{self, Color32, FontId, Key, Modifiers, RichText};
use giverny_core::limits::{self, Auto, GpuLimit, Limits, Machine, Mem};
use giverny_core::settings::{self, Kind, LimitField, Section, SettingDef, Value};

use crate::{Action, App};

use crate::chrome::Chrome;

pub struct SettingsState {
    pub section: Section,
    pub search: String,
    pub search_focus: bool,
    /// Key of the row being text-edited, with its in-progress buffer. Text and
    /// number rows commit on Enter or focus loss, not on every keystroke — a
    /// half-typed "1" in a "10000" field must not be written to disk.
    pub editing: Option<(String, String)>,
    /// A value that was typed but refused (a limit larger than the machine,
    /// say): the row's edit key and why. Cleared by the next good commit.
    pub error: Option<(String, String)>,
}

impl Default for SettingsState {
    fn default() -> Self {
        SettingsState {
            section: Section::Appearance,
            search: String::new(),
            search_focus: true,
            editing: None,
            error: None,
        }
    }
}

/// This machine's cores, RAM and GPUs, for the manager limits.
///
/// Detected once per process, off the UI thread: GPU detection runs
/// `nvidia-smi`, which can take a second, and none of it changes while
/// Giverny runs. `None` until the first detection finishes.
fn machine(ctx: &egui::Context) -> Option<&'static Machine> {
    static MACHINE: OnceLock<Machine> = OnceLock::new();
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("detect-machine".into())
            .spawn(move || {
                let _ = MACHINE.set(Machine::detect());
                ctx.request_repaint();
            });
        if let Err(err) = spawned {
            tracing::warn!("could not detect the machine: {err}");
        }
    });
    MACHINE.get()
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
    let c = app.chrome;
    let machine = machine(&ctx);
    let rows = visible(&state);
    // Suggestions for the restore list, from what tabs have actually run.
    let allowed: Vec<String> = cfg.behavior.restore_apps.clone();
    let suggestions = restore_suggestions(app, &allowed);

    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(14, 10))
        .show(ui, |ui| {
            header(ui, &mut state, &mut close, c);
            ui.add_space(6.0);
            ui.separator();
            ui.add_space(6.0);

            let footer_h = 22.0;
            let body_h = (ui.available_height() - footer_h).max(80.0);
            ui.horizontal_top(|ui| {
                ui.set_height(body_h);
                sections(ui, &mut state, &cfg, c);
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(10.0);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        // The scroll area sits inside a horizontal layout, so
                        // without this its rows would run left-to-right.
                        ui.vertical(|ui| {
                            body(
                                app,
                                ui,
                                &mut state,
                                &cfg,
                                machine,
                                &rows,
                                &suggestions,
                                &mut actions,
                                c,
                            );
                        });
                    });
            });
            ui.separator();
            footer(ui, &mut actions, &mut close, c);
        });

    // Remembered for the next Ctrl+, — this process and, through the state
    // file, the next one.
    if app.layout.settings_section.as_deref() != Some(state.section.title()) {
        app.layout.settings_section = Some(state.section.title().to_string());
        app.state_dirty = true;
    }
    if !close {
        app.settings = Some(state);
    }
    actions
}

fn header(ui: &mut egui::Ui, state: &mut SettingsState, close: &mut bool, c: Chrome) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("settings")
                .font(FontId::monospace(13.0))
                .color(c.accent),
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

fn sections(
    ui: &mut egui::Ui,
    state: &mut SettingsState,
    cfg: &giverny_core::config::Config,
    c: Chrome,
) {
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
                        .color(if selected { c.accent } else { Color32::GRAY }),
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
    machine: Option<&Machine>,
    rows: &[&'static SettingDef],
    suggestions: &[String],
    actions: &mut Vec<Action>,
    c: Chrome,
) {
    // Sections with no options of their own still have something to say.
    if state.search.is_empty() {
        match state.section {
            Section::Keys => return keys_section(ui, c),
            Section::About => return about_section(app, ui, actions, c),
            _ => {}
        }
    }

    if rows.is_empty() {
        ui.label(
            RichText::new("nothing here yet")
                .font(FontId::monospace(11.0))
                .color(c.dim),
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
            .color(c.dim),
        );
        ui.add_space(8.0);
    }

    if state.search.is_empty() && state.section == Section::ManagementPanel {
        return management_panel_page(ui, state, cfg, machine, rows, suggestions, actions, c);
    }

    if state.search.is_empty() && state.section == Section::Codex {
        ui.label(
            "Codex is detected automatically. Type codex normally in any tab; usage uses its existing sign-in.",
        );
        ui.label("Setup preserves existing hooks and writes a .giverny-bak backup. Codex requires you to review the hooks with /hooks.");
        for account in &app.codex.accounts {
            let target = &account.target;
            let path = crate::codex_watch::CodexWatch::hook_path(target);
            let command = crate::codex_watch::CodexWatch::hook_command(target);
            ui.label(path.display().to_string());
            ui.add(
                egui::Label::new(
                    RichText::new(format!(
                        "Events: {}\nCommand: {command}",
                        giverny_codex::hooks::EVENTS.join(", ")
                    ))
                    .monospace(),
                )
                .wrap(),
            );
            ui.horizontal(|ui| {
                if ui.button("install / repair hooks").clicked() {
                    actions.push(Action::CodexHooks(target.clone(), true));
                }
                if ui.button("remove Giverny hooks").clicked() {
                    actions.push(Action::CodexHooks(target.clone(), false));
                }
            });
        }
        if let Some(status) = &app.codex.status {
            ui.label(status);
        }
        ui.add_space(10.0);
    }
    for def in rows {
        if skip_gpus(cfg, machine, def) {
            continue;
        }
        // Sharing is a refinement of history per terminal: greyed out while
        // there is no per-terminal history to share from.
        let enabled = def.key != "behavior.history_also_shared" || cfg.behavior.history_per_tab;
        ui.add_enabled_ui(enabled, |ui| {
            row(ui, state, cfg, machine, def, suggestions, actions, c);
        });
        ui.add_space(10.0);
    }
}

/// No GPU, no GPU row — unless one is set, which then needs a reset.
fn skip_gpus(
    cfg: &giverny_core::config::Config,
    machine: Option<&Machine>,
    def: &SettingDef,
) -> bool {
    matches!(
        def.kind,
        Kind::Limit {
            field: LimitField::Gpus
        }
    ) && settings::is_default(cfg, def)
        && machine.is_none_or(|m| m.gpus.is_empty())
}

/// The table under the `columns` row: `management_panel.columns.*`.
const COLUMNS: &str = "management_panel.columns.";

/// Settings → Management panel, in the order a person reads it: the pane,
/// the skill and the names switch, the columns as one row of switches (in the pane's own order,
/// which is `SETTINGS`'), then the default lease and the limits under their
/// headings.
#[allow(clippy::too_many_arguments)]
fn management_panel_page(
    ui: &mut egui::Ui,
    state: &mut SettingsState,
    cfg: &giverny_core::config::Config,
    machine: Option<&Machine>,
    rows: &[&'static SettingDef],
    suggestions: &[String],
    actions: &mut Vec<Action>,
    c: Chrome,
) {
    let draw = |ui: &mut egui::Ui,
                state: &mut SettingsState,
                actions: &mut Vec<Action>,
                keep: &dyn Fn(&SettingDef) -> bool| {
        for def in rows.iter().copied().filter(|d| keep(d)) {
            if skip_gpus(cfg, machine, def) {
                continue;
            }
            row(ui, state, cfg, machine, def, suggestions, actions, c);
            ui.add_space(10.0);
        }
    };
    draw(ui, state, actions, &|d| {
        matches!(
            d.key,
            "claude.management_panel"
                | "management_panel.manage_skill"
                | "management_panel.ask_names"
        )
    });
    let columns: Vec<&'static SettingDef> = rows
        .iter()
        .copied()
        .filter(|d| d.key.starts_with(COLUMNS))
        .collect();
    columns_row(ui, state, cfg, &columns, actions, c);
    ui.add_space(10.0);
    ui.add_space(8.0);
    heading(ui, "default lease", c);
    draw(ui, state, actions, &|d| {
        matches!(d.kind, Kind::Lease { .. })
    });
    ui.add_space(8.0);
    heading(ui, "limits", c);
    draw(ui, state, actions, &|d| {
        matches!(d.kind, Kind::Limit { .. })
    });
}

/// One row for every column of the pane: a switch each, lit while the
/// column shows, and one ● ↺ putting every column back.
fn columns_row(
    ui: &mut egui::Ui,
    state: &mut SettingsState,
    cfg: &giverny_core::config::Config,
    columns: &[&'static SettingDef],
    actions: &mut Vec<Action>,
    c: Chrome,
) {
    let modified: Vec<&'static SettingDef> = columns
        .iter()
        .copied()
        .filter(|d| !settings::is_default(cfg, d))
        .collect();
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.set_width(230.0);
            ui.label(RichText::new("columns").font(FontId::monospace(12.5)));
            ui.label(
                RichText::new(COLUMNS.trim_end_matches('.'))
                    .font(FontId::monospace(10.0))
                    .color(c.dim),
            );
        });
        ui.horizontal_wrapped(|ui| {
            for def in columns {
                let on = settings::current(cfg, def)
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                if ui
                    .selectable_label(
                        on,
                        RichText::new(def.label)
                            .font(FontId::monospace(12.0))
                            .color(if on { c.accent } else { c.dim }),
                    )
                    .on_hover_text(format!("{}\n{}", def.doc, def.key))
                    .clicked()
                {
                    actions.push(Action::SetSetting(def.key.into(), Value::Bool(!on)));
                }
            }
            if !modified.is_empty() {
                ui.label(
                    RichText::new("●")
                        .font(FontId::monospace(9.0))
                        .color(c.amber),
                )
                .on_hover_text("changed from the default");
                if ui
                    .small_button(RichText::new("↺").font(FontId::monospace(10.0)))
                    .on_hover_text("every column back on")
                    .clicked()
                {
                    for def in &modified {
                        actions.push(Action::SetSetting(def.key.into(), def.default_value()));
                    }
                    state.editing = None;
                }
            }
        });
    });
}

/// A heading on the Management panel page — the fields show their own figures.
fn heading(ui: &mut egui::Ui, text: &str, c: Chrome) {
    ui.label(
        RichText::new(text)
            .font(FontId::monospace(12.5))
            .color(c.accent),
    );
    ui.add_space(10.0);
}

#[allow(clippy::too_many_arguments)]
fn row(
    ui: &mut egui::Ui,
    state: &mut SettingsState,
    cfg: &giverny_core::config::Config,
    machine: Option<&Machine>,
    def: &'static SettingDef,
    suggestions: &[String],
    actions: &mut Vec<Action>,
    c: Chrome,
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
                    .color(c.dim),
            );
        });

        ui.vertical(|ui| {
            // The limits are bare figures, their ● ↺ beside the field; the
            // doc stays on the def for search.
            if let Kind::Limit { field } = def.kind {
                return limit_widget(ui, state, cfg, machine, def, field, &value, actions, c);
            }
            if let Kind::Lease { field, .. } = def.kind {
                return lease_widget(ui, state, machine, def, field, &value, actions, c);
            }
            widget(ui, state, def, &value, suggestions, actions, c);
            ui.horizontal(|ui| {
                // The Management panel page is bare figures and switches; the
                // doc stays on the def for search (and on hover).
                if def.section != Section::ManagementPanel {
                    ui.label(
                        RichText::new(def.doc)
                            .font(FontId::monospace(10.0))
                            .color(c.dim),
                    );
                }
                if def.needs_restart && modified {
                    ui.label(
                        RichText::new("restart to apply")
                            .font(FontId::monospace(9.5))
                            .color(c.amber),
                    )
                    .on_hover_text("the change is saved; it loads at startup");
                }
                if modified {
                    changed_mark(ui, state, def, actions, c);
                }
            });
        });
    });
}

/// `● ↺`: the row is changed from its default, and the button putting the
/// default back.
fn changed_mark(
    ui: &mut egui::Ui,
    state: &mut SettingsState,
    def: &'static SettingDef,
    actions: &mut Vec<Action>,
    c: Chrome,
) {
    ui.label(
        RichText::new("●")
            .font(FontId::monospace(9.0))
            .color(c.amber),
    )
    .on_hover_text("changed from the default");
    if ui
        .small_button(RichText::new("↺").font(FontId::monospace(10.0)))
        .on_hover_text("reset to default")
        .clicked()
    {
        actions.push(Action::SetSetting(def.key.into(), def.default_value()));
        state.editing = None;
        state.error = None;
    }
}

fn widget(
    ui: &mut egui::Ui,
    state: &mut SettingsState,
    def: &'static SettingDef,
    value: &Value,
    suggestions: &[String],
    actions: &mut Vec<Action>,
    c: Chrome,
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
                        .color(if on { c.accent } else { c.dim }),
                )
                .clicked()
            {
                actions.push(Action::SetSetting(def.key.into(), Value::Bool(!on)));
            }
        }
        Kind::Choice { options, .. } => {
            let current = value.as_str().unwrap_or_default().to_string();
            // Wrapped, not a single row: the theme list outgrew the window,
            // and a choice you cannot see is a choice you do not have.
            ui.horizontal_wrapped(|ui| {
                for opt in *options {
                    let selected = current == *opt;
                    if ui
                        .selectable_label(
                            selected,
                            RichText::new(*opt)
                                .font(FontId::monospace(12.0))
                                .color(if selected { c.accent } else { Color32::GRAY }),
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
            // Point sizes move in quarters; a fraction like opacity, whose
            // useful values are 0.90-0.95, in hundredths.
            let (speed, decimals) = if max - min <= 1.0 {
                (0.005, 2)
            } else {
                (0.25, 1)
            };
            if ui
                .add(
                    egui::DragValue::new(&mut v)
                        .speed(speed)
                        .range(*min..=*max)
                        .fixed_decimals(decimals),
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
        Kind::StringList { .. } => list_widget(ui, state, def, value, suggestions, actions, c),
        // Drawn by `limit_widget` and `lease_widget`, which need the machine.
        Kind::Limit { .. } | Kind::Lease { .. } => {}
    }
}

/// A short text field committing on Enter or focus loss, like the text
/// rows. It shows `shown` — the figure in force, never the word `auto` —
/// and gives `Some(typed)` once something other than that is committed.
fn commit_field(
    ui: &mut egui::Ui,
    state: &mut SettingsState,
    key: &str,
    shown: &str,
) -> Option<String> {
    let editing = state.editing.as_ref().is_some_and(|(k, _)| k == key);
    let mut buf = match (&state.editing, editing) {
        (Some((_, b)), true) => b.clone(),
        _ => shown.to_string(),
    };
    let resp = ui.add(
        egui::TextEdit::singleline(&mut buf)
            .hint_text(shown)
            .desired_width(90.0)
            .font(FontId::monospace(12.0)),
    );
    if resp.changed() {
        state.editing = Some((key.into(), buf.clone()));
    }
    let done = resp.lost_focus() || ui.input(|i| i.key_pressed(Key::Enter));
    if done && editing {
        state.editing = None;
        if buf.trim() != shown {
            return Some(buf);
        }
    }
    None
}

fn dim(ui: &mut egui::Ui, text: String, c: Chrome) {
    ui.label(
        RichText::new(text)
            .font(FontId::monospace(11.0))
            .color(c.dim),
    );
}

fn refused(ui: &mut egui::Ui, state: &SettingsState, key: &str, c: Chrome) {
    if let Some((_, why)) = state.error.as_ref().filter(|(k, _)| k == key) {
        ui.label(
            RichText::new(why)
                .font(FontId::monospace(10.0))
                .color(c.amber),
        );
    }
}

/// One `[manager.limits]` row: a bare field holding the figure in
/// force — at `auto`, what that comes to on this machine. The row's ↺ puts
/// `auto` back.
#[allow(clippy::too_many_arguments)]
fn limit_widget(
    ui: &mut egui::Ui,
    state: &mut SettingsState,
    cfg: &giverny_core::config::Config,
    machine: Option<&Machine>,
    def: &'static SettingDef,
    field: LimitField,
    value: &Value,
    actions: &mut Vec<Action>,
    c: Chrome,
) {
    let limits = &cfg.manager.limits;
    if field == LimitField::Gpus {
        return gpu_rows(ui, state, limits, machine, def, actions, c);
    }
    let stored = value.as_str().unwrap_or("auto").to_string();
    let Some(m) = machine else {
        return dim(ui, "detecting this machine…".into(), c);
    };
    let shown = limit_figure(limits, m, field);
    let typed = ui
        .horizontal(|ui| {
            let typed = commit_field(ui, state, def.key, &shown);
            if *value != def.default_value() {
                changed_mark(ui, state, def, actions, c);
            }
            dim(ui, limit_share(limits, m, field), c);
            typed
        })
        .inner;
    if let Some(typed) = typed {
        let parsed = match field {
            LimitField::Cores => limits::parse_cores(&typed, m).map(|a| match a {
                Auto::Auto => "auto".to_string(),
                Auto::Set(n) => n.to_string(),
            }),
            _ => limits::parse_ram(&typed, m).map(|a| match a {
                Auto::Auto => "auto".to_string(),
                Auto::Set(mem) => limits::mem_text(mem),
            }),
        };
        match parsed {
            Ok(text) => {
                state.error = None;
                if text != stored {
                    actions.push(Action::SetSetting(def.key.into(), Value::Text(text)));
                }
            }
            Err(why) => state.error = Some((def.key.into(), why)),
        }
    }
    refused(ui, state, def.key, c);
}

/// One `[management_panel.lease]` row: a bare field holding the figure, its
/// ● ↺, and the share of this machine it is, like a limit's.
#[allow(clippy::too_many_arguments)]
fn lease_widget(
    ui: &mut egui::Ui,
    state: &mut SettingsState,
    machine: Option<&Machine>,
    def: &'static SettingDef,
    field: LimitField,
    value: &Value,
    actions: &mut Vec<Action>,
    c: Chrome,
) {
    let stored = value.as_str().unwrap_or_default().to_string();
    let Some(m) = machine else {
        return dim(ui, "detecting this machine…".into(), c);
    };
    let typed = ui
        .horizontal(|ui| {
            let typed = commit_field(ui, state, def.key, &stored);
            if *value != def.default_value() {
                changed_mark(ui, state, def, actions, c);
            }
            dim(ui, lease_share(&stored, m, field), c);
            typed
        })
        .inner;
    if let Some(typed) = typed {
        // `auto` (or nothing) here means the default lease.
        let parsed = match field {
            LimitField::Cores => limits::parse_cores(&typed, m).map(|a| match a {
                Auto::Auto => def.default_value().as_str().unwrap_or("3").to_string(),
                Auto::Set(n) => n.to_string(),
            }),
            _ => limits::parse_ram(&typed, m).map(|a| match a {
                Auto::Auto => def.default_value().as_str().unwrap_or("3G").to_string(),
                Auto::Set(mem) => limits::mem_text(mem),
            }),
        };
        match parsed {
            Ok(text) => {
                state.error = None;
                if text != stored {
                    actions.push(Action::SetSetting(def.key.into(), Value::Text(text)));
                }
            }
            Err(why) => state.error = Some((def.key.into(), why)),
        }
    }
    refused(ui, state, def.key, c);
}

/// What sits after a default-lease field: `of 16 cores · 19 %`,
/// `of 31.3G · 10 %`.
fn lease_share(stored: &str, m: &Machine, field: LimitField) -> String {
    match field {
        LimitField::Cores => {
            let n: u64 = stored.trim().parse().unwrap_or(0);
            format!(
                "of {} core{} · {} %",
                m.cores,
                if m.cores == 1 { "" } else { "s" },
                share_pct(n, m.cores.into())
            )
        }
        _ => {
            let mb = Mem::parse(stored).map_or(0, |x| x.0);
            format!("of {} · {} %", m.ram, share_pct(mb, m.ram.0))
        }
    }
}

/// The figure a cores or RAM limit comes to on `m`: `12`, `18.8G`.
fn limit_figure(limits: &Limits, m: &Machine, field: LimitField) -> String {
    let r = limits.resolve(m);
    match field {
        LimitField::Cores => r.cpu_cores.to_string(),
        _ => r.ram.to_string(),
    }
}

/// `part` as a whole percent of `whole` (0 when `whole` is).
fn share_pct(part: u64, whole: u64) -> u64 {
    if whole == 0 {
        return 0;
    }
    (part as f64 * 100.0 / whole as f64).round() as u64
}

/// What sits after a cores or RAM field: this machine's total and the
/// share the figure in force is of it — `of 16 cores · 88 %`,
/// `of 31.3G · 70 %`.
fn limit_share(limits: &Limits, m: &Machine, field: LimitField) -> String {
    let r = limits.resolve(m);
    match field {
        LimitField::Cores => format!(
            "of {} core{} · {} %",
            m.cores,
            if m.cores == 1 { "" } else { "s" },
            share_pct(r.cpu_cores.into(), m.cores.into())
        ),
        _ => format!("of {} · {} %", m.ram, share_pct(r.ram.0, m.ram.0)),
    }
}

/// `gpus`: a line per GPU the machine has. The file holds one list, so
/// setting one GPU writes them all — the rest at their auto figure — and a
/// list that comes back to every GPU at auto is written as `"auto"` again.
fn gpu_rows(
    ui: &mut egui::Ui,
    state: &mut SettingsState,
    limits: &Limits,
    machine: Option<&Machine>,
    def: &'static SettingDef,
    actions: &mut Vec<Action>,
    c: Chrome,
) {
    let Some(m) = machine else {
        return dim(ui, "detecting this machine…".into(), c);
    };
    let auto_list = Limits::default().resolve(m).gpus;
    let current: Vec<GpuLimit> = limits.gpus.get().cloned().unwrap_or(auto_list.clone());
    if m.gpus.is_empty() {
        // Shown only when set: then it needs its ↺.
        ui.horizontal(|ui| {
            dim(ui, "no GPU detected (nvidia-smi)".into(), c);
            if !matches!(limits.gpus, Auto::Auto) {
                changed_mark(ui, state, def, actions, c);
            }
        });
    }
    for (i, g) in m.gpus.iter().enumerate() {
        let first = i == 0;
        let key = format!("{}#{}", def.key, g.index);
        let set = current.iter().find(|l| l.index == g.index);
        // The VRAM in force, at auto too: the field never says `auto`.
        let stored = match set {
            Some(l) => l.vram.to_string(),
            None => "off".to_string(),
        };
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("{} {} {}", g.index, g.name, g.vram))
                    .font(FontId::monospace(11.0)),
            );
            let typed = commit_field(ui, state, &key, &stored);
            // One list in the file: one ● ↺, on the first GPU's line.
            if first && !matches!(limits.gpus, Auto::Auto) {
                changed_mark(ui, state, def, actions, c);
            }
            // The row already names the GPU's VRAM; say the share of it.
            if let Some(l) = set {
                dim(ui, format!("{} %", share_pct(l.vram.0, g.vram.0)), c);
            }
            if let Some(typed) = typed {
                let t = typed.trim().to_ascii_lowercase();
                let auto_vram = auto_list
                    .iter()
                    .find(|l| l.index == g.index)
                    .map(|l| l.vram);
                let vram = match t.as_str() {
                    "" | "auto" => Ok(auto_vram),
                    "off" | "none" | "0" => Ok(None),
                    _ => limits::parse_share_of(&t, g.vram, "VRAM").map(Some),
                };
                match vram {
                    Ok(vram) => {
                        state.error = None;
                        let mut next: Vec<GpuLimit> = current
                            .iter()
                            .filter(|l| l.index != g.index)
                            .cloned()
                            .collect();
                        if let Some(vram) = vram {
                            next.push(GpuLimit {
                                index: g.index,
                                vram,
                            });
                        }
                        next.sort_by_key(|l| l.index);
                        let text = if next == auto_list {
                            "auto".to_string()
                        } else {
                            limits::gpus_text(&next)
                        };
                        actions.push(Action::SetSetting(def.key.into(), Value::Text(text)));
                    }
                    Err(why) => state.error = Some((key.clone(), why)),
                }
            }
        });
        refused(ui, state, &key, c);
    }
    // Set in the file for a GPU this machine does not have.
    for l in current
        .iter()
        .filter(|l| !m.gpus.iter().any(|g| g.index == l.index))
    {
        dim(
            ui,
            format!("GPU {}: {} set, not detected", l.index, l.vram),
            c,
        );
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
    c: Chrome,
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
                    .color(c.dim),
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
                            .color(c.dim),
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
                    .color(c.dim),
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

fn keys_section(ui: &mut egui::Ui, c: Chrome) {
    ui.label(
        RichText::new("F1 shows this without leaving what you are doing.")
            .font(FontId::monospace(10.5))
            .color(c.dim),
    );
    ui.add_space(8.0);
    crate::keymap::table_ui(ui, "", c);
}

fn about_section(app: &App, ui: &mut egui::Ui, actions: &mut Vec<Action>, c: Chrome) {
    let line = |ui: &mut egui::Ui, k: &str, v: String| {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("{k:<10}"))
                    .font(FontId::monospace(11.5))
                    .color(c.dim),
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
    let link = |ui: &mut egui::Ui, label: &str, url: &str, hint: &str| {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("{label:<10}"))
                    .font(FontId::monospace(11.5))
                    .color(c.dim),
            );
            ui.hyperlink_to(RichText::new(url).font(FontId::monospace(11.5)), url)
                .on_hover_text(hint);
        });
    };
    link(
        ui,
        "repo",
        crate::update::REPO_URL,
        "the source, the issues, the releases",
    );
    link(
        ui,
        "support",
        crate::splash::SUPPORT,
        "the telegram group: ask, report, complain",
    );
    link(
        ui,
        "news",
        crate::splash::NEWS,
        "the telegram channel: what each release changed",
    );
    ui.add_space(10.0);
    if ui
        .button(RichText::new("open config.toml in a tab").font(FontId::monospace(11.5)))
        .clicked()
    {
        actions.push(Action::EditConfig);
    }
}

fn footer(ui: &mut egui::Ui, actions: &mut Vec<Action>, close: &mut bool, c: Chrome) {
    ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("/ search")
                    .font(FontId::monospace(10.5))
                    .color(c.dim),
            );
            ui.label(RichText::new("·").color(c.dim));
            if ui
                .link(
                    RichText::new("⇧⏎ edit config.toml")
                        .font(FontId::monospace(10.5))
                        .color(c.dim),
                )
                .clicked()
            {
                actions.push(Action::EditConfig);
            }
            ui.label(RichText::new("·").color(c.dim));
            if ui
                .link(
                    RichText::new("esc back")
                        .font(FontId::monospace(10.5))
                        .color(c.dim),
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
    fn account_dirs_are_remembered_not_just_inherited() {
        // The bug this guards: CLAUDE_CONFIG_DIR and CCTOP_CONFIG_DIRS come
        // from a shell rc, so the account list changed depending on whether
        // Giverny was started from a terminal or from the dock.
        let dir = std::env::temp_dir().join(format!("giverny-adopt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = giverny_core::state::Paths::at(&dir);
        let account = dir.join("envs/work/claude");
        std::fs::create_dir_all(&account).unwrap();
        std::fs::write(account.join(".claude.json"), r#"{"oauthAccount":{}}"#).unwrap();
        let not_an_account = dir.join("envs/empty");
        std::fs::create_dir_all(&not_an_account).unwrap();
        std::fs::write(
            giverny_core::config::config_path(paths.base()),
            giverny_core::settings::template(),
        )
        .unwrap();

        let mut cfg = giverny_core::config::Config::default();
        // SAFETY: single-threaded test. The real environment is set aside so
        // this asserts about the fixture rather than the developer's machine.
        let real = std::env::var_os("CLAUDE_CONFIG_DIR");
        unsafe {
            std::env::remove_var("CLAUDE_CONFIG_DIR");
            std::env::set_var(
                "CCTOP_CONFIG_DIRS",
                format!("{}:{}", account.display(), not_an_account.display()),
            );
        }
        crate::remember_env_accounts(&paths, &mut cfg);
        unsafe {
            std::env::remove_var("CCTOP_CONFIG_DIRS");
            if let Some(v) = real {
                std::env::set_var("CLAUDE_CONFIG_DIR", v);
            }
        }

        // The real one is kept; the empty directory is not silently adopted.
        assert!(cfg.behavior.extra_profile_dirs.contains(&account));
        assert!(!cfg.behavior.extra_profile_dirs.contains(&not_an_account));
        // And it is on disk, so the next launch finds it with no environment.
        let reloaded = giverny_core::config::load(paths.base());
        assert!(reloaded.behavior.extra_profile_dirs.contains(&account));
        let _ = std::fs::remove_dir_all(&dir);
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
        state.search = "titles.strip_host".into();
        assert_eq!(visible(&state).len(), 1);
        // The manager limits, by their table and by what they are.
        state.search = "limits".into();
        let keys: Vec<&str> = visible(&state).iter().map(|d| d.key).collect();
        assert_eq!(
            keys,
            [
                "manager.limits.cpu_cores",
                "manager.limits.ram",
                "manager.limits.gpus"
            ]
        );
        state.search = "vram".into();
        assert_eq!(visible(&state)[0].key, "manager.limits.gpus");
    }
}
