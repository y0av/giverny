//! Giverny — a native terminal built around Claude Code.

mod capture;
mod chrome;
mod claude_watch;
mod desktop;
mod icon;
mod keymap;
mod overlays;
mod rail;
mod settings_ui;
mod update;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Key, Modifiers};
use giverny_core::config;
use giverny_core::state::{self, Paths, SaveState};
use giverny_core::tabs::{CategoryId, TabId, Workspace};
use giverny_term::proxy::TabEvent;
use giverny_term::pty::{GridSize, SpawnCfg};
use giverny_term::render::theme::Theme;
use giverny_term::session::TermSession;
use giverny_term::tee::TeeEvent;
use giverny_term::widget::{DEFAULT_FONT_SIZE, RenderShared, TabView};

/// Category accent colors — Monet garden hues, assigned round-robin.
pub const CATEGORY_PALETTE: [Color32; 8] = [
    Color32::from_rgb(0x9a, 0x86, 0xb8), // wisteria
    Color32::from_rgb(0x5f, 0xa3, 0xa3), // lily-pond teal
    Color32::from_rgb(0xd9, 0xb5, 0x5f), // sunlight
    Color32::from_rgb(0xc3, 0x5b, 0x4e), // poppy
    Color32::from_rgb(0x5b, 0x7f, 0xa6), // pond blue
    Color32::from_rgb(0x7b, 0xa2, 0x5a), // garden green
    Color32::from_rgb(0xd0, 0x8a, 0xa2), // rose
    Color32::from_rgb(0x84, 0xc5, 0xc5), // water
];

pub fn category_color(index: usize) -> Color32 {
    CATEGORY_PALETTE[index % CATEGORY_PALETTE.len()]
}

/// Remember accounts that only the environment knew about.
///
/// An account kept somewhere unguessable is found through the environment —
/// `CLAUDE_CONFIG_DIR`, or a list in `CCTOP_CONFIG_DIRS` for people who keep
/// one. Both usually come from a shell rc, which means they exist when
/// Giverny starts from a terminal and not when it starts from a launcher, so
/// the account list changed depending on how the app was opened.
///
/// Writing them into the config the first time we see them makes the list
/// ours: from then on it is the same however Giverny was started, and it is
/// visible and editable in settings rather than hidden in a shell rc.
/// Whether dropping a file into a tab can work here.
///
/// The winit we build against (0.30.x, the version egui pins) delivers drops
/// on X11, Windows and macOS and has no `wl_data_device` handling, so a
/// Wayland session never sees them. This is a released-stack limit, not a
/// Wayland one: winit master has `winit-wayland/src/dnd.rs` and emits
/// `DragEntered { position }` — it arrives here once winit 0.31 ships that
/// work and egui finishes its 0.31 migration, and it brings drag positions
/// with it (which is what per-tab drop targeting needs).
///
/// Worth stating outright either way: silence looks like a bug in Giverny.
fn drag_drop_status() {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    if wayland {
        println!("file drag-and-drop  unavailable on this Wayland session");
        println!("      the winit egui pins has no Wayland drop support yet. To get it now, set");
        println!("      behavior.prefer_x11 = true (settings → terminal) and restart:");
        println!("      Giverny then runs under XWayland, where text is softer at");
        println!("      fractional scaling. A drop lands in the active tab: this winit");
        println!("      reports no drag position (winit master does; it is unreleased).");
    } else {
        println!("file drag-and-drop  available (X11/Windows/macOS)");
    }
    println!();
}

fn remember_env_accounts(paths: &Paths, cfg: &mut config::Config) {
    let mut from_env: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        from_env.push(PathBuf::from(dir));
    }
    if let Ok(list) = std::env::var("CCTOP_CONFIG_DIRS") {
        from_env.extend(list.split(':').filter(|p| !p.is_empty()).map(PathBuf::from));
    }

    // Anything found without being told stays out of the config — no point
    // recording ~/.claude or a dir the scan picks up anyway. This must not
    // consult the environment: the environment is what is being decided about,
    // so `discover()` here would find everything and record nothing.
    let found_anyway = giverny_claude::profiles::ambient_dirs();
    let mut merged = cfg.behavior.extra_profile_dirs.clone();
    let mut added = 0;
    for dir in from_env {
        if merged.contains(&dir)
            || found_anyway.contains(&dir)
            || !giverny_claude::profiles::looks_like_account(&dir)
        {
            continue;
        }
        merged.push(dir);
        added += 1;
    }
    if added == 0 {
        return;
    }
    let Some(def) = giverny_core::settings::by_key("behavior.extra_profile_dirs") else {
        return;
    };
    let value = giverny_core::settings::Value::List(
        merged.iter().map(|p| p.display().to_string()).collect(),
    );
    match giverny_core::settings::write(paths.base(), def, &value) {
        Ok(()) => {
            tracing::info!("remembered {added} account dir(s) from the environment");
            cfg.behavior.extra_profile_dirs = merged;
        }
        Err(err) => tracing::warn!("could not record account dirs: {err:#}"),
    }
}

/// Claude Code marks its own subprocesses with session-identity variables.
/// Launch Giverny *from* a Claude session — a tab, or `claude` in any
/// terminal — and those markers are inherited, then handed to every shell we
/// spawn: the `claude` inside each tab decides it is a child session and
/// turns off transcript saving, which silently disables resume.
///
/// Scrub them before any thread exists (the only safe window to mutate the
/// environment). `CLAUDE_CONFIG_DIR` is deliberately kept — that is account
/// selection, not session identity — as are `ANTHROPIC_*` credentials-ish
/// and provider settings we have no business touching.
fn scrub_inherited_claude_markers() {
    const MARKERS: &[&str] = &[
        "CLAUDECODE",
        "CLAUDE_CODE_CHILD_SESSION",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_SESSION_ID",
        "CLAUDE_CODE_SESSION_NAME",
        "CLAUDE_CODE_SESSION_KIND",
        "CLAUDE_CODE_JSONL_TRANSCRIPT",
        "CLAUDE_CODE_SESSION_LOG",
        "CLAUDE_CODE_BRIDGE_SESSION_ID",
        "CLAUDE_CODE_FORCE_SESSION_PERSISTENCE",
        "CLAUDE_PROJECT_DIR",
        "CLAUDE_PLUGIN_ROOT",
        "CLAUDE_PLUGIN_DATA",
        "CLAUDE_EFFORT",
    ];
    let inherited: Vec<String> = std::env::vars()
        .map(|(k, _)| k)
        .filter(|k| MARKERS.contains(&k.as_str()) || k.starts_with("CLAUDE_BG_"))
        .collect();
    if inherited.is_empty() {
        return;
    }
    for key in &inherited {
        // SAFETY: single-threaded here — called first thing in main.
        unsafe { std::env::remove_var(key) };
    }
    eprintln!(
        "giverny: cleared {} inherited Claude session marker(s) so tabs start clean",
        inherited.len()
    );
}

fn main() -> eframe::Result {
    scrub_inherited_claude_markers();

    // Subcommands that never open a window.
    match std::env::args().nth(1).as_deref() {
        Some("relay") => {
            giverny_claude::hooks::run_relay(&Paths::default_dirs().hook_spool());
            return Ok(());
        }
        Some("statusline") => {
            giverny_claude::hooks::run_statusline(&Paths::default_dirs().hook_spool());
            return Ok(());
        }
        Some("doctor") => {
            doctor();
            return Ok(());
        }
        Some("update") => {
            update_cli();
            return Ok(());
        }
        // Wayland cannot take an icon from the client; it reads one from an
        // installed desktop entry matched on app_id. See `desktop`.
        Some("install-desktop") => {
            match std::env::args().nth(2).as_deref() {
                Some("--remove") => desktop::remove(),
                _ => desktop::install(),
            }
            return Ok(());
        }
        Some("--version" | "-V") => {
            println!("giverny {}", update::CURRENT);
            return Ok(());
        }
        Some("--help" | "-h") => {
            println!(
                "giverny — a native terminal built around Claude Code\n\n\
                 USAGE:\n  giverny            launch the terminal\n  \
                 giverny doctor     diagnose Claude integration\n  \
                 giverny update     check for a newer release\n  \
                 giverny install-desktop [--remove]\n                     \
                 install the desktop entry + icons (needed for the\n                     \
                 taskbar icon on Wayland)\n  \
                 giverny relay      (internal) Claude Code hook entrypoint\n  \
                 giverny statusline (internal) Claude Code statusline entrypoint\n\n\
                 FLAGS:\n  -V, --version  print the version\n  \
                 -h, --help     print this help"
            );
            return Ok(());
        }
        _ => {}
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "giverny=info,warn".into()),
        )
        .init();

    // Backend choice has to happen before the event loop exists. winit picks
    // Wayland whenever WAYLAND_DISPLAY is set, and has no drop support there,
    // so this is the switch that buys file drag-and-drop.
    let paths = Paths::default_dirs();
    // `GIVERNY_NO_X11` is set by the fallback below, so a second attempt
    // cannot loop.
    let try_x11 = config::load(paths.base()).behavior.prefer_x11
        && std::env::var_os("GIVERNY_NO_X11").is_none()
        && std::env::var_os("DISPLAY").is_some()
        && std::env::var_os("WAYLAND_DISPLAY").is_some();
    let mut wayland_stashed = None;
    if try_x11 {
        wayland_stashed = std::env::var_os("WAYLAND_DISPLAY");
        // SAFETY: no threads yet — this runs before the event loop.
        unsafe { std::env::remove_var("WAYLAND_DISPLAY") };
        tracing::info!("prefer_x11: using X11/XWayland so file drops arrive");
    }

    // Reopen at the size the user left it. Read before the window exists, so
    // it can't be applied as a resize the user sees happen.
    let layout = state::load_layout(&paths);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id("giverny")
            .with_title("Giverny")
            .with_icon(icon::icon_data(16))
            .with_inner_size(layout.window_size().unwrap_or([1280.0, 820.0]))
            .with_maximized(layout.maximized)
            .with_min_inner_size([640.0, 400.0]),
        ..Default::default()
    };
    let result = eframe::run_native(
        "Giverny",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    );

    // No X server after all — no XWayland, or no XAUTHORITY. Preferring
    // drag-and-drop must not be able to leave the app unlaunchable, with the
    // only remedy being to edit a config file you cannot see.
    //
    // Re-exec rather than retry: winit refuses to build a second event loop in
    // one process (`RecreationAttempt`), so the only way back is a fresh one.
    if let Err(err) = &result
        && try_x11
    {
        tracing::warn!("prefer_x11 failed ({err}); restarting on Wayland");
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let exe = std::env::current_exe().unwrap_or_else(|_| "giverny".into());
            let mut cmd = std::process::Command::new(exe);
            cmd.args(std::env::args_os().skip(1))
                .env("GIVERNY_NO_X11", "1");
            // The child inherits *this* environment, where WAYLAND_DISPLAY was
            // removed — so it has to be put back explicitly or the child picks
            // X11 again and fails the same way.
            if let Some(wl) = wayland_stashed {
                cmd.env("WAYLAND_DISPLAY", wl);
            }
            let error = cmd.exec();
            tracing::error!("could not restart: {error}");
        }
    }
    result
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameTarget {
    Tab(TabId),
    Category(CategoryId),
}

#[derive(Debug, Clone)]
pub enum Action {
    NewTab {
        category: CategoryId,
        cwd: Option<PathBuf>,
    },
    NewCategory,
    CloseTab(TabId),
    Select(TabId),
    Cycle(i32),
    ToggleCollapse(CategoryId),
    StartRename(RenameTarget),
    CommitRename(RenameTarget, Option<String>),
    Respawn(TabId),
    DeleteCategory(CategoryId),
    SetCategoryColor(CategoryId, usize),
    MoveTab(TabId, CategoryId),
    InstallHooks,
    DismissHooksBanner,
    SetCategoryProfile(CategoryId, Option<PathBuf>),
    /// Jump to the next tab where Claude needs attention (then done-unseen).
    JumpAttention,
    TogglePalette,
    ToggleStatusline(bool),
    RefreshUsage,
    OpenSessions(TabId),
    /// Resume a specific past conversation in a tab.
    ResumeSpecific(TabId, String, PathBuf),
    /// Drag-and-drop: place a tab in a category at a position.
    ReorderTab(TabId, CategoryId, usize),
    /// Run the official install command in a visible tab.
    RunUpdate,
    DismissUpdate,
    ToggleSettings,
    ToggleKeys,
    /// Write one option back to config.toml and apply it now.
    SetSetting(String, giverny_core::settings::Value),
    /// Open config.toml in $EDITOR, in a tab.
    EditConfig,
}

pub struct TabRuntime {
    pub session: Option<TermSession>,
    pub view: TabView,
}

/// Rail width limits: narrow enough to be a strip, wide enough for long
/// tab titles, and the clamp a restored width is held to.
const RAIL_MIN: f32 = 180.0;
const RAIL_MAX: f32 = 420.0;

pub struct App {
    pub shared: RenderShared,
    pub ws: Workspace,
    pub rt: HashMap<TabId, TabRuntime>,
    /// In-progress inline rename: target + edit buffer.
    pub rename: Option<(RenameTarget, String)>,
    pub rename_needs_focus: bool,
    focus_terminal: bool,
    last_info_refresh: Instant,
    paths: Paths,
    state_dirty: bool,
    last_save: Instant,
    pub claude: claude_watch::ClaudeWatch,
    pub hooks_banner_dismissed: bool,
    /// Deferred automated actions (cwd fix, auto-resume) with due times.
    pending_inject: Vec<(Instant, TabId, Inject)>,
    pub palette: Option<overlays::PaletteState>,
    pub session_picker: Option<overlays::SessionPicker>,
    /// Last seen user-interaction counter per tab (detects "typed just now").
    input_seen: HashMap<TabId, u64>,
    /// Tab currently being dragged in the rail.
    pub dragging: Option<TabId>,
    /// Every live claude session started before hooks/statusline were
    /// installed, so none of them report anything (recomputed periodically).
    pub stale_sessions: bool,
    /// A newer release, once the background check finds one.
    pub update: Option<update::Available>,
    update_rx: Option<crossbeam_channel::Receiver<Option<update::Available>>>,
    pub update_dismissed: bool,
    /// Theme-derived colours for Giverny's own chrome.
    pub chrome: chrome::Chrome,
    pub settings: Option<settings_ui::SettingsState>,
    pub keys_overlay: Option<keymap::KeysOverlay>,
    capture: Option<capture::Capture>,
    /// Window size and rail width as last seen, persisted with the workspace.
    layout: state::Layout,
    pub cfg: config::Config,
    cfg_mtime: Option<std::time::SystemTime>,
    last_cfg_check: Instant,
}

/// Automated per-tab injections. All stand down once the user has typed.
#[derive(Debug, Clone)]
enum Inject {
    /// Raw bytes typed into the shell (the resume command).
    Raw(Vec<u8>),
    /// Verify the shell is in the expected directory; shell rc files that
    /// `cd` on startup (e.g. `cd ~/Dev`) override our spawn cwd — type a
    /// visible `cd` back when that happened.
    CwdFix(PathBuf),
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let paths = Paths::default_dirs();
        let mut cfg = config::load(paths.base());
        remember_env_accounts(&paths, &mut cfg);
        let theme = Theme::by_name(&cfg.theme.name);
        let family = (!cfg.font.family.is_empty()).then_some(cfg.font.family.as_str());
        let mut shared = RenderShared::with_family(theme, cfg.font.size, family)
            .or_else(|err| {
                tracing::warn!("configured font unusable ({err}); auto-detecting");
                RenderShared::new(Theme::by_name(&cfg.theme.name), cfg.font.size)
            })
            .expect("font discovery");
        shared.install_ui_fonts(&cc.egui_ctx);
        let chrome = chrome::Chrome::from_theme(&Theme::by_name(&cfg.theme.name));
        chrome.apply(&cc.egui_ctx, &Theme::by_name(&cfg.theme.name));

        let mut cfg_mtime = config_mtime(&paths);
        let restored = state::load(&paths);
        // Font size now lives in config.toml alone. One-time migration for
        // state files written when it lived here instead, so nobody's zoom
        // level is lost the first time they run this build.
        if let Some(st) = &restored
            && st.font_size != DEFAULT_FONT_SIZE
            && cfg.font.size == DEFAULT_FONT_SIZE
        {
            shared.set_font_size(st.font_size);
            if let Some(def) = giverny_core::settings::by_key("font.size") {
                let value = giverny_core::settings::Value::Float(st.font_size as f64);
                if giverny_core::settings::write(paths.base(), def, &value).is_ok() {
                    cfg.font.size = st.font_size;
                    cfg_mtime = config_mtime(&paths);
                }
            }
        }
        let layout = restored.as_ref().map(|st| st.layout).unwrap_or_default();
        let mut ws = restored
            .map(|st| st.workspace)
            .filter(|ws| !ws.tabs.is_empty())
            .unwrap_or_default();

        // Update check runs on its own thread so a slow network never
        // delays startup; the UI picks the answer up when it lands.
        let update_rx = if cfg.update.check {
            let (tx, rx) = crossbeam_channel::bounded(1);
            let base = paths.base().to_path_buf();
            let ping = cc.egui_ctx.clone();
            std::thread::Builder::new()
                .name("giverny update check".into())
                .spawn(move || {
                    let found = update::check(&base, true);
                    let _ = tx.send(found);
                    ping.request_repaint();
                })
                .ok()
                .map(|_| rx)
        } else {
            None
        };

        let wake_ctx = cc.egui_ctx.clone();
        let (claude, spooled) = claude_watch::ClaudeWatch::new(
            &paths.hook_spool(),
            &cfg.behavior.extra_profile_dirs,
            move || wake_ctx.request_repaint(),
        );
        // Events spooled while the app was closed: keep session captures.
        for msg in &spooled {
            let Some(id) = claude_watch::ClaudeWatch::tab_id_of(msg) else {
                continue;
            };
            match msg.hook_event() {
                Some("SessionStart") => {
                    if let Some(tab) = ws.tab_mut(id) {
                        tab.claude_session = msg.session_id().map(str::to_string);
                        tab.claude_config_dir =
                            msg.config_dir.as_deref().map(std::path::PathBuf::from);
                    }
                }
                Some("SessionEnd") => {
                    if let Some(tab) = ws.tab_mut(id) {
                        tab.claude_session = None;
                    }
                }
                _ => {}
            }
        }

        let mut app = App {
            shared,
            ws,
            rt: HashMap::new(),
            rename: None,
            rename_needs_focus: false,
            focus_terminal: true,
            last_info_refresh: Instant::now(),
            paths,
            state_dirty: false,
            last_save: Instant::now(),
            claude,
            hooks_banner_dismissed: false,
            pending_inject: Vec::new(),
            palette: None,
            session_picker: None,
            input_seen: HashMap::new(),
            dragging: None,
            stale_sessions: false,
            update: None,
            update_rx,
            update_dismissed: false,
            chrome,
            settings: None,
            keys_overlay: None,
            capture: capture::Capture::from_env(),
            layout,
            cfg_mtime,
            cfg,
            last_cfg_check: Instant::now(),
        };
        if app.ws.tabs.is_empty() {
            let cat = app.ws.categories[0].id;
            app.apply(
                &cc.egui_ctx,
                Action::NewTab {
                    category: cat,
                    cwd: None,
                },
            );
        }
        // Sessions for restored tabs spawn lazily on first focus; the state
        // file is rewritten now with clean_shutdown=false (crash marker).
        app.save_state(false);
        app
    }

    fn save_state(&mut self, clean_shutdown: bool) {
        let st = SaveState {
            version: state::STATE_VERSION,
            boot_id: state::boot_id(),
            clean_shutdown,
            workspace: self.ws.clone(),
            font_size: self.shared.font_size,
            layout: self.layout,
        };
        if let Err(err) = state::save(&self.paths, &st) {
            tracing::error!("state save failed: {err:#}");
        }
        self.state_dirty = false;
        self.last_save = Instant::now();
    }

    pub fn apply(&mut self, ctx: &egui::Context, action: Action) {
        self.state_dirty = true;
        match action {
            Action::NewTab { category, cwd } => {
                let cwd = cwd
                    .or_else(|| self.ws.active_tab().and_then(|t| t.cwd.clone()))
                    .or_else(dirs::home_dir)
                    .unwrap_or_else(|| PathBuf::from("/"));
                let id = self.ws.add_tab(category);
                self.ws.tab_mut(id).unwrap().cwd = Some(cwd);
                self.spawn_session(ctx, id, None);
                self.focus_terminal = true;
            }
            Action::NewCategory => {
                let n = self.ws.categories.len() + 1;
                let id = self.ws.add_category(&format!("category {n}"));
                self.rename = Some((RenameTarget::Category(id), format!("category {n}")));
                self.rename_needs_focus = true;
            }
            Action::CloseTab(id) => {
                if let Some(rt) = self.rt.remove(&id)
                    && let Some(session) = rt.session
                {
                    // Join off the UI thread; the loop exits quickly.
                    std::thread::spawn(move || session.shutdown());
                }
                self.ws.close_tab(id);
                state::remove_snapshot(&self.paths, id);
                self.focus_terminal = true;
            }
            Action::Select(id) => {
                self.ws.set_active(id);
                self.refresh_tab_info(id);
                self.claude.mark_viewed(id);
                self.focus_terminal = true;
            }
            Action::Cycle(delta) => {
                self.ws.cycle_active(delta);
                if let Some(id) = self.ws.active {
                    self.refresh_tab_info(id);
                    self.claude.mark_viewed(id);
                }
                self.focus_terminal = true;
            }
            Action::ToggleCollapse(id) => {
                if let Some(cat) = self.ws.category_mut(id) {
                    cat.collapsed = !cat.collapsed;
                }
            }
            Action::StartRename(target) => {
                let current = match &target {
                    RenameTarget::Tab(id) => self
                        .ws
                        .tab(*id)
                        // What they see, not the raw title behind it.
                        .map(|t| t.display_title(&self.cfg.titles))
                        .unwrap_or_default(),
                    RenameTarget::Category(id) => self
                        .ws
                        .category(*id)
                        .map(|c| c.name.clone())
                        .unwrap_or_default(),
                };
                self.rename = Some((target, current));
                self.rename_needs_focus = true;
            }
            Action::CommitRename(target, value) => {
                self.rename = None;
                let Some(value) = value else { return };
                let value = value.trim().to_string();
                match target {
                    RenameTarget::Tab(id) => {
                        if let Some(tab) = self.ws.tab_mut(id) {
                            tab.custom_title = (!value.is_empty()).then_some(value);
                        }
                    }
                    RenameTarget::Category(id) => {
                        if !value.is_empty()
                            && let Some(cat) = self.ws.category_mut(id)
                        {
                            cat.name = value;
                        }
                    }
                }
            }
            Action::Respawn(id) => {
                self.spawn_session(ctx, id, None);
                self.queue_resume(id);
                self.focus_terminal = true;
            }
            Action::InstallHooks => match self.claude.install_hooks() {
                Ok(n) => tracing::info!("hooks installed into {n} profile(s)"),
                Err(e) => tracing::error!("hook install: {e}"),
            },
            Action::DismissHooksBanner => self.hooks_banner_dismissed = true,
            Action::SetCategoryProfile(id, dir) => {
                if let Some(cat) = self.ws.category_mut(id) {
                    cat.profile_dir = dir;
                }
            }
            Action::JumpAttention => {
                use claude_watch::ClaudeState;
                let order: Vec<TabId> = self.ws.tabs.iter().map(|t| t.id).collect();
                if order.is_empty() {
                    return;
                }
                let start = self
                    .ws
                    .active
                    .and_then(|a| order.iter().position(|&x| x == a))
                    .map(|i| i + 1)
                    .unwrap_or(0);
                let ring = |k: usize| order[(start + k) % order.len()];
                let pick = (0..order.len())
                    .map(ring)
                    .find(|&t| self.claude.state_of(t) == ClaudeState::NeedsYou)
                    .or_else(|| {
                        (0..order.len())
                            .map(ring)
                            .find(|&t| self.claude.state_of(t) == ClaudeState::DoneUnseen)
                    });
                if let Some(id) = pick {
                    self.apply(ctx, Action::Select(id));
                }
            }
            Action::ToggleStatusline(enable) => match self.claude.set_statusline(enable) {
                Ok(()) => tracing::info!(
                    "statusline {}",
                    if enable { "installed" } else { "removed" }
                ),
                Err(e) => tracing::error!("statusline: {e}"),
            },
            Action::RefreshUsage => self.claude.refresh_stale_usage(0, true),
            Action::ToggleSettings => {
                self.settings = match self.settings.take() {
                    Some(_) => None,
                    None => Some(settings_ui::SettingsState::default()),
                };
                self.focus_terminal = self.settings.is_none();
            }
            Action::ToggleKeys => {
                self.keys_overlay = match self.keys_overlay.take() {
                    Some(_) => None,
                    None => Some(keymap::KeysOverlay::default()),
                };
            }
            Action::SetSetting(key, value) => {
                let Some(def) = giverny_core::settings::by_key(&key) else {
                    tracing::warn!("unknown setting {key}");
                    return;
                };
                match giverny_core::settings::write(self.paths.base(), def, &value) {
                    Ok(()) => {
                        // Apply now rather than waiting for the mtime poll, and
                        // record the mtime we just caused so the watcher does
                        // not reload the same content a second later.
                        self.apply_config(ctx, config::load(self.paths.base()));
                        self.cfg_mtime = config_mtime(&self.paths);
                    }
                    Err(err) => tracing::error!("could not write {key}: {err:#}"),
                }
            }
            Action::EditConfig => {
                let editor = std::env::var("VISUAL")
                    .or_else(|_| std::env::var("EDITOR"))
                    .unwrap_or_else(|_| "nano".into());
                let path = config::config_path(self.paths.base());
                let cat = self
                    .ws
                    .active_tab()
                    .map(|t| t.category)
                    .or_else(|| self.ws.categories.first().map(|c| c.id));
                if let Some(cat) = cat {
                    let id = self.ws.add_tab(cat);
                    self.ws.tab_mut(id).unwrap().cwd = dirs::home_dir();
                    self.spawn_session(ctx, id, None);
                    // Same deferred injection the resume path uses: give the
                    // shell time to be ready before typing into it.
                    self.pending_inject.push((
                        Instant::now() + Duration::from_millis(700),
                        id,
                        Inject::Raw(format!("{editor} {}\n", path.display()).into_bytes()),
                    ));
                    self.settings = None;
                    self.focus_terminal = true;
                }
            }
            Action::TogglePalette => {
                self.palette = if self.palette.is_some() {
                    None
                } else {
                    Some(Default::default())
                };
            }
            Action::OpenSessions(id) => {
                let cwd = self
                    .rt
                    .get(&id)
                    .and_then(|rt| rt.session.as_ref())
                    .and_then(|s| s.proc_cwd())
                    .or_else(|| self.ws.tab(id).and_then(|t| t.cwd.clone()));
                let Some(cwd) = cwd else { return };
                let dirs: Vec<PathBuf> = self
                    .claude
                    .profiles
                    .iter()
                    .map(|p| p.config_dir.clone())
                    .collect();
                let sessions = giverny_claude::registry::list_sessions(&dirs, &cwd);
                self.session_picker = Some(overlays::SessionPicker { tab: id, sessions });
            }
            Action::ResumeSpecific(id, sid, config_dir) => {
                if let Some(tab) = self.ws.tab_mut(id) {
                    tab.claude_session = Some(sid.clone());
                    tab.claude_config_dir = Some(config_dir);
                }
                let has_live_shell = self.rt.get(&id).is_some_and(|rt| rt.session.is_some())
                    && !self.ws.tab(id).is_some_and(|t| t.exited);
                if has_live_shell {
                    if let Some(cmd) = self.resume_command(&sid, id) {
                        // Ctrl+U clears any half-typed line first.
                        let mut bytes = vec![0x15];
                        bytes.extend(cmd);
                        if let Some(session) = self.rt.get(&id).and_then(|rt| rt.session.as_ref()) {
                            session.write(bytes);
                        }
                    }
                } else {
                    // Respawn queues the auto-resume for the new shell.
                    self.apply(ctx, Action::Respawn(id));
                }
                self.apply(ctx, Action::Select(id));
            }
            Action::DeleteCategory(id) => {
                self.ws.remove_category(id);
            }
            Action::SetCategoryColor(id, color_index) => {
                if let Some(cat) = self.ws.category_mut(id) {
                    cat.color_index = color_index;
                }
            }
            Action::MoveTab(tab, category) => {
                self.ws.move_tab_to_category(tab, category);
            }
            Action::ReorderTab(tab, category, index) => {
                self.ws.reorder_tab(tab, category, index);
            }
            Action::RunUpdate => {
                // Deliberately visible: a new tab runs the same command a new
                // user would, so nothing rewrites the binary out of sight.
                let category = self
                    .ws
                    .active_tab()
                    .map(|t| t.category)
                    .or_else(|| self.ws.categories.first().map(|c| c.id));
                if let Some(category) = category {
                    self.apply(
                        ctx,
                        Action::NewTab {
                            category,
                            cwd: None,
                        },
                    );
                    if let Some(id) = self.ws.active {
                        let mut cmd = update::install_command().into_bytes();
                        cmd.push(b'\r');
                        self.pending_inject.push((
                            Instant::now() + Duration::from_millis(1100),
                            id,
                            Inject::Raw(cmd),
                        ));
                    }
                }
                self.update_dismissed = true;
            }
            Action::DismissUpdate => self.update_dismissed = true,
        }
    }

    fn spawn_session(&mut self, ctx: &egui::Context, id: TabId, preseed: Option<String>) {
        let Some(tab) = self.ws.tab(id) else { return };
        let cwd = tab
            .cwd
            .clone()
            .filter(|p| p.is_dir())
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("/"));
        let profile_dir = self
            .ws
            .tab(id)
            .and_then(|t| self.ws.category(t.category))
            .and_then(|c| c.profile_dir.clone());
        let cfg = SpawnCfg {
            shell: None,
            cwd: cwd.clone(),
            env_extra: vec![],
            tab_id: format!("giverny-{}", id.0),
            nonce: fresh_nonce(id.0),
            claude_config_dir: profile_dir,
            size: GridSize {
                cols: 120,
                rows: 30,
                cell_width: 9,
                cell_height: 18,
            },
        };
        match TermSession::spawn(
            &cfg,
            ctx.clone(),
            self.shared.theme.clone(),
            preseed.as_deref(),
        ) {
            Ok(session) => {
                self.ws.tab_mut(id).unwrap().exited = false;
                let entry = self.rt.entry(id).or_insert_with(|| TabRuntime {
                    session: None,
                    view: TabView::default(),
                });
                entry.session = Some(session);
                // Startup rc files may `cd` away from the spawn dir; verify
                // and correct once the shell has settled.
                self.pending_inject.push((
                    Instant::now() + Duration::from_millis(900),
                    id,
                    Inject::CwdFix(cwd),
                ));
            }
            Err(err) => {
                tracing::error!("spawn failed for tab {id:?}: {err:#}");
                if let Some(tab) = self.ws.tab_mut(id) {
                    tab.exited = true;
                    tab.auto_title = format!("spawn failed: {err}");
                }
            }
        }
    }

    fn drain_events(&mut self) {
        let mut cwd_updates: Vec<(TabId, PathBuf)> = Vec::new();
        for (&id, rt) in &self.rt {
            let Some(session) = &rt.session else { continue };
            while let Ok(ev) = session.events.try_recv() {
                match ev {
                    TabEvent::Title(title) => {
                        if let Some(tab) = self.ws.tab_mut(id) {
                            tab.auto_title = title.unwrap_or_default();
                        }
                    }
                    TabEvent::Tee(events) => {
                        for te in events {
                            match te {
                                TeeEvent::CwdChanged(p) => cwd_updates.push((id, p)),
                                TeeEvent::RemoteCwd(p) => {
                                    tracing::debug!("remote cwd on tab {id:?}: {p:?}");
                                }
                                other => tracing::debug!("tee {id:?}: {other:?}"),
                            }
                        }
                    }
                    TabEvent::Bell => tracing::debug!("bell on tab {id:?}"),
                    TabEvent::ChildExit(status) => {
                        tracing::info!("tab {id:?} child exited: {status:?}");
                    }
                    TabEvent::LoopDone(_) => {
                        if let Some(tab) = self.ws.tab_mut(id) {
                            tab.exited = true;
                        }
                    }
                }
            }
        }
        for (id, cwd) in cwd_updates {
            if let Some(tab) = self.ws.tab_mut(id) {
                tab.git_branch = giverny_core::git::branch_of(&cwd);
                tab.cwd = Some(cwd);
                self.state_dirty = true;
            }
        }
    }

    /// Refresh cwd (via /proc) and git branch for one tab.
    fn refresh_tab_info(&mut self, id: TabId) {
        let pid = self
            .rt
            .get(&id)
            .and_then(|rt| rt.session.as_ref())
            .and_then(|s| s.child_pid);
        let Some(tab) = self.ws.tab_mut(id) else {
            return;
        };
        #[cfg(target_os = "linux")]
        if let Some(pid) = pid
            && let Ok(cwd) = std::fs::read_link(format!("/proc/{pid}/cwd"))
        {
            tab.cwd = Some(cwd);
        }
        #[cfg(not(target_os = "linux"))]
        let _ = pid;
        tab.git_branch = tab.cwd.as_deref().and_then(giverny_core::git::branch_of);
    }

    /// Hot-reload `config.toml` when it changes on disk.
    fn reload_config_if_changed(&mut self, ctx: &egui::Context) {
        if self.last_cfg_check.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_cfg_check = Instant::now();
        let mtime = config_mtime(&self.paths);
        if mtime == self.cfg_mtime {
            return;
        }
        self.cfg_mtime = mtime;
        let cfg = config::load(self.paths.base());
        self.apply_config(ctx, cfg);
    }

    /// Adopt a freshly loaded config, applying what can be applied live.
    fn apply_config(&mut self, ctx: &egui::Context, cfg: config::Config) {
        if cfg.theme.name != self.cfg.theme.name {
            let theme = Theme::by_name(&cfg.theme.name);
            self.shared.set_theme(theme.clone());
            for rt in self.rt.values() {
                if let Some(session) = &rt.session {
                    *session.shared.theme.write() = Theme::by_name(&cfg.theme.name);
                    session.mark_dirty();
                }
            }
            // The chrome is themed too, so the rail does not stay Monet-blue
            // around a Gruvbox grid.
            self.chrome = chrome::Chrome::from_theme(&theme);
            self.chrome.apply(ctx, &theme);
        }
        if cfg.font.size != self.cfg.font.size {
            self.shared.set_font_size(cfg.font.size);
        }
        if cfg.font.family != self.cfg.font.family {
            tracing::info!("font family changed — restart Giverny to apply");
        }
        self.cfg = cfg;
        tracing::info!("config reloaded");
    }

    /// Dropping files types their paths into the active tab, the way every
    /// other terminal behaves — and the way you hand Claude an image.
    ///
    /// Routed through the paste encoder, so the text is bracketed-paste
    /// wrapped and escape-sanitized: a filename is untrusted input, and a
    /// path arriving as if typed must not be able to run anything.
    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let paths: Vec<String> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.as_ref())
                .map(|p| p.display().to_string())
                .collect()
        });
        if paths.is_empty() {
            return;
        }
        let text = giverny_term::input::dropped_paths_text(&paths);
        let Some(id) = self.ws.active else { return };
        if let Some(session) = self.rt.get(&id).and_then(|rt| rt.session.as_ref()) {
            session.write(giverny_term::input::encode_paste(&text, session.mode()));
            session.note_user_input();
            self.focus_terminal = true;
        }
    }

    /// Notice window and rail resizes so they persist with everything else.
    ///
    /// Read back from the window rather than tracked at the drag, because the
    /// window manager has the last word: a tiled or snapped window ends up a
    /// size nobody asked for, and that is still the size to reopen at.
    fn track_layout(&mut self, ctx: &egui::Context) {
        let before = self.layout;
        let (maximized, viewport_rect) = ctx.input(|i| {
            let vp = i.viewport();
            (
                vp.maximized.unwrap_or(false) || vp.fullscreen.unwrap_or(false),
                i.viewport_rect(),
            )
        });
        // Deliberately not `viewport().inner_rect`: it is derived from the
        // window's *position*, which Wayland never reports, so it is None on
        // the primary platform. The egui surface is the window's inner area
        // everywhere; scaled by the zoom factor it is the logical size
        // `with_inner_size` wants back.
        let size = viewport_rect.size() * ctx.zoom_factor();
        self.layout.maximized = maximized;
        // A maximized window's size is the screen's, not the user's choice —
        // storing it would reopen unmaximized at monitor size.
        if !maximized && size.x > 0.0 && size.y > 0.0 {
            self.layout.window = Some([size.x, size.y]);
        }
        if let Some(rail) = egui::PanelState::load(ctx, egui::Id::new("rail")) {
            self.layout.rail_width = Some(rail.size().x);
        }

        let moved = |a: Option<f32>, b: Option<f32>| match (a, b) {
            (Some(a), Some(b)) => (a - b).abs() >= 1.0,
            (a, b) => a.is_some() != b.is_some(),
        };
        let changed = self.layout.maximized != before.maximized
            || moved(self.layout.rail_width, before.rail_width)
            || moved(
                self.layout.window.map(|w| w[0]),
                before.window.map(|w| w[0]),
            )
            || moved(
                self.layout.window.map(|w| w[1]),
                before.window.map(|w| w[1]),
            );
        if changed {
            self.state_dirty = true;
        }
    }

    /// Ctrl+±/0 changes the font size live. Write it back to `config.toml`,
    /// which is the single place the size lives — it used to be persisted
    /// separately in the state file too, where it silently outranked the
    /// config and the settings screen.
    fn persist_font_size(&mut self) {
        let live = self.shared.font_size;
        if (live - self.cfg.font.size).abs() < 0.01 {
            return;
        }
        let Some(def) = giverny_core::settings::by_key("font.size") else {
            return;
        };
        let value = giverny_core::settings::Value::Float(live as f64);
        match giverny_core::settings::write(self.paths.base(), def, &value) {
            Ok(()) => {
                self.cfg.font.size = live;
                self.cfg_mtime = config_mtime(&self.paths);
            }
            Err(err) => tracing::warn!("could not persist font size: {err:#}"),
        }
    }

    /// Remember what each tab is running, so a restart can bring it back.
    fn track_foreground(&mut self) {
        let seen: Vec<(TabId, Option<String>)> = self
            .rt
            .iter()
            .filter_map(|(&id, rt)| {
                let pid = rt.session.as_ref()?.child_pid?;
                Some((id, giverny_core::procs::foreground_command(pid)))
            })
            .collect();
        for (id, cmd) in seen {
            if let Some(tab) = self.ws.tab_mut(id)
                && tab.foreground != cmd
            {
                tab.foreground = cmd;
                self.state_dirty = true;
            }
        }
    }

    fn periodic_refresh(&mut self, ctx: &egui::Context) {
        self.reload_config_if_changed(ctx);
        if self.state_dirty && self.last_save.elapsed() > Duration::from_secs(2) {
            self.save_state(false);
        }
        if self.last_info_refresh.elapsed() < Duration::from_secs(2) {
            return;
        }
        self.last_info_refresh = Instant::now();
        if let Some(rx) = &self.update_rx
            && let Ok(found) = rx.try_recv()
        {
            self.update = found;
            self.update_rx = None;
        }
        self.persist_font_size();
        self.track_foreground();
        self.stale_sessions = self.claude.sessions_predate_settings();
        // Ask Claude Code to refresh accounts whose numbers have aged out.
        self.claude
            .refresh_stale_usage(self.cfg.usage.refresh_minutes, false);
        if let Some(id) = self.ws.active {
            self.refresh_tab_info(id);
        }
    }

    /// Restart the full-screen program this tab was running, when it is one
    /// Giverny is allowed to start by itself.
    fn queue_app_restore(&mut self, id: TabId) {
        let Some(tab) = self.ws.tab(id) else { return };
        // A tab with a Claude session resumes that instead.
        if tab.claude_session.is_some() {
            return;
        }
        let Some(cmd) = tab.foreground.clone() else {
            return;
        };
        if !giverny_core::procs::is_restorable(&cmd, &self.cfg.behavior.restore_apps) {
            tracing::info!("tab {id:?}: not restarting {cmd:?} (not on the restore list)");
            return;
        }
        let mut bytes = cmd.into_bytes();
        bytes.push(b'\r');
        self.pending_inject.push((
            Instant::now() + Duration::from_millis(1200),
            id,
            Inject::Raw(bytes),
        ));
    }

    /// Queue the auto-resume command for a freshly restored tab.
    fn queue_resume(&mut self, id: TabId) {
        let Some(sid) = self.ws.tab(id).and_then(|t| t.claude_session.clone()) else {
            return;
        };
        if let Some(cmd) = self.resume_command(&sid, id) {
            self.pending_inject.push((
                Instant::now() + Duration::from_millis(1300),
                id,
                Inject::Raw(cmd),
            ));
        }
    }

    /// Build the shell command that resumes `sid` in tab `id`, with every
    /// guard: id validation, double-resume protection, transcript lookup
    /// across profiles (self-heals a lost account association), and the
    /// conversation's own recorded cwd (`claude --resume` only finds a
    /// session from the directory it ran in).
    fn resume_command(&self, sid: &str, id: TabId) -> Option<Vec<u8>> {
        use giverny_claude::registry;
        if sid.len() != 36 || !sid.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
            tracing::warn!("tab {id:?}: malformed session id {sid:?} — not resuming");
            return None;
        }
        let all_dirs: Vec<PathBuf> = self
            .claude
            .profiles
            .iter()
            .map(|p| p.config_dir.clone())
            .collect();
        if registry::session_is_live(all_dirs.clone(), sid) {
            tracing::info!("claude session {sid} already live elsewhere — not resuming");
            return None;
        }

        let tab = self.ws.tab(id)?;
        let preferred = tab.claude_config_dir.clone();
        let mut search: Vec<PathBuf> = preferred.iter().cloned().collect();
        search.extend(
            all_dirs
                .into_iter()
                .filter(|d| Some(d) != preferred.as_ref()),
        );
        let Some((config_dir, transcript)) = search
            .iter()
            .find_map(|d| registry::find_transcript(d, sid).map(|t| (d.clone(), t)))
        else {
            tracing::info!("tab {id:?}: no transcript for session {sid} — skipping resume");
            return None;
        };

        let resume_dir = registry::transcript_cwd(&transcript).or_else(|| tab.cwd.clone());

        let mut cmd = String::new();
        if let Some(dir) = &resume_dir {
            cmd.push_str(&format!("cd \"{}\" && ", dir.display()));
        }
        let is_default_profile = dirs::home_dir().is_some_and(|h| h.join(".claude") == config_dir);
        if !is_default_profile {
            cmd.push_str(&format!("CLAUDE_CONFIG_DIR=\"{}\" ", config_dir.display()));
        }
        // `command` bypasses shell wrapper functions named `claude`.
        cmd.push_str(&format!("command claude --resume {sid}\r"));
        Some(cmd.into_bytes())
    }

    fn process_pending(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        let mut i = 0;
        while i < self.pending_inject.len() {
            if self.pending_inject[i].0 <= now {
                let (_, id, inject) = self.pending_inject.remove(i);
                let Some(session) = self.rt.get(&id).and_then(|rt| rt.session.as_ref()) else {
                    continue;
                };
                // The user took over — automated typing stands down.
                if session.had_user_input() {
                    continue;
                }
                match inject {
                    Inject::Raw(bytes) => session.write(bytes),
                    Inject::CwdFix(expected) => {
                        let actual = session.proc_cwd();
                        if actual.as_ref().is_some_and(|a| *a != expected) && expected.is_dir() {
                            session.write(format!("cd \"{}\"\r", expected.display()).into_bytes());
                        }
                    }
                }
            } else {
                i += 1;
            }
        }
        if !self.pending_inject.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) -> Vec<Action> {
        let mut actions = Vec::new();
        ctx.input_mut(|i| {
            let cs = Modifiers::CTRL | Modifiers::SHIFT;
            if i.consume_key(cs, Key::T)
                && let Some(cat) = self
                    .ws
                    .active_tab()
                    .map(|t| t.category)
                    .or_else(|| self.ws.categories.first().map(|c| c.id))
            {
                actions.push(Action::NewTab {
                    category: cat,
                    cwd: None,
                });
            }
            if i.consume_key(cs, Key::W)
                && let Some(id) = self.ws.active
            {
                actions.push(Action::CloseTab(id));
            }
            if i.consume_key(cs, Key::A) {
                actions.push(Action::JumpAttention);
            }
            if i.consume_key(cs, Key::P) {
                actions.push(Action::TogglePalette);
            }
            if i.consume_key(Modifiers::CTRL, Key::Comma) {
                actions.push(Action::ToggleSettings);
            }
            // F1 anywhere, and Ctrl+Shift+/ for muscle memory from editors.
            if i.consume_key(Modifiers::NONE, Key::F1) || i.consume_key(cs, Key::Slash) {
                actions.push(Action::ToggleKeys);
            }
            if i.consume_key(Modifiers::CTRL, Key::PageDown) {
                actions.push(Action::Cycle(1));
            }
            if i.consume_key(Modifiers::CTRL, Key::PageUp) {
                actions.push(Action::Cycle(-1));
            }
            if i.consume_key(Modifiers::NONE, Key::F2)
                && let Some(id) = self.ws.active
            {
                actions.push(Action::StartRename(RenameTarget::Tab(id)));
            }
        });
        actions
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Documentation capture (GIVERNY_CAPTURE); no-op otherwise.
        if let Some(cap) = &mut self.capture {
            cap.on_frame(ui.ctx());
            if cap.done() {
                self.capture = None;
            }
        }
        self.drain_events();

        let ctx = ui.ctx().clone();
        self.periodic_refresh(&ctx);
        self.handle_dropped_files(&ctx);
        self.process_pending(&ctx);

        // Claude awareness: hooks + registry + usage.
        let shell_pids: HashMap<TabId, u32> = self
            .rt
            .iter()
            .filter_map(|(&id, rt)| {
                rt.session
                    .as_ref()
                    .and_then(|s| s.child_pid)
                    .map(|p| (id, p))
            })
            .collect();
        let titles: HashMap<TabId, String> = self
            .ws
            .tabs
            .iter()
            .map(|t| (t.id, t.display_title(&self.cfg.titles)))
            .collect();
        // Typing in a tab counts as attending to it: declining a permission
        // prompt (Escape) emits no hook, so nothing else would clear the flag.
        for (&id, rt) in &self.rt {
            let Some(session) = &rt.session else { continue };
            let seq = session.input_seq();
            if self
                .input_seen
                .insert(id, seq)
                .is_some_and(|prev| prev != seq)
            {
                self.claude.mark_attended(id);
            }
        }

        let effects = self.claude.tick(&shell_pids, self.ws.active, &titles);
        for (id, session, config_dir) in effects.captured {
            if let Some(tab) = self.ws.tab_mut(id) {
                tab.claude_session = session;
                if config_dir.is_some() {
                    tab.claude_config_dir = config_dir;
                }
                self.state_dirty = true;
            }
        }
        for (summary, body) in effects.notify {
            desktop_notify(summary, body);
        }
        if effects.animating {
            ctx.request_repaint_after(Duration::from_millis(120));
        } else {
            // Heartbeat: egui only repaints on demand, so without this the
            // registry scan (and therefore state transitions for tabs whose
            // output isn't waking the UI) would stall while the window idles.
            ctx.request_repaint_after(Duration::from_millis(700));
        }

        let mut actions = self.shortcuts(&ctx);

        egui::Panel::left("rail")
            .resizable(true)
            .default_size(
                self.layout
                    .rail_width_in(RAIL_MIN..=RAIL_MAX)
                    .unwrap_or(240.0),
            )
            .size_range(RAIL_MIN..=RAIL_MAX)
            .show(ui, |ui| {
                actions.extend(rail::show(self, ui));
            });
        self.track_layout(&ctx);

        for action in actions.drain(..) {
            self.apply(&ctx, action);
        }

        egui::CentralPanel::default().show(ui, |ui| {
            // Settings take the terminal's place, so the rail stays visible
            // and changes to it can be watched landing. The shell behind is
            // untouched and keeps running.
            if self.settings.is_some() {
                let acts = settings_ui::settings_ui(self, ui);
                actions.extend(acts);
                return;
            }
            let Some(active) = self.ws.active else {
                ui.centered_and_justified(|ui| {
                    ui.label("no tabs — Ctrl+Shift+T opens one");
                });
                return;
            };

            // Category accent strip above the pane: you always know where you are.
            let accent = self
                .ws
                .tab(active)
                .and_then(|t| self.ws.category(t.category))
                .map(|c| category_color(c.color_index))
                .unwrap_or(Color32::GRAY);
            let (strip, _) = ui.allocate_exact_size(
                egui::Vec2::new(ui.available_width(), 3.0),
                egui::Sense::hover(),
            );
            ui.painter().rect_filled(strip, 0.0, accent);

            // Files hovering over the window: say what a drop will do, so a
            // drag is not a guess. Wayland never reports this (below).
            // Name the destination: with this winit a drop cannot be aimed at
            // a particular tab — no drag position is reported — so the one
            // thing to be clear about is which tab it lands in. winit master
            // does report positions, which is what hovering a tab would need.
            let hovering = ctx.input(|i| i.raw.hovered_files.len());
            if hovering > 0 {
                let into = self
                    .ws
                    .tab(active)
                    .map(|t| t.display_title(&self.cfg.titles))
                    .unwrap_or_default();
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "drop {} path{} into  {into}",
                            hovering,
                            if hovering == 1 { "" } else { "s" }
                        ))
                        .font(egui::FontId::monospace(11.0))
                        .color(self.chrome.accent),
                    );
                });
            }

            let exited = self.ws.tab(active).is_some_and(|t| t.exited);
            if exited {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("process exited")
                            .color(Color32::from_rgb(0xd9, 0xb5, 0x5f)),
                    );
                    if ui.small_button("respawn").clicked() {
                        actions.push(Action::Respawn(active));
                    }
                });
            }

            // Lazy restore: a tab from a previous run spawns its shell on
            // first focus, pre-seeded with its saved scrollback.
            if !self.rt.contains_key(&active) && !self.ws.tab(active).is_some_and(|t| t.exited) {
                let preseed = state::load_snapshot(&self.paths, active);
                self.spawn_session(&ctx, active, preseed);
                // The tab had a live Claude conversation — resume it; or a
                // full-screen app worth starting again.
                self.queue_resume(active);
                self.queue_app_restore(active);
            }

            if let Some(rt) = self.rt.get_mut(&active) {
                if let Some(session) = &mut rt.session {
                    let response = rt.view.show(ui, &mut self.shared, session);
                    if self.focus_terminal {
                        response.request_focus();
                        self.focus_terminal = false;
                    }
                } else {
                    ui.centered_and_justified(|ui| {
                        ui.label("no session");
                    });
                }
            }
        });

        actions.extend(overlays::palette_ui(self, &ctx));
        actions.extend(overlays::sessions_ui(self, &ctx));
        actions.extend(keymap::overlay_ui(self, &ctx));

        for action in actions {
            self.apply(&ctx, action);
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // Scrollback snapshots for every live tab, then the final state write.
        let ids: Vec<TabId> = self.rt.keys().copied().collect();
        for id in ids {
            if let Some(session) = self.rt.get(&id).and_then(|rt| rt.session.as_ref())
                && let Some(dump) = session.snapshot_ansi(4000)
                && let Err(err) = state::save_snapshot(&self.paths, id, &dump)
            {
                tracing::error!("snapshot save failed for {id:?}: {err:#}");
            }
        }
        self.save_state(true);
        for (_, rt) in self.rt.drain() {
            if let Some(session) = rt.session {
                session.shutdown();
            }
        }
    }
}

/// `giverny doctor` — print exactly what the app sees of your Claude setup.
fn doctor() {
    use giverny_claude::{hooks, profiles, registry, usage};

    println!("giverny doctor\n");

    // Hook transport differs by platform: a unix socket where one exists,
    // otherwise the spool file the relay always falls back to.
    #[cfg(unix)]
    {
        let socket = hooks::socket_path();
        let app_running = std::os::unix::net::UnixStream::connect(&socket).is_ok();
        println!("relay        unix socket {}", socket.display());
        println!(
            "app running  {}\n",
            if app_running {
                "yes (hooks deliver live)"
            } else {
                "no (events spool to disk until it starts)"
            }
        );
    }
    #[cfg(not(unix))]
    {
        println!(
            "relay        spool file {}\n",
            Paths::default_dirs().hook_spool().display()
        );
    }

    // On Wayland the taskbar icon comes from the desktop entry, not from us.
    #[cfg(target_os = "linux")]
    if let Some((entry, installed)) = desktop::status() {
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
        println!(
            "desktop entry {}",
            match (installed, wayland) {
                (true, _) => format!("installed ✓  {}", entry.display()),
                (false, true) => "MISSING — run `giverny install-desktop` \
                                  (Wayland takes the taskbar icon from it)"
                    .to_string(),
                (false, false) => "not installed (X11 uses the built-in window icon)".to_string(),
            }
        );
        println!();
    }

    let cfg = config::load(Paths::default_dirs().base());
    let profs = profiles::discover(&cfg.behavior.extra_profile_dirs);
    if profs.is_empty() {
        println!("NO CLAUDE PROFILES FOUND — is Claude Code installed?");
        return;
    }
    // Name the sources consulted: when an account goes missing, "where do
    // these come from" is the actual question.
    let mut sources = vec!["~/.claude"];
    if giverny_claude::profiles::ambient_dirs().len() > 1 {
        sources.push("claude* in ~ and ~/.config");
    }
    if std::env::var_os("CLAUDE_CONFIG_DIR").is_some() {
        sources.push("$CLAUDE_CONFIG_DIR");
    }
    if std::env::var_os("CCTOP_CONFIG_DIRS").is_some() {
        sources.push("$CCTOP_CONFIG_DIRS");
    }
    if !cfg.behavior.extra_profile_dirs.is_empty() {
        sources.push("config");
    }
    println!(
        "profiles ({} found via {}):",
        profs.len(),
        sources.join(" + ")
    );
    drag_drop_status();
    println!("      an account kept elsewhere? add its directory in settings → claude,");
    println!("      or as behavior.extra_profile_dirs in config.toml");
    let now = jiff::Timestamp::now();
    for p in &profs {
        let settings = p.config_dir.join("settings.json");
        let hooks_ok = hooks::installed_in(&settings);
        let sl_ok = hooks::statusline_installed_in(&settings);
        println!(
            "\n  @{}  {}",
            p.name,
            p.email.as_deref().unwrap_or("(identity unknown)")
        );
        println!("    dir        {}", p.config_dir.display());
        println!(
            "    hooks      {}",
            if hooks_ok {
                "installed ✓"
            } else {
                "MISSING — click 'install' in the rail"
            }
        );
        println!(
            "    statusline {}",
            if sl_ok {
                "installed ✓ (live usage)"
            } else {
                "not installed (usage from cache only)"
            }
        );
        match usage::read(&p.config_dir) {
            Some(u) => {
                let age = usage::age_minutes(&u, now);
                let buckets: Vec<String> = u
                    .limits
                    .iter()
                    .map(|l| format!("{} {}%", l.label(), l.effective_percent(now).round()))
                    .collect();
                println!("    usage      {} (cache {age}m old)", buckets.join(", "));
            }
            None => println!("    usage      no cache yet — run /usage once in this account"),
        }
    }

    let dirs: Vec<PathBuf> = profs.iter().map(|p| p.config_dir.clone()).collect();
    let live = registry::scan(dirs);
    println!("\nlive claude sessions ({}):", live.len());
    let mut stale = 0;
    for s in &live {
        // Sessions that started before settings.json was last written never
        // loaded our hooks or statusline.
        let settings_at = std::fs::metadata(s.config_dir.join("settings.json"))
            .and_then(|m| m.modified())
            .ok();
        let started = std::time::UNIX_EPOCH
            .checked_add(std::time::Duration::from_millis(s.entry.started_at_ms));
        let predates = matches!((settings_at, started), (Some(a), Some(b)) if b < a);
        if predates {
            stale += 1;
        }
        println!(
            "  pid {:<8} {:<6} {:<26} {:<12} {}",
            s.entry.pid,
            s.entry.status,
            s.entry.name.as_deref().unwrap_or("-"),
            if predates { "PRE-HOOKS" } else { "hooked" },
            s.entry.cwd.display()
        );
    }
    if stale > 0 {
        println!(
            "\n  ⟳ {stale} session(s) started before hooks/statusline were installed.\n    \
             Claude Code reads settings.json at session start — exit and re-run\n    \
             claude in those tabs to get live states and live usage."
        );
    }

    println!(
        "\nnotes\n  · hooks load when a claude session STARTS — restart claude after installing\n  \
         · notifications fire when claude needs YOU (permission prompts, questions),\n    \
         not when it merely finishes"
    );
}

/// `giverny update` — headless check, for scripts and impatient users.
fn update_cli() {
    let paths = Paths::default_dirs();
    println!("giverny {CURRENT}", CURRENT = update::CURRENT);
    match update::fetch_latest() {
        Ok(latest) => match update::is_newer(&latest, update::CURRENT) {
            Some(true) => {
                println!(
                    "update available: {latest}\n\nrun:\n  {}",
                    update::install_command()
                );
            }
            _ => println!("up to date (latest release is {latest})"),
        },
        Err(err) => println!("could not reach github: {err}"),
    }
    let _ = paths;
}

fn config_mtime(paths: &Paths) -> Option<std::time::SystemTime> {
    std::fs::metadata(config::config_path(paths.base()))
        .ok()
        .and_then(|m| m.modified().ok())
}

fn desktop_notify(summary: String, body: String) {
    std::thread::spawn(move || {
        let _ = notify_rust::Notification::new()
            .appname("Giverny")
            .summary(&summary)
            .body(&body)
            .show();
    });
}

fn fresh_nonce(salt: u64) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}{:x}{:x}", nanos, std::process::id(), salt)
}
