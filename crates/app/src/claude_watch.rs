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
use giverny_core::config::ClaudeConfig;
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
    /// The most any window has been used, for as long as that window lasts.
    /// Keyed by `LimitEntry::kind`.
    pub peak: HashMap<String, Peak>,
}

/// The high-water mark of one window.
///
/// Usage within a window only ever goes up, but the two sources it can be read
/// from disagree: the status line reports the turn it is in, while the cache
/// holds whatever `/usage` last fetched, which can be minutes behind and is
/// then written with a *fresh* timestamp. Reading "whichever was sampled last"
/// therefore walks backwards, and the bar bounces between two numbers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Peak {
    pub percent: f64,
    /// The window this belongs to. A different reset is a different window,
    /// and the mark starts again.
    pub resets: Option<jiff::Timestamp>,
}

/// Where a reading falls against a high-water mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Window {
    /// The mark's own window: the reading can only raise it.
    Same,
    /// A window that closed before the mark's opened — a number nothing
    /// current should be showing.
    Older,
    /// A later window, or one the mark cannot vouch for: start again.
    Other,
}

impl Peak {
    /// Two reset times this close apart name the same window. The sources do
    /// not spell it the same way — the push says `17:30:00`, the cache
    /// `17:30:00.062934` — and a window never renews less than five hours
    /// after the last one did, so an hour is slack, not ambiguity.
    const SAME_WINDOW_SECS: i64 = 3600;
    /// Without a reset time to go by, a number that has fallen this far below
    /// the mark is a new window rather than a source that is behind.
    const A_RESET_NOT_A_DISAGREEMENT: f64 = 25.0;

    /// Which window `read` is from, as far as this mark can tell.
    ///
    /// Every running `claude` pushes the percentage *its own* last request
    /// was answered with, and a session that has sat idle for an hour pushes
    /// an hour-old number — with the current window's reset time beside it,
    /// because that has not changed. A known reset time is what identifies
    /// the window, so a number that is merely behind never ends the mark,
    /// however far behind it is.
    fn place(&self, read: &Reading, now: jiff::Timestamp) -> Window {
        match (self.resets, read.resets) {
            // The mark's window is over; whatever comes next starts afresh.
            (Some(mine), _) if mine <= now => Window::Other,
            (Some(mine), Some(theirs)) => {
                let apart = theirs.as_second() - mine.as_second();
                if apart.abs() < Self::SAME_WINDOW_SECS {
                    Window::Same
                } else if apart < 0 {
                    Window::Older
                } else {
                    Window::Other
                }
            }
            // A mark with no reset time and a reading that has one: the
            // reading knows more.
            (None, Some(_)) => Window::Other,
            (_, None) if read.percent + Self::A_RESET_NOT_A_DISAGREEMENT < self.percent => {
                Window::Other
            }
            (_, None) => Window::Same,
        }
    }
}

/// One usage bar's numbers, taken from whichever source is freshest.
///
/// The cache and the statusline push disagree whenever the cache has stopped
/// being refreshed, and they have to be read as a set: a percentage from the
/// push beside a severity from the cache is how a week 21% used came up red,
/// the cache still holding the last thing it managed to fetch.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    pub percent: f64,
    /// The percentage came from a statusline push rather than the cache.
    pub live: bool,
    /// Out, or nearly. Red.
    pub critical: bool,
    /// When the window renews, if anything still knows.
    pub resets: Option<jiff::Timestamp>,
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
    /// A side instance (`GIVERNY_NO_ACCOUNT_SETUP`): read the accounts, never
    /// write them. See [`leaves_accounts_alone`].
    leave_accounts: bool,
    /// The `refreshInterval` our status line entries get, from the config.
    statusline_refresh: hooks::Refresh,
    /// When the link was last checked for a target that is gone.
    #[cfg(unix)]
    last_link_check: Instant,
    /// The Claude processes, by account and pid, that were running when
    /// their account's hooks were first installed. Claude Code reads hooks
    /// when a session starts, so these report nothing until restarted. Each
    /// drops out when its process ends. See [`ClaudeWatch::sessions_without_hooks`].
    predate_hooks: HashSet<(PathBuf, u32)>,
}

/// The environment variable that makes this a side instance.
pub const NO_ACCOUNT_SETUP_ENV: &str = "GIVERNY_NO_ACCOUNT_SETUP";

/// Is this a side instance that must leave every account's Claude config
/// alone?
///
/// Each account's `settings.json` names Giverny's link for its hooks and
/// status line, and every Giverny points that link at itself and adopts the
/// accounts it finds. A second Giverny started to test a build therefore
/// re-points every session's hooks at the test binary, which `cargo clean`
/// can then delete out from under them.
///
/// `GIVERNY_NO_ACCOUNT_SETUP=1` turns all of that off: the link, hooks,
/// status lines and auto mode are read but never written, at startup or
/// from the UI. Set and not empty or `0` counts as on.
///
/// An explicit switch rather than a guess: a test build differs from the
/// installed one only in its path, which is exactly what a real reinstall
/// (`cargo install`, a moved build) also changes.
pub fn leaves_accounts_alone(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|v| !v.is_empty() && v != "0")
}

/// What `claude.statusline_refresh_seconds` asks of our status line entries:
/// unset fills in the default only where an entry has none, set writes it.
pub fn statusline_refresh(cfg: &ClaudeConfig) -> hooks::Refresh {
    match cfg.statusline_refresh_seconds {
        None => hooks::Refresh::Fill(ClaudeConfig::DEFAULT_STATUSLINE_REFRESH_S),
        Some(secs) => hooks::Refresh::Set(secs),
    }
}

/// What a write refused by a side instance reports, for the UI's log line.
const LEFT_ALONE: &str = "side instance (GIVERNY_NO_ACCOUNT_SETUP): account settings left alone";

/// How often the on-disk usage caches are re-read. The numbers inside them
/// only move when Claude Code fetches (minutes apart), and anything faster —
/// a statusline push, a finished refresh — updates the panel directly, so
/// polling harder buys nothing.
const USAGE_READ_INTERVAL: Duration = Duration::from_secs(60);
/// How often the cache files are checked for having been rewritten.
const CACHE_STAT_INTERVAL: Duration = Duration::from_secs(2);
/// How often accounts are looked for again while none inside WSL is known.
const LOOK_AGAIN_INTERVAL: Duration = Duration::from_secs(45);
/// How often the link is checked for a target that is gone: a build that
/// pointed it at itself, then was deleted (`cargo clean`).
#[cfg(unix)]
const LINK_CHECK_INTERVAL: Duration = Duration::from_secs(5);

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
        statusline_refresh: hooks::Refresh,
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

        let leave_accounts =
            leaves_accounts_alone(std::env::var_os(NO_ACCOUNT_SETUP_ENV).as_deref());
        if leave_accounts {
            tracing::info!("{LEFT_ALONE}");
        } else {
            // Before anything is written: what is written names the link.
            #[cfg(unix)]
            if let Err(err) = hooks::point_link() {
                tracing::warn!("giverny link not pointed here: {err}");
            }
            // Reads nothing from Giverny's config but `statusline_refresh`,
            // which the caller makes the unset one when the config does not
            // parse, so that is no reason to skip it.
            Self::adopt_statusline_where_hooked(&profiles, statusline_refresh);
        }
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
            leave_accounts,
            statusline_refresh,
            #[cfg(unix)]
            last_link_check: Instant::now(),
            predate_hooks: HashSet::new(),
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
        if self.leave_accounts {
            return Err(LEFT_ALONE.into());
        }
        let mut ok = 0;
        let mut errs = Vec::new();
        for p in &self.profiles {
            let settings = p.config_dir.join("settings.json");
            let first = !hooks::installed_in(&settings);
            match hooks::install_into(&settings) {
                Ok(_) => {
                    ok += 1;
                    if first {
                        self.predate_hooks.extend(
                            self.scanned
                                .live
                                .iter()
                                .filter(|s| s.config_dir == p.config_dir)
                                .map(|s| (s.config_dir.clone(), s.entry.pid)),
                        );
                    }
                }
                Err(e) => errs.push(format!("{}: {e}", p.name)),
            }
            // Live usage comes with it — the on-disk cache goes stale for
            // accounts that aren't actively running Claude. Profiles with a
            // statusline of their own are left alone (set_statusline errs).
            if let Err(e) = hooks::set_statusline(&settings, true, self.statusline_refresh) {
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
    fn adopt_statusline_where_hooked(profiles: &[Profile], refresh: hooks::Refresh) {
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
            // An entry of ours from before `refreshInterval` was written gains
            // it here, so existing installs see the cold-cache warning too.
            if !hooks::statusline_installed_in(&settings)
                || hooks::needs_path_refresh(&settings)
                || hooks::statusline_refresh_stale(&settings, refresh)
            {
                match hooks::set_statusline(&settings, true, refresh) {
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
        self.remember_peaks();

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
                    let live = &self.scanned.live;
                    self.predate_hooks.retain(|(dir, pid)| {
                        live.iter()
                            .any(|s| &s.config_dir == dir && s.entry.pid == *pid)
                    });
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
                    let _ = tx.send(ScanResult { live });
                })
                .is_ok()
            {
                self.scan_rx = Some(rx);
            }
        }

        // A build that pointed the link at itself and was then deleted left
        // every session's hooks naming nothing: take the link back.
        #[cfg(unix)]
        if !self.leave_accounts && self.last_link_check.elapsed() >= LINK_CHECK_INTERVAL {
            self.last_link_check = Instant::now();
            if hooks::link_is_dangling() {
                match hooks::point_link() {
                    Ok(()) => {
                        tracing::info!("giverny link named a binary that is gone: pointed here")
                    }
                    Err(err) => tracing::warn!("giverny link not pointed here: {err}"),
                }
            }
        }

        // Background agents: a handful of small files, so a slower tick than
        // the session registry is plenty.
        if self.last_jobs.elapsed() >= Duration::from_secs(3) {
            self.last_jobs = Instant::now();
            let dirs: Vec<PathBuf> = self.profiles.iter().map(|p| p.config_dir.clone()).collect();
            // Finished agents drop off: the list is what still wants
            // watching, not a record of everything that ever ran.
            self.jobs = jobs::scan(dirs)
                .into_iter()
                .filter(|job| job.worth_watching())
                .collect();
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

    /// Keep each window's high-water mark up to date.
    ///
    /// A reading from the window the mark is for can only raise it; one from
    /// a later window replaces it; one that belongs to no window the mark can
    /// place is let in only if it has not fallen tens of points below the mark.
    fn remember_peaks(&mut self) {
        let now = jiff::Timestamp::now();
        for acc in &mut self.accounts {
            let Some(usage) = &acc.usage else { continue };
            for limit in &usage.limits {
                let read = Self::sampled(acc, limit, now);
                let peak = acc.peak.entry(limit.kind.clone()).or_insert(Peak {
                    percent: read.percent,
                    resets: read.resets,
                });
                match peak.place(&read, now) {
                    Window::Same => peak.percent = peak.percent.max(read.percent),
                    // The mark's window is the current one; this number is
                    // from before it.
                    Window::Older => {}
                    Window::Other => {
                        *peak = Peak {
                            percent: read.percent,
                            resets: read.resets,
                        }
                    }
                }
            }
        }
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
        // The panels are rebuilt from the profiles, so anything the panel
        // learned rather than read — the last push, how far each window has
        // got — has to be carried over or it resets every minute.
        let mut previous: HashMap<PathBuf, (Option<LiveUsage>, HashMap<String, Peak>)> = self
            .accounts
            .drain(..)
            .map(|a| (a.profile.config_dir, (a.live, a.peak)))
            .collect();
        self.accounts = self
            .profiles
            .iter()
            .map(|p| {
                let (live, peak) = previous.remove(&p.config_dir).unwrap_or_default();
                AccountPanel {
                    usage: usage::read(&p.config_dir),
                    live,
                    peak,
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
        if self.leave_accounts {
            tracing::info!("auto mode: {LEFT_ALONE}");
            return;
        }
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
        if self.leave_accounts {
            return;
        }
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
        if self.leave_accounts {
            return Err(LEFT_ALONE.into());
        }
        let mut errs = Vec::new();
        for p in &self.profiles {
            let settings = p.config_dir.join("settings.json");
            if let Err(e) = hooks::set_statusline(&settings, enable, self.statusline_refresh) {
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

    /// Follow `claude.statusline_refresh_seconds`: every account whose status
    /// line is ours gets the `refreshInterval` it asks for. One that is off,
    /// or someone else's, is left alone.
    pub fn set_statusline_refresh(&mut self, refresh: hooks::Refresh) {
        self.statusline_refresh = refresh;
        if self.leave_accounts {
            return;
        }
        for p in &self.profiles {
            let settings = p.config_dir.join("settings.json");
            if !hooks::statusline_refresh_stale(&settings, refresh) {
                continue;
            }
            match hooks::set_statusline(&settings, true, refresh) {
                Ok(()) => tracing::info!("statusline refresh {refresh:?} for {}", p.name),
                Err(e) => tracing::warn!("statusline refresh unchanged for {}: {e}", p.name),
            }
        }
    }

    /// Do all profiles have the live-usage statusline?
    pub fn statusline_on(&self) -> bool {
        !self.accounts.is_empty() && self.accounts.iter().all(|a| a.statusline_on)
    }

    /// When the Claude on `account` can work again, if anything says.
    ///
    /// The message on screen names a reset time too, in the local words of
    /// whoever is reading it ("resets 3pm"); this is the same moment as a
    /// timestamp, read the same way the usage bars read it — the status line
    /// push first, the cache behind it. The cache matters least here of
    /// anywhere: refreshing it means running `claude` against the very
    /// account that is out of limit.
    ///
    /// An account nobody could name falls back to whichever account reopens
    /// first. A tab whose session never fired a hook has no account against
    /// its name, and waiting forever is worse than waking on the wrong
    /// window.
    pub fn window_reopens(&self, account: Option<&str>) -> Option<jiff::Timestamp> {
        let now = jiff::Timestamp::now();
        let named = account
            .and_then(|name| self.accounts.iter().find(|a| a.profile.name == name))
            .and_then(|panel| Self::reopens_for(panel, now));
        named.or_else(|| {
            self.accounts
                .iter()
                .filter_map(|panel| Self::reopens_for(panel, now))
                .min()
        })
    }

    /// The window that is actually out, not merely the next one to come
    /// round: a session stopped by the weekly limit is not freed when the
    /// five-hour one resets.
    fn reopens_for(panel: &AccountPanel, now: jiff::Timestamp) -> Option<jiff::Timestamp> {
        let limits = panel.usage.as_ref().map(|u| u.limits.as_slice());
        let read = |kind: &str| {
            limits?
                .iter()
                .find(|l| l.kind == kind)
                .map(|l| Self::reading(panel, l, now))
        };
        let spent = ["session", "weekly_all"]
            .iter()
            .filter_map(|kind| read(kind))
            .filter(|r| r.percent >= 95.0)
            .filter_map(|r| r.resets)
            .max();
        spent
            .or_else(|| read("session").and_then(|r| r.resets))
            // No cache at all, which is every account whose numbers have only
            // ever come from the status line.
            .or_else(|| panel.live.as_ref().and_then(|l| l.five_hour_resets))
            .filter(|at| *at > now)
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

    /// What one bar should show: the statusline push where it is fresher than
    /// the on-disk cache, the cache otherwise.
    /// What one bar shows: the freshest sample, never lower than this window
    /// has already been seen to reach.
    pub fn reading(
        acc: &AccountPanel,
        limit: &giverny_claude::usage::LimitEntry,
        now: jiff::Timestamp,
    ) -> Reading {
        let mut read = Self::sampled(acc, limit, now);
        if let Some(peak) = acc.peak.get(&limit.kind)
            && peak.place(&read, now) != Window::Other
            && peak.percent > read.percent
        {
            read.percent = peak.percent;
            read.critical = read.critical || peak.percent >= 95.0;
            read.resets = peak.resets.or(read.resets);
        }
        read
    }

    /// The freshest of the two sources, whichever that is right now.
    fn sampled(
        acc: &AccountPanel,
        limit: &giverny_claude::usage::LimitEntry,
        now: jiff::Timestamp,
    ) -> Reading {
        let pushed = acc.live.as_ref().filter(|live| {
            let cache_age_ms = acc
                .usage
                .as_ref()
                .map(|u| (now.as_millisecond() - u.fetched_at_ms as i64).max(0))
                .unwrap_or(i64::MAX);
            (live.at.elapsed().as_millis() as i64) < cache_age_ms
        });
        let window = |pick: fn(&LiveUsage) -> Option<f64>,
                      reset: fn(&LiveUsage) -> Option<jiff::Timestamp>| {
            (
                pushed.and_then(pick).map(|p| p.clamp(0.0, 100.0)),
                pushed.and_then(reset).filter(|at| *at > now),
            )
        };
        let (fresh, pushed_reset) = match limit.kind.as_str() {
            "session" => window(|l| l.five_hour, |l| l.five_hour_resets),
            "weekly_all" => window(|l| l.seven_day, |l| l.seven_day_resets),
            // A scoped window (one model's own allowance) is not in the push.
            _ => (None, None),
        };
        let percent = fresh.unwrap_or_else(|| limit.effective_percent(now));
        // The cache's severity describes the cache's percentage. Where that is
        // not the number being shown — a push took over, or the window it
        // measured has since lapsed — the number on screen decides for itself.
        let speaks_for_itself = fresh.is_some() || limit.rolled_over(now);
        Reading {
            percent,
            live: fresh.is_some(),
            critical: percent >= 95.0 || (!speaks_for_itself && limit.critical()),
            // The push's reset time first: it comes from the running Claude,
            // while the cache can be hours old — and a cache that has fallen
            // behind the window it describes claims the reset already
            // happened, which is why this line went missing for anyone whose
            // cache refresh was failing.
            resets: pushed_reset.or_else(|| limit.resets_at_ts().filter(|at| *at > now)),
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
    pub(crate) fn for_tests() -> Self {
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
            leave_accounts: false,
            statusline_refresh: statusline_refresh(&ClaudeConfig::default()),
            #[cfg(unix)]
            last_link_check: Instant::now(),
            predate_hooks: HashSet::new(),
            refreshing: Arc::new(Mutex::new(HashSet::new())),
            attempted: Arc::new(Mutex::new(HashMap::new())),
            cache_dirty: Arc::new(AtomicBool::new(false)),
        }
    }

    /// How many Claude sessions started before their account's hooks were
    /// first installed, and are still running without them.
    pub fn sessions_without_hooks(&self) -> usize {
        self.predate_hooks.len()
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
            peak: HashMap::new(),
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
            peak: HashMap::new(),
            statusline_on: true,
        };

        // Stale cache + fresh push ⇒ push wins and is flagged live.
        let read = ClaudeWatch::reading(&mk(120, Some(77.0)), &limit, now);
        assert_eq!((read.percent, read.live), (77.0, true));
        // No push ⇒ cache value, not flagged.
        let read = ClaudeWatch::reading(&mk(120, None), &limit, now);
        assert_eq!((read.percent, read.live), (5.0, false));
    }

    /// Why a rate-limited session never woke up again: the reopening time was
    /// read from the on-disk cache alone, and a rate-limited account is
    /// exactly the one whose cache cannot refresh, because refreshing it
    /// means running `claude` against the account that is out of limit.
    #[test]
    fn a_reopening_comes_from_the_push_when_the_cache_has_lapsed() {
        use giverny_claude::usage::{AccountUsage, LimitEntry};
        let now = jiff::Timestamp::now();
        let at = |mins: i64| now + jiff::Span::new().minutes(mins);
        let limit = |kind: &str, percent: u32, resets: jiff::Timestamp| -> LimitEntry {
            serde_json::from_str(&format!(
                r#"{{"kind":"{kind}","percent":{percent},"severity":"critical",
                     "is_active":true,"resets_at":"{resets}"}}"#
            ))
            .unwrap()
        };
        let panel = |name: &str, limits: Vec<LimitEntry>, live: Option<LiveUsage>| AccountPanel {
            profile: Profile {
                name: name.into(),
                config_dir: PathBuf::from("/tmp").join(name),
                email: None,
                account_uuid: None,
            },
            usage: Some(AccountUsage {
                fetched_at_ms: (now.as_millisecond() - 6 * 3_600_000) as u64,
                limits,
            }),
            live,
            peak: HashMap::new(),
            statusline_on: true,
        };

        // The cache is six hours old: its five-hour window "reopened" an hour
        // ago, which reads as no reopening at all. The push knows better.
        let mut w = ClaudeWatch::for_tests();
        w.accounts = vec![panel(
            "a",
            vec![limit("session", 100, at(-60))],
            Some(LiveUsage {
                at: Instant::now(),
                five_hour: Some(100.0),
                seven_day: Some(40.0),
                five_hour_resets: Some(at(35)),
                seven_day_resets: None,
            }),
        )];
        assert_eq!(w.window_reopens(Some("a")), Some(at(35)));

        // A tab whose session never fired a hook has no account against its
        // name. Waking on another account's window beats waiting forever.
        assert_eq!(w.window_reopens(None), Some(at(35)));
        assert_eq!(w.window_reopens(Some("nobody")), Some(at(35)));

        // Stopped by the weekly limit: the five-hour window coming round in
        // half an hour does not free it.
        let mut w = ClaudeWatch::for_tests();
        w.accounts = vec![panel(
            "a",
            vec![
                limit("session", 100, at(30)),
                limit("weekly_all", 100, at(4_000)),
            ],
            None,
        )];
        assert_eq!(w.window_reopens(Some("a")), Some(at(4_000)));

        // Only the five-hour window is out: the weekly one is not the answer.
        let mut w = ClaudeWatch::for_tests();
        w.accounts = vec![panel(
            "a",
            vec![
                limit("session", 100, at(30)),
                limit("weekly_all", 20, at(4_000)),
            ],
            None,
        )];
        assert_eq!(w.window_reopens(Some("a")), Some(at(30)));

        // Nothing known at all is still nothing: no guessing.
        let w = ClaudeWatch::for_tests();
        assert_eq!(w.window_reopens(Some("a")), None);
    }

    /// ita's bar bouncing between 90 and 99: two sources sampled at different
    /// moments, taking turns at being the fresher one. Within a window usage
    /// only goes up, so the bar does too.
    #[test]
    fn a_window_never_walks_backwards() {
        use giverny_claude::usage::{AccountUsage, LimitEntry};
        let now = jiff::Timestamp::now();
        let resets = now + jiff::Span::new().hours(2);
        let limit = |percent: u32, at: jiff::Timestamp| -> LimitEntry {
            serde_json::from_str(&format!(
                r#"{{"kind":"session","percent":{percent},"severity":"normal",
                     "is_active":true,"resets_at":"{at}"}}"#
            ))
            .unwrap()
        };
        let panel =
            |cache_age_min: i64, cache: LimitEntry, push: Option<(f64, u64)>| AccountPanel {
                profile: Profile {
                    name: "a".into(),
                    config_dir: PathBuf::from("/tmp/x"),
                    email: None,
                    account_uuid: None,
                },
                usage: Some(AccountUsage {
                    fetched_at_ms: (now.as_millisecond() - cache_age_min * 60_000) as u64,
                    limits: vec![cache],
                }),
                live: push.map(|(percent, age_s)| LiveUsage {
                    at: Instant::now() - Duration::from_secs(age_s),
                    five_hour: Some(percent),
                    seven_day: None,
                    five_hour_resets: Some(resets),
                    seven_day_resets: None,
                }),
                peak: HashMap::new(),
                statusline_on: true,
            };

        let mut w = ClaudeWatch::for_tests();
        // The cache is ten minutes old and the push just arrived: 99.
        w.accounts = vec![panel(10, limit(90, resets), Some((99.0, 1)))];
        w.remember_peaks();
        let acc = &w.accounts[0];
        let read = ClaudeWatch::reading(acc, &acc.usage.as_ref().unwrap().limits[0], now);
        assert_eq!(read.percent, 99.0);

        // `/usage` refreshes, writing a number it fetched minutes ago: the
        // freshest *sample* is now the lower one. The bar holds.
        let peak = w.accounts[0].peak.clone();
        w.accounts = vec![panel(0, limit(90, resets), Some((99.0, 600)))];
        w.accounts[0].peak = peak;
        w.remember_peaks();
        let acc = &w.accounts[0];
        let limits = &acc.usage.as_ref().unwrap().limits;
        assert_eq!(ClaudeWatch::sampled(acc, &limits[0], now).percent, 90.0);
        assert_eq!(ClaudeWatch::reading(acc, &limits[0], now).percent, 99.0);

        // The window resets: a different reset time is a different window, and
        // the mark goes with it.
        let later = now + jiff::Span::new().hours(7);
        let peak = w.accounts[0].peak.clone();
        w.accounts = vec![panel(0, limit(3, later), None)];
        w.accounts[0].peak = peak;
        w.remember_peaks();
        let acc = &w.accounts[0];
        let limits = &acc.usage.as_ref().unwrap().limits;
        assert_eq!(ClaudeWatch::reading(acc, &limits[0], now).percent, 3.0);
    }

    /// ita's 5h bar cycling 44 → 70 → 75 every second or two. Every running
    /// `claude` pushes the percentage its own last request was answered with,
    /// so a session idle since the window stood at 44 keeps saying 44 — with
    /// the current window's reset time — between the pushes of the busy ones.
    /// Thirty-one points below the mark read as a new window, and the mark
    /// started again from 44 on every lap. The cache, meanwhile, spells the
    /// same reset with microseconds the push does not have.
    #[test]
    fn an_idle_session_does_not_restart_the_window() {
        use giverny_claude::usage::{AccountUsage, LimitEntry};
        let now = jiff::Timestamp::now();
        // The push's reset, whole seconds; the cache's, the same moment
        // written the way `/usage` writes it.
        let resets = jiff::Timestamp::from_second(now.as_second() + 5_400).unwrap();
        let cache_resets = resets + jiff::SignedDuration::from_micros(62_934);
        let limit: LimitEntry = serde_json::from_str(&format!(
            r#"{{"kind":"session","percent":72,"severity":"warning",
                 "is_active":true,"resets_at":"{cache_resets}"}}"#
        ))
        .unwrap();
        let mut w = ClaudeWatch::for_tests();
        w.accounts.push(AccountPanel {
            profile: Profile {
                name: "acct".into(),
                config_dir: PathBuf::from("/tmp/giverny-test-acct"),
                email: None,
                account_uuid: None,
            },
            usage: Some(AccountUsage {
                fetched_at_ms: (now.as_millisecond() - 120_000) as u64,
                limits: vec![limit],
            }),
            live: None,
            peak: HashMap::new(),
            statusline_on: true,
        });
        // What `giverny statusline` relays: Claude Code's own payload shape.
        let push = |percent: u32| {
            msg(&format!(
                r#"{{"tab_id":"giverny-7","config_dir":"/tmp/giverny-test-acct",
                    "event":{{"hook_event_name":"GivernyStatusLine",
                             "rate_limits":{{"five_hour":{{"used_percentage":{percent},
                                                         "resets_at":{}}}}}}}}}"#,
                resets.as_second()
            ))
        };
        let shown = |w: &ClaudeWatch| {
            let acc = &w.accounts[0];
            ClaudeWatch::reading(acc, &acc.usage.as_ref().unwrap().limits[0], now).percent
        };

        // Before any push the cache is the only source: 72.
        w.remember_peaks();
        assert_eq!(shown(&w), 72.0);
        let mut seen = Vec::new();
        for _lap in 0..3 {
            for percent in [44, 70, 75] {
                // A tick: the peaks are brought up to date, then the push lands.
                w.remember_peaks();
                feed(&mut w, &push(percent), Some(TAB));
                seen.push(shown(&w));
                w.remember_peaks();
                seen.push(shown(&w));
            }
        }
        // It climbed to 75 once and stayed.
        assert!(
            seen.windows(2).all(|p| p[1] >= p[0]),
            "walked back: {seen:?}"
        );
        assert_eq!(seen.last(), Some(&75.0));

        // The window renews: a reset five hours on is a new window, and the
        // bar starts again from what the new window says.
        let next = jiff::Timestamp::from_second(resets.as_second() + 5 * 3600).unwrap();
        w.accounts[0].usage = None;
        w.accounts[0].peak.insert(
            "session".into(),
            Peak {
                percent: 75.0,
                resets: Some(resets),
            },
        );
        let fresh: LimitEntry = serde_json::from_str(&format!(
            r#"{{"kind":"session","percent":3,"severity":"normal",
                 "is_active":true,"resets_at":"{next}"}}"#
        ))
        .unwrap();
        w.accounts[0].live = None;
        w.accounts[0].usage = Some(AccountUsage {
            fetched_at_ms: now.as_millisecond() as u64,
            limits: vec![fresh],
        });
        w.remember_peaks();
        assert_eq!(shown(&w), 3.0);
    }

    /// An account whose cache has stopped being refreshed, which is every
    /// account whose `claude` Giverny cannot run: the statusline push is the
    /// only thing still telling the truth, and the cache holds whatever it
    /// last managed to fetch.
    #[test]
    fn a_stale_cache_decides_nothing_the_push_has_answered() {
        use giverny_claude::usage::{AccountUsage, LimitEntry};
        let now: jiff::Timestamp = "2025-10-09T12:00:00Z".parse().unwrap();
        let at = |s: &str| -> jiff::Timestamp { s.parse().unwrap() };
        // What his cache still held: a week that ran out, days ago.
        let limit: LimitEntry = serde_json::from_str(
            r#"{"kind":"weekly_all","percent":97,"severity":"critical","is_active":true,
                "resets_at":"2025-10-06T00:00:00Z"}"#,
        )
        .unwrap();
        let mk = |cache_age_min: i64, live: Option<LiveUsage>| AccountPanel {
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
            live,
            peak: HashMap::new(),
            statusline_on: true,
        };
        let push = |percent: f64, resets: Option<&str>| LiveUsage {
            at: Instant::now(),
            five_hour: None,
            seven_day: Some(percent),
            five_hour_resets: None,
            seven_day_resets: resets.map(at),
        };

        // The bar ita saw red at 21%: the percentage was the push's, the
        // colour the cache's. One source answers for a window, or none does.
        let read = ClaudeWatch::reading(
            &mk(4_000, Some(push(21.0, Some("2025-10-12T13:00:00Z")))),
            &limit,
            now,
        );
        assert_eq!(
            (read.percent, read.live, read.critical),
            (21.0, true, false)
        );
        assert_eq!(read.resets, Some(at("2025-10-12T13:00:00Z")));

        // Still critical when the number itself says so.
        let read = ClaudeWatch::reading(&mk(4_000, Some(push(99.0, None))), &limit, now);
        assert!(read.critical);

        // No push at all: the window the cache measured has lapsed, so it
        // reports neither its percentage nor its severity nor its reset.
        let read = ClaudeWatch::reading(&mk(4_000, None), &limit, now);
        assert_eq!(
            (read.percent, read.live, read.critical),
            (0.0, false, false)
        );
        assert_eq!(read.resets, None);
    }

    /// A cache that is still describing the window it is in keeps its say.
    #[test]
    fn a_current_cache_is_believed() {
        use giverny_claude::usage::{AccountUsage, LimitEntry};
        let now: jiff::Timestamp = "2025-10-09T12:00:00Z".parse().unwrap();
        let limit: LimitEntry = serde_json::from_str(
            r#"{"kind":"weekly_all","percent":88,"severity":"critical","is_active":true,
                "resets_at":"2025-10-09T14:30:00Z"}"#,
        )
        .unwrap();
        let acc = AccountPanel {
            profile: Profile {
                name: "a".into(),
                config_dir: PathBuf::from("/tmp/x"),
                email: None,
                account_uuid: None,
            },
            usage: Some(AccountUsage {
                fetched_at_ms: (now.as_millisecond() - 60_000) as u64,
                limits: vec![],
            }),
            live: None,
            peak: HashMap::new(),
            statusline_on: true,
        };
        let read = ClaudeWatch::reading(&acc, &limit, now);
        assert_eq!(
            (read.percent, read.live, read.critical),
            (88.0, false, true)
        );
        assert_eq!(read.resets, Some("2025-10-09T14:30:00Z".parse().unwrap()));
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

    #[test]
    fn a_side_instance_is_asked_for_explicitly() {
        use std::ffi::OsStr;
        assert!(!leaves_accounts_alone(None));
        assert!(!leaves_accounts_alone(Some(OsStr::new(""))));
        assert!(!leaves_accounts_alone(Some(OsStr::new("0"))));
        assert!(leaves_accounts_alone(Some(OsStr::new("1"))));
        assert!(leaves_accounts_alone(Some(OsStr::new("yes"))));
    }

    fn scratch_account(name: &str) -> (PathBuf, Profile) {
        let dir = std::env::temp_dir().join(format!(
            "giverny-watch-{name}-{}-{}",
            std::process::id(),
            jiff::Timestamp::now().as_nanosecond()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let profile = Profile {
            name: name.into(),
            config_dir: dir.clone(),
            email: None,
            account_uuid: None,
        };
        (dir, profile)
    }

    /// A side instance writes no account, whichever way it is asked to.
    #[test]
    fn a_side_instance_leaves_the_accounts_alone() {
        let (dir, profile) = scratch_account("side");
        let settings = dir.join("settings.json");
        let before = r#"{"theme":"dark"}"#;
        std::fs::write(&settings, before).unwrap();
        let mut w = ClaudeWatch::for_tests();
        w.profiles = vec![profile];
        w.leave_accounts = true;

        assert!(w.install_hooks().is_err());
        assert!(w.set_statusline(true).is_err());
        w.set_auto_mode(true);
        w.ensure_auto_mode();
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Sessions running when hooks are first installed never loaded them:
    /// those, and only those, are counted until they end. A later install
    /// over hooks already there counts nothing.
    #[test]
    fn sessions_older_than_the_first_install_are_counted_until_they_end() {
        let (dir, profile) = scratch_account("first");
        let mut w = ClaudeWatch::for_tests();
        w.profiles = vec![profile];
        let live = |pid: u32| registry::LiveSession {
            entry: serde_json::from_str(&format!(r#"{{"pid":{pid},"sessionId":"s-{pid}"}}"#))
                .unwrap(),
            config_dir: dir.clone(),
        };
        w.scanned.live = vec![live(11), live(12)];

        w.install_hooks().unwrap();
        assert_eq!(w.sessions_without_hooks(), 2);

        // Installed again: nothing new predates it.
        w.scanned.live.push(live(13));
        w.install_hooks().unwrap();
        assert_eq!(w.sessions_without_hooks(), 2);

        // One of them restarted (gone from the registry): one left.
        let (tx, rx) = crossbeam_channel::bounded(1);
        tx.send(ScanResult {
            live: vec![live(12), live(13)],
        })
        .unwrap();
        w.scan_rx = Some(rx);
        w.tick(&HashMap::new(), None, &HashMap::new());
        assert_eq!(w.sessions_without_hooks(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
