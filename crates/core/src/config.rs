//! `~/.config/giverny/config.toml` — user settings, written with comments on
//! first run and hot-reloaded when the file changes.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub font: FontConfig,
    pub theme: ThemeConfig,
    pub window: WindowConfig,
    pub titles: TitlesConfig,
    pub behavior: BehaviorConfig,
    pub usage: UsageConfig,
    pub claude: ClaudeConfig,
    pub update: UpdateConfig,
}

/// How Claude Code itself is launched in a tab.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeConfig {
    /// Start every session in auto mode, by setting `permissions.defaultMode`
    /// in each account's `settings.json`.
    pub auto_mode: bool,
    /// Suppress Claude Code's "resume from summary / resume full session
    /// as-is" prompt, so a resumed conversation comes back whole.
    pub skip_resume_summary: bool,
    /// Pick a session back up when the usage window that stopped it reopens.
    pub resume_after_limit: bool,
    /// The `refreshInterval` written on Giverny's status line entry, in
    /// seconds; 0 writes none. Unset (the default) is not the same as set to
    /// the default: unset, an entry with no value gets
    /// [`Self::DEFAULT_STATUSLINE_REFRESH_S`] and a value set there by hand is
    /// left alone; set, this value is written to every account.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub statusline_refresh_seconds: Option<u64>,
}

impl ClaudeConfig {
    /// How often Claude Code reruns the status line while a session sits
    /// idle, unless the user says otherwise. Without a `refreshInterval` it
    /// runs only when the conversation changes, so the cold-cache warning
    /// would never show on an idle session, the only kind it is for. The
    /// cache it warns about expires after 5 minutes at the shortest, so 30 s
    /// late costs nothing, while a short tick would launch `giverny
    /// statusline` in every open session that often for no gain.
    pub const DEFAULT_STATUSLINE_REFRESH_S: u64 = 30;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
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
    if let Some(program) = program_title(out) {
        return program;
    }
    if cfg.shorten_paths {
        return shorten_paths(out);
    }
    out.to_string()
}

/// A title that is nothing but the path to the program is that program.
///
/// Windows sets a console's title to the command line that opened it, and
/// ConPTY passes that on, so a PowerShell tab announces itself as
/// `C:\WINDOWS\System32\WindowsPowerShell\v1.0\powershell.exe` — a rail's
/// width of path saying one word. Unconditional: no `[titles]` option makes a
/// tab want to be called that.
fn program_title(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let name = trimmed.rsplit(['\\', '/']).next()?;
    let stem = name
        .strip_suffix(".exe")
        .or_else(|| name.strip_suffix(".EXE"))?;
    // Only a bare path: anything with arguments is a title someone chose.
    (!stem.is_empty() && !trimmed.contains(char::is_whitespace)).then(|| stem.to_string())
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
#[serde(default)]
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
#[serde(default)]
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
#[serde(default)]
pub struct FontConfig {
    /// Preferred family; empty = auto-detect a monospace font.
    pub family: String,
    pub size: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeConfig {
    /// Built-in theme name: `monet-dark`, `monet-light`, `ink`.
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowConfig {
    /// How much of the window's background is painted: 1.0 is solid, lower
    /// lets the desktop show through. Text and anything a program colours
    /// stay solid either way.
    pub opacity: f32,
}

impl WindowConfig {
    /// The lowest opacity the app will paint at. Below it, text over a busy
    /// wallpaper stops being readable on a compositor that does not blur.
    pub const MIN_OPACITY: f32 = 0.5;

    /// `opacity` as the app uses it: inside the supported range whatever the
    /// file says, and solid when it says nothing a number can be made of.
    pub fn opacity(&self) -> f32 {
        if self.opacity.is_nan() {
            return 1.0;
        }
        self.opacity.clamp(Self::MIN_OPACITY, 1.0)
    }

    /// Whether the window has to be see-through at all.
    pub fn translucent(&self) -> bool {
        self.opacity() < 1.0
    }
}

impl Default for WindowConfig {
    fn default() -> Self {
        WindowConfig { opacity: 1.0 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BehaviorConfig {
    /// Re-run `claude --resume` for restored tabs: `auto`, `prompt`, `off`.
    pub restore_claude: RestoreClaude,
    /// Desktop notifications when Claude needs you.
    pub notifications: bool,
    /// Scrollback lines kept per tab.
    pub scrollback_lines: usize,
    /// Extra `CLAUDE_CONFIG_DIR`s to treat as accounts.
    pub extra_profile_dirs: Vec<PathBuf>,
    /// Ask winit for the X11 backend on Linux (drag-and-drop works there;
    /// Wayland has no drop support in winit). Softer text under XWayland.
    pub prefer_x11: bool,
    /// Programs a restored tab may start again by itself. Anything not
    /// listed is remembered but never re-run — replaying an arbitrary last
    /// command could deploy, delete or push something.
    pub restore_apps: Vec<String>,
    /// Which shell a new tab opens on Windows. Ignored everywhere else,
    /// where `$SHELL` answers the question.
    pub windows_shell: WindowsShell,
    /// Each tab's shell keeps its own history file, restored with the tab
    /// (bash, zsh, fish; set inside the tab's shell, no rc file written).
    pub history_per_tab: bool,
    /// With `history_per_tab`, bash also appends each command to the
    /// shell's usual history file.
    pub history_also_shared: bool,
}

/// The shell a Windows tab opens. `Auto` prefers WSL — where Claude Code and
/// unix tooling usually live — but only when a distribution is installed;
/// `wsl.exe` exists on every Windows whether or not there is anything behind
/// it, and a tab spawned into an empty one dies on an error message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WindowsShell {
    #[default]
    Auto,
    Wsl,
    Powershell,
    Cmd,
}

impl WindowsShell {
    pub fn as_str(self) -> &'static str {
        match self {
            WindowsShell::Auto => "auto",
            WindowsShell::Wsl => "wsl",
            WindowsShell::Powershell => "powershell",
            WindowsShell::Cmd => "cmd",
        }
    }
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
            prefer_x11: false,
            restore_claude: RestoreClaude::Auto,
            notifications: true,
            scrollback_lines: 10_000,
            extra_profile_dirs: Vec::new(),
            restore_apps: crate::procs::DEFAULT_RESTORE_APPS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            windows_shell: WindowsShell::Auto,
            history_per_tab: false,
            history_also_shared: false,
        }
    }
}

pub fn config_path(base: &Path) -> PathBuf {
    base.join("config.toml")
}

/// Dotted paths in `input` that `known` (the parsed config, re-serialized)
/// lacks. A key this build does not know — usually one a newer build wrote.
fn unknown_keys(input: &toml::Value, known: &toml::Value, prefix: &str, out: &mut Vec<String>) {
    let (Some(input), Some(known)) = (input.as_table(), known.as_table()) else {
        return;
    };
    for (key, value) in input {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match known.get(key) {
            Some(k) => unknown_keys(value, k, &path, out),
            None => out.push(path),
        }
    }
}

/// Config text, parsed as far as it goes.
#[derive(Debug, Clone)]
pub struct Parsed {
    pub config: Config,
    /// Keys this build does not know, dropped (dotted paths).
    pub unknown: Vec<String>,
    /// Keys whose value could not be used — the wrong type, say — each with
    /// why. Such a key keeps the value it had (`previous`'s), and every other
    /// key in the file still applies.
    pub invalid: Vec<(String, String)>,
}

impl Parsed {
    /// Did anything at or under `key` (a dotted path) fail to apply?
    pub fn invalid_under(&self, key: &str) -> bool {
        self.invalid.iter().any(|(path, _)| {
            path == key
                || path.starts_with(&format!("{key}."))
                || key.starts_with(&format!("{path}."))
        })
    }
}

/// Parse config text. Keys this build does not know are dropped and returned
/// beside the config rather than failing the whole file; a value of the wrong
/// type is still an error. See [`parse_over`] for a parse that keeps the
/// rest of the file when one value is wrong.
pub fn parse(text: &str) -> Result<(Config, Vec<String>), toml::de::Error> {
    let cfg: Config = toml::from_str(text)?;
    let mut unknown = Vec::new();
    if let (Ok(input), Ok(known)) = (text.parse::<toml::Value>(), toml::Value::try_from(&cfg)) {
        unknown_keys(&input, &known, "", &mut unknown);
    }
    Ok((cfg, unknown))
}

/// Parse config text key by key, so one bad value costs that value and not
/// the file. Only text that is not TOML at all is an error. A key whose value
/// does not fit keeps `previous`'s value for it (the defaults, at startup)
/// and is listed in [`Parsed::invalid`]; unknown keys are dropped as in
/// [`parse`].
pub fn parse_over(text: &str, previous: &Config) -> Result<Parsed, toml::de::Error> {
    let input: toml::Value = text.parse()?;
    if let Ok((config, unknown)) = parse(text) {
        return Ok(Parsed {
            config,
            unknown,
            invalid: Vec::new(),
        });
    }
    let empty = || toml::Value::Table(toml::Table::new());
    let mut good = empty();
    let mut invalid = Vec::new();
    if let Some(table) = input.as_table() {
        for (key, value) in table {
            salvage(&mut good, &[key.as_str()], value, &mut invalid);
        }
    }
    // A key that did not fit keeps what was there before it, where that
    // value is itself one the config takes.
    if let Ok(prev) = toml::Value::try_from(previous) {
        for (path, _) in &invalid {
            let keys: Vec<&str> = path.split('.').collect();
            if let Some(old) = lookup(&prev, &keys) {
                let mut with = good.clone();
                insert(&mut with, &keys, old.clone());
                if with.clone().try_into::<Config>().is_ok() {
                    good = with;
                }
            }
        }
    }
    let config: Config = good.try_into().unwrap_or_default();
    let mut unknown = Vec::new();
    if let Ok(known) = toml::Value::try_from(&config) {
        unknown_keys(&input, &known, "", &mut unknown);
    }
    Ok(Parsed {
        config,
        unknown,
        invalid,
    })
}

/// Add `value` at `keys` to `good` if the config still deserializes with it.
/// A table that does not fit whole is tried key by key, so what is wrong is
/// narrowed to the values themselves; a value that does not fit is recorded
/// in `invalid` with why.
fn salvage(
    good: &mut toml::Value,
    keys: &[&str],
    value: &toml::Value,
    invalid: &mut Vec<(String, String)>,
) {
    let mut with = good.clone();
    insert(&mut with, keys, value.clone());
    let err = match with.clone().try_into::<Config>() {
        Ok(_) => {
            *good = with;
            return;
        }
        Err(err) => err,
    };
    if let Some(table) = value.as_table().filter(|t| !t.is_empty()) {
        for (key, inner) in table {
            let mut path = keys.to_vec();
            path.push(key);
            salvage(good, &path, inner, invalid);
        }
        return;
    }
    invalid.push((keys.join("."), err.message().trim().to_string()));
}

fn lookup<'a>(value: &'a toml::Value, keys: &[&str]) -> Option<&'a toml::Value> {
    keys.iter().try_fold(value, |v, k| v.as_table()?.get(*k))
}

/// Set `keys` in `root` to `value`, creating the tables on the way.
fn insert(root: &mut toml::Value, keys: &[&str], value: toml::Value) {
    let Some((last, parents)) = keys.split_last() else {
        return;
    };
    let mut at = root;
    for key in parents {
        let Some(table) = at.as_table_mut() else {
            return;
        };
        at = table
            .entry(key.to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    }
    if let Some(table) = at.as_table_mut() {
        table.insert(last.to_string(), value);
    }
}

/// `None` only when there is no file. Any other failure to read it (a byte
/// that is not UTF-8, no permission, an editor holding it mid-save) is an
/// error like a file that does not parse: taking it for a first run would
/// write the template over the user's file.
fn read(path: &Path, previous: &Config) -> Option<Result<Parsed, String>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => return Some(Err(format!("cannot read it: {err}"))),
    };
    Some(match parse_over(&text, previous) {
        Ok(parsed) => {
            if !parsed.unknown.is_empty() {
                tracing::warn!(
                    "config.toml: ignoring unknown keys: {}",
                    parsed.unknown.join(", ")
                );
            }
            for (key, why) in &parsed.invalid {
                tracing::error!("config.toml: ignoring {key} ({why}); it keeps its current value");
            }
            Ok(parsed)
        }
        Err(err) => Err(err.to_string()),
    })
}

/// Load the config, writing the commented template on first run. Unknown keys
/// are warned about and skipped, a value that does not fit is reported and
/// left at its default, and a file that is not TOML at all is reported and
/// ignored rather than blocking startup.
pub fn load(base: &Path) -> Config {
    load_or(base, &Config::default())
}

/// Like [`load`], but what cannot be used keeps `previous`'s value instead of
/// the default — the whole of it when the file is not TOML at all — so a
/// hot-reload never resets running settings.
pub fn load_or(base: &Path, previous: &Config) -> Config {
    match load_checked(base, previous) {
        Ok(parsed) => parsed.config,
        Err(err) => {
            tracing::error!("config.toml ignored ({err}); keeping previous settings");
            previous.clone()
        }
    }
}

/// Like [`load_or`], but says what could not be read rather than quietly
/// standing other values in for it: `Err` when the file exists and is not
/// TOML at all, and [`Parsed::invalid`] for each value that did not fit.
/// Values stood in are not what the user configured, so a caller that would
/// write them somewhere else (an account's `settings.json`, say) must not
/// treat them as if they were. A missing file is not an error: it is the
/// first run, and gets the template and defaults as [`load`] does. A file
/// that is there but cannot be read is an error, and is never overwritten.
pub fn load_checked(base: &Path, previous: &Config) -> Result<Parsed, String> {
    let path = config_path(base);
    match read(&path, previous) {
        Some(result) => result,
        None => {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            // Generated from the settings table, so the file can never
            // document an option the app does not have.
            let _ = std::fs::write(&path, crate::settings::template());
            Ok(Parsed {
                config: Config::default(),
                unknown: Vec::new(),
                invalid: Vec::new(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Windows names a console after the program that opened it, and ConPTY
    /// forwards that as the title.
    #[test]
    fn a_program_path_is_shown_as_the_program() {
        let cfg = TitlesConfig::default();
        assert_eq!(
            display_title(
                r"C:\WINDOWS\System32\WindowsPowerShell\v1.0\powershell.exe",
                &cfg
            ),
            "powershell"
        );
        assert_eq!(display_title(r"C:\WINDOWS\system32\cmd.exe", &cfg), "cmd");
        // A title someone chose is left alone, even when it names a program.
        assert_eq!(
            display_title("build C:\\tools\\make.exe", &cfg),
            "build C:\\tools\\make.exe"
        );
        assert_eq!(display_title("~/Dev/giverny", &cfg), "~/Dev/giverny");
    }

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

    /// A file that is not TOML at all is reported, not passed off as
    /// defaults, and is left exactly as the user wrote it.
    #[test]
    fn unparseable_config_is_an_error_not_defaults() {
        let dir = scratch("broken");
        let broken = "[font\nsize = 16.0\n";
        std::fs::write(config_path(&dir), broken).unwrap();
        assert!(load_checked(&dir, &Config::default()).is_err());
        // `load_or` still keeps what was running, and neither one rewrites
        // the file.
        let previous = Config {
            font: FontConfig {
                size: 21.0,
                ..FontConfig::default()
            },
            ..Config::default()
        };
        assert_eq!(load_or(&dir, &previous).font.size, 21.0);
        assert_eq!(std::fs::read_to_string(config_path(&dir)).unwrap(), broken);

        // A missing file is a first run, not an error.
        let fresh = scratch("broken-fresh");
        assert!(load_checked(&fresh, &Config::default()).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&fresh);
    }

    /// A file that is there but cannot be read is not a first run: it is
    /// reported, the running settings stay, and it is never overwritten
    /// with the template.
    #[test]
    fn an_unreadable_config_is_an_error_and_left_alone() {
        let dir = scratch("unreadable");
        let bytes = b"[font]\nsize = 21.0 # caf\xe9\n";
        std::fs::write(config_path(&dir), bytes).unwrap();
        assert!(load_checked(&dir, &Config::default()).is_err());
        let previous = Config {
            font: FontConfig {
                size: 17.0,
                ..FontConfig::default()
            },
            ..Config::default()
        };
        assert_eq!(load_or(&dir, &previous).font.size, 17.0);
        assert_eq!(std::fs::read(config_path(&dir)).unwrap(), bytes);
        let _ = std::fs::remove_dir_all(&dir);
    }

    const ONE_BAD_VALUE: &str = "[font]\nsize = \"big\"\nfamily = \"Iosevka\"\n\
        [theme]\nname = \"ink\"\n\
        [behavior]\nscrollback_lines = 500\nnotifications = false\n\
        [claude]\nresume_after_limit = false\nauto_mode = true\n";

    /// One value of the wrong type costs that value, not the file: every
    /// other key, in its section and in the others, still applies, and the
    /// bad one is named.
    #[test]
    fn one_bad_value_keeps_every_other_key() {
        let parsed = parse_over(ONE_BAD_VALUE, &Config::default()).unwrap();
        let cfg = &parsed.config;
        assert_eq!(cfg.font.size, FontConfig::default().size, "bad: default");
        assert_eq!(cfg.font.family, "Iosevka", "same section, kept");
        assert_eq!(cfg.theme.name, "ink");
        assert_eq!(cfg.behavior.scrollback_lines, 500);
        assert!(!cfg.behavior.notifications);
        assert!(!cfg.claude.resume_after_limit);
        assert!(cfg.claude.auto_mode);
        let names: Vec<&str> = parsed.invalid.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, ["font.size"]);
        assert!(!parsed.invalid[0].1.is_empty(), "with why");
        assert!(parsed.unknown.is_empty(), "a bad key is not an unknown one");
        assert!(parsed.invalid_under("font"));
        assert!(parsed.invalid_under("font.size"));
        assert!(!parsed.invalid_under("claude"));
        assert!(!parsed.invalid_under("fon"));

        // On a reload the bad key keeps what was running, not the default.
        let previous = Config {
            font: FontConfig {
                size: 21.0,
                ..FontConfig::default()
            },
            ..Config::default()
        };
        let reloaded = parse_over(ONE_BAD_VALUE, &previous).unwrap().config;
        assert_eq!(reloaded.font.size, 21.0);
        assert_eq!(reloaded.font.family, "Iosevka");
        assert_eq!(reloaded.theme.name, "ink");
    }

    /// A whole section of the wrong shape, and a bad value inside a table,
    /// are each narrowed to what is wrong.
    #[test]
    fn bad_values_are_found_at_any_depth() {
        let text = "titles = 5\n[font]\nsize = 15.0\n\
            [usage]\nrefresh_minutes = \"lots\"\n[update]\ncheck = false\n";
        let parsed = parse_over(text, &Config::default()).unwrap();
        let mut names: Vec<&str> = parsed.invalid.iter().map(|(k, _)| k.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["titles", "usage.refresh_minutes"]);
        assert_eq!(parsed.config.font.size, 15.0);
        assert!(!parsed.config.update.check);
        assert_eq!(
            parsed.config.usage.refresh_minutes,
            UsageConfig::default().refresh_minutes
        );
        assert!(parsed.config.titles.strip_host_prefix, "default kept");
    }

    /// Unknown keys are dropped and listed as before, beside a bad value or
    /// without one, and never count as invalid.
    #[test]
    fn unknown_keys_behave_as_before() {
        let text = format!("{ONE_BAD_VALUE}[font.extra]\nweight = 3\n[nonsense]\nx = 1\n");
        let parsed = parse_over(&text, &Config::default()).unwrap();
        let mut unknown = parsed.unknown.clone();
        unknown.sort_unstable();
        assert_eq!(unknown, ["font.extra", "nonsense"]);
        assert_eq!(parsed.invalid.len(), 1);
        assert_eq!(parsed.config.font.family, "Iosevka");

        let clean = "[font]\nsize = 15.0\nweight = 3\n";
        let parsed = parse_over(clean, &Config::default()).unwrap();
        assert_eq!(parsed.unknown, ["font.weight"]);
        assert!(parsed.invalid.is_empty());
        assert_eq!(parsed.config.font.size, 15.0);
        assert_eq!(parse(clean).unwrap().1, parsed.unknown);
    }

    /// A startup with one bad value loads every other key and leaves the
    /// file as written.
    #[test]
    fn a_bad_value_on_disk_loads_the_rest() {
        let dir = scratch("one-bad");
        std::fs::write(config_path(&dir), ONE_BAD_VALUE).unwrap();
        let parsed = load_checked(&dir, &Config::default()).unwrap();
        assert_eq!(parsed.config.theme.name, "ink");
        assert_eq!(parsed.invalid.len(), 1);
        assert_eq!(load(&dir).font.family, "Iosevka");
        assert_eq!(
            std::fs::read_to_string(config_path(&dir)).unwrap(),
            ONE_BAD_VALUE
        );
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
    fn opacity_is_kept_inside_what_the_app_paints() {
        let at = |opacity| WindowConfig { opacity }.opacity();
        assert_eq!(at(1.0), 1.0);
        assert_eq!(at(0.92), 0.92);
        assert_eq!(at(0.1), WindowConfig::MIN_OPACITY);
        assert_eq!(at(-3.0), WindowConfig::MIN_OPACITY);
        assert_eq!(at(1.5), 1.0, "above 1.0 is solid, not an error");
        assert_eq!(at(f32::NAN), 1.0);
        assert!(!WindowConfig::default().translucent());
        assert!(!WindowConfig { opacity: 2.0 }.translucent());
        assert!(WindowConfig { opacity: 0.95 }.translucent());
    }

    #[test]
    fn opacity_reads_from_the_window_table() {
        let dir = scratch("opacity");
        std::fs::write(config_path(&dir), "[window]\nopacity = 0.92\n").unwrap();
        let cfg = load(&dir);
        assert_eq!(cfg.window.opacity(), 0.92);
        assert_eq!(cfg.theme.name, "monet-dark");
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

    #[test]
    fn unknown_keys_keep_the_known_ones() {
        let (cfg, unknown) = parse(
            "[claude]\nauto_mode = true\nfuture_key = 1\n[future_section]\nx = 2\n[font]\nsize = 20.0\n",
        )
        .unwrap();
        assert!(cfg.claude.auto_mode);
        assert_eq!(cfg.font.size, 20.0);
        assert_eq!(unknown, ["claude.future_key", "future_section"]);
    }

    #[test]
    fn reload_keeps_previous_on_invalid_value_but_not_on_unknown_key() {
        let dir = std::env::temp_dir().join(format!("giverny-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut prev = Config::default();
        prev.claude.auto_mode = true;
        prev.font.size = 31.0;
        // Unknown key: the known keys still apply.
        std::fs::write(config_path(&dir), "[font]\nsize = 20.0\nnew_key = true\n").unwrap();
        assert_eq!(load_or(&dir, &prev).font.size, 20.0);
        // Invalid value: that one keeps its running value, while the rest of
        // the file still applies.
        std::fs::write(
            config_path(&dir),
            "[font]\nsize = \"big\"\n[claude]\nauto_mode = true\nresume_after_limit = false\n",
        )
        .unwrap();
        let kept = load_or(&dir, &prev);
        assert_eq!(kept.font.size, 31.0);
        assert!(kept.claude.auto_mode);
        assert!(!kept.claude.resume_after_limit);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
