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
use giverny_claude::jobs::{self, Job};
use giverny_claude::profiles::{self, Profile};
use giverny_claude::registry;
use giverny_claude::usage::{self, AccountUsage};
use giverny_claude::wsl;
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
    /// A background shell is alive in this session while the agent itself is
    /// at its prompt. Not a working state — marked, never animated.
    pub background: bool,
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
    /// When each window reopens, if the push says. The on-disk cache carries
    /// this too, but a cache older than the window it describes has a reset
    /// time in the past — and a lapsed reset time is no reset time at all,
    /// which is how a live 90% ends up with nothing next to it.
    pub five_hour_resets: Option<jiff::Timestamp>,
    pub seven_day_resets: Option<jiff::Timestamp>,
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
    /// Background agents across every account — the Claudes with no tab.
    pub jobs: Vec<Job>,
    pub accounts: Vec<AccountPanel>,
    pub hooks_installed: bool,
    hook_rx: Option<Receiver<RelayMsg>>,
    last_scan: Instant,
    /// The last registry scan, and whether every live session predates the
    /// settings file. Both are filesystem work — for an account inside WSL,
    /// filesystem work across a share — so they happen on a worker and the UI
    /// reads whatever it last said.
    scan_rx: Option<crossbeam_channel::Receiver<ScanResult>>,
    scanned: ScanResult,
    last_jobs: Instant,
    last_usage: Instant,
    /// When each account's cache file was last seen changing, so a rewrite is
    /// noticed rather than waited out.
    cache_mtimes: Arc<Mutex<HashMap<PathBuf, std::time::SystemTime>>>,
    last_cache_stat: Instant,
    /// One stat sweep at a time: a share that is slow to answer must not
    /// stack up threads behind it.
    stat_in_flight: Arc<AtomicFlag>,
    /// What a later, warmer discovery found, the one worker looking, and when
    /// it last looked.
    late: Arc<Mutex<Option<Vec<Profile>>>>,
    last_look: Instant,
    late_in_flight: Arc<AtomicFlag>,
    extra_dirs: Vec<PathBuf>,
}

/// How often the on-disk usage caches are re-read. The numbers inside them
/// only move when Claude Code fetches (minutes apart), and anything faster —
/// a statusline push, a finished refresh — updates the panel directly, so
/// polling harder buys nothing.
const USAGE_READ_INTERVAL: Duration = Duration::from_secs(60);
/// How often the cache files are checked for having been rewritten.
const CACHE_STAT_INTERVAL: Duration = Duration::from_secs(2);
/// How often accounts are looked for again while none inside WSL is known.
const LOOK_AGAIN_INTERVAL: Duration = Duration::from_secs(45);

/// A bool two threads share. `AtomicBool` in a name that says what it is for.
#[derive(Default)]
pub struct AtomicFlag(AtomicBool);

impl AtomicFlag {
    fn get(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
    fn set(&self, value: bool) {
        self.0.store(value, Ordering::Relaxed);
    }
}

fn needs_you(notification_type: &str) -> bool {
    matches!(
        notification_type,
        "permission_prompt" | "elicitation_dialog" | "agent_needs_input"
    )
}

/// The session is back at its prompt. Only `idle_prompt` says that.
fn idle_kind(notification_type: &str) -> bool {
    notification_type == "idle_prompt"
}

/// A *piece* of work finished — a subagent, a task. Worth a done-marker in a
/// background tab, but it is not evidence the session stopped: these fire
/// mid-turn, while the main agent carries on with the result. Treating them
/// as "finished" is what used to kill the spinner half way through the work.
fn finished_kind(notification_type: &str) -> bool {
    matches!(notification_type, "agent_completed" | "task_completed")
}

/// One tab's state, reconciled with what Claude Code says about that session
/// right now.
///
/// Hooks and the registry answer different questions. Hooks bracket a *turn*;
/// the registry says what the session is doing right now, including the one
/// state no hook marks the start of: blocked on the user (`waiting`).
fn merge_registry(
    current: ClaudeState,
    hooks_own: bool,
    live: &giverny_claude::registry::SessionEntry,
) -> ClaudeState {
    // Working is unambiguous evidence Claude is running again, so it always
    // clears a stale flag — even under hook authority. A declined permission
    // prompt emits no hook to clear the one it raised, and a turn that ended
    // before this one left a tick behind that "finished" no longer describes.
    if live.busy() && matches!(current, ClaudeState::NeedsYou | ClaudeState::DoneUnseen) {
        return ClaudeState::Busy;
    }
    if hooks_own {
        // Hooks are authoritative for a session that emits them: they mark
        // the end of a turn exactly, and the registry must not re-open one it
        // closed.
        return current;
    }
    match current {
        // Nothing here has heard from a hook, so these are ours to keep until
        // the user attends to them.
        ClaudeState::NeedsYou | ClaudeState::DoneUnseen => current,
        _ if live.busy() => ClaudeState::Busy,
        // Blocked on the user with no hook to say so — the state a session
        // started before hooks were installed would otherwise sit silent in.
        _ if live.waiting() => ClaudeState::NeedsYou,
        _ => ClaudeState::Idle,
    }
}

/// One pass over the session registries, done on a worker.
#[derive(Default)]
struct ScanResult {
    live: Vec<registry::LiveSession>,
    /// Every live session started before its `settings.json` was last
    /// written, so none of them loaded the hooks in it.
    stale: bool,
}

/// When one rate-limit window resets, out of a statusline push.
///
/// The field has been spelled more than one way across Claude Code versions,
/// and a moment is written variously: an RFC 3339 string, epoch seconds, or
/// epoch milliseconds. Read whichever one is there.
fn reset_time(window: &serde_json::Value) -> Option<jiff::Timestamp> {
    for name in ["resets_at", "reset_at", "resets_at_ms", "reset_at_ms"] {
        let Some(value) = window.get(name) else {
            continue;
        };
        if let Some(text) = value.as_str()
            && let Ok(at) = text.parse::<jiff::Timestamp>()
        {
            return Some(at);
        }
        if let Some(number) = value.as_i64() {
            // Milliseconds if it is far too large to be seconds.
            let millis = if number > 100_000_000_000 {
                number
            } else {
                number * 1000
            };
            if let Ok(at) = jiff::Timestamp::from_millisecond(millis) {
                return Some(at);
            }
        }
    }
    None
}

impl ClaudeWatch {
    pub fn new(
        spool: &Path,
        extra_dirs: &[PathBuf],
        wake: impl Fn() + Send + 'static,
    ) -> (Self, Vec<RelayMsg>) {
        let profiles = profiles::discover(extra_dirs);
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
            jobs: Vec::new(),
            accounts: Vec::new(),
            hook_rx,
            last_scan: Instant::now() - Duration::from_secs(10),
            scan_rx: None,
            scanned: ScanResult::default(),
            last_jobs: Instant::now() - Duration::from_secs(10),
            last_usage: Instant::now() - USAGE_READ_INTERVAL,
            cache_mtimes: Arc::new(Mutex::new(HashMap::new())),
            last_cache_stat: Instant::now() - CACHE_STAT_INTERVAL,
            stat_in_flight: Arc::new(AtomicFlag::default()),
            late: Arc::new(Mutex::new(None)),
            last_look: Instant::now(),
            late_in_flight: Arc::new(AtomicFlag::default()),
            extra_dirs: extra_dirs.to_vec(),
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
                // Installed once, but not for everything we listen to now: a
                // new event in a new version. Consent was given; bring the
                // file up to date rather than asking again.
                if hooks::partly_installed_in(&settings) {
                    match hooks::install_into(&settings) {
                        Ok(_) => tracing::info!("hooks brought up to date for {}", p.name),
                        Err(e) => tracing::warn!("hook update failed for {}: {e}", p.name),
                    }
                }
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

    /// The profile directory a session means by the `CLAUDE_CONFIG_DIR` it
    /// reports. A session inside WSL reports the path it can open
    /// (`/home/x/.claude`); profiles here are keyed by the path Windows can
    /// open. Anything else passes through unchanged.
    fn canonical_dir(&self, reported: Option<&str>) -> Option<PathBuf> {
        let reported = PathBuf::from(reported?);
        let known: Vec<PathBuf> = self.profiles.iter().map(|p| p.config_dir.clone()).collect();
        Some(wsl::canonical_config_dir(&reported, &known).unwrap_or(reported))
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
        let config_dir = self.canonical_dir(msg.config_dir.as_deref());
        let account = self.account_of(config_dir.as_deref());
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
                effects
                    .captured
                    .push((tab_id, msg.session_id().map(str::to_string), config_dir));
            }
            Some("UserPromptSubmit") => entry.state = ClaudeState::Busy,
            // A tool call is work happening now, whoever asked for it. It is
            // what tells a tab apart from the turn that ended before it: a
            // session carrying on after a permission was granted, or an agent
            // continuing by itself, emits nothing else.
            Some("PostToolUse") => entry.state = ClaudeState::Busy,
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
                    } else if idle_kind(kind) {
                        entry.state = if is_active {
                            ClaudeState::Idle
                        } else {
                            ClaudeState::DoneUnseen
                        };
                    } else if finished_kind(kind) {
                        // A subagent or task finished. Only meaningful if the
                        // session itself is not working — otherwise the main
                        // agent is still going and the spinner stays.
                        if entry.state != ClaudeState::Busy && !is_active {
                            entry.state = ClaudeState::DoneUnseen;
                        }
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

        // Registry scan: baseline busy/idle + identity, ~1 Hz, off-thread.
        if let Some(rx) = &self.scan_rx {
            match rx.try_recv() {
                Ok(result) => {
                    self.scan_rx = None;
                    self.scanned = result;
                    self.merge_scan(shell_pids, &mut effects);
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => self.scan_rx = None,
                Err(crossbeam_channel::TryRecvError::Empty) => {}
            }
        }
        if self.scan_rx.is_none() && self.last_scan.elapsed() >= Duration::from_secs(1) {
            self.last_scan = Instant::now();
            let dirs: Vec<PathBuf> = self.profiles.iter().map(|p| p.config_dir.clone()).collect();
            let (tx, rx) = crossbeam_channel::bounded(1);
            if std::thread::Builder::new()
                .name("giverny session scan".into())
                .spawn(move || {
                    let live = registry::scan(dirs);
                    // Asked here too: it is another `stat` per session, and
                    // the answer only matters once a scan has happened.
                    let stale = !live.is_empty()
                        && live.iter().all(|s| {
                            let settings = s.config_dir.join("settings.json");
                            match (
                                std::fs::metadata(&settings).and_then(|m| m.modified()),
                                std::time::UNIX_EPOCH
                                    .checked_add(Duration::from_millis(s.entry.started_at_ms)),
                            ) {
                                (Ok(settings_at), Some(started)) => started < settings_at,
                                _ => false,
                            }
                        });
                    let _ = tx.send(ScanResult { live, stale });
                })
                .is_ok()
            {
                self.scan_rx = Some(rx);
            }
        }

        // Background agents: a handful of small files, so a slower tick than
        // the session registry is plenty.
        if self.last_jobs.elapsed() >= Duration::from_secs(3) {
            self.last_jobs = Instant::now();
            let dirs: Vec<PathBuf> = self.profiles.iter().map(|p| p.config_dir.clone()).collect();
            self.jobs = jobs::scan(dirs);
        }

        // Re-read the caches when the file says so, when a refresh we asked
        // for has just rewritten one, or on the slow timer as a backstop.
        //
        // The timer alone meant a number could be a minute out of date with a
        // file that had already been rewritten — Claude Code updates the cache
        // itself every time a session fetches usage, which is the freshest
        // source there is short of the statusline push.
        self.watch_caches();
        self.look_again();
        if self.cache_dirty.swap(false, Ordering::Relaxed)
            || self.last_usage.elapsed() >= USAGE_READ_INTERVAL
        {
            self.refresh_usage();
        }

        effects.animating = self
            .tabs
            .values()
            .any(|t| matches!(t.state, ClaudeState::Busy | ClaudeState::NeedsYou))
            || self
                .jobs
                .iter()
                .any(|j| j.live && j.state == jobs::JobState::Working);
        effects
    }

    /// Fold the last scan into per-tab state.
    fn merge_scan(&mut self, shell_pids: &HashMap<TabId, u32>, effects: &mut WatchEffects) {
        for tab in self.tabs.values_mut() {
            tab.seen_in_scan = false;
        }
        // Which tab holds which conversation, as the hooks reported it. This
        // is the only way to match a session that runs where our process ids
        // mean nothing: an entry inside a WSL distribution carries a Linux pid
        // and the tab's shell is a `wsl.exe` on the Windows side, so the
        // ancestry walk below can never connect the two. Without a match the
        // tab is "not seen in the scan", and five seconds after its last hook
        // it goes back to showing no Claude at all — which is what a tab does
        // between turns, all day.
        let by_session: HashMap<String, TabId> = self
            .tabs
            .iter()
            .filter_map(|(id, tab)| Some((tab.session_id.clone()?, *id)))
            .collect();
        {
            for live in self.scanned.live.clone() {
                let Some(tab_id) = shell_pids
                    .iter()
                    .find(|(_, shell)| registry::has_ancestor(live.entry.pid, **shell))
                    .map(|(id, _)| *id)
                    .or_else(|| by_session.get(&live.entry.session_id).copied())
                else {
                    continue;
                };
                let account = self.account_of(Some(&live.config_dir));
                let entry = self.tabs.entry(tab_id).or_default();
                entry.seen_in_scan = true;
                entry.background = live.entry.background_shell();
                // Remember which conversation this tab is holding, so it can
                // be resumed after a restart. Hooks report this too, but only
                // for sessions that started *after* they were installed —
                // every older session would otherwise be lost on restart
                // despite the registry naming it the whole time.
                if entry.session_id.as_deref() != Some(live.entry.session_id.as_str()) {
                    effects.captured.push((
                        tab_id,
                        Some(live.entry.session_id.clone()),
                        Some(live.config_dir.clone()),
                    ));
                }
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
                entry.state = merge_registry(entry.state, hooks_own, &live.entry);
            }
            // Sessions gone from the registry: clear unless hooks spoke recently.
            for tab in self.tabs.values_mut() {
                let hook_recent = tab
                    .last_hook
                    .is_some_and(|t| t.elapsed() < Duration::from_secs(5));
                if !tab.seen_in_scan && !hook_recent && tab.state != ClaudeState::DoneUnseen {
                    tab.state = ClaudeState::None;
                    tab.background = false;
                    tab.session_name = None;
                    // The session is gone — its hook evidence goes with it, so
                    // a future claude (with or without hooks) starts fresh.
                    tab.last_hook = None;
                }
            }
        }
    }

    /// A statusline push: official `rate_limits` for one account.
    fn apply_statusline(&mut self, msg: &RelayMsg) {
        let window =
            |key: &str| -> Option<&serde_json::Value> { msg.event.get("rate_limits")?.get(key) };
        let pct = |key: &str| -> Option<f64> { window(key)?.get("used_percentage")?.as_f64() };
        let resets = |key: &str| -> Option<jiff::Timestamp> { reset_time(window(key)?) };
        let live = LiveUsage {
            at: Instant::now(),
            five_hour: pct("five_hour"),
            seven_day: pct("seven_day"),
            five_hour_resets: resets("five_hour"),
            seven_day_resets: resets("seven_day"),
        };
        if live.five_hour.is_none() && live.seven_day.is_none() {
            return;
        }
        // Attribute to the account: explicit config dir, else the default profile.
        let dir = self
            .canonical_dir(msg.config_dir.as_deref())
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

    /// Watch the cache files for being rewritten, off the UI thread.
    ///
    /// A `stat` is cheap until the file is inside a stopped WSL distribution,
    /// where the first touch of the share starts it and takes seconds. Twice
    /// a second on the UI thread, that is a frozen window. The thread sets the
    /// same dirty flag a refresh we asked for sets, and the read happens on
    /// the next tick either way.
    fn watch_caches(&mut self) {
        if self.last_cache_stat.elapsed() < CACHE_STAT_INTERVAL || self.stat_in_flight.get() {
            return;
        }
        self.last_cache_stat = Instant::now();
        let paths: Vec<PathBuf> = self
            .profiles
            .iter()
            .map(|p| profiles::identity_path(&p.config_dir))
            .collect();
        let seen = Arc::clone(&self.cache_mtimes);
        let dirty = Arc::clone(&self.cache_dirty);
        let in_flight = Arc::clone(&self.stat_in_flight);
        in_flight.set(true);
        let _ = std::thread::Builder::new()
            .name("giverny usage stat".into())
            .spawn(move || {
                for path in paths {
                    let Ok(at) = std::fs::metadata(&path).and_then(|m| m.modified()) else {
                        continue;
                    };
                    let mut seen = seen.lock().unwrap();
                    if seen.get(&path) != Some(&at) {
                        seen.insert(path, at);
                        dirty.store(true, Ordering::Relaxed);
                    }
                }
                in_flight.set(false);
            });
    }

    /// Look for accounts again, off the UI thread, while none inside WSL has
    /// turned up.
    ///
    /// Discovery runs once, at startup — and a distribution that has to boot
    /// first can take longer to say where its home is than anything here will
    /// wait for it. Asked while it was still cold, it says nothing, and the
    /// account living in it is then missing for the whole run: no usage, no
    /// identity, and a resumed session attributed to nobody. It will answer a
    /// minute later; this is what asks again.
    fn look_again(&mut self) {
        if !cfg!(windows)
            || self.late_in_flight.get()
            || self.last_look.elapsed() < LOOK_AGAIN_INTERVAL
        {
            return;
        }
        if self
            .profiles
            .iter()
            .any(|p| wsl::is_wsl_path(&p.config_dir))
        {
            return;
        }
        if let Some(found) = self.late.lock().ok().and_then(|mut l| l.take())
            && found.len() > self.profiles.len()
        {
            self.profiles = found;
            self.refresh_usage();
            return;
        }
        self.last_look = Instant::now();
        let extra = self.extra_dirs.clone();
        let slot = Arc::clone(&self.late);
        let in_flight = Arc::clone(&self.late_in_flight);
        in_flight.set(true);
        let _ = std::thread::Builder::new()
            .name("giverny accounts".into())
            .spawn(move || {
                let found = profiles::discover(&extra);
                if let Ok(mut slot) = slot.lock() {
                    *slot = Some(found);
                }
                in_flight.set(false);
            });
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

    /// Start every Claude session in auto mode, in every account.
    ///
    /// Claude Code reads `settings.json` when a session starts, so this
    /// changes the next `claude`, not the ones already running.
    pub fn set_auto_mode(&mut self, enable: bool) {
        for p in &self.profiles {
            let settings = p.config_dir.join("settings.json");
            match hooks::set_auto_mode(&settings, enable) {
                Ok(()) => tracing::info!(
                    "auto mode {} for {}",
                    if enable { "on" } else { "off" },
                    p.name
                ),
                Err(err) => tracing::warn!("auto mode unchanged for {}: {err}", p.name),
            }
        }
    }

    /// Apply the auto-mode setting to accounts that have no permission mode
    /// of their own — an account added after the toggle was turned on, or one
    /// whose settings.json was rewritten. A mode set by hand is never
    /// overridden here; only the explicit toggle does that.
    pub fn ensure_auto_mode(&mut self) {
        let missing: Vec<PathBuf> = self
            .profiles
            .iter()
            .map(|p| p.config_dir.join("settings.json"))
            .filter(|s| hooks::default_mode_in(s).is_none())
            .collect();
        for settings in missing {
            if let Err(err) = hooks::set_auto_mode(&settings, true) {
                tracing::warn!("auto mode unchanged for {}: {err}", settings.display());
            }
        }
    }

    /// Do all accounts start Claude in auto mode?
    pub fn auto_mode_on(&self) -> bool {
        !self.profiles.is_empty()
            && self
                .profiles
                .iter()
                .all(|p| hooks::auto_mode_in(&p.config_dir.join("settings.json")))
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
        self.scanned.stale
    }

    /// When the five-hour window for `account` reopens, if the numbers say.
    ///
    /// The message on screen names a reset time too, in the local words of
    /// whoever is reading it ("resets 3pm"); this is the same moment as a
    /// timestamp, from the cache Claude Code writes.
    pub fn window_reopens(&self, account: &str) -> Option<jiff::Timestamp> {
        let panel = self.accounts.iter().find(|a| a.profile.name == account)?;
        let now = jiff::Timestamp::now();
        panel
            .usage
            .as_ref()?
            .limits
            .iter()
            .filter(|l| l.kind == "session")
            .filter_map(|l| l.resets_at.as_deref()?.parse::<jiff::Timestamp>().ok())
            .find(|at| *at > now)
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
            jobs: Vec::new(),
            accounts: Vec::new(),
            hooks_installed: true,
            hook_rx: None,
            last_scan: Instant::now(),
            scan_rx: None,
            scanned: ScanResult::default(),
            last_jobs: Instant::now(),
            last_usage: Instant::now(),
            cache_mtimes: Arc::new(Mutex::new(HashMap::new())),
            last_cache_stat: Instant::now(),
            stat_in_flight: Arc::new(AtomicFlag::default()),
            late: Arc::new(Mutex::new(None)),
            last_look: Instant::now(),
            late_in_flight: Arc::new(AtomicFlag::default()),
            extra_dirs: Vec::new(),
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

    fn session(status: &str) -> giverny_claude::registry::SessionEntry {
        serde_json::from_str(&format!(
            r#"{{"pid":1,"sessionId":"s-1","status":"{status}"}}"#
        ))
        .expect("session fixture")
    }

    #[test]
    fn a_background_shell_is_not_the_agent_working() {
        // Measured against a live session: `busy` holds through minutes of
        // back-to-back tool calls, so `shell` is not "running a command" — it
        // is a shell left running while the agent waits at its prompt, often
        // with a question for you. Claude Code's own session list calls that
        // working; a spinner must not, or the tab that wants you looks like
        // the tab that doesn't.
        assert!(session("busy").busy());
        assert!(!session("shell").busy(), "the agent is at its prompt");
        assert!(session("shell").background_shell());
        assert!(!session("idle").busy());
        assert!(!session("waiting").busy());
        assert!(session("waiting").waiting(), "blocked on the user");
    }

    #[test]
    fn hooks_stay_authoritative_over_the_registry() {
        use ClaudeState::*;
        // Hooks mark the end of a turn exactly. The registry may not re-open
        // one they closed — a session left with a background shell reads as
        // "shell" for as long as that shell lives, which is hours.
        assert_eq!(merge_registry(Idle, true, &session("shell")), Idle);
        assert_eq!(merge_registry(Idle, true, &session("busy")), Idle);
        assert_eq!(merge_registry(Busy, true, &session("idle")), Busy);
        assert_eq!(
            merge_registry(DoneUnseen, true, &session("idle")),
            DoneUnseen
        );
        // The one exception: a working session clears a stale attention flag,
        // because declining a prompt emits no hook to clear it.
        assert_eq!(merge_registry(NeedsYou, true, &session("busy")), Busy);
        assert_eq!(merge_registry(NeedsYou, true, &session("idle")), NeedsYou);
    }

    #[test]
    fn without_hooks_the_registry_drives_every_state() {
        use ClaudeState::*;
        assert_eq!(merge_registry(Idle, false, &session("busy")), Busy);
        assert_eq!(merge_registry(Busy, false, &session("idle")), Idle);
        // A background shell leaves the agent idle: marked in the rail, not
        // spun.
        assert_eq!(merge_registry(Busy, false, &session("shell")), Idle);
        // A session blocked on a permission prompt with no hook to report it
        // used to read as idle: the flag now comes from the registry.
        assert_eq!(merge_registry(Idle, false, &session("waiting")), NeedsYou);
        // Attention states are the user's to clear, not the registry's.
        assert_eq!(
            merge_registry(DoneUnseen, false, &session("idle")),
            DoneUnseen
        );
    }

    #[test]
    fn a_subagent_finishing_does_not_end_the_turn() {
        let mut w = ClaudeWatch::for_tests();
        feed(&mut w, &hook("SessionStart", ""), Some(TabId(1)));
        feed(&mut w, &hook("UserPromptSubmit", ""), Some(TabId(1)));
        assert_eq!(w.state_of(TAB), ClaudeState::Busy);

        // These fire mid-turn, while the main agent carries on with the
        // result. Treating them as "finished" is what stopped the spinner
        // half way through the work.
        for kind in ["agent_completed", "task_completed"] {
            let m = hook("Notification", &format!(r#","notification_type":"{kind}""#));
            feed(&mut w, &m, Some(TabId(1)));
            assert_eq!(w.state_of(TAB), ClaudeState::Busy, "{kind} mid-turn");
        }

        // Once the session is genuinely at its prompt, it is done.
        let m = hook("Notification", r#","notification_type":"idle_prompt""#);
        feed(&mut w, &m, Some(TabId(1)));
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
                five_hour_resets: None,
                seven_day_resets: None,
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
    fn a_reset_is_read_however_it_is_written() {
        let at = |v: serde_json::Value| super::reset_time(&v).map(|t| t.as_second());
        // Epoch seconds, epoch milliseconds, and RFC 3339 all mean the moment.
        assert_eq!(
            at(serde_json::json!({ "resets_at": 1_760_000_000 })),
            Some(1_760_000_000)
        );
        assert_eq!(
            at(serde_json::json!({ "resets_at_ms": 1_760_000_000_000i64 })),
            Some(1_760_000_000)
        );
        assert_eq!(
            at(serde_json::json!({ "reset_at": "2025-10-09T08:53:20Z" })),
            Some(1_760_000_000)
        );
        // Nothing to read, or something unreadable, is no reset rather than a
        // wrong one — the bar then simply says nothing about renewal.
        assert_eq!(at(serde_json::json!({ "used_percentage": 12 })), None);
        assert_eq!(at(serde_json::json!({ "resets_at": "soon" })), None);
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
