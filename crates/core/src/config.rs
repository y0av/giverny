//! `~/.config/giverny/config.toml` — user settings, written with comments on
//! first run and hot-reloaded when the file changes.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub font: FontConfig,
    pub theme: ThemeConfig,
    pub behavior: BehaviorConfig,
    pub usage: UsageConfig,
    pub update: UpdateConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UsageConfig {
    /// Ask Claude Code to refresh its usage cache (`claude -p /usage`) when
    /// an account's numbers are older than this. 0 disables it, leaving the
    /// panel dependent on whatever Claude last wrote.
    pub refresh_minutes: u64,
}

impl Default for UsageConfig {
    fn default() -> Self {
        UsageConfig {
            refresh_minutes: 10,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UpdateConfig {
    /// Ask GitHub once a day whether a newer release exists. This is the
    /// only network request Giverny makes; set false to make it zero.
    pub check: bool,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        UpdateConfig { check: true }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FontConfig {
    /// Preferred family; empty = auto-detect a monospace font.
    pub family: String,
    pub size: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThemeConfig {
    /// Built-in theme name: `monet-dark`, `monet-light`, `ink`.
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BehaviorConfig {
    /// Re-run `claude --resume` for restored tabs: `auto`, `prompt`, `off`.
    pub restore_claude: RestoreClaude,
    /// Desktop notifications when Claude needs you.
    pub notifications: bool,
    /// Scrollback lines kept per tab.
    pub scrollback_lines: usize,
    /// Extra `CLAUDE_CONFIG_DIR`s to treat as accounts.
    pub extra_profile_dirs: Vec<PathBuf>,
    /// Programs a restored tab may start again by itself. Anything not
    /// listed is remembered but never re-run — replaying an arbitrary last
    /// command could deploy, delete or push something.
    pub restore_apps: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RestoreClaude {
    Auto,
    Prompt,
    Off,
}

impl Default for FontConfig {
    fn default() -> Self {
        FontConfig {
            family: String::new(),
            size: 13.0,
        }
    }
}

impl Default for ThemeConfig {
    fn default() -> Self {
        ThemeConfig {
            name: "monet-dark".into(),
        }
    }
}

impl Default for BehaviorConfig {
    fn default() -> Self {
        BehaviorConfig {
            restore_claude: RestoreClaude::Auto,
            notifications: true,
            scrollback_lines: 10_000,
            extra_profile_dirs: Vec::new(),
            restore_apps: crate::procs::DEFAULT_RESTORE_APPS
                .iter()
                .map(|s| s.to_string())
                .collect(),
        }
    }
}

const TEMPLATE: &str = r#"# Giverny configuration.
# Edit and save — the app picks changes up without restarting.

[font]
# family = "JetBrainsMono Nerd Font"   # empty = auto-detect
family = ""
size = 13.0

[theme]
# monet-dark | monet-light | ink
name = "monet-dark"

[behavior]
# Re-run `claude --resume` in restored tabs: auto | prompt | off
restore_claude = "auto"
notifications = true
scrollback_lines = 10000
# Extra CLAUDE_CONFIG_DIRs to show as accounts (beyond ~/.claude and
# anything in $CCTOP_CONFIG_DIRS).
extra_profile_dirs = []
# Full-screen programs a restored tab may relaunch on its own (btop, k9s,
# lazygit and friends by default). Everything else is remembered but never
# re-run, since replaying a last command could deploy or delete something.
# restore_apps = ["btop", "htop", "k9s"]

[usage]
# Refresh account usage by asking Claude Code to update its own cache
# (runs `claude -p /usage` for accounts whose numbers are older than this,
# and no more often than that per account). Set 0 to never refresh.
# The caches themselves are re-read every 60s regardless, plus immediately
# after a refresh; live statusline pushes land the moment they arrive.
refresh_minutes = 10

[update]
# Ask GitHub once a day whether a newer Giverny exists. This is the only
# network request Giverny ever makes — set false and it makes none.
# (Setting GIVERNY_NO_UPDATE in the environment also disables it.)
check = true
"#;

pub fn config_path(base: &Path) -> PathBuf {
    base.join("config.toml")
}

/// Load the config, writing the commented template on first run. Invalid
/// files are reported and ignored rather than blocking startup.
pub fn load(base: &Path) -> Config {
    let path = config_path(base);
    match std::fs::read_to_string(&path) {
        Ok(text) => match toml::from_str::<Config>(&text) {
            Ok(cfg) => cfg,
            Err(err) => {
                tracing::error!("config.toml ignored ({err}); using defaults");
                Config::default()
            }
        },
        Err(_) => {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&path, TEMPLATE);
            Config::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("giverny-cfg-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn first_run_writes_template_that_parses_to_defaults() {
        let dir = scratch("first");
        let cfg = load(&dir);
        assert!(config_path(&dir).exists(), "template written");
        assert_eq!(cfg.font.size, 13.0);
        assert_eq!(cfg.behavior.restore_claude, RestoreClaude::Auto);

        // The template on disk must itself be valid and match the defaults.
        let reparsed = load(&dir);
        assert_eq!(reparsed.theme.name, cfg.theme.name);
        assert_eq!(reparsed.behavior.scrollback_lines, 10_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn partial_config_keeps_defaults_for_the_rest() {
        let dir = scratch("partial");
        std::fs::write(config_path(&dir), "[font]\nsize = 16.5\n").unwrap();
        let cfg = load(&dir);
        assert_eq!(cfg.font.size, 16.5);
        assert_eq!(cfg.theme.name, "monet-dark", "unspecified sections default");
        assert!(cfg.behavior.notifications);
        assert!(cfg.update.check);
        assert_eq!(cfg.usage.refresh_minutes, 10);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn broken_config_falls_back_instead_of_failing() {
        let dir = scratch("broken");
        std::fs::write(config_path(&dir), "this is not toml {{{").unwrap();
        let cfg = load(&dir);
        assert_eq!(cfg.font.size, 13.0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
