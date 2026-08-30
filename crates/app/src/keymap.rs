//! Every binding, declared once.
//!
//! Rendered in two places — the `keys` section of settings and the `F1`
//! overlay — from this one table, so the two cannot drift apart. It also
//! feeds the docs.
//!
//! Read-only for now. Rebinding needs chord capture, conflict detection and,
//! the part that actually bites, a rule for when a binding may shadow a key
//! the shell or Claude needs; the table is shaped so that drops in behind it.

use eframe::egui::{self, FontId, Key, Modifiers, RichText};

use crate::{Action, App};

use crate::chrome::Chrome;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    /// Works anywhere in the window.
    Global,
    /// Handled by the terminal grid, before the key reaches the shell.
    Terminal,
    /// The rail: tabs and categories.
    Rail,
}

impl Scope {
    pub fn label(self) -> &'static str {
        match self {
            Scope::Global => "global",
            Scope::Terminal => "terminal",
            Scope::Rail => "rail",
        }
    }
}

pub struct Binding {
    pub chord: &'static str,
    pub action: &'static str,
    pub scope: Scope,
}

pub const BINDINGS: &[Binding] = &[
    Binding {
        chord: "Ctrl+Shift+T",
        action: "new tab in the active category",
        scope: Scope::Global,
    },
    Binding {
        chord: "Ctrl+Shift+W",
        action: "close the active tab",
        scope: Scope::Global,
    },
    Binding {
        chord: "Ctrl+Shift+A",
        action: "jump to the next tab where Claude needs you",
        scope: Scope::Global,
    },
    Binding {
        chord: "Ctrl+Shift+P",
        action: "fuzzy tab palette",
        scope: Scope::Global,
    },
    Binding {
        chord: "Ctrl+Tab / Ctrl+Shift+Tab",
        action: "back / forward through recently used tabs (hold Ctrl to keep going)",
        scope: Scope::Global,
    },
    Binding {
        chord: "Ctrl+PageUp / PageDown",
        action: "previous / next tab",
        scope: Scope::Global,
    },
    Binding {
        chord: "Ctrl+,",
        action: "settings",
        scope: Scope::Global,
    },
    Binding {
        chord: "F1",
        action: "this list",
        scope: Scope::Global,
    },
    Binding {
        chord: "Ctrl+Shift+F",
        action: "search scrollback (Enter / Shift+Enter to step)",
        scope: Scope::Terminal,
    },
    Binding {
        chord: "Ctrl+Shift+E",
        action: "label every path and URL on screen; letter opens, shift+letter types it",
        scope: Scope::Terminal,
    },
    Binding {
        chord: "Ctrl+Shift+C",
        action: "copy the selection",
        scope: Scope::Terminal,
    },
    Binding {
        chord: "Ctrl +  /  −  /  0",
        action: "font size, bigger / smaller / reset",
        scope: Scope::Terminal,
    },
    Binding {
        chord: "Ctrl+click",
        action: "open a path or URL under the cursor",
        scope: Scope::Terminal,
    },
    Binding {
        chord: "double / triple click",
        action: "select word / line (copies on release)",
        scope: Scope::Terminal,
    },
    Binding {
        chord: "F2  or  double-click",
        action: "rename a tab",
        scope: Scope::Rail,
    },
    Binding {
        chord: "middle-click",
        action: "close a tab",
        scope: Scope::Rail,
    },
    Binding {
        chord: "drag",
        action: "reorder, or move between categories",
        scope: Scope::Rail,
    },
    Binding {
        chord: "right-click",
        action: "tab and category menus (move, colour, account, sessions)",
        scope: Scope::Rail,
    },
];

fn matches(b: &Binding, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let needle = needle.to_lowercase();
    format!("{} {} {}", b.chord, b.action, b.scope.label())
        .to_lowercase()
        .contains(&needle)
}

/// The table itself, grouped by scope. Shared by both views.
pub fn table_ui(ui: &mut egui::Ui, filter: &str, c: Chrome) {
    let mut any = false;
    for scope in [Scope::Global, Scope::Terminal, Scope::Rail] {
        let rows: Vec<&Binding> = BINDINGS
            .iter()
            .filter(|b| b.scope == scope && matches(b, filter))
            .collect();
        if rows.is_empty() {
            continue;
        }
        any = true;
        ui.add_space(4.0);
        ui.label(
            RichText::new(scope.label())
                .font(FontId::monospace(10.5))
                .color(c.accent),
        );
        for b in rows {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{:<24}", b.chord)).font(FontId::monospace(11.5)));
                ui.label(
                    RichText::new(b.action)
                        .font(FontId::monospace(11.5))
                        .color(c.dim),
                );
            });
        }
    }
    if !any {
        ui.label(
            RichText::new("no matching keys")
                .font(FontId::monospace(11.0))
                .color(c.dim),
        );
    }
}

pub struct KeysOverlay {
    pub filter: String,
    pub needs_focus: bool,
}

impl Default for KeysOverlay {
    fn default() -> Self {
        KeysOverlay {
            filter: String::new(),
            needs_focus: true,
        }
    }
}

/// The `F1` view: the same table, without leaving what you were doing.
pub fn overlay_ui(app: &mut App, ctx: &egui::Context) -> Vec<Action> {
    let actions = Vec::new();
    let Some(mut state) = app.keys_overlay.take() else {
        return actions;
    };
    let c = app.chrome;
    let mut close = ctx.input_mut(|i| {
        i.consume_key(Modifiers::NONE, Key::Escape) || i.consume_key(Modifiers::NONE, Key::F1)
    });

    egui::Window::new("giverny-keys")
        .title_bar(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.set_width(520.0);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("keys")
                        .font(FontId::monospace(12.5))
                        .color(c.accent),
                );
                let field = ui.add(
                    egui::TextEdit::singleline(&mut state.filter)
                        .hint_text("filter")
                        .desired_width(200.0)
                        .font(FontId::monospace(11.5)),
                );
                if state.needs_focus {
                    field.request_focus();
                    state.needs_focus = false;
                }
            });
            ui.separator();
            egui::ScrollArea::vertical()
                .max_height(420.0)
                .show(ui, |ui| table_ui(ui, &state.filter, c));
            ui.separator();
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("esc close")
                        .font(FontId::monospace(10.0))
                        .color(c.dim),
                );
                if ui
                    .link(
                        RichText::new("· settings")
                            .font(FontId::monospace(10.0))
                            .color(c.dim),
                    )
                    .clicked()
                {
                    close = true;
                }
            });
        });

    if !close {
        app.keys_overlay = Some(state);
    }
    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_binding_is_findable_and_unique() {
        let mut seen = std::collections::HashSet::new();
        for b in BINDINGS {
            assert!(
                seen.insert((b.chord, b.scope)),
                "duplicate binding {} in {}",
                b.chord,
                b.scope.label()
            );
            assert!(!b.action.is_empty());
            assert!(matches(b, ""), "{} not shown unfiltered", b.chord);
        }
    }

    #[test]
    fn filter_matches_chord_and_action() {
        let tab = BINDINGS.iter().find(|b| b.chord == "Ctrl+Shift+T").unwrap();
        assert!(matches(tab, "ctrl+shift+t"));
        assert!(matches(tab, "new tab"));
        assert!(!matches(tab, "scrollback"));
    }
}
