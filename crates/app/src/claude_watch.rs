//! App-side Claude awareness: merges the hook relay stream with the
//! `sessions/<pid>.json` registry into per-tab states, and refreshes the
//! per-account usage meters.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
    /// Fresher percentages pushed by the statusline (official `rate_limits`),
    /// overriding the on-disk cache for the windows they cover.
    pub live: Option<LiveUsage>,
    pub statusline_on: bool,
}

/// Where an account's displayed numbers came from, and how old they are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Pushed by the statusline this many minutes ago.
    Live(i64),
    /// Read from Claude Code's on-disk cache, this many minutes old.
    Cache(i64),
    None,
}

/// Push-based usage from Claude Code's statusline payload.
#[derive(Debug, Clone)]
pub struct LiveUsage {
    pub at: Instant,
    pub five_hour: Option<f64>,
    pub seven_day: Option<f64>,
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
    /// Accounts with a refresh currently running, so we never stack them.
    refreshing: Arc<Mutex<HashSet<PathBuf>>>,
    /// When we last *asked* for a refresh, successful or not. Age alone can't
    /// gate the sweep: an account whose cache never appears (logged out, no
    /// `claude` on PATH) reads as infinitely old and would be retried on every
    /// tick forever.
    attempted: Arc<Mutex<HashMap<PathBuf, Instant>>>,
    /// Set by a refresh thread when it finishes, so the panel picks up the new
    /// numbers on the next frame instead of waiting out the read interval.
    cache_dirty: Arc<AtomicBool>,
    pub tabs: HashMap<TabId, ClaudeTab>,
    pub accounts: Vec<AccountPanel>,
    pub hooks_installed: bool,
    hook_rx: Option<Receiver<RelayMsg>>,
    last_scan: Instant,
    last_usage: Instant,
}

/// How often the on-disk usage caches are re-read. The numbers inside them
/// only move when Claude Code fetches (minutes apart), and anything faster —
/// a statusline push, a finished refresh — updates the panel directly, so
/// polling harder buys nothing.
const USAGE_READ_INTERVAL: Duration = Duration::from_secs(60);

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
        // Unix: a socket for instant delivery. Elsewhere (or if binding
        // fails): poll the spool file the relay always falls back to.
        #[cfg(unix)]
        let listener = hooks::spawn_listener(spool, wake);
        #[cfg(not(unix))]
        let listener = hooks::spawn_spool_watcher(spool, wake);
        let (hook_rx, spooled) = match listener {
            Ok((rx, spooled)) => (Some(rx), spooled),
            Err(err) => {
                tracing::warn!("hook listener unavailable: {err:#}");
                (None, Vec::new())
            }
        };

        Self::adopt_statusline_where_hooked(&profiles);
        let mut watch = ClaudeWatch {
            refreshing: Arc::new(Mutex::new(HashSet::new())),
            attempted: Arc::new(Mutex::new(HashMap::new())),
            cache_dirty: Arc::new(AtomicBool::new(false)),
            hooks_installed: Self::check_installed(&profiles),
            profiles,
            tabs: HashMap::new(),
            accounts: Vec::new(),
            hook_rx,
            last_scan: Instant::now() - Duration::from_secs(10),
            last_usage: Instant::now() - USAGE_READ_INTERVAL,
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
            let settings = p.config_dir.join("settings.json");
            match hooks::install_into(&settings) {
                Ok(_) => ok += 1,
                Err(e) => errs.push(format!("{}: {e}", p.name)),
            }
            // Live usage comes with it — the on-disk cache goes stale for
            // accounts that aren't actively running Claude. Profiles with a
            // statusline of their own are left alone (set_statusline errs).
            if let Err(e) = hooks::set_statusline(&settings, true) {
                tracing::info!("statusline skipped for {}: {e}", p.name);
            }
        }
        self.hooks_installed = Self::check_installed(&self.profiles);
        self.refresh_usage();
        if errs.is_empty() {
            Ok(ok)
        } else {
            Err(errs.join("; "))
        }
    }

    /// Profiles that already have our hooks get the live-usage statusline
    /// too: installing hooks is the consent boundary, and without this the
    /// usage panel silently shows day-old numbers.
    fn adopt_statusline_where_hooked(profiles: &[Profile]) {
        for p in profiles {
            let settings = p.config_dir.join("settings.json");
            if !hooks::installed_in(&settings) {
                continue;
            }
            // Our entries point at whichever binary installed them. After a
            // `cargo install` or a moved build, that path can be stale —
            // rewrite it to the running executable so the relay keeps working.
            if hooks::needs_path_refresh(&settings) {
                match hooks::install_into(&settings) {
                    Ok(_) => tracing::info!("hook paths refreshed for {}", p.name),
                    Err(e) => tracing::warn!("hook refresh failed for {}: {e}", p.name),
                }
            }
            if !hooks::statusline_installed_in(&settings) || hooks::needs_path_refresh(&settings) {
                match hooks::set_statusline(&settings, true) {
                    Ok(()) => tracing::info!("live-usage statusline enabled for {}", p.name),
                    Err(e) => tracing::info!("statusline skipped for {}: {e}", p.name),
                }
            }
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
        // Statusline pushes carry usage, not tab state.
        if msg.hook_event() == Some(hooks::STATUSLINE_EVENT) {
            self.apply_statusline(msg);
            return;
        }
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
                // State authority is PER TAB: only once this tab's session has
                // actually emitted hook events do hooks own its state (the
                // registry file can lag with a stale "busy" and must not stomp
                // a crisp Stop). Hooks load at claude startup — a session
                // started before install never fires them, and a global
                // hooks-installed check would freeze such tabs; per-tab
                // evidence keeps the registry driving exactly those.
                let hooks_own = entry.last_hook.is_some();
                // "busy" is unambiguous evidence Claude is running again, so
                // it always clears a stale attention flag — even under hook
                // authority (a declined prompt emits no hook to clear it).
                if entry.state == ClaudeState::NeedsYou && live.entry.busy() {
                    entry.state = ClaudeState::Busy;
                } else if !hooks_own {
                    match entry.state {
                        ClaudeState::NeedsYou | ClaudeState::DoneUnseen => {}
                        _ => {
                            entry.state = if live.entry.busy() {
                                ClaudeState::Busy
                            } else {
                                ClaudeState::Idle
                            };
                        }
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
                    // The session is gone — its hook evidence goes with it, so
                    // a future claude (with or without hooks) starts fresh.
                    tab.last_hook = None;
                }
            }
        }

        // Re-read the caches on a slow timer, or straight away when a refresh
        // we asked for has just rewritten one.
        if self.cache_dirty.swap(false, Ordering::Relaxed)
            || self.last_usage.elapsed() >= USAGE_READ_INTERVAL
        {
            self.refresh_usage();
        }

        effects.animating = self
            .tabs
            .values()
            .any(|t| matches!(t.state, ClaudeState::Busy | ClaudeState::NeedsYou));
        effects
    }

    /// A statusline push: official `rate_limits` for one account.
    fn apply_statusline(&mut self, msg: &RelayMsg) {
        let pct = |key: &str| -> Option<f64> {
            msg.event
                .get("rate_limits")?
                .get(key)?
                .get("used_percentage")?
                .as_f64()
        };
        let live = LiveUsage {
            at: Instant::now(),
            five_hour: pct("five_hour"),
            seven_day: pct("seven_day"),
        };
        if live.five_hour.is_none() && live.seven_day.is_none() {
            return;
        }
        // Attribute to the account: explicit config dir, else the default profile.
        let dir = msg
            .config_dir
            .as_deref()
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".claude")));
        let Some(dir) = dir else { return };
        if let Some(acc) = self
            .accounts
            .iter_mut()
            .find(|a| a.profile.config_dir == dir)
        {
            acc.live = Some(live);
        }
    }

    fn refresh_usage(&mut self) {
        self.last_usage = Instant::now();
        let previous: HashMap<PathBuf, (Option<LiveUsage>, bool)> = self
            .accounts
            .drain(..)
            .map(|a| (a.profile.config_dir, (a.live, a.statusline_on)))
            .collect();
        self.accounts = self
            .profiles
            .iter()
            .map(|p| {
                let (live, _) = previous
                    .get(&p.config_dir)
                    .cloned()
                    .unwrap_or((None, false));
                AccountPanel {
                    usage: usage::read(&p.config_dir),
                    live,
                    statusline_on: hooks::statusline_installed_in(
                        &p.config_dir.join("settings.json"),
                    ),
                    profile: p.clone(),
                }
            })
            .collect();
    }

    /// Is a refresh due for an account? Split out from the sweep so the two
    /// ways it can be spared — young numbers, and a recent attempt — are
    /// testable without spawning anything.
    fn refresh_due(
        age_minutes: i64,
        since_attempt: Option<Duration>,
        max_age_minutes: u64,
    ) -> bool {
        let window = Duration::from_secs(max_age_minutes * 60);
        if age_minutes < max_age_minutes as i64 {
            return false;
        }
        // An account with no readable cache is infinitely "old", so the age
        // test never spares it; the attempt clock is what stops the retry loop.
        since_attempt.is_none_or(|since| since >= window)
    }

    /// Ask Claude Code to refresh accounts whose numbers have aged out.
    /// `max_age_minutes == 0` disables the sweep; `force` refreshes everything
    /// now (the user asked), subject only to the in-flight guard.
    pub fn refresh_stale_usage(&self, max_age_minutes: u64, force: bool) {
        if max_age_minutes == 0 && !force {
            return;
        }
        let now = jiff::Timestamp::now();
        for acc in &self.accounts {
            if !force {
                let age = acc
                    .usage
                    .as_ref()
                    .map(|u| usage::age_minutes(u, now))
                    .unwrap_or(i64::MAX);
                let since = self
                    .attempted
                    .lock()
                    .unwrap()
                    .get(&acc.profile.config_dir)
                    .map(|t| t.elapsed());
                if !Self::refresh_due(age, since, max_age_minutes) {
                    continue;
                }
            }
            self.spawn_refresh(acc.profile.config_dir.clone());
        }
    }

    fn spawn_refresh(&self, config_dir: PathBuf) {
        {
            let mut busy = self.refreshing.lock().unwrap();
            if !busy.insert(config_dir.clone()) {
                return; // already refreshing this account
            }
        }
        self.attempted
            .lock()
            .unwrap()
            .insert(config_dir.clone(), Instant::now());
        let busy = Arc::clone(&self.refreshing);
        let dirty = Arc::clone(&self.cache_dirty);
        let _ = std::thread::Builder::new()
            .name("giverny usage refresh".into())
            .spawn(move || {
                match usage::refresh_via_cli(&config_dir) {
                    Ok(()) => {
                        tracing::info!("usage refreshed for {}", config_dir.display());
                        // Show the new numbers without waiting for the timer.
                        dirty.store(true, Ordering::Relaxed);
                    }
                    Err(err) => tracing::info!("usage refresh skipped: {err}"),
                }
                busy.lock().unwrap().remove(&config_dir);
            });
    }

    /// Is any account mid-refresh (for the spinner in the rail)?
    pub fn refresh_in_flight(&self) -> bool {
        !self.refreshing.lock().unwrap().is_empty()
    }

    /// Turn the live-usage statusline on/off for every profile.
    pub fn set_statusline(&mut self, enable: bool) -> Result<(), String> {
        let mut errs = Vec::new();
        for p in &self.profiles {
            if let Err(e) = hooks::set_statusline(&p.config_dir.join("settings.json"), enable) {
                errs.push(format!("{}: {e}", p.name));
            }
        }
        self.refresh_usage();
        if errs.is_empty() {
            Ok(())
        } else {
            Err(errs.join("; "))
        }
    }

    /// Do all profiles have the live-usage statusline?
    pub fn statusline_on(&self) -> bool {
        !self.accounts.is_empty() && self.accounts.iter().all(|a| a.statusline_on)
    }

    /// Claude Code loads `settings.json` when a session starts, so hooks and
    /// the statusline do nothing for sessions that were already running.
    /// True when every live session predates the settings file — i.e. the
    /// user needs to restart claude for any of it to take effect.
    pub fn sessions_predate_settings(&self) -> bool {
        let live = registry::scan(self.profiles.iter().map(|p| p.config_dir.clone()));
        if live.is_empty() {
            return false;
        }
        live.iter().all(|s| {
            let settings = s.config_dir.join("settings.json");
            match (
                std::fs::metadata(&settings).and_then(|m| m.modified()),
                std::time::UNIX_EPOCH.checked_add(Duration::from_millis(s.entry.started_at_ms)),
            ) {
                (Ok(settings_at), Some(started)) => started < settings_at,
                _ => false,
            }
        })
    }

    /// How fresh this account's numbers actually are, and from where.
    /// Reporting only the cache age reads as "stale" even when a live push
    /// has already overridden the bars.
    pub fn freshness(acc: &AccountPanel, now: jiff::Timestamp) -> Freshness {
        let cache_min = acc.usage.as_ref().map(|u| usage::age_minutes(u, now));
        let live_min = acc
            .live
            .as_ref()
            .map(|l| (l.at.elapsed().as_secs() / 60) as i64);
        match (live_min, cache_min) {
            (Some(l), Some(c)) if l <= c => Freshness::Live(l),
            (Some(l), None) => Freshness::Live(l),
            (_, Some(c)) => Freshness::Cache(c),
            (None, None) => Freshness::None,
        }
    }

    /// Percent to display for one bucket: the statusline push when it is
    /// fresher than the on-disk cache, else the cache value.
    pub fn display_percent(
        acc: &AccountPanel,
        limit: &giverny_claude::usage::LimitEntry,
        now: jiff::Timestamp,
    ) -> (f64, bool) {
        let cached = limit.effective_percent(now);
        let Some(live) = &acc.live else {
            return (cached, false);
        };
        let cache_age_ms = acc
            .usage
            .as_ref()
            .map(|u| (now.as_millisecond() - u.fetched_at_ms as i64).max(0))
            .unwrap_or(i64::MAX);
        if live.at.elapsed().as_millis() as i64 >= cache_age_ms {
            return (cached, false);
        }
        let fresh = match limit.kind.as_str() {
            "session" => live.five_hour,
            "weekly_all" => live.seven_day,
            _ => None,
        };
        match fresh {
            Some(p) => (p.clamp(0.0, 100.0), true),
            None => (cached, false),
        }
    }

    /// The user looked at the tab: done-markers clear.
    pub fn mark_viewed(&mut self, tab: TabId) {
        if let Some(entry) = self.tabs.get_mut(&tab)
            && entry.state == ClaudeState::DoneUnseen
        {
            entry.state = ClaudeState::Idle;
        }
    }

    /// The user typed in the tab: attention has been given, whatever the
    /// outcome. Declining a permission prompt (Escape) produces no hook at
    /// all, so without this the flag would blink forever.
    pub fn mark_attended(&mut self, tab: TabId) {
        if let Some(entry) = self.tabs.get_mut(&tab)
            && matches!(entry.state, ClaudeState::NeedsYou | ClaudeState::DoneUnseen)
        {
            entry.state = ClaudeState::Idle;
        }
    }

    pub fn state_of(&self, tab: TabId) -> ClaudeState {
        self.tabs.get(&tab).map(|t| t.state).unwrap_or_default()
    }

    /// Test seam: a watcher with no listener and no profiles.
    #[cfg(test)]
    fn for_tests() -> Self {
        ClaudeWatch {
            profiles: Vec::new(),
            tabs: HashMap::new(),
            accounts: Vec::new(),
            hooks_installed: true,
            hook_rx: None,
            last_scan: Instant::now(),
            last_usage: Instant::now(),
            refreshing: Arc::new(Mutex::new(HashSet::new())),
            attempted: Arc::new(Mutex::new(HashMap::new())),
            cache_dirty: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Is the hook relay socket actually listening?
    pub fn relay_listening(&self) -> bool {
        self.hook_rx.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAB: TabId = TabId(7);

    fn msg(json: &str) -> RelayMsg {
        serde_json::from_str(json).expect("relay msg fixture")
    }

    fn hook(event: &str, extra: &str) -> RelayMsg {
        msg(&format!(
            r#"{{"tab_id":"giverny-7","config_dir":null,
                "event":{{"hook_event_name":"{event}","session_id":"s-1"{extra}}}}}"#
        ))
    }

    fn feed(w: &mut ClaudeWatch, m: &RelayMsg, active: Option<TabId>) -> WatchEffects {
        let mut fx = WatchEffects::default();
        w.handle_msg(m, active, "tab", &mut fx);
        fx
    }

    #[test]
    fn turn_lifecycle_drives_states() {
        let mut w = ClaudeWatch::for_tests();
        assert_eq!(w.state_of(TAB), ClaudeState::None);

        let fx = feed(&mut w, &hook("SessionStart", ""), Some(TAB));
        assert_eq!(w.state_of(TAB), ClaudeState::Idle);
        assert_eq!(fx.captured.len(), 1, "session id captured for resume");

        feed(&mut w, &hook("UserPromptSubmit", ""), Some(TAB));
        assert_eq!(w.state_of(TAB), ClaudeState::Busy, "spinner while working");

        // Finishing in the FOCUSED tab returns to idle...
        feed(&mut w, &hook("Stop", ""), Some(TAB));
        assert_eq!(w.state_of(TAB), ClaudeState::Idle);

        // ...but finishing in a background tab leaves a done marker.
        feed(&mut w, &hook("UserPromptSubmit", ""), Some(TabId(1)));
        feed(&mut w, &hook("Stop", ""), Some(TabId(1)));
        assert_eq!(w.state_of(TAB), ClaudeState::DoneUnseen);
        w.mark_viewed(TAB);
        assert_eq!(
            w.state_of(TAB),
            ClaudeState::Idle,
            "viewing clears the marker"
        );
    }

    #[test]
    fn attention_notifications_only_for_needs_you() {
        let mut w = ClaudeWatch::for_tests();
        feed(&mut w, &hook("SessionStart", ""), Some(TabId(1)));

        for kind in [
            "permission_prompt",
            "elicitation_dialog",
            "agent_needs_input",
        ] {
            let m = hook("Notification", &format!(r#","notification_type":"{kind}""#));
            let fx = feed(&mut w, &m, Some(TabId(1)));
            assert_eq!(w.state_of(TAB), ClaudeState::NeedsYou, "{kind}");
            assert_eq!(
                fx.notify.len(),
                1,
                "{kind} must raise a desktop notification"
            );
        }

        // Completion kinds never notify; they only mark done.
        let m = hook("Notification", r#","notification_type":"agent_completed""#);
        let fx = feed(&mut w, &m, Some(TabId(1)));
        assert!(fx.notify.is_empty(), "completions must not notify");
        assert_eq!(w.state_of(TAB), ClaudeState::DoneUnseen);
    }

    #[test]
    fn typing_clears_a_stale_attention_flag() {
        // Declining a permission prompt (Escape) emits no hook at all — only
        // the user's keystroke tells us the flag is stale.
        let mut w = ClaudeWatch::for_tests();
        feed(&mut w, &hook("SessionStart", ""), Some(TAB));
        let m = hook(
            "Notification",
            r#","notification_type":"permission_prompt""#,
        );
        feed(&mut w, &m, Some(TAB));
        assert_eq!(w.state_of(TAB), ClaudeState::NeedsYou);

        w.mark_attended(TAB);
        assert_eq!(w.state_of(TAB), ClaudeState::Idle, "typing clears the flag");

        // Working tabs keep their spinner when the user types.
        feed(&mut w, &hook("UserPromptSubmit", ""), Some(TAB));
        w.mark_attended(TAB);
        assert_eq!(w.state_of(TAB), ClaudeState::Busy);
    }

    #[test]
    fn session_end_clears_state_and_resume_target() {
        let mut w = ClaudeWatch::for_tests();
        feed(&mut w, &hook("SessionStart", ""), Some(TAB));
        let fx = feed(&mut w, &hook("SessionEnd", ""), Some(TAB));
        assert_eq!(w.state_of(TAB), ClaudeState::None);
        assert_eq!(
            fx.captured,
            vec![(TAB, None, None)],
            "resume target cleared"
        );
    }

    #[test]
    fn statusline_push_updates_live_usage_not_tab_state() {
        let mut w = ClaudeWatch::for_tests();
        w.accounts.push(AccountPanel {
            profile: Profile {
                name: "acct".into(),
                config_dir: PathBuf::from("/tmp/giverny-test-acct"),
                email: None,
                account_uuid: None,
            },
            usage: None,
            live: None,
            statusline_on: true,
        });
        let m = msg(
            r#"{"tab_id":"giverny-7","config_dir":"/tmp/giverny-test-acct",
                "event":{"hook_event_name":"GivernyStatusLine",
                         "rate_limits":{"five_hour":{"used_percentage":42.0},
                                        "seven_day":{"used_percentage":13.0}}}}"#,
        );
        feed(&mut w, &m, Some(TAB));
        let live = w.accounts[0].live.as_ref().expect("live usage recorded");
        assert_eq!(live.five_hour, Some(42.0));
        assert_eq!(live.seven_day, Some(13.0));
        assert_eq!(
            w.state_of(TAB),
            ClaudeState::None,
            "statusline is not tab state"
        );
    }

    #[test]
    fn live_percent_wins_only_when_fresher_than_cache() {
        use giverny_claude::usage::{AccountUsage, LimitEntry};
        let now = jiff::Timestamp::now();
        let limit: LimitEntry = serde_json::from_str(
            r#"{"kind":"session","percent":5,"severity":"normal","is_active":true}"#,
        )
        .unwrap();

        let mk = |cache_age_min: i64, live: Option<f64>| AccountPanel {
            profile: Profile {
                name: "a".into(),
                config_dir: PathBuf::from("/tmp/x"),
                email: None,
                account_uuid: None,
            },
            usage: Some(AccountUsage {
                fetched_at_ms: (now.as_millisecond() - cache_age_min * 60_000) as u64,
                limits: vec![],
            }),
            live: live.map(|p| LiveUsage {
                at: Instant::now(),
                five_hour: Some(p),
                seven_day: None,
            }),
            statusline_on: true,
        };

        // Stale cache + fresh push ⇒ push wins and is flagged live.
        let (pct, is_live) = ClaudeWatch::display_percent(&mk(120, Some(77.0)), &limit, now);
        assert_eq!((pct, is_live), (77.0, true));
        // No push ⇒ cache value, not flagged.
        let (pct, is_live) = ClaudeWatch::display_percent(&mk(120, None), &limit, now);
        assert_eq!((pct, is_live), (5.0, false));
    }

    #[test]
    fn refresh_waits_for_the_numbers_to_age() {
        let mins = |m: u64| Some(Duration::from_secs(m * 60));
        // Young numbers are left alone however long ago we last asked.
        assert!(!ClaudeWatch::refresh_due(3, None, 10));
        assert!(!ClaudeWatch::refresh_due(9, mins(60), 10));
        // Aged out and never asked, or asked long enough ago.
        assert!(ClaudeWatch::refresh_due(10, None, 10));
        assert!(ClaudeWatch::refresh_due(45, mins(11), 10));
    }

    #[test]
    fn an_account_that_never_caches_is_not_retried_in_a_loop() {
        // No readable cache reads as infinitely old, so only the attempt clock
        // stands between us and spawning `claude -p /usage` every tick.
        assert!(ClaudeWatch::refresh_due(i64::MAX, None, 10));
        for secs in [1, 30, 120, 599] {
            assert!(
                !ClaudeWatch::refresh_due(i64::MAX, Some(Duration::from_secs(secs)), 10),
                "retried {secs}s after the last attempt"
            );
        }
        assert!(ClaudeWatch::refresh_due(
            i64::MAX,
            Some(Duration::from_secs(600)),
            10
        ));
    }
}
