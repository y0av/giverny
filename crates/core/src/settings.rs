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
use crate::limits::{self, Auto, GpuLimit, Mem};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Section {
    Appearance,
    Terminal,
    Titles,
    Restore,
    Claude,
    ManagementPanel,
    Codex,
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
        Section::ManagementPanel,
        Section::Codex,
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
            Section::ManagementPanel => "management panel",
            Section::Codex => "codex",
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
    /// One of `[manager.limits]`: `"auto"` by default, else a figure.
    /// Carried as [`Value::Text`] in the form the file holds — `"auto"`,
    /// `"8"`, `"16G"`, or for GPUs a TOML array (`[]`,
    /// `[{ index = 0, vram = "20G" }]`) — and written back as the TOML type
    /// the ledger reads (an integer for cores, a string for RAM, an array
    /// of tables for GPUs).
    Limit {
        field: LimitField,
    },
    /// One of `[management_panel.lease]`: a figure, never `auto`. Carried as
    /// [`Value::Text`] (`"3"`, `"3G"`) and written back as the ledger reads
    /// it: an integer for cores, a size string for RAM. `field` is
    /// [`LimitField::Cores`] or [`LimitField::Ram`].
    Lease {
        field: LimitField,
        default: &'static str,
    },
}

/// Which of `[manager.limits]` a [`Kind::Limit`] row edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitField {
    Cores,
    Ram,
    Gpus,
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

    /// Everything above the leaf: `font`, or `manager.limits`.
    pub fn table(&self) -> &str {
        self.key.rsplit_once('.').map_or(self.key, |(t, _)| t)
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
            Kind::Limit { .. } => Value::Text("auto".into()),
            Kind::Lease { default, .. } => Value::Text((*default).into()),
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
        doc: "Preferred monospace family; empty auto-detects. Hebrew comes from the bundled \
              Cascadia Mono unless this family has Hebrew of its own.",
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
                "catppuccin-mauve",
                "rouen",
                "phosphor",
                "abyss",
                "synthwave",
                "workbench",
                "riso",
                "red-rock",
            ],
        },
    },
    SettingDef {
        key: "window.opacity",
        label: "window opacity",
        section: Section::Appearance,
        doc: "How solid the window's background is; below 1.0 the desktop shows through.",
        note: &[
            "Only backgrounds: text, the cursor, selections, images and cells",
            "a program colours itself stay solid, and so do menus and the",
            "settings screen. 0.90-0.95 keeps text readable over a busy",
            "wallpaper on a desktop that does not blur behind windows (GNOME).",
            "Moving between 1.0 and anything lower needs a restart, since the",
            "window is created see-through or not; between values below 1.0",
            "it changes live. Stays solid on WSLg and on X11 without a",
            "compositor.",
        ],
        needs_restart: true,
        kind: Kind::Float {
            default: 1.0,
            min: 0.5,
            max: 1.0,
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
        key: "titles.strip_claude_glyph",
        label: "drop Claude's title glyph",
        section: Section::Titles,
        doc: "Drop the glyph Claude Code puts in front of its title (✳, ◐, a spinner).",
        note: &[
            "The rail's own mark beside the title already shows whether",
            "Claude is working, and keeps showing it while background",
            "agents run. A title you typed yourself is never touched.",
        ],
        needs_restart: false,
        kind: Kind::Bool { default: true },
    },
    SettingDef {
        key: "rail.animate",
        label: "animate the rail",
        section: Section::Titles,
        doc: "Spin a working tab's mark and pulse a tab that wants you; off removes the spinners and the pulse.",
        note: &[
            "Off, no spinner, ring or pulse is drawn: a working tab's mark is",
            "left blank and a waiting one shows its amber flag, still. The",
            "rail stops waking the window to animate them.",
        ],
        needs_restart: false,
        kind: Kind::Bool { default: true },
    },
    SettingDef {
        key: "rail.calm_busy",
        label: "calm busy mark",
        section: Section::Titles,
        doc: "A working tab breathes a slow dot in its colour instead of spinning.",
        note: &[
            "One breath every three seconds, everywhere the spinner would be:",
            "tab rows, category headers, background agents. Still a different",
            "thing from the amber flag of a tab that wants you, in shape,",
            "colour and pace. With \"animate the rail\" off the dot holds",
            "still, full.",
        ],
        needs_restart: false,
        kind: Kind::Bool { default: true },
    },
    SettingDef {
        key: "rail.tab_figures",
        label: "tab CPU/mem figures",
        section: Section::Titles,
        doc: "Each tab's CPU and memory in the rail, and a collapsed group's sum.",
        note: &[
            "Off, no row or group header draws figures, and the title,",
            "sub-line and group name take the room they held. The status",
            "line and the management panel still read them.",
        ],
        needs_restart: false,
        kind: Kind::Bool { default: true },
    },
    SettingDef {
        key: "rail.place_titles",
        label: "name Claude tabs after their session",
        section: Section::Titles,
        doc: "Title a Claude tab after its session's name, as a place: Rotem's Workshop.",
        note: &[
            "Only for a session that has a name (management panel → name",
            "every session); the room is drawn once per",
            "session and kept across resumes. Claude's own title moves to",
            "the tab's hover. A title you typed yourself still wins.",
        ],
        needs_restart: false,
        kind: Kind::Bool { default: false },
    },
    SettingDef {
        key: "rail.total_line",
        label: "total CPU/mem line",
        section: Section::Appearance,
        doc: "The summed CPU and memory line under the account bars.",
        note: &[
            "What Giverny runs now, summed: `23% CPU  2.0G RAM`. Off, the",
            "bars end the rail's bottom panel. The status line and the",
            "management panel still read the figures.",
        ],
        needs_restart: false,
        kind: Kind::Bool { default: true },
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
        key: "behavior.history_per_tab",
        label: "history per terminal",
        section: Section::Terminal,
        doc: "Each tab's shell keeps its own history, back when the tab is restored.",
        note: &[
            "Set inside the shells Giverny starts (bash, zsh, fish) by a",
            "start-up file of its own that runs your usual rc; no rc file of",
            "yours is touched, and a shell started from the tab keeps its own",
            "history. An rc that sets HISTFILE itself wins. Off leaves the",
            "shell's history as the shell has it. Applies to shells started",
            "from here on.",
        ],
        needs_restart: false,
        kind: Kind::Bool { default: false },
    },
    SettingDef {
        key: "behavior.history_also_shared",
        label: "also add to shell history",
        section: Section::Terminal,
        doc: "With history per terminal, also append each command to the usual history (bash).",
        note: &[
            "Appends to ~/.bash_history as each command runs; never rewrites it.",
            "zsh and fish keep only the tab's own history.",
            "Applies to shells started from here on.",
        ],
        needs_restart: false,
        kind: Kind::Bool { default: false },
    },
    SettingDef {
        key: "behavior.notifications",
        label: "desktop notifications",
        section: Section::Terminal,
        doc: "Notify when an agent needs you, or Codex finishes in a background tab.",
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
        key: "behavior.windows_shell",
        label: "shell on Windows",
        section: Section::Terminal,
        doc: "Which shell a new tab opens on Windows.",
        note: &[
            "`auto` opens WSL when a distribution is installed, because that",
            "is where Claude Code and unix tooling usually live, and falls",
            "back to PowerShell when there is none. Set it explicitly to get",
            "a Windows shell on a machine that has both.",
            "Applies to tabs opened from here on; existing ones keep theirs.",
            "Ignored off Windows, where $SHELL answers this.",
        ],
        needs_restart: false,
        kind: Kind::Choice {
            default: "auto",
            options: &["auto", "wsl", "powershell", "cmd"],
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
        key: "codex.enabled",
        label: "detect Codex automatically",
        section: Section::Codex,
        doc: "Show detected Codex accounts and usage automatically. Turn off to disable integration.",
        note: &[],
        needs_restart: false,
        kind: Kind::Bool { default: true },
    },
    SettingDef {
        key: "codex.executable",
        label: "Codex executable",
        section: Section::Codex,
        doc: "Executable name on PATH, or an explicit path.",
        note: &[],
        needs_restart: false,
        kind: Kind::Text {
            default: "codex",
            placeholder: "codex",
        },
    },
    SettingDef {
        key: "codex.home",
        label: "Codex home",
        section: Section::Codex,
        doc: "Empty uses CODEX_HOME, then the default .codex directory.",
        note: &[],
        needs_restart: false,
        kind: Kind::Text {
            default: "",
            placeholder: "automatic",
        },
    },
    SettingDef {
        key: "codex.restore",
        label: "restore Codex conversations",
        section: Section::Codex,
        doc: "Auto reconnects saved conversations on tab focus; prompt asks first; off skips restore.",
        note: &[],
        needs_restart: false,
        kind: Kind::Choice {
            default: "auto",
            options: &["auto", "prompt", "off"],
        },
    },
    SettingDef {
        key: "codex.usage_refresh_minutes",
        label: "usage refresh interval",
        section: Section::Codex,
        doc: "Minutes between Codex usage requests. Zero means manual refresh only.",
        note: &[],
        needs_restart: false,
        kind: Kind::Int {
            default: 10,
            min: 0,
            max: 1440,
        },
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
        key: "claude.management_panel",
        label: "management panel",
        // Shown at the top of Management panel; the key stays under
        // [claude], where it always was.
        section: Section::ManagementPanel,
        doc: "Show the tab's subagents — running, planned and done — in a table under the terminal.",
        note: &[
            "Running and Done come from Claude Code's own files and need no",
            "setup. On, in each account you installed Giverny's hooks in, it",
            "adds a subagentStatusLine (giverny relay --subagent-line) and the",
            "giverny Claude Code plugin to that account's settings.json, never",
            "over a line of your own: /giverny:manage runs a team of",
            "subagents and adds Planned rows, titles, ETAs and landings",
            "(docs/management-panel.md). Off removes both again.",
            "Done rows stay until /clear or a fresh claude in the tab. The",
            "pane appears only in a tab whose session has spawned a",
            "subagent; off, nothing is read or drawn.",
        ],
        needs_restart: false,
        kind: Kind::Bool { default: true },
    },
    SettingDef {
        key: "claude.resume_after_limit",
        label: "resume after a limit",
        section: Section::Claude,
        doc: "Pick a session back up when the usage window that stopped it reopens.",
        note: &[
            "A session that runs out of limit stops mid-task and stays stopped",
            "until someone comes back to it — which, for work started in the",
            "evening, means the morning. With this on, Giverny waits for the",
            "window to reset and asks it to carry on.",
            "Off by default: it spends the new window without being asked, and",
            "a tab you have typed in since it stopped is left alone either way.",
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
        key: "management_panel.manage_skill",
        label: "manage skill",
        section: Section::ManagementPanel,
        doc: "Ship the /giverny:manage skill with the plugin the management panel installs.",
        note: &[
            "Written only where the management panel's plugin is (accounts holding",
            "Giverny's hooks, with claude.management_panel on). Off removes only the",
            "skill: the plugin keeps giverny-manage, its hook and /giverny:clear-done.",
        ],
        needs_restart: false,
        kind: Kind::Bool { default: true },
    },
    SettingDef {
        key: "management_panel.ask_names",
        label: "name every session",
        section: Section::ManagementPanel,
        doc: "Give every Claude session in a Giverny tab a name at its start; the full name shows on its status line.",
        note: &[
            "Giverny draws the first name; the surname is the tab's category",
            "(else its repo): `Opus 5.5 (Rotem Giverny)`.",
            "A session has it before its first turn; `giverny-manage name` reads",
            "it back. Off, no session is named, managers included, and no name",
            "shows on any status line.",
        ],
        needs_restart: false,
        kind: Kind::Bool { default: false },
    },
    // The columns, in the order the management panel draws them: Settings shows
    // their switches in this order, and the pane's test
    // `the_settings_list_the_columns_in_the_panes_order` keeps the two in step.
    // `management_panel.done_rows` / `done_last` have no row here; the pane still
    // honours them when set by hand.
    SettingDef {
        key: "management_panel.columns.stage",
        label: "stage",
        section: Section::ManagementPanel,
        doc: "STAGE: Running, Next up or Done.",
        note: &[],
        needs_restart: false,
        kind: Kind::Bool { default: true },
    },
    SettingDef {
        key: "management_panel.columns.id",
        label: "task id",
        section: Section::ManagementPanel,
        doc: "The task's id (a feed row's key, a subagent's name).",
        note: &[],
        needs_restart: false,
        kind: Kind::Bool { default: true },
    },
    SettingDef {
        key: "management_panel.columns.title",
        label: "title",
        section: Section::ManagementPanel,
        doc: "The task's title, or a subagent's description: the column that takes the room left.",
        note: &[],
        needs_restart: false,
        kind: Kind::Bool { default: true },
    },
    SettingDef {
        key: "management_panel.columns.elapsed",
        label: "elapsed",
        section: Section::ManagementPanel,
        doc: "How long the row has worked: a stopwatch while it runs.",
        note: &[],
        needs_restart: false,
        kind: Kind::Bool { default: true },
    },
    SettingDef {
        key: "management_panel.columns.eta",
        label: "ETA",
        section: Section::ManagementPanel,
        doc: "Time left on a Running row, the estimate of a Next up one, how late or early a Done one landed.",
        note: &[],
        needs_restart: false,
        kind: Kind::Bool { default: true },
    },
    SettingDef {
        key: "management_panel.columns.now",
        label: "now",
        section: Section::ManagementPanel,
        doc: "What the row is doing now: its last tool call, a wait, a queue place.",
        note: &[],
        needs_restart: false,
        kind: Kind::Bool { default: true },
    },
    SettingDef {
        key: "management_panel.columns.tokens",
        label: "tokens",
        section: Section::ManagementPanel,
        doc: "The tokens the row has used.",
        note: &[],
        needs_restart: false,
        kind: Kind::Bool { default: true },
    },
    SettingDef {
        key: "management_panel.columns.usage",
        label: "CPU / RAM",
        section: Section::ManagementPanel,
        doc: "What the row uses: on a Running row, live CPU and memory of its `giverny manage run` commands, else of its worker's own processes; on a Done row, the measured memory peak.",
        note: &[],
        needs_restart: false,
        kind: Kind::Bool { default: true },
    },
    // No `management_panel.lease.cpu_cores` row: a lease
    // reserves RAM only (CPU is shared and measured), so the key, still
    // read where a file has it, counts for nothing.
    SettingDef {
        key: "management_panel.lease.ram",
        label: "RAM",
        section: Section::ManagementPanel,
        doc: "Memory a task's lease holds when nothing says otherwise. Also each Claude tab's protected memory (`MemoryLow`) in `giverny-claude.slice`. A lease reserves RAM only: CPU is shared and measured.",
        note: &[
            "A size like \"3G\" or \"512M\" (a bare number is GiB); --ram overrides it.",
            "What `giverny manage run` claims for a task that holds no lease;",
            "it also sets the run's CARGO_BUILD_JOBS (a job a GiB).",
        ],
        needs_restart: false,
        kind: Kind::Lease {
            field: LimitField::Ram,
            default: "3G",
        },
    },
    SettingDef {
        key: "manager.limits.cpu_cores",
        label: "CPU cores",
        section: Section::ManagementPanel,
        doc: "The hard CPU ceiling of every Claude tab and `manage run` together (`giverny-claude.slice`; on macOS held by Giverny itself, a duty cycle on the busy commands). Cores are shared, never leased: a claim waits only while they are measured busy. auto = all but 2.",
        note: &[
            "The resource ledger (`giverny manage claim`) grants workers RAM",
            "and GPUs out of these limits, across every manager on this",
            "machine, and reads them on each claim: an edit applies to the",
            "next one. A number, or \"auto\" (cores - 2, at least 1).",
        ],
        needs_restart: false,
        kind: Kind::Limit {
            field: LimitField::Cores,
        },
    },
    SettingDef {
        key: "manager.limits.ram",
        label: "RAM",
        section: Section::ManagementPanel,
        doc: "Memory all manager sessions together may hand to workers, and the hard memory ceiling of every Claude tab and `manage run` together (`giverny-claude.slice`; on macOS held by Giverny itself, which kills the biggest command over it). auto = 70 %.",
        note: &["A size like \"16G\" or \"512M\" (a bare number is GiB), or \"auto\"."],
        needs_restart: false,
        kind: Kind::Limit {
            field: LimitField::Ram,
        },
    },
    SettingDef {
        key: "manager.limits.gpus",
        label: "GPUs",
        section: Section::ManagementPanel,
        doc: "GPUs and VRAM manager sessions may use. auto = 90 % of each GPU's VRAM.",
        note: &[
            "GPUs are found with nvidia-smi; without it there are none. A list",
            "like [{ index = 0, vram = \"20G\" }] names the GPUs and how much of",
            "each; [] gives managers none.",
        ],
        needs_restart: false,
        kind: Kind::Limit {
            field: LimitField::Gpus,
        },
    },
    SettingDef {
        key: "update.check",
        label: "check for updates",
        section: Section::Updates,
        doc: "Ask GitHub whether a newer Giverny exists, hourly while it is open.",
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
    if let Kind::Limit { field } = def.kind {
        return limit_text(field, node).map(Value::Text);
    }
    if let Kind::Lease { field, .. } = def.kind {
        return lease_text(field, node).map(Value::Text);
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

/// A `[manager.limits]` value in the form [`Kind::Limit`] carries it:
/// read with the ledger's own types, so the screen shows what the ledger
/// would grant from.
fn limit_text(field: LimitField, node: &toml::Value) -> Option<String> {
    let node = node.clone();
    Some(match field {
        LimitField::Cores => match node.try_into::<Auto<u32>>().ok()? {
            Auto::Auto => "auto".into(),
            Auto::Set(n) => n.to_string(),
        },
        LimitField::Ram => match node.try_into::<Auto<Mem>>().ok()? {
            Auto::Auto => "auto".into(),
            Auto::Set(m) => limits::mem_text(m),
        },
        LimitField::Gpus => match node.try_into::<Auto<Vec<GpuLimit>>>().ok()? {
            Auto::Auto => "auto".into(),
            Auto::Set(g) => limits::gpus_text(&g),
        },
    })
}

/// A `[management_panel.lease]` value in the form [`Kind::Lease`] carries it.
fn lease_text(field: LimitField, node: &toml::Value) -> Option<String> {
    let node = node.clone();
    Some(match field {
        LimitField::Ram => limits::mem_text(node.try_into::<Mem>().ok()?),
        _ => node.try_into::<u32>().ok()?.to_string(),
    })
}

/// A [`Kind::Lease`] value as the TOML the ledger reads; `None` when the
/// text is not one.
fn lease_toml(field: LimitField, text: &str) -> Option<toml_edit::Value> {
    let t = text.trim();
    match field {
        LimitField::Ram => Mem::parse(t)
            .filter(|m| m.0 > 0)
            .map(|m| limits::mem_text(m).into()),
        _ => t.parse::<i64>().ok().filter(|n| *n > 0).map(Into::into),
    }
}

/// A [`Kind::Limit`] value as the TOML the ledger reads. `None` when the
/// text is not one (the screen validates before it gets here).
fn limit_toml(field: LimitField, text: &str) -> Option<toml_edit::Value> {
    let t = text.trim();
    if t.eq_ignore_ascii_case("auto") {
        return Some("auto".into());
    }
    match field {
        LimitField::Cores => t.parse::<i64>().ok().filter(|n| *n > 0).map(Into::into),
        LimitField::Ram => Mem::parse(t).map(|m| limits::mem_text(m).into()),
        LimitField::Gpus => {
            let v: toml_edit::Value = t.parse().ok()?;
            // Only what the ledger can read back.
            let probe: toml::Value = toml::from_str(&format!("g = {t}")).ok()?;
            probe.get("g")?.clone().try_into::<Vec<GpuLimit>>().ok()?;
            Some(v)
        }
    }
}

/// The TOML for `value` as option `def` stores it.
fn encode(def: &SettingDef, value: &Value) -> anyhow::Result<toml_edit::Value> {
    match (&def.kind, value) {
        (Kind::Limit { field }, Value::Text(t)) => limit_toml(*field, t)
            .ok_or_else(|| anyhow::anyhow!("{}: `{t}` is not a limit", def.key)),
        (Kind::Lease { field, .. }, Value::Text(t)) => lease_toml(*field, t)
            .ok_or_else(|| anyhow::anyhow!("{}: `{t}` is not a lease", def.key)),
        _ => Ok(toml_edit_value(value)),
    }
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
    let parents = parts.len() - 1;
    for (i, part) in parts[..parents].iter().enumerate() {
        if !node.contains_key(part) {
            let mut table = toml_edit::Table::new();
            // `[manager.limits]` alone, not an empty `[manager]`
            // above it.
            table.set_implicit(i + 1 < parents);
            node.insert(part, toml_edit::Item::Table(table));
        }
        node = node
            .get_mut(part)
            .and_then(|item| item.as_table_mut())
            .ok_or_else(|| anyhow::anyhow!("{} is not a table in config.toml", part))?;
    }

    let leaf = parts[parts.len() - 1];
    let encoded = encode(def, value)?;
    match node.get_mut(leaf) {
        Some(item) => {
            let slot = item.as_value_mut().ok_or_else(|| {
                anyhow::anyhow!("{} is not a plain value in config.toml", def.key)
            })?;
            // The decor is the whitespace and comments *around* the value —
            // `size = 11.0  # deliberately small`. Replacing the value alone
            // would take the user's note with it.
            let decor = slot.decor().clone();
            *slot = encoded;
            *slot.decor_mut() = decor;
        }
        None => {
            node.insert(leaf, toml_edit::Item::Value(encoded));
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

/// The default as the file holds it: a lease's cores as an integer, not
/// the text the screen carries.
fn render_default(def: &SettingDef) -> String {
    let default = def.default_value();
    encode(def, &default)
        .map(|v| v.to_string().trim().to_string())
        .unwrap_or_else(|_| render_value(&default))
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
            _ => out.push_str(&format!("{} = {}\n", def.leaf(), render_default(def))),
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
            _ => format!("`{}`", render_default(def)),
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
        assert_eq!(parsed.window.opacity, defaults.window.opacity);
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
        assert_eq!(parsed.manager, defaults.manager);
        assert_eq!(parsed.management_panel, defaults.management_panel);
        assert!(
            text.contains("\n[management_panel.lease]\n") && !text.contains("\ncpu_cores = 3\n"),
            "the lease is RAM only:\n{text}"
        );
        assert!(
            text.contains("\n[manager.limits]\n"),
            "limits get their own table:\n{text}"
        );
    }

    #[test]
    fn every_option_resolves_against_a_live_config() {
        // Unknown keys are tolerated, so this checks
        // every option resolves under the path the settings table names.
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

    /// The rail's calm look is the default: both are on until someone
    /// turns them off, and a file without them reads as on.
    #[test]
    fn calm_marks_are_on_by_default() {
        let absent: Config = toml::from_str("[rail]\n[titles]\n").unwrap();
        for key in ["rail.calm_busy", "titles.strip_claude_glyph"] {
            let def = by_key(key).expect("declared");
            assert!(matches!(def.kind, Kind::Bool { default: true }), "{key}");
            assert_eq!(current(&absent, def), Some(Value::Bool(true)), "{key}");
        }
    }

    /// The rail's two motion options sit with the title glyph toggle in tabs
    /// & titles, right after it, though their keys stay in `[rail]`; the
    /// tab CPU/mem figures, drawn on the tab rows too, follow them.
    /// The total line is under the account bars, not on a
    /// tab, and stays in appearance.
    #[test]
    fn rail_motion_options_follow_the_title_glyph() {
        let titles: Vec<&str> = in_section(Section::Titles).map(|d| d.key).collect();
        let glyph = titles
            .iter()
            .position(|k| *k == "titles.strip_claude_glyph")
            .expect("glyph toggle is in tabs & titles");
        assert_eq!(
            titles[glyph + 1..glyph + 4],
            ["rail.animate", "rail.calm_busy", "rail.tab_figures"]
        );
        // Tabs titled after their session's place, next to the figures,
        // off unless asked for: upstream's rail unchanged.
        assert_eq!(titles.get(glyph + 4), Some(&"rail.place_titles"));
        assert!(matches!(
            by_key("rail.place_titles").unwrap().kind,
            Kind::Bool { default: false }
        ));
        assert_eq!(
            by_key("rail.total_line").unwrap().section,
            Section::Appearance
        );
    }

    #[test]
    fn opacity_is_an_opt_in_appearance_float() {
        let def = by_key("window.opacity").expect("window.opacity is declared");
        assert_eq!(def.section, Section::Appearance);
        match def.kind {
            Kind::Float { default, min, max } => {
                assert_eq!(default, 1.0, "solid unless asked for");
                assert_eq!(min, f64::from(config::WindowConfig::MIN_OPACITY));
                assert_eq!(max, 1.0);
            }
            ref other => panic!("window.opacity is {other:?}"),
        }
        assert!(is_default(&Config::default(), def));
    }

    #[test]
    fn opacity_round_trips_through_the_file() {
        let dir = scratch("opacity");
        std::fs::write(config::config_path(&dir), template()).unwrap();
        write(&dir, by_key("window.opacity").unwrap(), &Value::Float(0.92)).unwrap();
        let cfg: Config =
            toml::from_str(&std::fs::read_to_string(config::config_path(&dir)).unwrap()).unwrap();
        assert_eq!(cfg.window.opacity, 0.92);
        assert_eq!(cfg.theme.name, "monet-dark");
        let _ = std::fs::remove_dir_all(&dir);
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
    fn a_limit_written_into_a_file_without_the_table_gets_only_that_table() {
        let dir = scratch("limits-new");
        std::fs::write(config::config_path(&dir), "[font]\nsize = 13.0\n").unwrap();
        write(
            &dir,
            by_key("manager.limits.cpu_cores").unwrap(),
            &Value::Text("4".into()),
        )
        .unwrap();
        let text = std::fs::read_to_string(config::config_path(&dir)).unwrap();
        assert!(!text.contains("[manager]\n"), "{text}");
        assert!(text.contains("[manager.limits]\ncpu_cores = 4"), "{text}");
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
