//! Every user-facing option, declared once.
//!
//! Settings screens rot: an option lands in the struct, the UI never learns
//! about it, the docs disagree with both. So the table below is the single
//! declaration, and it generates the commented `config.toml` template, the
//! rows of the settings screen, and the options table in the docs. An option
//! that is not here is not in the app — and the tests prove it, by parsing the
//! generated template back into `Config` and comparing against the defaults.
//!
//! Reading stays with serde (`Config`); this module owns *presentation* and
//! *write-back*. Values are read out of a serialized `Config` by dotted path,
//! so a key that drifts away from the struct fails a test rather than silently
//! showing nothing.

use std::path::Path;

use crate::config::{self, Config};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Section {
    Appearance,
    Terminal,
    Titles,
    Restore,
    Claude,
    Keys,
    Updates,
    About,
}

impl Section {
    /// Rail order of the settings screen.
    pub const ALL: &'static [Section] = &[
        Section::Appearance,
        Section::Terminal,
        Section::Titles,
        Section::Restore,
        Section::Claude,
        Section::Keys,
        Section::Updates,
        Section::About,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Section::Appearance => "appearance",
            Section::Terminal => "terminal",
            Section::Titles => "tabs & titles",
            Section::Restore => "restore",
            Section::Claude => "claude",
            Section::Keys => "keys",
            Section::Updates => "updates",
            Section::About => "about",
        }
    }
}

/// What kind of value an option holds — picks the widget, the template
/// rendering, and how a written value is validated.
#[derive(Debug, Clone)]
pub enum Kind {
    Bool {
        default: bool,
    },
    /// Bounds are UI limits, not validation: a hand-edited file may hold
    /// anything, and the app clamps where it matters.
    Float {
        default: f64,
        min: f64,
        max: f64,
    },
    Int {
        default: i64,
        min: i64,
        max: i64,
    },
    Text {
        default: &'static str,
        /// Shown when the value is empty — usually what empty *means*.
        placeholder: &'static str,
    },
    Choice {
        default: &'static str,
        options: &'static [&'static str],
    },
    /// A list of strings, edited as a list (restore_apps, extra dirs).
    StringList {
        /// `None` = empty by default; `Some` supplies a non-empty default.
        default: Option<fn() -> Vec<String>>,
    },
}

#[derive(Debug, Clone)]
pub struct SettingDef {
    /// Dotted TOML path — also shown under the label, so the screen teaches
    /// the file.
    pub key: &'static str,
    pub label: &'static str,
    pub section: Section,
    /// One line, shown in the UI and as a comment in the template.
    pub doc: &'static str,
    /// Extra lines for the template only, where the *why* is worth having in
    /// the file but too long for a settings row.
    pub note: &'static [&'static str],
    /// Changing this does nothing until Giverny restarts. The screen says so
    /// once you have changed it, rather than looking broken.
    pub needs_restart: bool,
    pub kind: Kind,
}

impl SettingDef {
    /// `["font", "size"]`
    pub fn path(&self) -> impl Iterator<Item = &str> {
        self.key.split('.')
    }

    pub fn table(&self) -> &str {
        self.key.split('.').next().unwrap_or(self.key)
    }

    pub fn leaf(&self) -> &str {
        self.key.rsplit('.').next().unwrap_or(self.key)
    }

    pub fn default_value(&self) -> Value {
        match &self.kind {
            Kind::Bool { default } => Value::Bool(*default),
            Kind::Float { default, .. } => Value::Float(*default),
            Kind::Int { default, .. } => Value::Int(*default),
            Kind::Text { default, .. } => Value::Text((*default).into()),
            Kind::Choice { default, .. } => Value::Text((*default).into()),
            Kind::StringList { default } => Value::List(default.map(|f| f()).unwrap_or_default()),
        }
    }
}

/// A value moving between the UI, the config file and `Config`.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    Float(f64),
    Int(i64),
    Text(String),
    List(Vec<String>),
}

impl Value {
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Float(f) => Some(*f),
            Value::Int(i) => Some(*i as f64),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            Value::Float(f) => Some(*f as i64),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[String]> {
        match self {
            Value::List(v) => Some(v),
            _ => None,
        }
    }
}

fn default_restore_apps() -> Vec<String> {
    crate::procs::DEFAULT_RESTORE_APPS
        .iter()
        .map(|s| s.to_string())
        .collect()
}

pub const SETTINGS: &[SettingDef] = &[
    SettingDef {
        key: "font.family",
        label: "font family",
        section: Section::Appearance,
        doc: "Preferred monospace family; empty auto-detects.",
        note: &["Applied at startup: the glyph atlas is built once."],
        needs_restart: true,
        kind: Kind::Text {
            default: "",
            placeholder: "auto-detect",
        },
    },
    SettingDef {
        key: "font.size",
        label: "font size",
        section: Section::Appearance,
        doc: "Point size of the terminal grid.",
        note: &["Ctrl +/-/0 changes this live and writes it back here."],
        needs_restart: false,
        kind: Kind::Float {
            default: 13.0,
            min: 6.0,
            max: 40.0,
        },
    },
    SettingDef {
        key: "theme.name",
        label: "theme",
        section: Section::Appearance,
        doc: "Colour theme for the grid and the chrome around it.",
        note: &[],
        needs_restart: false,
        // Kept in step with `Theme::NAMES` by a test in the app crate —
        // core cannot see the themes, so the check lives where both are.
        kind: Kind::Choice {
            default: "monet-dark",
            options: &[
                "monet-dark",
                "monet-light",
                "ink",
                "tokyo-night",
                "gruvbox",
                "nord",
                "catppuccin",
            ],
        },
    },
    SettingDef {
        key: "titles.strip_host_prefix",
        label: "strip user@host:",
        section: Section::Titles,
        doc: "Drop the `user@host:` your shell puts in front of every title.",
        note: &["The rail is narrow and that prefix is the same on every tab."],
        needs_restart: false,
        kind: Kind::Bool { default: true },
    },
    SettingDef {
        key: "titles.shorten_paths",
        label: "shorten paths",
        section: Section::Titles,
        doc: "Abbreviate every directory but the last: ~/Dev/bobo becomes ~/D/bobo.",
        note: &[],
        needs_restart: false,
        kind: Kind::Bool { default: false },
    },
    SettingDef {
        key: "behavior.scrollback_lines",
        label: "scrollback lines",
        section: Section::Terminal,
        doc: "Lines kept above the screen, per tab.",
        note: &[],
        needs_restart: false,
        kind: Kind::Int {
            default: 10_000,
            min: 0,
            max: 1_000_000,
        },
    },
    SettingDef {
        key: "behavior.notifications",
        label: "desktop notifications",
        section: Section::Terminal,
        doc: "Notify when Claude needs you in a background tab.",
        note: &[],
        needs_restart: false,
        kind: Kind::Bool { default: true },
    },
    SettingDef {
        key: "behavior.prefer_x11",
        label: "prefer X11 (Linux)",
        section: Section::Terminal,
        doc: "Run under X11/XWayland instead of Wayland.",
        note: &[
            "Rarely needed. Drag-and-drop works on Wayland now, so this is",
            "only for working around a Wayland driver or compositor problem.",
            "The cost: under XWayland, text is softer at fractional scaling.",
            "Ignored where there is no X server.",
        ],
        needs_restart: true,
        kind: Kind::Bool { default: false },
    },
    SettingDef {
        key: "behavior.restore_claude",
        label: "resume conversations",
        section: Section::Restore,
        doc: "Re-run `claude --resume` in restored tabs.",
        note: &[],
        needs_restart: false,
        kind: Kind::Choice {
            default: "auto",
            options: &["auto", "prompt", "off"],
        },
    },
    SettingDef {
        key: "behavior.restore_apps",
        label: "programs to restart",
        section: Section::Restore,
        doc: "Full-screen programs a restored tab may start again by itself.",
        note: &[
            "Anything not listed is remembered but never re-run: replaying an",
            "arbitrary last command could deploy, delete or push something.",
        ],
        needs_restart: false,
        kind: Kind::StringList {
            default: Some(default_restore_apps),
        },
    },
    SettingDef {
        key: "behavior.extra_profile_dirs",
        label: "account directories",
        section: Section::Claude,
        doc: "Account directories kept somewhere Giverny would not find on its own.",
        note: &[
            "Found automatically: ~/.claude, $CLAUDE_CONFIG_DIR, and claude*",
            "directories in ~ and ~/.config. Anything elsewhere goes here.",
            "Dirs named by the environment are copied here the first time they",
            "are seen, so the account list does not depend on whether Giverny",
            "was started from a shell or from a launcher.",
        ],
        needs_restart: true,
        kind: Kind::StringList { default: None },
    },
    SettingDef {
        key: "claude.auto_mode",
        label: "start Claude in auto mode",
        section: Section::Claude,
        doc: "Every new Claude session starts in auto mode instead of asking for each permission.",
        note: &[
            "Written as `permissions.defaultMode = \"auto\"` into each account's",
            "settings.json — Claude Code's own setting, so it applies however you",
            "start it, not only to sessions Giverny launches.",
            "Sessions already running keep the mode they were started with.",
            "Turning it off removes the key again, unless you have since set a",
            "different mode by hand.",
        ],
        needs_restart: false,
        kind: Kind::Bool { default: false },
    },
    SettingDef {
        key: "claude.skip_resume_summary",
        label: "resume conversations whole",
        section: Section::Claude,
        doc: "Skip Claude Code's offer to resume from a summary, and resume the full session.",
        note: &[
            "Resuming a session over 70 minutes old and 100k tokens, Claude Code",
            "asks whether to `Resume from summary (recommended)` or `Resume full",
            "session as-is`. This answers as-is, every time, by raising the",
            "thresholds it checks (CLAUDE_CODE_RESUME_THRESHOLD_MINUTES and",
            "CLAUDE_CODE_RESUME_TOKEN_THRESHOLD) for tabs Giverny spawns.",
            "The full transcript costs more of your limits than a summary does —",
            "which is exactly what that prompt is warning about.",
        ],
        needs_restart: false,
        kind: Kind::Bool { default: false },
    },
    SettingDef {
        key: "usage.refresh_minutes",
        label: "usage refresh",
        section: Section::Claude,
        doc: "Ask Claude Code to refresh an account once its numbers are this old. 0 never asks.",
        note: &[
            "Runs `claude -p /usage`, and no more often than this per account.",
            "The caches themselves are re-read every 60s regardless, plus",
            "immediately after a refresh; statusline pushes land as they arrive.",
        ],
        needs_restart: false,
        kind: Kind::Int {
            default: 10,
            min: 0,
            max: 1440,
        },
    },
    SettingDef {
        key: "update.check",
        label: "check for updates",
        section: Section::Updates,
        doc: "Ask GitHub once a day whether a newer Giverny exists.",
        note: &[
            "The only network request Giverny ever makes - set false and it",
            "makes none. GIVERNY_NO_UPDATE in the environment also disables it.",
        ],
        needs_restart: false,
        kind: Kind::Bool { default: true },
    },
];

pub fn by_key(key: &str) -> Option<&'static SettingDef> {
    SETTINGS.iter().find(|s| s.key == key)
}

pub fn in_section(section: Section) -> impl Iterator<Item = &'static SettingDef> {
    SETTINGS.iter().filter(move |s| s.section == section)
}

/// Current value of an option, read out of a live `Config`.
///
/// Goes through serde rather than a hand-written match per key: a key that no
/// longer matches the struct returns `None` here and fails the tests, instead
/// of quietly rendering a stale default.
pub fn current(cfg: &Config, def: &SettingDef) -> Option<Value> {
    let doc = toml::Value::try_from(cfg).ok()?;
    let mut node = &doc;
    for part in def.path() {
        node = node.get(part)?;
    }
    Some(match (node, &def.kind) {
        (toml::Value::Boolean(b), _) => Value::Bool(*b),
        (toml::Value::Float(f), _) => Value::Float(*f),
        (toml::Value::Integer(i), Kind::Float { .. }) => Value::Float(*i as f64),
        (toml::Value::Integer(i), _) => Value::Int(*i),
        (toml::Value::String(s), _) => Value::Text(s.clone()),
        (toml::Value::Array(a), _) => Value::List(
            a.iter()
                .map(|v| match v {
                    toml::Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect(),
        ),
        _ => return None,
    })
}

/// Is this option still at its default?
pub fn is_default(cfg: &Config, def: &SettingDef) -> bool {
    current(cfg, def).is_some_and(|v| v == def.default_value())
}

fn toml_edit_value(value: &Value) -> toml_edit::Value {
    match value {
        Value::Bool(b) => (*b).into(),
        Value::Float(f) => (*f).into(),
        Value::Int(i) => (*i).into(),
        Value::Text(s) => s.as_str().into(),
        Value::List(items) => {
            let mut arr = toml_edit::Array::new();
            for item in items {
                arr.push(item.as_str());
            }
            // Long lists wrap; short ones stay on one line.
            if items.len() > 6 {
                for item in arr.iter_mut() {
                    item.decor_mut().set_prefix("\n    ");
                }
                arr.set_trailing("\n");
            }
            toml_edit::Value::Array(arr)
        }
    }
}

/// Write one option back to `config.toml`, in place.
///
/// Format-preserving on purpose: the file ships full of comments explaining
/// what each key does, and users add their own. A settings screen that
/// serializes the whole struct over the top would delete all of it — the
/// mistake Windows Terminal explicitly designed around.
pub fn write(base: &Path, def: &SettingDef, value: &Value) -> anyhow::Result<()> {
    let path = config::config_path(base);
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: toml_edit::DocumentMut = text.parse()?;

    // Walk (creating) the tables above the leaf.
    let mut node = doc.as_table_mut();
    let parts: Vec<&str> = def.path().collect();
    for part in &parts[..parts.len() - 1] {
        if !node.contains_key(part) {
            let mut table = toml_edit::Table::new();
            table.set_implicit(false);
            node.insert(part, toml_edit::Item::Table(table));
        }
        node = node
            .get_mut(part)
            .and_then(|item| item.as_table_mut())
            .ok_or_else(|| anyhow::anyhow!("{} is not a table in config.toml", part))?;
    }

    let leaf = parts[parts.len() - 1];
    match node.get_mut(leaf) {
        Some(item) => {
            let slot = item.as_value_mut().ok_or_else(|| {
                anyhow::anyhow!("{} is not a plain value in config.toml", def.key)
            })?;
            // The decor is the whitespace and comments *around* the value —
            // `size = 11.0  # deliberately small`. Replacing the value alone
            // would take the user's note with it.
            let decor = slot.decor().clone();
            *slot = toml_edit_value(value);
            *slot.decor_mut() = decor;
        }
        None => {
            node.insert(leaf, toml_edit::Item::Value(toml_edit_value(value)));
        }
    }

    write_atomic(&path, doc.to_string().as_bytes())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn render_value(value: &Value) -> String {
    toml_edit_value(value).to_string().trim().to_string()
}

/// The commented `config.toml` written on first run, generated from the table
/// above so it can never describe options the app does not have.
pub fn template() -> String {
    let mut out = String::from(
        "# Giverny configuration.\n\
         # Edit and save — the app picks changes up without restarting.\n\
         # Every option here is also in the settings screen (Ctrl+,).\n",
    );
    let mut current_table = "";
    for def in SETTINGS {
        if def.table() != current_table {
            current_table = def.table();
            out.push_str(&format!("\n[{current_table}]\n"));
        }
        out.push_str(&format!("# {}\n", def.doc));
        for line in def.note {
            out.push_str(&format!("# {line}\n"));
        }
        if def.needs_restart {
            out.push_str("# Takes effect when Giverny restarts.\n");
        }
        if let Kind::Choice { options, .. } = &def.kind {
            out.push_str(&format!("# One of: {}\n", options.join(" | ")));
        }
        let default = def.default_value();
        match &default {
            // A long default list would swamp the file, and writing it out
            // invites editing one entry when the whole list is what counts.
            // Absent means default, so show the shape instead.
            Value::List(items) if items.len() > 6 => {
                let example = Value::List(items.iter().take(3).cloned().collect());
                out.push_str(&format!(
                    "# Setting this replaces the default list of {}. For example:\n# {} = {}\n",
                    items.len(),
                    def.leaf(),
                    render_value(&example)
                ));
            }
            _ => out.push_str(&format!("{} = {}\n", def.leaf(), render_value(&default))),
        }
    }
    out
}

/// The options table for the docs — the third thing generated from the
/// schema, so `docs/options.md` cannot describe a different app than the one
/// that ships. A test compares it against the checked-in file.
pub fn markdown() -> String {
    let mut out = String::from(
        "# Options\n\n\
         <!-- Generated from crates/core/src/settings.rs.\n     \
         Regenerate: cargo run -p giverny-core --example options -->\n\n\
         Everything in `~/.config/giverny/config.toml`, and everything in the \
         settings screen (`Ctrl+,`) — they are the same list.\n\n\
         | Key | Default | What it does |\n|---|---|---|\n",
    );
    for def in SETTINGS {
        let default = match def.default_value() {
            Value::List(items) if items.len() > 6 => format!("{} programs", items.len()),
            other => format!("`{}`", render_value(&other)),
        };
        let mut doc = def.doc.replace('|', "\\|");
        if let Kind::Choice { options, .. } = &def.kind {
            doc.push_str(&format!(" One of: {}.", options.join(", ")));
        }
        if def.needs_restart {
            doc.push_str(" Takes effect on restart.");
        }
        out.push_str(&format!("| `{}` | {} | {} |\n", def.key, default, doc));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_template_parses_to_exactly_the_defaults() {
        // The canary for schema drift: if a default here disagrees with the
        // struct, or a key does not exist, this fails.
        let text = template();
        let parsed: Config = toml::from_str(&text).expect("template is valid toml");
        let defaults = Config::default();
        assert_eq!(parsed.font.size, defaults.font.size);
        assert_eq!(parsed.font.family, defaults.font.family);
        assert_eq!(parsed.theme.name, defaults.theme.name);
        assert_eq!(
            parsed.behavior.restore_claude,
            defaults.behavior.restore_claude
        );
        assert_eq!(
            parsed.behavior.notifications,
            defaults.behavior.notifications
        );
        assert_eq!(
            parsed.behavior.scrollback_lines,
            defaults.behavior.scrollback_lines
        );
        assert_eq!(parsed.behavior.restore_apps, defaults.behavior.restore_apps);
        assert_eq!(
            parsed.behavior.extra_profile_dirs,
            defaults.behavior.extra_profile_dirs
        );
        assert_eq!(parsed.usage.refresh_minutes, defaults.usage.refresh_minutes);
        assert_eq!(parsed.update.check, defaults.update.check);
    }

    #[test]
    fn every_option_resolves_against_a_live_config() {
        // `deny_unknown_fields` catches keys the struct lacks; this catches
        // keys the struct has under a different path.
        let cfg = Config::default();
        for def in SETTINGS {
            let value = current(&cfg, def);
            assert!(value.is_some(), "{} does not resolve", def.key);
            assert_eq!(
                value.unwrap(),
                def.default_value(),
                "{} default disagrees with Config::default()",
                def.key
            );
            assert!(is_default(&cfg, def), "{} not seen as default", def.key);
        }
    }

    #[test]
    fn the_docs_table_is_in_step_with_the_schema() {
        // The generated docs are checked in so they are browsable on GitHub;
        // this is what stops them describing an older set of options.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/options.md");
        let checked_in = std::fs::read_to_string(path).unwrap_or_default();
        assert_eq!(
            checked_in,
            markdown(),
            "docs/options.md is stale — regenerate with \
             `cargo run -p giverny-core --example options`"
        );
    }

    #[test]
    fn keys_are_unique_and_grouped_by_table() {
        let mut seen = std::collections::HashSet::new();
        for def in SETTINGS {
            assert!(seen.insert(def.key), "duplicate key {}", def.key);
            assert!(def.key.contains('.'), "{} needs a table", def.key);
        }
        // The template writes one [table] header per run of keys, so entries
        // sharing a table must be adjacent.
        let mut tables: Vec<&str> = Vec::new();
        for def in SETTINGS {
            if tables.last() != Some(&def.table()) {
                assert!(
                    !tables.contains(&def.table()),
                    "{} is split across the table list",
                    def.table()
                );
                tables.push(def.table());
            }
        }
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("giverny-set-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn writing_a_value_keeps_every_comment() {
        let dir = scratch("comments");
        std::fs::write(config::config_path(&dir), template()).unwrap();
        let before = std::fs::read_to_string(config::config_path(&dir)).unwrap();

        write(&dir, by_key("font.size").unwrap(), &Value::Float(16.0)).unwrap();
        write(&dir, by_key("update.check").unwrap(), &Value::Bool(false)).unwrap();

        let after = std::fs::read_to_string(config::config_path(&dir)).unwrap();
        let comments = |s: &str| {
            s.lines()
                .filter(|l| l.trim_start().starts_with('#'))
                .count()
        };
        assert_eq!(
            comments(&before),
            comments(&after),
            "a comment was lost:\n{after}"
        );

        let cfg: Config = toml::from_str(&after).unwrap();
        assert_eq!(cfg.font.size, 16.0);
        assert!(!cfg.update.check);
        assert_eq!(cfg.theme.name, "monet-dark", "untouched keys survive");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn writing_preserves_a_users_own_comments_and_layout() {
        let dir = scratch("user-comments");
        std::fs::write(
            config::config_path(&dir),
            "# my notes\n[font]\nsize = 11.0  # deliberately small\n",
        )
        .unwrap();

        write(&dir, by_key("font.size").unwrap(), &Value::Float(12.0)).unwrap();

        let after = std::fs::read_to_string(config::config_path(&dir)).unwrap();
        assert!(
            after.contains("# my notes"),
            "leading comment lost: {after}"
        );
        assert!(
            after.contains("# deliberately small"),
            "trailing comment lost: {after}"
        );
        assert!(after.contains("12"), "value not written: {after}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn writing_into_a_file_missing_the_table_creates_it() {
        let dir = scratch("missing");
        std::fs::write(config::config_path(&dir), "[font]\nsize = 13.0\n").unwrap();
        write(
            &dir,
            by_key("usage.refresh_minutes").unwrap(),
            &Value::Int(30),
        )
        .unwrap();
        let cfg: Config =
            toml::from_str(&std::fs::read_to_string(config::config_path(&dir)).unwrap()).unwrap();
        assert_eq!(cfg.usage.refresh_minutes, 30);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lists_round_trip() {
        let dir = scratch("lists");
        std::fs::write(config::config_path(&dir), template()).unwrap();
        let apps = vec!["btop".to_string(), "k9s".to_string()];
        write(
            &dir,
            by_key("behavior.restore_apps").unwrap(),
            &Value::List(apps.clone()),
        )
        .unwrap();
        let cfg: Config =
            toml::from_str(&std::fs::read_to_string(config::config_path(&dir)).unwrap()).unwrap();
        assert_eq!(cfg.behavior.restore_apps, apps);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
