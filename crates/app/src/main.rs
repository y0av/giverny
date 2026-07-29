//! Giverny — a native terminal built around Claude Code.

mod claude_watch;
mod overlays;
mod rail;

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

fn main() -> eframe::Result {
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
        Some("--help" | "-h") => {
            println!(
                "giverny — a native terminal built around Claude Code\n\n\
                 USAGE:\n  giverny            launch the terminal\n  \
                 giverny doctor     diagnose Claude integration\n  \
                 giverny relay      (internal) Claude Code hook entrypoint\n  \
                 giverny statusline (internal) Claude Code statusline entrypoint"
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

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id("giverny")
            .with_title("Giverny")
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([640.0, 400.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Giverny",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
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
    OpenSessions(TabId),
    /// Resume a specific past conversation in a tab.
    ResumeSpecific(TabId, String, PathBuf),
    /// Drag-and-drop: place a tab in a category at a position.
    ReorderTab(TabId, CategoryId, usize),
}

pub struct TabRuntime {
    pub session: Option<TermSession>,
    pub view: TabView,
}

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
        let cfg = config::load(paths.base());
        let theme = Theme::by_name(&cfg.theme.name);
        let family = (!cfg.font.family.is_empty()).then_some(cfg.font.family.as_str());
        let mut shared = RenderShared::with_family(theme, cfg.font.size, family)
            .or_else(|err| {
                tracing::warn!("configured font unusable ({err}); auto-detecting");
                RenderShared::new(Theme::by_name(&cfg.theme.name), cfg.font.size)
            })
            .expect("font discovery");
        shared.install_ui_fonts(&cc.egui_ctx);

        let cfg_mtime = config_mtime(&paths);
        let restored = state::load(&paths);
        // A font size chosen live with Ctrl+± wins over the config default.
        if let Some(st) = &restored
            && st.font_size != DEFAULT_FONT_SIZE
        {
            shared.set_font_size(st.font_size);
        }
        let mut ws = restored
            .map(|st| st.workspace)
            .filter(|ws| !ws.tabs.is_empty())
            .unwrap_or_default();

        let wake_ctx = cc.egui_ctx.clone();
        let (claude, spooled) =
            claude_watch::ClaudeWatch::new(&paths.hook_spool(), move || wake_ctx.request_repaint());
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
                        .map(|t| t.title().to_string())
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
    fn reload_config_if_changed(&mut self) {
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
        if cfg.theme.name != self.cfg.theme.name {
            self.shared.set_theme(Theme::by_name(&cfg.theme.name));
            for rt in self.rt.values() {
                if let Some(session) = &rt.session {
                    *session.shared.theme.write() = Theme::by_name(&cfg.theme.name);
                    session.mark_dirty();
                }
            }
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

    fn periodic_refresh(&mut self) {
        self.reload_config_if_changed();
        if self.state_dirty && self.last_save.elapsed() > Duration::from_secs(2) {
            self.save_state(false);
        }
        if self.last_info_refresh.elapsed() < Duration::from_secs(2) {
            return;
        }
        self.last_info_refresh = Instant::now();
        if let Some(id) = self.ws.active {
            self.refresh_tab_info(id);
        }
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
        self.drain_events();
        self.periodic_refresh();

        let ctx = ui.ctx().clone();
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
            .map(|t| (t.id, t.title().to_string()))
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
            .default_size(240.0)
            .size_range(180.0..=420.0)
            .show(ui, |ui| {
                actions.extend(rail::show(self, ui));
            });

        for action in actions.drain(..) {
            self.apply(&ctx, action);
        }

        egui::CentralPanel::default().show(ui, |ui| {
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
                // The tab had a live Claude conversation — resume it.
                self.queue_resume(active);
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

    let profs = profiles::discover(&[]);
    if profs.is_empty() {
        println!("NO CLAUDE PROFILES FOUND — is Claude Code installed?");
        return;
    }
    println!(
        "profiles ({} found via ~/.claude + CCTOP_CONFIG_DIRS):",
        profs.len()
    );
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
    for s in &live {
        println!(
            "  pid {:<8} {:<6} {:<28} {}",
            s.entry.pid,
            s.entry.status,
            s.entry.name.as_deref().unwrap_or("-"),
            s.entry.cwd.display()
        );
    }

    println!(
        "\nnotes\n  · hooks load when a claude session STARTS — restart claude after installing\n  \
         · notifications fire when claude needs YOU (permission prompts, questions),\n    \
         not when it merely finishes"
    );
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
