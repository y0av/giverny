//! `~/.config/giverny/config.toml` — user settings, written with comments on
//! first run and hot-reloaded when the file changes.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub font: FontConfig,
    pub theme: ThemeConfig,
    pub titles: TitlesConfig,
    pub behavior: BehaviorConfig,
    pub usage: UsageConfig,
    pub update: UpdateConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TitlesConfig {
    /// Drop a leading `user@host:` from titles the shell sets.
    pub strip_host_prefix: bool,
    /// Abbreviate every directory but the last: `~/Dev/bobo` → `~/D/bobo`.
    pub shorten_paths: bool,
}

impl Default for TitlesConfig {
    fn default() -> Self {
        TitlesConfig {
            strip_host_prefix: true,
            shorten_paths: false,
        }
    }
}

/// Tidy a title the shell set, for a rail that is 240px wide.
///
/// Applied at *display* time, never to the stored title: toggling either
/// option then takes effect on every existing tab at once, instead of only on
/// titles set afterwards.
pub fn display_title(raw: &str, cfg: &TitlesConfig) -> String {
    let mut out = raw;
    if cfg.strip_host_prefix {
        out = strip_host_prefix(out);
    }
    if cfg.shorten_paths {
        return shorten_paths(out);
    }
    out.to_string()
}

/// `yoz@yoz-framework:~/Dev/bobo` → `~/Dev/bobo`.
///
/// Narrow on purpose: only `name@host:` at the very start, where both parts
/// look like a name. `ssh: user@host` and titles that merely contain an `@`
/// are left alone.
fn strip_host_prefix(title: &str) -> &str {
    let Some(colon) = title.find(':') else {
        return title;
    };
    let (prefix, rest) = title.split_at(colon);
    let Some((user, host)) = prefix.split_once('@') else {
        return title;
    };
    let plain = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_'))
    };
    if plain(user) && plain(host) {
        rest[1..].trim_start()
    } else {
        title
    }
}

/// `~/Dev/claude_test/giverny` → `~/D/c/giverny`. Only the last segment keeps
/// its name — the one you are actually in.
fn shorten_paths(title: &str) -> String {
    title
        .split(' ')
        .map(|word| {
            if !word.contains('/') || word.len() < 12 {
                return word.to_string();
            }
            let parts: Vec<&str> = word.split('/').collect();
            let last = parts.len() - 1;
            parts
                .iter()
                .enumerate()
                .map(|(i, part)| {
                    if i == last || part.is_empty() || *part == "~" {
                        (*part).to_string()
                    } else {
                        part.chars().next().map(String::from).unwrap_or_default()
                    }
                })
                .collect::<Vec<_>>()
                .join("/")
        })
        .collect::<Vec<_>>()
        .join(" ")
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
            // Generated from the settings table, so the file can never
            // document an option the app does not have.
            let _ = std::fs::write(&path, crate::settings::template());
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
    fn host_prefix_is_stripped_only_when_it_really_is_one() {
        let cfg = TitlesConfig::default();
        // The actual shape oh-my-zsh sets, and the reason for the option.
        assert_eq!(
            display_title("yoz@yoz-framework:~/Dev/bobo", &cfg),
            "~/Dev/bobo"
        );
        assert_eq!(display_title("a@b: spaced", &cfg), "spaced");
        // Left alone: no colon, an @ that is not a prefix, and titles whose
        // prefix is not a plain name@host.
        for keep in [
            "✳ Claude Code",
            "btop",
            "ssh: user@host",
            "npm run build: watching",
            "git log --author=me@example.com",
            "~/Dev/bobo",
        ] {
            assert_eq!(
                display_title(keep, &cfg),
                keep,
                "{keep} should be untouched"
            );
        }
    }

    #[test]
    fn shortening_keeps_the_directory_you_are_in() {
        let cfg = TitlesConfig {
            strip_host_prefix: true,
            shorten_paths: true,
        };
        assert_eq!(
            display_title("yoz@host:~/Dev/claude_test/giverny", &cfg),
            "~/D/c/giverny"
        );
        // Short paths and non-paths are not worth mangling.
        assert_eq!(display_title("~/Dev", &cfg), "~/Dev");
        assert_eq!(display_title("btop", &cfg), "btop");
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
