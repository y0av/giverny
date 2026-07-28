//! App-side Claude awareness: merges the hook relay stream with the
//! `sessions/<pid>.json` registry into per-tab states, and refreshes the
//! per-account usage meters.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use giverny_claude::hooks::{self, RelayMsg};
use giverny_claude::profiles::{self, Profile};
use giverny_claude::registry;
use giverny_claude::usage::{self, AccountUsage};
use giverny_core::tabs::TabId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClaudeState {
    /// No Claude running in this tab.
    #[default]
    None,
    /// Claude open, waiting at its prompt.
    Idle,
    /// Claude is working.
    Busy,
    /// Claude needs the user (permission / question / agent input).
    NeedsYou,
    /// Claude finished while the tab was in the background.
    DoneUnseen,
}

#[derive(Debug, Clone, Default)]
pub struct ClaudeTab {
    pub state: ClaudeState,
    pub session_id: Option<String>,
    pub session_name: Option<String>,
    /// Short account name (profile) this tab's Claude runs under.
    pub account: Option<String>,
    last_hook: Option<Instant>,
    seen_in_scan: bool,
}

pub struct AccountPanel {
    pub profile: Profile,
    pub usage: Option<AccountUsage>,
}

/// Side effects for the app to apply after a tick.
#[derive(Default)]
pub struct WatchEffects {
    /// `(tab, session_id, config_dir)` — `None` session means it ended.
    pub captured: Vec<(TabId, Option<String>, Option<PathBuf>)>,
    /// Desktop notifications to fire: `(summary, body)`.
    pub notify: Vec<(String, String)>,
    /// Any tab is animating (spinner/pulse) — keep repainting.
    pub animating: bool,
}

pub struct ClaudeWatch {
    pub profiles: Vec<Profile>,
    pub tabs: HashMap<TabId, ClaudeTab>,
    pub accounts: Vec<AccountPanel>,
    pub hooks_installed: bool,
    hook_rx: Option<Receiver<RelayMsg>>,
    last_scan: Instant,
    last_usage: Instant,
}

fn needs_you(notification_type: &str) -> bool {
    matches!(
        notification_type,
        "permission_prompt" | "elicitation_dialog" | "agent_needs_input"
    )
}

fn done_kind(notification_type: &str) -> bool {
    matches!(
        notification_type,
        "idle_prompt" | "agent_completed" | "task_completed"
    )
}

impl ClaudeWatch {
    pub fn new(spool: &Path, wake: impl Fn() + Send + 'static) -> (Self, Vec<RelayMsg>) {
        let profiles = profiles::discover(&[]);
        #[cfg(unix)]
        let (hook_rx, spooled) = match hooks::spawn_listener(spool, wake) {
            Ok((rx, spooled)) => (Some(rx), spooled),
            Err(err) => {
                tracing::warn!("hook listener unavailable: {err:#}");
                (None, Vec::new())
            }
        };
        #[cfg(not(unix))]
        let (hook_rx, spooled) = {
            let _ = (spool, wake);
            (None, Vec::new())
        };

        let mut watch = ClaudeWatch {
            hooks_installed: Self::check_installed(&profiles),
            profiles,
            tabs: HashMap::new(),
            accounts: Vec::new(),
            hook_rx,
            last_scan: Instant::now() - Duration::from_secs(10),
            last_usage: Instant::now() - Duration::from_secs(60),
        };
        watch.refresh_usage();
        (watch, spooled)
    }

    fn check_installed(profiles: &[Profile]) -> bool {
        !profiles.is_empty()
            && profiles
                .iter()
                .all(|p| hooks::installed_in(&p.config_dir.join("settings.json")))
    }

    pub fn install_hooks(&mut self) -> Result<usize, String> {
        let mut ok = 0;
        let mut errs = Vec::new();
        for p in &self.profiles {
            match hooks::install_into(&p.config_dir.join("settings.json")) {
                Ok(_) => ok += 1,
                Err(e) => errs.push(format!("{}: {e}", p.name)),
            }
        }
        self.hooks_installed = Self::check_installed(&self.profiles);
        if errs.is_empty() {
            Ok(ok)
        } else {
            Err(errs.join("; "))
        }
    }

    pub fn tab_id_of(msg: &RelayMsg) -> Option<TabId> {
        let raw = msg.tab_id.as_deref()?;
        raw.strip_prefix("giverny-")?.parse::<u64>().ok().map(TabId)
    }

    fn account_of(&self, config_dir: Option<&Path>) -> Option<String> {
        let dir = config_dir?;
        profiles::find(&self.profiles, dir).map(|p| p.name.clone())
    }

    /// Apply one hook message. `active` = the currently focused tab.
    pub fn handle_msg(
        &mut self,
        msg: &RelayMsg,
        active: Option<TabId>,
        tab_title: &str,
        effects: &mut WatchEffects,
    ) {
        let Some(tab_id) = Self::tab_id_of(msg) else {
            return;
        };
        let account = self.account_of(msg.config_dir.as_deref().map(Path::new));
        let entry = self.tabs.entry(tab_id).or_default();
        entry.last_hook = Some(Instant::now());
        if account.is_some() {
            entry.account = account;
        }
        let is_active = active == Some(tab_id);

        match msg.hook_event() {
            Some("SessionStart") => {
                entry.state = ClaudeState::Idle;
                entry.session_id = msg.session_id().map(str::to_string);
                effects.captured.push((
                    tab_id,
                    msg.session_id().map(str::to_string),
                    msg.config_dir.as_deref().map(PathBuf::from),
                ));
            }
            Some("UserPromptSubmit") => entry.state = ClaudeState::Busy,
            Some("Stop") => {
                entry.state = if is_active {
                    ClaudeState::Idle
                } else {
                    ClaudeState::DoneUnseen
                };
            }
            Some("Notification") => {
                if let Some(kind) = msg.notification_type() {
                    if needs_you(kind) {
                        entry.state = ClaudeState::NeedsYou;
                        effects.notify.push((
                            format!("{tab_title} — needs you"),
                            msg.message()
                                .unwrap_or("Claude is waiting for input")
                                .to_string(),
                        ));
                    } else if done_kind(kind) {
                        entry.state = if is_active {
                            ClaudeState::Idle
                        } else {
                            ClaudeState::DoneUnseen
                        };
                    }
                }
            }
            Some("SessionEnd") => {
                entry.state = ClaudeState::None;
                entry.session_id = None;
                entry.session_name = None;
                effects.captured.push((tab_id, None, None));
            }
            _ => {}
        }
    }

    /// Periodic merge: hook stream + registry scan + usage refresh.
    /// `shell_pids` maps tabs to their shell process ids.
    pub fn tick(
        &mut self,
        shell_pids: &HashMap<TabId, u32>,
        active: Option<TabId>,
        titles: &HashMap<TabId, String>,
    ) -> WatchEffects {
        let mut effects = WatchEffects::default();

        // Hook stream first (crisp transitions).
        let msgs: Vec<RelayMsg> = self
            .hook_rx
            .as_ref()
            .map(|rx| rx.try_iter().collect())
            .unwrap_or_default();
        for msg in &msgs {
            let title = Self::tab_id_of(msg)
                .and_then(|id| titles.get(&id).cloned())
                .unwrap_or_else(|| "tab".into());
            self.handle_msg(msg, active, &title, &mut effects);
        }

        // Registry scan: baseline busy/idle + identity, ~1 Hz.
        if self.last_scan.elapsed() >= Duration::from_secs(1) {
            self.last_scan = Instant::now();
            for tab in self.tabs.values_mut() {
                tab.seen_in_scan = false;
            }
            let dirs: Vec<PathBuf> = self.profiles.iter().map(|p| p.config_dir.clone()).collect();
            for live in registry::scan(dirs) {
                let Some(tab_id) = shell_pids
                    .iter()
                    .find(|(_, shell)| registry::has_ancestor(live.entry.pid, **shell))
                    .map(|(id, _)| *id)
                else {
                    continue;
                };
                let account = self.account_of(Some(&live.config_dir));
                let entry = self.tabs.entry(tab_id).or_default();
                entry.seen_in_scan = true;
                entry.session_id = Some(live.entry.session_id.clone());
                entry.session_name = live.entry.name.clone();
                if account.is_some() {
                    entry.account = account;
                }
                // Hook-set attention states are stickier than the registry.
                match entry.state {
                    ClaudeState::NeedsYou => {
                        if live.entry.busy() {
                            entry.state = ClaudeState::Busy;
                        }
                    }
                    ClaudeState::DoneUnseen => {}
                    _ => {
                        entry.state = if live.entry.busy() {
                            ClaudeState::Busy
                        } else {
                            ClaudeState::Idle
                        };
                    }
                }
            }
            // Sessions gone from the registry: clear unless hooks spoke recently.
            for tab in self.tabs.values_mut() {
                let hook_recent = tab
                    .last_hook
                    .is_some_and(|t| t.elapsed() < Duration::from_secs(5));
                if !tab.seen_in_scan && !hook_recent && tab.state != ClaudeState::DoneUnseen {
                    tab.state = ClaudeState::None;
                    tab.session_name = None;
                }
            }
        }

        if self.last_usage.elapsed() >= Duration::from_secs(10) {
            self.refresh_usage();
        }

        effects.animating = self
            .tabs
            .values()
            .any(|t| matches!(t.state, ClaudeState::Busy | ClaudeState::NeedsYou));
        effects
    }

    fn refresh_usage(&mut self) {
        self.last_usage = Instant::now();
        self.accounts = self
            .profiles
            .iter()
            .map(|p| AccountPanel {
                profile: p.clone(),
                usage: usage::read(&p.config_dir),
            })
            .collect();
    }

    /// The user looked at the tab: done-markers clear.
    pub fn mark_viewed(&mut self, tab: TabId) {
        if let Some(entry) = self.tabs.get_mut(&tab)
            && entry.state == ClaudeState::DoneUnseen
        {
            entry.state = ClaudeState::Idle;
        }
    }

    pub fn state_of(&self, tab: TabId) -> ClaudeState {
        self.tabs.get(&tab).map(|t| t.state).unwrap_or_default()
    }
}
