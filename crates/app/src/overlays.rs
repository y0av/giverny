//! Overlay windows: the fuzzy tab palette (Ctrl+Shift+P) and the past-session
//! picker (right-click a tab → sessions…).

use eframe::egui::{self, Align2, Color32, FontId, Key, Modifiers, RichText};
use giverny_claude::registry::PastSession;
use giverny_core::tabs::TabId;

use crate::{Action, App};

const DIM: Color32 = Color32::from_rgb(0x6b, 0x78, 0x80);
const AMBER: Color32 = Color32::from_rgb(0xd9, 0xb5, 0x5f);

// ---- fuzzy tab palette -----------------------------------------------------

pub struct PaletteState {
    pub query: String,
    pub selected: usize,
    pub needs_focus: bool,
}

impl Default for PaletteState {
    fn default() -> Self {
        Self {
            query: String::new(),
            selected: 0,
            needs_focus: true,
        }
    }
}

/// Subsequence fuzzy match; higher is better, `None` = no match.
pub fn fuzzy_score(needle: &str, hay: &str) -> Option<i32> {
    if needle.is_empty() {
        return Some(0);
    }
    let hay_lc: Vec<char> = hay.to_lowercase().chars().collect();
    let mut score = 0i32;
    let mut pos = 0usize;
    let mut last_hit: Option<usize> = None;
    for nc in needle.to_lowercase().chars() {
        let found = hay_lc[pos..].iter().position(|&hc| hc == nc)?;
        let idx = pos + found;
        score += 2;
        if last_hit == Some(idx.wrapping_sub(1)) {
            score += 3; // consecutive run
        }
        if idx == 0 || !hay_lc[idx - 1].is_alphanumeric() {
            score += 2; // word start
        }
        last_hit = Some(idx);
        pos = idx + 1;
    }
    Some(score - (hay_lc.len() as i32 / 8))
}

pub fn palette_ui(app: &mut App, ctx: &egui::Context) -> Vec<Action> {
    let mut actions = Vec::new();
    let Some(mut st) = app.palette.take() else {
        return actions;
    };

    let mut close = false;
    let mut commit = false;
    ctx.input_mut(|i| {
        if i.consume_key(Modifiers::NONE, Key::Escape) {
            close = true;
        }
        if i.consume_key(Modifiers::NONE, Key::Enter) {
            commit = true;
        }
        if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
            st.selected = st.selected.saturating_add(1);
        }
        if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
            st.selected = st.selected.saturating_sub(1);
        }
    });

    let mut items: Vec<(TabId, String, i32)> = app
        .ws
        .tabs
        .iter()
        .filter_map(|t| {
            let cat = app
                .ws
                .category(t.category)
                .map(|c| c.name.clone())
                .unwrap_or_default();
            let cwd = t
                .cwd
                .as_deref()
                .map(|p| giverny_core::short_path(p, 28))
                .unwrap_or_default();
            let label = format!("{cat} › {}   {cwd}", t.title());
            fuzzy_score(&st.query, &label).map(|s| (t.id, label, s))
        })
        .collect();
    if !st.query.is_empty() {
        items.sort_by_key(|(_, _, s)| -s);
    }
    items.truncate(12);
    if !items.is_empty() {
        st.selected = st.selected.min(items.len() - 1);
    }

    if commit {
        if let Some((id, ..)) = items.get(st.selected) {
            actions.push(Action::Select(*id));
        }
        close = true;
    }

    egui::Window::new("giverny-palette")
        .title_bar(false)
        .resizable(false)
        .anchor(Align2::CENTER_TOP, [0.0, 90.0])
        .show(ctx, |ui| {
            ui.set_width(420.0);
            let te = ui.add(
                egui::TextEdit::singleline(&mut st.query)
                    .hint_text("jump to tab…")
                    .desired_width(f32::INFINITY)
                    .font(FontId::monospace(13.0)),
            );
            if st.needs_focus {
                te.request_focus();
                st.needs_focus = false;
            }
            if te.changed() {
                st.selected = 0;
            }
            ui.add_space(4.0);
            for (i, (id, label, _)) in items.iter().enumerate() {
                let resp = ui.selectable_label(
                    i == st.selected,
                    RichText::new(label).font(FontId::monospace(12.0)),
                );
                if resp.clicked() {
                    actions.push(Action::Select(*id));
                    close = true;
                }
            }
            if items.is_empty() {
                ui.label(
                    RichText::new("no matches")
                        .font(FontId::monospace(11.0))
                        .color(DIM),
                );
            }
        });

    if !close {
        app.palette = Some(st);
    }
    actions
}

// ---- past-session picker ---------------------------------------------------

pub struct SessionPicker {
    pub tab: TabId,
    pub sessions: Vec<PastSession>,
}

pub fn sessions_ui(app: &mut App, ctx: &egui::Context) -> Vec<Action> {
    let mut actions = Vec::new();
    let Some(picker) = app.session_picker.take() else {
        return actions;
    };

    let mut close = false;
    ctx.input_mut(|i| {
        if i.consume_key(Modifiers::NONE, Key::Escape) {
            close = true;
        }
    });

    egui::Window::new("giverny-sessions")
        .title_bar(false)
        .resizable(false)
        .anchor(Align2::CENTER_TOP, [0.0, 90.0])
        .show(ctx, |ui| {
            ui.set_width(460.0);
            ui.label(
                RichText::new("RESUME A CONVERSATION")
                    .font(FontId::monospace(10.0))
                    .color(DIM),
            );
            ui.add_space(4.0);
            if picker.sessions.is_empty() {
                ui.label(
                    RichText::new("no past sessions in this directory")
                        .font(FontId::monospace(11.5))
                        .color(DIM),
                );
            }
            for s in &picker.sessions {
                let age = s
                    .modified
                    .and_then(|m| m.elapsed().ok())
                    .map(humanize)
                    .unwrap_or_default();
                let account = giverny_claude::profiles::find(&app.claude.profiles, &s.config_dir)
                    .map(|p| format!("@{}", p.name))
                    .unwrap_or_default();
                let suffix = if s.live { "  · live" } else { "" };
                let label = format!("{:<44} {age:>4} {account}{suffix}", truncate(&s.title, 44));
                let text = RichText::new(label)
                    .font(FontId::monospace(11.5))
                    .color(if s.live {
                        DIM
                    } else {
                        Color32::from_rgb(0xd7, 0xdd, 0xe2)
                    });
                let resp = ui
                    .add_enabled_ui(!s.live, |ui| ui.selectable_label(false, text))
                    .inner;
                if s.live {
                    resp.on_hover_text("already open in another terminal");
                } else if resp.clicked() {
                    actions.push(Action::ResumeSpecific(
                        picker.tab,
                        s.id.clone(),
                        s.config_dir.clone(),
                    ));
                    close = true;
                }
            }
            ui.add_space(2.0);
            ui.label(
                RichText::new("esc to close")
                    .font(FontId::monospace(9.5))
                    .color(AMBER),
            );
        });

    if !close {
        app.session_picker = Some(picker);
    }
    actions
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max - 1).collect::<String>() + "…"
}

fn humanize(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_prefers_word_starts_and_runs() {
        assert!(fuzzy_score("", "anything").is_some());
        assert!(fuzzy_score("xyz", "abc").is_none());
        let exact = fuzzy_score("api", "work › api server").unwrap();
        let scattered = fuzzy_score("api", "a-thing-with-p-and-i").unwrap();
        assert!(exact > scattered, "{exact} vs {scattered}");
    }
}
