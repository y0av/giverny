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

pub use giverny_core::agent::AgentState as ClaudeState;

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
    /// Work this session delegated — subagents, a workflow, teammates, a
    /// cloud session — was still running when its last turn ended, and the
    /// registry has not said since that it stopped: the agent is free to
    /// talk while its workers go on. Set by the `Stop` hook's
    /// `background_tasks`, kept while the registry says `busy` (which, with
    /// the turn over, is that work: Claude Code counts it as working), and
    /// cleared by the first scan that does not, or by the next turn.
    pub agents: bool,
    /// What the tab was when a compaction began: Busy for one Claude Code
    /// started mid-turn, whatever the last turn left for a `/compact`. The
    /// compaction's `SessionStart` puts it back.
    before_compact: Option<ClaudeState>,
    /// The user's prompts in this session, oldest first, in full (at most
    /// [`PROMPT_HISTORY`]): from the `UserPromptSubmit` hook, and read back
    /// from the transcript for a session adopted mid-way or resumed.
    pub prompts: Vec<String>,
    /// For each prompt at the end of `prompts` a hook reported, when it
    /// reached Giverny. Kept until a transcript read is sure to have it or
    /// sure not to. See [`ClaudeWatch::apply_found_prompts`].
    heard: Vec<Heard>,
    last_hook: Option<Instant>,
    /// When `last_hook` was heard, by the wall clock (ms since the epoch):
    /// what a registry entry's `statusUpdatedAt` is told apart by, older
    /// than what the hooks said or newer.
    heard_at_ms: u64,
    seen_in_scan: bool,
}

/// When a hook's prompt reached Giverny: after the hook stream was last
/// drained before it, and by the drain that took it.
#[derive(Debug, Clone, Copy)]
struct Heard {
    after: Instant,
    by: Instant,
}

/// How far apart a prompt's hook and its line in the transcript can be, as
/// Giverny sees them. Claude Code writes the prompt once its hooks have run,
/// and the hook can reach Giverny up to a spool poll (400 ms) after it ran.
/// A prompt heard this long before a read began is in it; one heard this long
/// after a read finished is not. Between the two, a read can't say.
const SEAM: Duration = Duration::from_secs(1);

/// How many of a session's prompts a tab keeps.
pub const PROMPT_HISTORY: usize = 200;

/// Keep only the newest [`PROMPT_HISTORY`] prompts.
fn cap_history(prompts: &mut Vec<String>) {
    let over = prompts.len().saturating_sub(PROMPT_HISTORY);
    prompts.drain(..over);
}

impl ClaudeTab {
    /// Something this session started still works while the agent waits at
    /// its prompt: a background shell or delegated work. What the rail's
    /// three dots say.
    pub fn working_in_background(&self) -> bool {
        self.background || self.agents
    }

    /// The conversation this tab holds. A different one — `/clear`, a
    /// `/resume`, a new `claude` — has not been asked anything yet, as far as
    /// this tab knows, and its transcript is to be read again: a session
    /// resumed in the same tab (`claude --resume` after an exit, `/resume`
    /// back after a `/clear`) has prompts this tab once read and dropped.
    fn set_session(&mut self, session: Option<String>, asked: &mut HashMap<String, u64>) {
        if self.session_id != session {
            self.prompts.clear();
            self.heard.clear();
            for id in [&self.session_id, &session].into_iter().flatten() {
                asked.remove(id);
            }
        }
        self.session_id = session;
    }

    /// Keep only the newest [`PROMPT_HISTORY`] prompts.
    fn cap_prompts(&mut self) {
        cap_history(&mut self.prompts);
        let over = self.heard.len().saturating_sub(self.prompts.len());
        self.heard.drain(..over);
    }
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
    /// When the reading behind the mark was taken, as far as known.
    pub since: Sample,
}

/// When a reading was taken, as far as Giverny can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    /// The reading is no newer than this.
    pub at: Option<jiff::Timestamp>,
    pub how: Taken,
}

/// How much `Sample::at` can be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Taken {
    /// A push whose numbers have not been seen changing: they are no newer
    /// than `at`, and may be from long before it.
    Heard,
    /// A push whose numbers were seen changing at `at`: an answer from the
    /// API, just arrived.
    Seen,
    /// The on-disk cache, fetched at `at` — give or take the minutes
    /// `/usage` can lag.
    Cache,
}

impl Sample {
    /// Heard, with no idea when it was taken.
    pub fn heard() -> Sample {
        Sample {
            at: None,
            how: Taken::Heard,
        }
    }

    fn dated(&self) -> bool {
        self.how != Taken::Heard
    }
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
    /// A cache fetched this recently and far below the mark says the usage
    /// was reset.
    const CACHE_SAYS_RESET_SECS: i64 = 15 * 60;

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

    /// Does `read` take over from the mark, or does the mark stand?
    ///
    /// Within a window usage only climbs — until the account's usage is
    /// reset early: the window keeps its reset time and starts
    /// again from zero, and every session that has not asked the API since
    /// keeps pushing what it was answered before. So a lower number replaces
    /// the mark only when it was seen arriving after the mark's own; a higher
    /// one only when it is not known to be from before the mark; and a
    /// number from an older window never does.
    fn takes(&self, read: &Reading, sample: Sample, now: jiff::Timestamp) -> bool {
        let mine = self.since;
        let after_mine = mine.at.is_none_or(|m| sample.at.is_some_and(|t| t > m));
        let far = Self::A_RESET_NOT_A_DISAGREEMENT;
        match self.place(read, now) {
            Window::Other => true,
            Window::Older => false,
            Window::Same if read.percent >= self.percent => {
                let before_mine =
                    matches!((sample.at, mine.at), (Some(theirs), Some(m)) if theirs <= m);
                // A number of unknown age far above a dated one is what an
                // idle session says after a reset, not a climb.
                let stale = !sample.dated() && mine.dated() && read.percent >= self.percent + far;
                !before_mine && !stale
            }
            Window::Same => match sample.how {
                // A session's own numbers going down: it is how a reset looks.
                Taken::Seen => after_mine,
                // The cache lags by minutes, so only a fresh one far below
                // speaks for a reset — and it is believed over numbers of
                // unknown age whenever it was fetched.
                Taken::Cache => {
                    let fresh = sample.at.is_some_and(|t| {
                        now.duration_since(t)
                            < jiff::SignedDuration::from_secs(Self::CACHE_SAYS_RESET_SECS)
                    });
                    fresh && read.percent + far <= self.percent && (after_mine || !mine.dated())
                }
                Taken::Heard => false,
            },
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
    /// When these numbers were taken, as near as Giverny can tell: the
    /// moment their session's numbers last changed, or, for numbers it has
    /// not seen change, when it first heard them — they are no newer than
    /// that. Nothing for a push that names no session.
    pub since: Sample,
}

impl LiveUsage {
    fn same_numbers(&self, other: &LiveUsage) -> bool {
        (
            self.five_hour,
            self.seven_day,
            self.five_hour_resets,
            self.seven_day_resets,
        ) == (
            other.five_hour,
            other.seven_day,
            other.five_hour_resets,
            other.seven_day_resets,
        )
    }
}

/// Side effects for the app to apply after a tick.
#[derive(Default)]
pub struct WatchEffects {
    pub codex: Vec<RelayMsg>,
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
    /// The live Giverny that has the relay socket, when this one could not
    /// take it: every hook goes to that one's window, not this one's.
    relay_holder: Option<hooks::Holder>,
    /// Second launches asking this window to come forward, each the
    /// connection to answer ([`hooks::spawn_listener`]).
    #[cfg(unix)]
    raise_rx: Receiver<std::os::unix::net::UnixStream>,
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
    /// When the link was last checked for a target that is gone.
    #[cfg(unix)]
    last_link_check: Instant,
    /// The Claude processes, by account and pid, that were running when
    /// their account's hooks were first installed. Claude Code reads hooks
    /// when a session starts, so these report nothing until restarted. Each
    /// drops out when its process ends. See [`ClaudeWatch::sessions_without_hooks`].
    predate_hooks: HashSet<(PathBuf, u32)>,
    /// Sessions whose transcript has been asked for its prompts, so each is
    /// read once, with the number of the latest read asked for, and what the
    /// readers found. Only the latest read of a session is taken: reads run
    /// in parallel, and an earlier one can finish last.
    prompts_asked: HashMap<String, u64>,
    reads_asked: u64,
    /// When the hook stream was last drained: a message drained after it
    /// reached Giverny no earlier.
    hooks_drained: Instant,
    /// Each session's last usage push, by account and session id: what an
    /// account's `live` is chosen from ([`ClaudeWatch::apply_statusline`]).
    pushes: HashMap<(PathBuf, String), LiveUsage>,
    /// When this watcher began listening. A session first heard from long
    /// after that is one that has just had its first answer.
    listening: Instant,
    prompts_found: (
        crossbeam_channel::Sender<FoundPrompt>,
        crossbeam_channel::Receiver<FoundPrompt>,
    ),
}

/// Prompts read from a transcript.
struct FoundPrompt {
    session: String,
    /// Which read of the session this is, of [`ClaudeWatch::prompts_asked`].
    read: u64,
    /// When the transcript was opened: a prompt Claude Code wrote before
    /// this is in it.
    started: Instant,
    /// When the transcript had been read: a prompt Claude Code wrote after
    /// this is not in it.
    done: Instant,
    prompts: Vec<String>,
}

/// A transcript's prompts, then those a hook reported that the read does not
/// already end with: the read can land before or after a hook's prompt
/// reaches the transcript. The last `fresh` heard ones reached Giverny only
/// after the transcript was read, so they are new however they read: a
/// second "yes" is a second turn.
fn merge_history(read: &[String], heard: &[String], fresh: usize) -> Vec<String> {
    let same = |a: &[String], b: &[String]| {
        a.iter()
            .zip(b)
            .all(|(a, b)| a.split_whitespace().eq(b.split_whitespace()))
    };
    // The longest tail of the read that the heard prompts start with.
    let maybe_read = heard.len().saturating_sub(fresh);
    let overlap = (0..=maybe_read.min(read.len()))
        .rev()
        .find(|&n| same(&read[read.len() - n..], &heard[..n]))
        .unwrap_or(0);
    read.iter().chain(&heard[overlap..]).cloned().collect()
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

/// What a write refused by a side instance reports, for the UI's log line.
const LEFT_ALONE: &str = "side instance (GIVERNY_NO_ACCOUNT_SETUP): account settings left alone";

/// How often the on-disk usage caches are re-read. The numbers inside them
/// only move when Claude Code fetches (minutes apart), and anything faster —
/// a statusline push, a finished refresh — updates the panel directly, so
/// polling harder buys nothing.
const USAGE_READ_INTERVAL: Duration = Duration::from_secs(60);
/// Sessions heard from within this long of Giverny starting were already
/// running: what they push is no newer than when it was heard, and may be
/// much older. One first heard later has just been answered by the API.
const HEARD_AT_START: Duration = Duration::from_secs(60);
/// A session silent this long has ended; its last push is dropped. A running
/// one pushes every second ([`hooks::STATUSLINE_REFRESH_S`]).
const PUSH_FORGOTTEN: Duration = Duration::from_secs(300);
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
///
/// `heard`: when the last hook from this session was heard, by the wall
/// clock — `None` for a session that has sent none, whose state the registry
/// drives. With hooks, the registry speaks only where it is news: a status it
/// wrote (`statusUpdatedAt`) after that hook. One written before is what the
/// hooks have since told better — the `busy` a turn's start left in the file,
/// read by a scan a moment before the `Stop` and applied just after it,
/// re-opened the turn the `Stop` had closed, and nothing but the next hook
/// ever closed it again: an idle tab spinning for hours.
///
/// `agents`: the turn that ended left delegated work running
/// ([`ClaudeTab::agents`]). Claude Code's `busy` counts that work as the
/// session working, so while it runs `busy` is no evidence of a new turn.
fn merge_registry(
    current: ClaudeState,
    heard: Option<u64>,
    agents: bool,
    live: &giverny_claude::registry::SessionEntry,
) -> ClaudeState {
    let written = live.status_updated_at;
    // Written after the last hook. A file that does not date its status
    // (older Claude Code) is taken at its word, as it always was.
    let news = heard.is_none_or(|heard| written == 0 || written > heard);
    // Working is evidence Claude is running again, so it clears a stale
    // flag — even under hook authority. A declined permission prompt emits
    // no hook to clear the one it raised, and a turn that ended before this
    // one left a tick behind that "finished" no longer describes. Not while
    // its subagents run on: that `busy` is theirs, and the tick for the turn
    // they outlived stays. Not a `busy` older than the hook that raised the
    // flag, either: that is the turn the flag ended.
    if live.busy()
        && news
        && !agents
        && matches!(current, ClaudeState::NeedsYou | ClaudeState::DoneUnseen)
    {
        return ClaudeState::Busy;
    }
    if let Some(heard) = heard {
        // Hooks are authoritative for a session that emits them: they mark
        // the end of a turn exactly, and the registry must not re-open one it
        // closed. The one thing they can miss is that end: a `Stop` lost (the
        // machine slept as it fired), or a turn interrupted, which sends
        // none. The session back at its prompt since the last hook ends the
        // turn the hooks still think runs. The timestamp must say so: an
        // undated `idle` may be the one before the turn began.
        let at_prompt = live.status == "idle" || live.background_shell();
        if current == ClaudeState::Busy && at_prompt && written > heard {
            return ClaudeState::DoneUnseen;
        }
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

/// Now, by the wall clock, in milliseconds since the epoch: what Claude Code
/// dates a registry status by.
fn now_ms() -> u64 {
    u64::try_from(jiff::Timestamp::now().as_millisecond()).unwrap_or(0)
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
        wake: impl Fn() + Send + 'static,
    ) -> (Self, Vec<RelayMsg>) {
        let profiles = profiles::discover(extra_dirs);
        // Unix: a socket for instant delivery. Elsewhere (or if binding
        // fails): poll the spool file the relay always falls back to.
        #[cfg(unix)]
        let (raise_tx, raise_rx) = crossbeam_channel::unbounded();
        #[cfg(unix)]
        let listener = hooks::spawn_listener(spool, raise_tx, wake);
        #[cfg(not(unix))]
        let listener = hooks::spawn_spool_watcher(spool, wake);
        let mut relay_holder = None;
        let (hook_rx, spooled) = match listener {
            Ok((rx, spooled)) => (Some(rx), spooled),
            Err(err) => {
                relay_holder = err.downcast_ref::<hooks::Held>().map(|held| held.holder);
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
            // Reads nothing from Giverny's config, so a config that does not
            // parse is no reason to skip it.
            Self::adopt_statusline_where_hooked(&profiles);
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
            relay_holder,
            #[cfg(unix)]
            raise_rx,
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
            #[cfg(unix)]
            last_link_check: Instant::now(),
            predate_hooks: HashSet::new(),
            prompts_asked: HashMap::new(),
            reads_asked: 0,
            hooks_drained: Instant::now(),
            pushes: HashMap::new(),
            listening: Instant::now(),
            prompts_found: crossbeam_channel::unbounded(),
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
            // An entry of ours from before `refreshInterval` was written gains
            // it here, so existing installs see the cold-cache warning too.
            if !hooks::statusline_installed_in(&settings)
                || hooks::needs_path_refresh(&settings)
                || hooks::statusline_lacks_refresh(&settings)
            {
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
        if msg.provider == "codex" {
            effects.codex.push(msg.clone());
            return;
        }
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
        entry.heard_at_ms = now_ms();
        if account.is_some() {
            entry.account = account;
        }
        let is_active = active == Some(tab_id);

        match msg.hook_event() {
            // A compaction is the same session carrying on: the turn it
            // interrupted, or the wave of work still running at the prompt
            // it was asked from, goes on after it.
            Some("SessionStart")
                if msg.event.get("source").and_then(|v| v.as_str()) == Some("compact") =>
            {
                if let Some(was) = entry.before_compact.take() {
                    entry.state = was;
                }
                entry.set_session(
                    msg.session_id().map(str::to_string),
                    &mut self.prompts_asked,
                );
                effects
                    .captured
                    .push((tab_id, msg.session_id().map(str::to_string), config_dir));
            }
            Some("SessionStart") => {
                entry.state = ClaudeState::Idle;
                entry.agents = false;
                entry.before_compact = None;
                entry.set_session(
                    msg.session_id().map(str::to_string),
                    &mut self.prompts_asked,
                );
                effects
                    .captured
                    .push((tab_id, msg.session_id().map(str::to_string), config_dir));
            }
            Some("UserPromptSubmit") => {
                entry.state = ClaudeState::Busy;
                // A compaction cancelled with Esc says nothing more.
                entry.before_compact = None;
                // A turn is running: whatever it leaves behind, its `Stop`
                // says.
                entry.agents = false;
                // The hook fires for every turn, also the ones Claude Code or
                // another agent sends (a finished background task, a
                // subagent's hand-back, another session's message): those
                // are work, not prompts. Its payload names no sender, so they
                // are known by their text.
                // A paste is spelled as the transcript has it once its tags
                // are off, so a read of it meets this one.
                if let Some(prompt) = msg
                    .prompt()
                    .map(registry::unwrap_pastes)
                    .map(|p| p.trim().to_string())
                    .filter(|p| !p.is_empty() && !registry::is_injected_prompt(p))
                {
                    entry.prompts.push(prompt);
                    entry.heard.push(Heard {
                        after: self.hooks_drained,
                        by: Instant::now(),
                    });
                    entry.cap_prompts();
                }
            }
            // A tool call is work happening now, whoever asked for it. It is
            // what tells a tab apart from the turn that ended before it: a
            // session carrying on after a permission was granted, or an agent
            // continuing by itself, emits nothing else.
            // Except a subagent's once the turn has ended with it running:
            // it arrives under this session with its `agent_id`, and is the
            // delegated work the dots already show, not the session busy.
            Some("PostToolUse") if entry.agents && msg.agent_id().is_some() => {}
            Some("PostToolUse") => {
                entry.state = ClaudeState::Busy;
                entry.agents = false;
            }
            // A subagent compacting its own context is its work, as its
            // tool calls are.
            Some("PreCompact") if msg.agent_id().is_some() => {}
            // Compacting is work, and nothing else says so: `/compact` sends
            // no `UserPromptSubmit`.
            Some("PreCompact") => {
                entry.before_compact.get_or_insert(entry.state);
                entry.state = ClaudeState::Busy;
            }
            Some("Stop") => {
                entry.before_compact = None;
                entry.state = if is_active {
                    ClaudeState::Idle
                } else {
                    ClaudeState::DoneUnseen
                };
                // The turn is over; are its subagents? The hook says so
                // exactly as the turn ends, where the registry's `busy`
                // cannot yet tell them from the turn itself.
                entry.agents = msg.delegated_work_running();
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
                        entry.before_compact = None;
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
                entry.agents = false;
                entry.before_compact = None;
                entry.set_session(None, &mut self.prompts_asked);
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

        // Hook stream first (crisp transitions). A message this drain finds
        // was not there for the last one, so reached Giverny after it began.
        let drained = Instant::now();
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
        self.hooks_drained = drained;

        self.apply_found_prompts();

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
                    self.merge_scan(shell_pids, active, &mut effects);
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

    /// Background job `id`, if BACKGROUND lists it.
    pub fn job(&self, id: &str) -> Option<&Job> {
        self.jobs.iter().find(|j| j.id == id)
    }

    /// Fold the last scan into per-tab state.
    fn merge_scan(
        &mut self,
        shell_pids: &HashMap<TabId, u32>,
        active: Option<TabId>,
        effects: &mut WatchEffects,
    ) {
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
        // One session per tab. Two can sit under one tab's shell (a `claude`
        // started from a `!` command in another), and taking each in turn
        // switched the tab's session twice a scan: its prompts cleared and
        // both transcripts read again every second. The tab holds the newer
        // one, the one in front, as its SessionStart hook says; the older
        // one comes back when it ends.
        let mut shown: Vec<(TabId, registry::LiveSession)> = Vec::new();
        for live in self.scanned.live.clone() {
            let Some(tab_id) = shell_pids
                .iter()
                .find(|(_, shell)| registry::has_ancestor(live.entry.pid, **shell))
                .map(|(id, _)| *id)
                .or_else(|| by_session.get(&live.entry.session_id).copied())
            else {
                continue;
            };
            let held = self.tabs.get(&tab_id).and_then(|t| t.session_id.as_deref());
            let rank = |s: &registry::LiveSession| {
                (
                    s.entry.started_at_ms,
                    held == Some(s.entry.session_id.as_str()),
                )
            };
            match shown.iter_mut().find(|(id, _)| *id == tab_id) {
                Some((_, kept)) if rank(&live) > rank(kept) => *kept = live,
                Some(_) => {}
                None => shown.push((tab_id, live)),
            }
        }
        let mut ask_prompts: Vec<(String, PathBuf, u64)> = Vec::new();
        {
            for (tab_id, live) in shown {
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
                entry.set_session(Some(live.entry.session_id.clone()), &mut self.prompts_asked);
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
                let heard = entry.last_hook.map(|_| entry.heard_at_ms);
                let hooks_own = heard.is_some();
                let was = entry.state;
                entry.state = merge_registry(entry.state, heard, entry.agents, &live.entry);
                // What the registry ended was a compaction, not a turn: the
                // tab goes back to what it was, as the compaction's own
                // `SessionStart` would have it — no tick for a `/compact`.
                if entry.state == ClaudeState::DoneUnseen
                    && was == ClaudeState::Busy
                    && let Some(before) = entry.before_compact.take()
                {
                    entry.state = before;
                }
                // A turn the registry ended in the tab you are looking at is
                // seen already, as a `Stop` there would have it.
                if entry.state == ClaudeState::DoneUnseen
                    && was != ClaudeState::DoneUnseen
                    && active == Some(tab_id)
                {
                    entry.state = ClaudeState::Idle;
                }
                // Delegated work the last `Stop` reported runs for as long as
                // the registry says `busy`; the first scan that says
                // otherwise is its end, and the dots go with it.
                // A session without hooks never sets it: its
                // `busy` is a spinner, as it cannot tell the two apart.
                entry.agents &= hooks_own && live.entry.busy();
                // A session new to the tab has its earlier turns read back
                // from the transcript, once: adopted mid-way, resumed after a
                // restart, or resumed with a prompt on the command line
                // (`claude -c "go on"`), whose hook is heard before this scan
                // and is no reason to skip the read. A session with no hooks
                // at all never says what was asked: each turn it starts is
                // read again.
                let sid = &live.entry.session_id;
                let new_turn =
                    !hooks_own && was != ClaudeState::Busy && entry.state == ClaudeState::Busy;
                if new_turn {
                    self.prompts_asked.remove(sid);
                }
                if !self.prompts_asked.contains_key(sid) {
                    self.reads_asked += 1;
                    self.prompts_asked.insert(sid.clone(), self.reads_asked);
                    ask_prompts.push((sid.clone(), live.config_dir.clone(), self.reads_asked));
                }
            }
            // Sessions gone from the registry: clear unless hooks spoke recently.
            for tab in self.tabs.values_mut() {
                let hook_recent = tab
                    .last_hook
                    .is_some_and(|t| t.elapsed() < Duration::from_secs(5));
                if !tab.seen_in_scan && !hook_recent && tab.state != ClaudeState::DoneUnseen {
                    tab.state = ClaudeState::None;
                    tab.background = false;
                    tab.agents = false;
                    tab.session_name = None;
                    // The session is gone — its hook evidence goes with it, so
                    // a future claude (with or without hooks) starts fresh.
                    tab.last_hook = None;
                }
            }
        }
        self.read_prompts(ask_prompts);
    }

    /// A statusline push: official `rate_limits` for one account.
    ///
    /// Every running `claude` pushes what *its own* last request was answered
    /// with, so the account's sessions disagree whenever one has been idle —
    /// and after an early reset they disagree by the whole window.
    /// Each session's push is kept, dated by when its numbers
    /// were seen to change, and the account shows the freshest.
    fn apply_statusline(&mut self, msg: &RelayMsg) {
        let window =
            |key: &str| -> Option<&serde_json::Value> { msg.event.get("rate_limits")?.get(key) };
        let pct = |key: &str| -> Option<f64> { window(key)?.get("used_percentage")?.as_f64() };
        let resets = |key: &str| -> Option<jiff::Timestamp> { reset_time(window(key)?) };
        let mut live = LiveUsage {
            at: Instant::now(),
            five_hour: pct("five_hour"),
            seven_day: pct("seven_day"),
            five_hour_resets: resets("five_hour"),
            seven_day_resets: resets("seven_day"),
            since: Sample::heard(),
        };
        if live.five_hour.is_none() && live.seven_day.is_none() {
            return;
        }
        // Attribute to the account: explicit config dir, else the default profile.
        let dir = self
            .canonical_dir(msg.config_dir.as_deref())
            .or_else(|| dirs::home_dir().map(|h| h.join(".claude")));
        let Some(dir) = dir else { return };
        let Some(acc) = self
            .accounts
            .iter_mut()
            .find(|a| a.profile.config_dir == dir)
        else {
            return;
        };
        let session = msg
            .event
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned();
        if !session.is_empty() {
            let now = jiff::Timestamp::now();
            live.since = match self.pushes.get(&(dir.clone(), session.clone())) {
                Some(prev) if prev.same_numbers(&live) => prev.since,
                Some(_) => Sample {
                    at: Some(now),
                    how: Taken::Seen,
                },
                // A session already running when Giverny started may be
                // holding numbers from hours ago; one that turns up later
                // has just had its first answer.
                None => Sample {
                    at: Some(now),
                    how: if self.listening.elapsed() < HEARD_AT_START {
                        Taken::Heard
                    } else {
                        Taken::Seen
                    },
                },
            };
        }
        self.pushes.insert((dir.clone(), session), live);
        self.pushes.retain(|_, p| p.at.elapsed() < PUSH_FORGOTTEN);
        // The freshest numbers, said to be as live as the latest push.
        let mine = self.pushes.iter().filter(|((d, _), _)| *d == dir);
        let last_heard = mine.clone().map(|(_, p)| p.at).max();
        acc.live = mine
            .map(|(_, p)| p)
            .max_by_key(|p| (p.since.how == Taken::Seen, p.since.at, p.at))
            .map(|p| LiveUsage {
                at: last_heard.unwrap_or(p.at),
                ..p.clone()
            });
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

    /// Keep each window's high-water mark up to date ([`Peak::takes`]).
    ///
    /// The cache is heard as well as the push it is usually behind: after an
    /// early reset, a fresh cache may be the first to know.
    fn remember_peaks(&mut self) {
        let now = jiff::Timestamp::now();
        for acc in &mut self.accounts {
            let Some(usage) = &acc.usage else { continue };
            for limit in &usage.limits {
                let read = Self::sampled(acc, limit, now);
                let cache = Self::cached(acc, limit, now);
                for (read, sample) in cache
                    .into_iter()
                    .chain([(read, Self::sample_of(acc, &read))])
                {
                    let peak = acc.peak.entry(limit.kind.clone()).or_insert(Peak {
                        percent: read.percent,
                        resets: read.resets,
                        since: sample,
                    });
                    if !peak.takes(&read, sample, now) {
                        continue;
                    }
                    let climb =
                        peak.place(&read, now) == Window::Same && read.percent >= peak.percent;
                    *peak = Peak {
                        percent: read.percent,
                        // A climb is the mark's own window, however spelt.
                        resets: if climb {
                            peak.resets.or(read.resets)
                        } else {
                            read.resets
                        },
                        // A climb on a number of unknown age keeps the date
                        // the mark had: it says nothing about when.
                        since: if climb && !sample.dated() && peak.since.dated() {
                            peak.since
                        } else {
                            sample
                        },
                    };
                }
            }
        }
    }

    /// The cache's own reading of a window it still describes.
    fn cached(
        acc: &AccountPanel,
        limit: &giverny_claude::usage::LimitEntry,
        now: jiff::Timestamp,
    ) -> Option<(Reading, Sample)> {
        let usage = acc.usage.as_ref()?;
        let resets = limit.resets_at_ts().filter(|at| *at > now)?;
        let percent = limit.effective_percent(now);
        Some((
            Reading {
                percent,
                live: false,
                critical: percent >= 95.0 || limit.critical(),
                resets: Some(resets),
            },
            Sample {
                at: jiff::Timestamp::from_millisecond(usage.fetched_at_ms as i64).ok(),
                how: Taken::Cache,
            },
        ))
    }

    /// When the reading [`ClaudeWatch::sampled`] gave was taken.
    fn sample_of(acc: &AccountPanel, read: &Reading) -> Sample {
        match (&acc.live, &acc.usage) {
            (Some(live), _) if read.live => live.since,
            (_, Some(usage)) => Sample {
                at: jiff::Timestamp::from_millisecond(usage.fetched_at_ms as i64).ok(),
                how: Taken::Cache,
            },
            _ => Sample::heard(),
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

    /// What one bar shows: the freshest sample, unless the window's mark
    /// stands against it ([`Peak::takes`]) — a number behind the mark, of
    /// unknown age, or from an older window.
    pub fn reading(
        acc: &AccountPanel,
        limit: &giverny_claude::usage::LimitEntry,
        now: jiff::Timestamp,
    ) -> Reading {
        let mut read = Self::sampled(acc, limit, now);
        if let Some(peak) = acc.peak.get(&limit.kind)
            && !peak.takes(&read, Self::sample_of(acc, &read), now)
        {
            // Below the mark in its own window, the reading's severity still
            // counts; a stale or older-window one says nothing at all.
            let behind = peak.place(&read, now) == Window::Same && read.percent <= peak.percent;
            read.critical = peak.percent >= 95.0 || (behind && read.critical);
            read.percent = peak.percent;
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

    /// The prompt to pin above this tab: its last one, while Claude runs.
    pub fn prompt_of(&self, tab: TabId) -> Option<&str> {
        self.prompts_of(tab)?.last().map(String::as_str)
    }

    /// This tab's prompts, oldest first, while Claude runs and has had one.
    pub fn prompts_of(&self, tab: TabId) -> Option<&[String]> {
        let tab = self.tabs.get(&tab)?;
        if tab.state == ClaudeState::None || tab.prompts.is_empty() {
            return None;
        }
        Some(&tab.prompts)
    }

    /// Read these sessions' last prompts from their transcripts, off the UI
    /// thread: for an account inside WSL the file is across a share.
    fn read_prompts(&self, sessions: Vec<(String, PathBuf, u64)>) {
        if sessions.is_empty() {
            return;
        }
        let tx = self.prompts_found.0.clone();
        let _ = std::thread::Builder::new()
            .name("giverny last prompt".into())
            .spawn(move || {
                for (session, dir, read) in sessions {
                    let started = Instant::now();
                    let prompts = registry::find_transcript(&dir, &session)
                        .map(|path| registry::prompt_history(&path))
                        .unwrap_or_default();
                    if !prompts.is_empty() {
                        let _ = tx.send(FoundPrompt {
                            session,
                            read,
                            started,
                            done: Instant::now(),
                            prompts,
                        });
                    }
                }
            });
    }

    /// Prompt histories read back from transcripts. A tab no hook speaks for
    /// takes the read as it is; one a hook has told of prompts since keeps
    /// those, after the ones read. A read that is not its session's latest
    /// is stale and dropped.
    ///
    /// Whether a heard prompt is in the read goes by when it was heard (see
    /// [`SEAM`]): one heard well after the read is a new turn however it
    /// reads, and one heard well before it is met by its text. One heard
    /// around the read is met by its text for now, and the transcript is
    /// read again, by when the read is sure: a repeated "yes" written just
    /// after the read is not the earlier "yes", and a prompt whose hook
    /// reached Giverny late is not added twice.
    fn apply_found_prompts(&mut self) {
        let found: Vec<FoundPrompt> = self.prompts_found.1.try_iter().collect();
        for found in found {
            if self.prompts_asked.get(&found.session) != Some(&found.read) {
                continue;
            }
            let mut unsure = false;
            for tab in self.tabs.values_mut() {
                if tab.session_id.as_deref() != Some(found.session.as_str()) {
                    continue;
                }
                if tab.last_hook.is_none() {
                    tab.prompts = found.prompts.clone();
                    tab.heard.clear();
                } else {
                    let heard = &tab.prompts[tab.prompts.len() - tab.heard.len()..];
                    let read = tab
                        .heard
                        .iter()
                        .take_while(|h| h.by + SEAM <= found.started)
                        .count();
                    let fresh = tab
                        .heard
                        .iter()
                        .filter(|h| h.after >= found.done + SEAM)
                        .count();
                    tab.prompts = merge_history(&found.prompts, heard, fresh);
                    unsure |= read + fresh < tab.heard.len();
                    tab.heard.drain(..read);
                }
                tab.cap_prompts();
            }
            if unsure {
                self.prompts_asked.remove(&found.session);
            }
        }
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
            relay_holder: None,
            #[cfg(unix)]
            raise_rx: crossbeam_channel::never(),
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
            #[cfg(unix)]
            last_link_check: Instant::now(),
            predate_hooks: HashSet::new(),
            prompts_asked: HashMap::new(),
            reads_asked: 0,
            hooks_drained: Instant::now(),
            pushes: HashMap::new(),
            listening: Instant::now(),
            prompts_found: crossbeam_channel::unbounded(),
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

    /// The Giverny that has the relay instead of this one, if a live one does.
    pub fn relay_holder(&self) -> Option<hooks::Holder> {
        self.relay_holder
    }

    /// Second launches waiting for this window to come forward and say so.
    #[cfg(unix)]
    pub fn raise_requests(&self) -> Vec<std::os::unix::net::UnixStream> {
        self.raise_rx.try_iter().collect()
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

    /// A `/compact` is work the rail shows, and once it is done the tab is
    /// what it was: idle, or waving for work still running. One Claude Code
    /// starts mid-turn keeps the turn busy.
    #[test]
    fn a_compaction_breathes_and_gives_the_tab_back() {
        let mut w = ClaudeWatch::for_tests();
        let compact = || hook("SessionStart", r#","source":"compact""#);
        let waves = |w: &ClaudeWatch| w.tabs[&TAB].working_in_background();
        feed(&mut w, &hook("SessionStart", ""), Some(TAB));
        feed(&mut w, &hook("UserPromptSubmit", ""), Some(TAB));
        let agent = r#","background_tasks":[{"id":"a1","type":"subagent","status":"running"}]"#;
        feed(&mut w, &hook("Stop", agent), Some(TAB));
        assert_eq!(w.state_of(TAB), ClaudeState::Idle);
        assert!(waves(&w));

        feed(
            &mut w,
            &hook("PreCompact", r#","trigger":"manual""#),
            Some(TAB),
        );
        assert_eq!(w.state_of(TAB), ClaudeState::Busy, "compacting");
        feed(&mut w, &compact(), Some(TAB));
        assert_eq!(w.state_of(TAB), ClaudeState::Idle, "back at the prompt");
        assert!(waves(&w), "its subagent still runs");

        // Auto-compacted mid-turn: the turn goes on.
        feed(&mut w, &hook("UserPromptSubmit", ""), Some(TAB));
        feed(
            &mut w,
            &hook("PreCompact", r#","trigger":"auto""#),
            Some(TAB),
        );
        feed(&mut w, &compact(), Some(TAB));
        assert_eq!(w.state_of(TAB), ClaudeState::Busy, "still the turn");
        // Hooks installed before `PreCompact` was relayed: kept, not idled.
        feed(&mut w, &compact(), Some(TAB));
        assert_eq!(w.state_of(TAB), ClaudeState::Busy);
        feed(&mut w, &hook("Stop", ""), Some(TAB));
        assert_eq!(w.state_of(TAB), ClaudeState::Idle);

        // A subagent compacting its own context is not the session's.
        feed(
            &mut w,
            &hook("PreCompact", r#","trigger":"auto","agent_id":"a1""#),
            Some(TAB),
        );
        assert_eq!(w.state_of(TAB), ClaudeState::Idle);

        // Cancelled with Esc: no `SessionStart`, and the next turn's
        // compaction is not handed the old prompt's state.
        feed(
            &mut w,
            &hook("PreCompact", r#","trigger":"manual""#),
            Some(TAB),
        );
        feed(&mut w, &hook("UserPromptSubmit", ""), Some(TAB));
        feed(
            &mut w,
            &hook("PreCompact", r#","trigger":"auto""#),
            Some(TAB),
        );
        feed(&mut w, &compact(), Some(TAB));
        assert_eq!(w.state_of(TAB), ClaudeState::Busy);
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

    /// When the fixtures' last hook was heard, by the wall clock.
    const HEARD: Option<u64> = Some(1_000_000);
    /// A session no hook has been heard from.
    const NO_HOOKS: Option<u64> = Option::None;

    /// A registry entry whose status was written at `at` (ms).
    fn dated(status: &str, at: u64) -> giverny_claude::registry::SessionEntry {
        let mut entry = session(status);
        entry.status_updated_at = at;
        entry
    }

    #[test]
    fn the_last_prompt_is_kept_per_tab() {
        let mut w = ClaudeWatch::for_tests();
        let other = |event: &str, extra: &str| {
            msg(&format!(
                r#"{{"tab_id":"giverny-8","config_dir":"/home/u/.claude-work",
                    "event":{{"hook_event_name":"{event}","session_id":"s-2"{extra}}}}}"#
            ))
        };
        feed(&mut w, &hook("SessionStart", ""), Some(TAB));
        assert_eq!(w.prompt_of(TAB), None, "nothing asked yet");

        feed(
            &mut w,
            &hook(
                "UserPromptSubmit",
                r#","prompt":"fix the build\nthen test""#,
            ),
            Some(TAB),
        );
        feed(
            &mut w,
            &other("UserPromptSubmit", r#","prompt":"other account""#),
            Some(TAB),
        );
        assert_eq!(w.prompt_of(TAB), Some("fix the build\nthen test"));
        assert_eq!(w.prompt_of(TabId(8)), Some("other account"));

        // Still shown once the turn is over; replaced by the next prompt.
        feed(&mut w, &hook("Stop", ""), Some(TAB));
        assert_eq!(w.prompt_of(TAB), Some("fix the build\nthen test"));
        feed(
            &mut w,
            &hook("UserPromptSubmit", r#","prompt":"  and lint  ""#),
            Some(TAB),
        );
        assert_eq!(w.prompt_of(TAB), Some("and lint"));
        // A payload without one leaves the last one standing.
        feed(&mut w, &hook("UserPromptSubmit", ""), Some(TAB));
        assert_eq!(w.prompt_of(TAB), Some("and lint"));

        // Claude gone: no bar, and the next session starts with none.
        feed(&mut w, &hook("SessionEnd", ""), Some(TAB));
        assert_eq!(w.prompt_of(TAB), None);
        feed(&mut w, &hook("SessionStart", ""), Some(TAB));
        assert_eq!(w.prompt_of(TAB), None);
        assert_eq!(w.prompt_of(TabId(8)), Some("other account"));
    }

    #[test]
    fn turns_nobody_typed_never_reach_the_prompt_bar() {
        let mut w = ClaudeWatch::for_tests();
        feed(&mut w, &hook("SessionStart", ""), Some(TAB));
        let submit = |p: &str| {
            hook(
                "UserPromptSubmit",
                &format!(r#","prompt":{}"#, serde_json::to_string(p).unwrap()),
            )
        };
        feed(&mut w, &submit("start the two workers"), Some(TAB));
        feed(&mut w, &hook("Stop", ""), Some(TAB));
        // What the hook was sent, live (Claude Code 2.1.295), for a turn it
        // took while the user typed nothing: a background command finishing,
        // a subagent handing back.
        for injected in [
            "<task-notification>\n<task-id>bdjbssyyc</task-id>\n\
             <tool-use-id>toolu_019raXNAx3pWWeWBpxr7QJNu</tool-use-id>\n\
             <status>completed</status>\n\
             <summary>Background command \"Sleep 8 seconds\" completed (exit code 0)</summary>\n\
             </task-notification>",
            "<agent-message from=\"ad09f1c404af364a1\">\n[Subagent hand-back] The text \
             below is the final report of a subagent this session delegated to.\n  hi\n\
             </agent-message>",
            "3 background agents were stopped by the user: \"giverny#21 drop pane total row\"",
            "Your claude.ai usage limit has reset. Continue the task you were working on.",
            "The pass-spike plugin sent a message:\npass-spike: worker t3 asks for a decision.",
            // Another session's message, as Claude Code 2.1.296
            // queued it and as it rendered it into the turn.
            include_str!("../../claude/testdata/peer-message-prompt.txt"),
            include_str!("../../claude/testdata/peer-message-rendered.txt"),
            // The hook can't tell these from typed ones; the transcript can,
            // and a later read brings them back.
            "<channel>\n<title>Giverny releases</title>\n</channel>\n\nwhy is this feed empty?",
            "<system-reminder id=\"7abcbf89088787d4\">\nhi\n</system-reminder id=\"7abcbf89088787d4\">",
            "The coordinator sent a message: how does the prompt bar know that's not me?",
        ] {
            feed(&mut w, &submit(injected), Some(TAB));
            assert_eq!(w.state_of(TAB), ClaudeState::Busy, "still work: {injected}");
            assert_eq!(
                w.prompt_of(TAB),
                Some("start the two workers"),
                "{injected}"
            );
            feed(&mut w, &hook("Stop", ""), Some(TAB));
        }
        assert_eq!(w.prompts_of(TAB).unwrap(), ["start the two workers"]);
        // A message typed while the turn ran fires the hook too, and is one.
        feed(&mut w, &submit("queued typed message"), Some(TAB));
        assert_eq!(w.prompt_of(TAB), Some("queued typed message"));
    }

    #[test]
    fn every_prompt_is_kept_in_order() {
        let mut w = ClaudeWatch::for_tests();
        feed(&mut w, &hook("SessionStart", ""), Some(TAB));
        for p in ["one", "two", "three"] {
            feed(
                &mut w,
                &hook("UserPromptSubmit", &format!(r#","prompt":"{p}""#)),
                Some(TAB),
            );
        }
        assert_eq!(w.prompts_of(TAB).unwrap(), ["one", "two", "three"]);
        assert_eq!(w.prompt_of(TAB), Some("three"));

        let mut long: Vec<String> = (0..PROMPT_HISTORY + 5).map(|i| i.to_string()).collect();
        cap_history(&mut long);
        assert_eq!(long.len(), PROMPT_HISTORY);
        assert_eq!(long[0], "5", "the oldest go first");
    }

    #[test]
    fn a_read_history_and_the_hooks_meet_once() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        // The read already has the prompt a hook told of.
        assert_eq!(
            merge_history(&s(&["a", "b", "c"]), &s(&["c"]), 0),
            s(&["a", "b", "c"])
        );
        // It was read before the hook's prompt reached the transcript.
        assert_eq!(
            merge_history(&s(&["a", "b"]), &s(&["c"]), 0),
            s(&["a", "b", "c"])
        );
        assert_eq!(
            merge_history(&s(&["a", "b"]), &s(&["b", "c"]), 0),
            s(&["a", "b", "c"])
        );
        assert_eq!(merge_history(&s(&[]), &s(&["c"]), 0), s(&["c"]));
        // Spelled with other line breaks, it is still the one prompt.
        assert_eq!(
            merge_history(&s(&["a", "b\nc"]), &s(&["b c"]), 0),
            s(&["a", "b\nc"])
        );
    }

    #[test]
    fn a_repeated_prompt_heard_after_the_read_is_a_second_turn() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            merge_history(&s(&["a", "yes"]), &s(&["yes"]), 1),
            s(&["a", "yes", "yes"])
        );
        assert_eq!(
            merge_history(&s(&["a", "yes"]), &s(&["yes", "yes"]), 1),
            s(&["a", "yes", "yes"])
        );

        // Through the watcher: a hook's "yes" from well after the transcript
        // was read is a second "yes"; one from well before it is the read's
        // own. Neither asks for another read.
        for (after, want) in [(true, &["a", "yes", "yes"][..]), (false, &["a", "yes"][..])] {
            let mut w = ClaudeWatch::for_tests();
            feed(&mut w, &hook("SessionStart", ""), Some(TAB));
            w.prompts_asked.insert("s-1".into(), 1);
            let started = Instant::now() + 2 * SEAM;
            let done = started + Duration::from_millis(5);
            if after {
                w.hooks_drained = done + SEAM;
            }
            feed(
                &mut w,
                &hook("UserPromptSubmit", r#","prompt":"yes""#),
                Some(TAB),
            );
            w.prompts_found
                .0
                .send(FoundPrompt {
                    session: "s-1".into(),
                    read: 1,
                    started,
                    done,
                    prompts: s(&["a", "yes"]),
                })
                .unwrap();
            w.apply_found_prompts();
            assert_eq!(w.prompts_of(TAB), Some(&s(want)[..]), "after: {after}");
            assert!(w.prompts_asked.contains_key("s-1"), "after: {after}");
        }
    }

    /// A prompt heard around a read, while it ran or soon after it, may or
    /// may not be in it: the transcript is read again, by when it is sure.
    #[test]
    fn a_prompt_heard_around_the_read_is_read_again() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let read = |w: &mut ClaudeWatch, read: u64, started: Instant, prompts: &[&str]| {
            w.prompts_asked.insert("s-1".into(), read);
            w.prompts_found
                .0
                .send(FoundPrompt {
                    session: "s-1".into(),
                    read,
                    started,
                    done: started + Duration::from_millis(5),
                    prompts: s(prompts),
                })
                .unwrap();
            w.apply_found_prompts();
        };
        // The "yes" heard while the read ran: the same as the turn before
        // it, but written after the read's snapshot. The first read can't
        // tell it from the earlier one; the one after it can.
        let mut w = ClaudeWatch::for_tests();
        feed(&mut w, &hook("SessionStart", ""), Some(TAB));
        let started = Instant::now();
        feed(
            &mut w,
            &hook("UserPromptSubmit", r#","prompt":"yes""#),
            Some(TAB),
        );
        read(&mut w, 1, started, &["a", "yes"]);
        assert_eq!(w.prompts_of(TAB).unwrap(), ["a", "yes"]);
        assert!(!w.prompts_asked.contains_key("s-1"), "read again");
        read(&mut w, 2, Instant::now() + 2 * SEAM, &["a", "yes", "yes"]);
        assert_eq!(w.prompts_of(TAB).unwrap(), ["a", "yes", "yes"]);
        assert!(w.prompts_asked.contains_key("s-1"), "and that is the last");

        // A prompt the read already has, heard just after it finished: a
        // hook that waited out the Windows spool poll. Added once.
        let mut w = ClaudeWatch::for_tests();
        feed(&mut w, &hook("SessionStart", ""), Some(TAB));
        let started = Instant::now();
        w.hooks_drained = started + Duration::from_millis(400);
        feed(
            &mut w,
            &hook("UserPromptSubmit", r#","prompt":"go""#),
            Some(TAB),
        );
        read(&mut w, 1, started, &["a", "go"]);
        assert_eq!(w.prompts_of(TAB).unwrap(), ["a", "go"]);
        assert!(!w.prompts_asked.contains_key("s-1"), "read again");
        read(&mut w, 2, Instant::now() + 2 * SEAM, &["a", "go"]);
        assert_eq!(w.prompts_of(TAB).unwrap(), ["a", "go"]);
        assert!(w.prompts_asked.contains_key("s-1"), "and that is the last");
    }

    #[test]
    fn a_pasted_prompt_from_the_hook_loses_its_tags() {
        let mut w = ClaudeWatch::for_tests();
        feed(&mut w, &hook("SessionStart", ""), Some(TAB));
        feed(
            &mut w,
            &hook(
                "UserPromptSubmit",
                r#","prompt":"<pasted_content id=\"1\">\nthe log\n</pasted_content id=\"1\">\nwhy""#,
            ),
            Some(TAB),
        );
        assert_eq!(w.prompt_of(TAB), Some("the log\nwhy"));
    }

    #[test]
    fn a_read_that_finishes_late_is_dropped() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let mut w = ClaudeWatch::for_tests();
        let tab = w.tabs.entry(TAB).or_default();
        tab.session_id = Some("s-1".into());
        tab.state = ClaudeState::Busy;
        // A tab without hooks asked twice, one turn apart; the first read
        // was the slow one.
        w.prompts_asked.insert("s-1".into(), 2);
        for (read, prompts) in [(2, s(&["a", "b"])), (1, s(&["a"]))] {
            w.prompts_found
                .0
                .send(FoundPrompt {
                    session: "s-1".into(),
                    read,
                    started: Instant::now(),
                    done: Instant::now(),
                    prompts,
                })
                .unwrap();
        }
        w.apply_found_prompts();
        assert_eq!(w.prompt_of(TAB), Some("b"));
    }

    /// A session resumed in the tab it left reads its prompts back again:
    /// `claude --resume` after an exit, `/resume` back after a `/clear`.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_session_resumed_in_the_same_tab_reads_its_prompts_again() {
        let dir =
            std::env::temp_dir().join(format!("giverny-resumed-prompts-{}", std::process::id()));
        let proj = dir.join("projects").join("-home-u-proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("s-1.jsonl"),
            concat!(
                r#"{"type":"user","message":{"content":"what changed"}}"#,
                "\n",
            ),
        )
        .unwrap();

        let mut w = ClaudeWatch::for_tests();
        let me = std::process::id();
        let mut live = session("idle");
        live.pid = me;
        w.scanned = ScanResult {
            live: vec![registry::LiveSession {
                entry: live,
                config_dir: dir.clone(),
            }],
        };
        let shells = HashMap::from([(TAB, me)]);
        let read_back = |w: &mut ClaudeWatch| {
            w.merge_scan(&shells, None, &mut WatchEffects::default());
            let deadline = Instant::now() + Duration::from_secs(5);
            while w.prompt_of(TAB).is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
                w.apply_found_prompts();
            }
            w.prompt_of(TAB).map(str::to_string)
        };
        let start = |id: &str| {
            msg(&format!(
                r#"{{"tab_id":"giverny-7","config_dir":null,
                "event":{{"hook_event_name":"SessionStart","session_id":"{id}"}}}}"#
            ))
        };
        feed(&mut w, &start("s-1"), Some(TAB));
        assert_eq!(read_back(&mut w).as_deref(), Some("what changed"));

        // Exit, then `claude --resume` the same session.
        feed(&mut w, &hook("SessionEnd", ""), Some(TAB));
        feed(&mut w, &start("s-1"), Some(TAB));
        assert_eq!(w.prompt_of(TAB), None);
        assert_eq!(read_back(&mut w).as_deref(), Some("what changed"));

        // `/clear`, then `/resume` back to it.
        feed(&mut w, &start("s-9"), Some(TAB));
        feed(&mut w, &start("s-1"), Some(TAB));
        assert_eq!(w.prompt_of(TAB), None);
        assert_eq!(read_back(&mut w).as_deref(), Some("what changed"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `claude -c "go on"`: the session's start and its first prompt are
    /// both heard before the next scan, and its earlier turns are still read
    /// back, ahead of the prompt, whether or not the transcript has it yet.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_resume_with_a_prompt_reads_the_earlier_turns_back() {
        let dir =
            std::env::temp_dir().join(format!("giverny-resume-prompt-{}", std::process::id()));
        let proj = dir.join("projects").join("-home-u-proj");
        std::fs::create_dir_all(&proj).unwrap();
        let turn = |p: &str| format!(r#"{{"type":"user","message":{{"content":"{p}"}}}}"#);
        for written in [false, true] {
            let mut lines = vec![turn("first"), turn("second")];
            if written {
                lines.push(turn("go on"));
            }
            std::fs::write(proj.join("s-1.jsonl"), lines.join("\n") + "\n").unwrap();

            let mut w = ClaudeWatch::for_tests();
            let me = std::process::id();
            let mut live = session("busy");
            live.pid = me;
            w.scanned = ScanResult {
                live: vec![registry::LiveSession {
                    entry: live,
                    config_dir: dir.clone(),
                }],
            };
            feed(&mut w, &hook("SessionStart", ""), Some(TAB));
            feed(
                &mut w,
                &hook("UserPromptSubmit", r#","prompt":"go on""#),
                Some(TAB),
            );
            w.merge_scan(
                &HashMap::from([(TAB, me)]),
                None,
                &mut WatchEffects::default(),
            );
            assert!(w.prompts_asked.contains_key("s-1"), "a read is asked");
            let deadline = Instant::now() + Duration::from_secs(5);
            while w.prompts_of(TAB).is_some_and(|p| p.len() < 3) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
                w.apply_found_prompts();
            }
            assert_eq!(
                w.prompts_of(TAB).unwrap(),
                ["first", "second", "go on"],
                "written: {written}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two sessions under one tab's shell, a `claude` started from a `!`
    /// command in another: the tab holds the newer one, and reads its
    /// transcript once, not on every scan.
    #[test]
    fn two_sessions_under_one_shell_do_not_take_turns() {
        let me = std::process::id();
        let live = |id: &str, started_at_ms: u64| {
            let mut entry = session("busy");
            entry.pid = me;
            entry.session_id = id.into();
            entry.started_at_ms = started_at_ms;
            registry::LiveSession {
                entry,
                config_dir: PathBuf::from("/nonexistent/giverny-two-sessions"),
            }
        };
        let mut w = ClaudeWatch::for_tests();
        w.scanned = ScanResult {
            live: vec![live("s-1", 1), live("s-2", 2)],
        };
        let shells = HashMap::from([(TAB, me)]);
        w.merge_scan(&shells, None, &mut WatchEffects::default());
        let read = w.prompts_asked.get("s-2").copied();
        assert!(read.is_some());
        for _ in 0..3 {
            let mut fx = WatchEffects::default();
            w.merge_scan(&shells, None, &mut fx);
            assert_eq!(w.tabs[&TAB].session_id.as_deref(), Some("s-2"));
            assert_eq!(w.prompts_asked.get("s-2").copied(), read, "not read again");
            assert!(!w.prompts_asked.contains_key("s-1"));
            assert!(fx.captured.is_empty(), "the resume target holds still");
        }

        // The newer one ends: the tab goes back to the older.
        w.scanned.live.retain(|s| s.entry.session_id == "s-1");
        w.merge_scan(&shells, None, &mut WatchEffects::default());
        assert_eq!(w.tabs[&TAB].session_id.as_deref(), Some("s-1"));

        // With no start times to go by, the tab keeps the one it holds.
        w.scanned.live = vec![live("s-2", 0), live("s-1", 0)];
        for _ in 0..3 {
            w.merge_scan(&shells, None, &mut WatchEffects::default());
            assert_eq!(w.tabs[&TAB].session_id.as_deref(), Some("s-1"));
        }
    }

    #[test]
    fn a_new_conversation_in_the_tab_drops_the_old_prompt() {
        let mut w = ClaudeWatch::for_tests();
        feed(&mut w, &hook("SessionStart", ""), Some(TAB));
        feed(
            &mut w,
            &hook("UserPromptSubmit", r#","prompt":"old""#),
            Some(TAB),
        );
        let resumed = msg(r#"{"tab_id":"giverny-7","config_dir":null,
            "event":{"hook_event_name":"SessionStart","session_id":"s-9"}}"#);
        feed(&mut w, &resumed, Some(TAB));
        assert_eq!(w.prompt_of(TAB), None);
    }

    /// A session no hook has reported a prompt for — adopted mid-way, or
    /// resumed after a restart — has it read back from its transcript.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_adopted_session_reads_its_prompt_from_the_transcript() {
        let dir =
            std::env::temp_dir().join(format!("giverny-adopted-transcript-{}", std::process::id()));
        let proj = dir.join("projects").join("-home-u-proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("s-1.jsonl"),
            concat!(
                r#"{"type":"user","message":{"content":"what changed\nsince friday"}}"#,
                "\n",
                r#"{"type":"last-prompt","lastPrompt":"what changed since friday"}"#,
                "\n",
            ),
        )
        .unwrap();

        let mut w = ClaudeWatch::for_tests();
        let me = std::process::id();
        let mut live = session("busy");
        live.pid = me;
        w.scanned = ScanResult {
            live: vec![registry::LiveSession {
                entry: live,
                config_dir: dir.clone(),
            }],
        };
        let shells = HashMap::from([(TAB, me)]);
        w.merge_scan(&shells, None, &mut WatchEffects::default());
        let deadline = Instant::now() + Duration::from_secs(5);
        while w.prompt_of(TAB).is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
            w.apply_found_prompts();
        }
        assert_eq!(w.prompt_of(TAB), Some("what changed\nsince friday"));

        // Read once per session, not on every scan.
        w.merge_scan(&shells, None, &mut WatchEffects::default());
        assert!(w.prompts_asked.contains_key("s-1"));

        // A hook's prompt is newer than the transcript's and is not replaced.
        feed(
            &mut w,
            &hook("UserPromptSubmit", r#","prompt":"from the hook""#),
            Some(TAB),
        );
        w.prompts_found
            .0
            .send(FoundPrompt {
                session: "s-1".into(),
                read: w.prompts_asked["s-1"],
                started: Instant::now(),
                done: Instant::now(),
                prompts: vec!["stale".into()],
            })
            .unwrap();
        w.apply_found_prompts();
        assert_eq!(w.prompt_of(TAB), Some("from the hook"));
        let _ = std::fs::remove_dir_all(&dir);
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
        assert_eq!(merge_registry(Idle, HEARD, false, &session("shell")), Idle);
        assert_eq!(merge_registry(Idle, HEARD, false, &session("busy")), Idle);
        assert_eq!(merge_registry(Busy, HEARD, false, &session("idle")), Busy);
        assert_eq!(
            merge_registry(DoneUnseen, HEARD, false, &session("idle")),
            DoneUnseen
        );
        // The one exception: a working session clears a stale attention flag,
        // because declining a prompt emits no hook to clear it.
        assert_eq!(
            merge_registry(NeedsYou, HEARD, false, &session("busy")),
            Busy
        );
        assert_eq!(
            merge_registry(NeedsYou, HEARD, false, &session("idle")),
            NeedsYou
        );
        // ...unless the turn's subagents are what keeps it `busy`: the tick
        // for the turn they outlived stays.
        assert_eq!(
            merge_registry(DoneUnseen, HEARD, true, &session("busy")),
            DoneUnseen
        );
        assert_eq!(merge_registry(Idle, HEARD, true, &session("busy")), Idle);
    }

    /// A dated registry status speaks under hook authority only where it is
    /// newer than the last hook: the `busy` a turn's start wrote does not
    /// re-open the turn its `Stop` closed, and an `idle` written after the
    /// last hook ends a turn whose `Stop` never came.
    #[test]
    fn a_registry_status_older_than_the_hooks_is_old_news() {
        use ClaudeState::*;
        let heard = HEARD.unwrap();
        let before = heard - 5_000;
        let after = heard + 100;
        // The stale `busy` a scan read just before the `Stop`.
        assert_eq!(
            merge_registry(DoneUnseen, HEARD, false, &dated("busy", before)),
            DoneUnseen
        );
        assert_eq!(
            merge_registry(NeedsYou, HEARD, false, &dated("busy", before)),
            NeedsYou
        );
        // A `busy` written since: Claude Code working again (a declined
        // prompt, a turn no hook announced).
        assert_eq!(
            merge_registry(NeedsYou, HEARD, false, &dated("busy", after)),
            Busy
        );
        assert_eq!(
            merge_registry(DoneUnseen, HEARD, false, &dated("busy", after)),
            Busy
        );
        // Back at its prompt after the last hook: the turn is over.
        assert_eq!(
            merge_registry(Busy, HEARD, false, &dated("idle", after)),
            DoneUnseen
        );
        assert_eq!(
            merge_registry(Busy, HEARD, false, &dated("shell", after)),
            DoneUnseen
        );
        // ...but not on the `idle` the turn began from, nor while it waits on
        // you or still works.
        assert_eq!(
            merge_registry(Busy, HEARD, false, &dated("idle", before)),
            Busy
        );
        assert_eq!(
            merge_registry(Busy, HEARD, false, &dated("waiting", after)),
            Busy
        );
        assert_eq!(
            merge_registry(Busy, HEARD, false, &dated("busy", after)),
            Busy
        );
        // A turn's end the hooks marked stays theirs.
        assert_eq!(
            merge_registry(Idle, HEARD, false, &dated("busy", after)),
            Idle
        );
    }

    /// The race: a scan reads `busy` an instant before a background tab's
    /// `Stop`, and is merged an instant after it. The tab keeps its tick,
    /// and the `idle` Claude Code then writes changes nothing. Without that
    /// `Stop` the same `idle` ends the turn, and in the tab you look at it
    /// ends it seen.
    #[test]
    fn a_stale_busy_does_not_reopen_a_finished_turn() {
        let mut w = ClaudeWatch::for_tests();
        let me = std::process::id();
        let shells = HashMap::from([(TAB, me)]);
        let scan = |w: &mut ClaudeWatch, status: &str, at: u64, active: Option<TabId>| {
            let mut live = dated(status, at);
            live.pid = me;
            w.scanned = ScanResult {
                live: vec![registry::LiveSession {
                    entry: live,
                    config_dir: PathBuf::from("/tmp/giverny-nowhere-stale-busy"),
                }],
            };
            w.merge_scan(&shells, active, &mut WatchEffects::default());
        };

        feed(&mut w, &hook("SessionStart", ""), Some(TAB));
        feed(&mut w, &hook("UserPromptSubmit", ""), Some(TAB));
        let turn_began = now_ms();
        scan(&mut w, "busy", turn_began, None);
        assert_eq!(w.state_of(TAB), ClaudeState::Busy);

        std::thread::sleep(Duration::from_millis(5));
        feed(&mut w, &hook("Stop", ""), None);
        assert_eq!(w.state_of(TAB), ClaudeState::DoneUnseen);
        scan(&mut w, "busy", turn_began, None);
        assert_eq!(
            w.state_of(TAB),
            ClaudeState::DoneUnseen,
            "the busy the turn began with is not a new turn"
        );
        scan(&mut w, "idle", now_ms() + 1, None);
        assert_eq!(w.state_of(TAB), ClaudeState::DoneUnseen);

        // A new turn whose `Stop` is lost: the `idle` written after its last
        // hook ends it, unseen in the background...
        feed(&mut w, &hook("UserPromptSubmit", ""), None);
        let turn_began = now_ms();
        scan(&mut w, "busy", turn_began, None);
        assert_eq!(w.state_of(TAB), ClaudeState::Busy);
        let heard = w.tabs[&TAB].heard_at_ms;
        scan(&mut w, "idle", heard - 1, None);
        assert_eq!(w.state_of(TAB), ClaudeState::Busy, "the idle before it");
        scan(&mut w, "idle", now_ms() + 1, None);
        assert_eq!(w.state_of(TAB), ClaudeState::DoneUnseen);

        // ...and seen in the tab you are looking at.
        feed(&mut w, &hook("UserPromptSubmit", ""), Some(TAB));
        scan(&mut w, "idle", now_ms() + 1, Some(TAB));
        assert_eq!(w.state_of(TAB), ClaudeState::Idle);

        // A `/compact` in the background whose `idle` comes before its
        // `SessionStart`: the tab is given back as it was, not ticked.
        feed(&mut w, &hook("PreCompact", r#","trigger":"manual""#), None);
        assert_eq!(w.state_of(TAB), ClaudeState::Busy);
        scan(&mut w, "idle", now_ms() + 1, None);
        assert_eq!(
            w.state_of(TAB),
            ClaudeState::Idle,
            "no tick for a compaction"
        );
        feed(
            &mut w,
            &hook("SessionStart", r#","source":"compact""#),
            None,
        );
        assert_eq!(w.state_of(TAB), ClaudeState::Idle);
    }

    #[test]
    fn without_hooks_the_registry_drives_every_state() {
        use ClaudeState::*;
        assert_eq!(
            merge_registry(Idle, NO_HOOKS, false, &session("busy")),
            Busy
        );
        assert_eq!(
            merge_registry(Busy, NO_HOOKS, false, &session("idle")),
            Idle
        );
        // A background shell leaves the agent idle: marked in the rail, not
        // spun.
        assert_eq!(
            merge_registry(Busy, NO_HOOKS, false, &session("shell")),
            Idle
        );
        // A session blocked on a permission prompt with no hook to report it
        // used to read as idle: the flag now comes from the registry.
        assert_eq!(
            merge_registry(Idle, NO_HOOKS, false, &session("waiting")),
            NeedsYou
        );
        // Attention states are the user's to clear, not the registry's.
        assert_eq!(
            merge_registry(DoneUnseen, NO_HOOKS, false, &session("idle")),
            DoneUnseen
        );
    }

    /// A turn that ends with its subagents still at work leaves the agent
    /// free to talk while they run: Idle, and working in the background —
    /// the rail's three dots — until the registry stops saying `busy`, which
    /// is how Claude Code says the last of them is done. It was a hollow
    /// ring for as long as its workers ran.
    #[test]
    fn subagents_outliving_the_turn_are_background_work_until_they_end() {
        let mut w = ClaudeWatch::for_tests();
        let me = std::process::id();
        let shells = HashMap::from([(TAB, me)]);
        let scan = |w: &mut ClaudeWatch, status: &str| {
            let mut live = session(status);
            live.pid = me;
            w.scanned = ScanResult {
                live: vec![registry::LiveSession {
                    entry: live,
                    config_dir: PathBuf::from("/tmp/giverny-nowhere-subagents"),
                }],
            };
            w.merge_scan(&shells, None, &mut WatchEffects::default());
        };
        let agent = r#","background_tasks":[{"id":"a1","type":"subagent","status":"running"}]"#;
        let dots = |w: &ClaudeWatch| w.tabs[&TAB].working_in_background();

        feed(&mut w, &hook("SessionStart", ""), Some(TAB));
        feed(&mut w, &hook("UserPromptSubmit", ""), Some(TAB));
        scan(&mut w, "busy");
        assert_eq!(w.state_of(TAB), ClaudeState::Busy);

        // The turn ends with a worker running: the agent is at its prompt.
        feed(&mut w, &hook("Stop", agent), Some(TAB));
        scan(&mut w, "busy");
        assert_eq!(w.state_of(TAB), ClaudeState::Idle);
        assert!(dots(&w), "Idle with its worker running: the dots");

        // The worker's own tool calls come under this session, named by its
        // `agent_id`: still the dots, not a spinner.
        feed(
            &mut w,
            &hook("PostToolUse", r#","agent_id":"a1","tool_name":"Bash""#),
            Some(TAB),
        );
        scan(&mut w, "busy");
        assert_eq!(w.state_of(TAB), ClaudeState::Idle);
        assert!(dots(&w), "a worker's tool call keeps the dots");

        // Asked something meanwhile, it is working again, workers or not:
        // the spinner, and the next `Stop` says what still runs.
        feed(&mut w, &hook("UserPromptSubmit", ""), Some(TAB));
        scan(&mut w, "busy");
        assert_eq!(w.state_of(TAB), ClaudeState::Busy);
        feed(&mut w, &hook("Stop", agent), Some(TAB));
        scan(&mut w, "busy");
        assert!(dots(&w));

        // The last worker is done: the registry's next word is `idle`.
        scan(&mut w, "idle");
        assert_eq!(w.state_of(TAB), ClaudeState::Idle);
        assert!(!dots(&w), "nothing runs: the ring");

        // A turn that leaves nothing behind is not mistaken for one that
        // did by a registry not yet rewritten at its end.
        feed(&mut w, &hook("UserPromptSubmit", ""), Some(TAB));
        feed(
            &mut w,
            &hook("Stop", r#","background_tasks":[]"#),
            Some(TAB),
        );
        scan(&mut w, "busy");
        assert_eq!(w.state_of(TAB), ClaudeState::Idle);
        assert!(!dots(&w));

        // Workers done but a background shell left: still the dots, now
        // the shell's.
        feed(&mut w, &hook("Stop", agent), Some(TAB));
        scan(&mut w, "shell");
        assert!(!w.tabs[&TAB].agents);
        assert!(dots(&w));

        // In a tab you are not looking at, the turn stays unseen while its
        // workers keep the registry `busy`: that is not a new turn. The
        // rail waves for them, and ticks once they end.
        feed(&mut w, &hook("UserPromptSubmit", ""), Some(TAB));
        feed(&mut w, &hook("Stop", agent), None);
        scan(&mut w, "busy");
        assert_eq!(w.state_of(TAB), ClaudeState::DoneUnseen);
        assert!(dots(&w));
    }

    /// A turn that ends with work still running is not done: the tab waves
    /// for it, seen or unseen, and is left holding what the turn left —
    /// unseen, the tick; seen, the ring — once the last of it ends. Nothing
    /// on the way may drop an unseen turn to `Idle`, or the tick would never
    /// show (a manager's tab ticked while its worker built).
    #[test]
    fn a_turn_whose_work_runs_on_waves_and_ticks_only_once_it_ends() {
        let me = std::process::id();
        let shells = HashMap::from([(TAB, me)]);
        let scan = |w: &mut ClaudeWatch, status: &str| {
            let mut live = session(status);
            live.pid = me;
            w.scanned = ScanResult {
                live: vec![registry::LiveSession {
                    entry: live,
                    config_dir: PathBuf::from("/tmp/giverny-nowhere-unseen"),
                }],
            };
            w.merge_scan(&shells, None, &mut WatchEffects::default());
        };
        let agent = r#","background_tasks":[{"id":"a1","type":"subagent","status":"running"}]"#;
        let worker_call = hook(
            "PostToolUse",
            r#","agent_id":"a58ff8469ff4515c6","tool_name":"Bash""#,
        );
        let waves = |w: &ClaudeWatch| w.tabs[&TAB].working_in_background();

        // Unseen: the turn ends with a worker running, out of sight.
        let mut w = ClaudeWatch::for_tests();
        feed(&mut w, &hook("SessionStart", ""), Some(TAB));
        feed(&mut w, &hook("UserPromptSubmit", ""), Some(TAB));
        scan(&mut w, "busy");
        feed(&mut w, &hook("Stop", agent), None);
        for _ in 0..3 {
            feed(&mut w, &worker_call, None);
            scan(&mut w, "busy");
            assert_eq!(w.state_of(TAB), ClaudeState::DoneUnseen);
            assert!(waves(&w), "the worker runs: the wave, not the tick");
        }
        // The worker ends: still unseen, nothing running — the tick.
        scan(&mut w, "idle");
        assert_eq!(w.state_of(TAB), ClaudeState::DoneUnseen);
        assert!(!waves(&w), "nothing runs: the tick");
        // Looked at, the tick clears to the ring.
        w.mark_viewed(TAB);
        assert_eq!(w.state_of(TAB), ClaudeState::Idle);

        // Seen: the wave while the worker runs, the ring after.
        let mut w = ClaudeWatch::for_tests();
        feed(&mut w, &hook("SessionStart", ""), Some(TAB));
        feed(&mut w, &hook("UserPromptSubmit", ""), Some(TAB));
        feed(&mut w, &hook("Stop", agent), Some(TAB));
        scan(&mut w, "busy");
        assert_eq!(w.state_of(TAB), ClaudeState::Idle);
        assert!(waves(&w));
        scan(&mut w, "idle");
        assert_eq!(w.state_of(TAB), ClaudeState::Idle);
        assert!(!waves(&w), "the ring");

        // A background shell holds the tick back the same.
        let mut w = ClaudeWatch::for_tests();
        feed(&mut w, &hook("SessionStart", ""), Some(TAB));
        feed(&mut w, &hook("UserPromptSubmit", ""), Some(TAB));
        feed(&mut w, &hook("Stop", r#","background_tasks":[]"#), None);
        scan(&mut w, "shell");
        assert_eq!(w.state_of(TAB), ClaudeState::DoneUnseen);
        assert!(waves(&w), "the shell runs: the wave");
        scan(&mut w, "idle");
        assert_eq!(w.state_of(TAB), ClaudeState::DoneUnseen);
        assert!(!waves(&w), "the shell is gone: the tick");
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
                since: Sample::heard(),
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
                since: Sample::heard(),
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
                    since: Sample::heard(),
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
                since: Sample::heard(),
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

    /// One account as an early reset found it: its cache, and what each of its
    /// sessions' status lines relays, in Claude Code's own payload shape.
    struct EarlyReset {
        w: ClaudeWatch,
        now: jiff::Timestamp,
        /// The five-hour window before the reset, and the one after it.
        old_5h: jiff::Timestamp,
        new_5h: jiff::Timestamp,
        /// The week: a reset leaves its reset time where it was.
        week: jiff::Timestamp,
    }

    impl EarlyReset {
        const DIR: &str = "/tmp/giverny-test-early-reset";

        /// `cache`: (5h %, 7d %, minutes since `/usage` fetched it), the 5h
        /// window being the pre-reset one.
        fn new(session: u32, weekly: u32, cache_age_min: i64) -> Self {
            use giverny_claude::usage::AccountUsage;
            let now = jiff::Timestamp::now();
            let at = |mins: i64| jiff::Timestamp::from_second(now.as_second() + mins * 60).unwrap();
            let mut t = EarlyReset {
                w: ClaudeWatch::for_tests(),
                now,
                old_5h: at(40),
                new_5h: at(5 * 60 - 5),
                week: at(3 * 24 * 60),
            };
            t.w.accounts.push(AccountPanel {
                profile: Profile {
                    name: "acct".into(),
                    config_dir: PathBuf::from(Self::DIR),
                    email: None,
                    account_uuid: None,
                },
                usage: Some(AccountUsage {
                    fetched_at_ms: (now.as_millisecond() - cache_age_min * 60_000) as u64,
                    limits: vec![],
                }),
                live: None,
                peak: HashMap::new(),
                statusline_on: true,
            });
            t.cache(session, t.old_5h, weekly, cache_age_min);
            t
        }

        /// `/usage` writes the cache, as `cachedUsageUtilization` turns into
        /// limits — the week's reset spelt with microseconds, as it does.
        fn cache(&mut self, session: u32, five: jiff::Timestamp, weekly: u32, age_min: i64) {
            use giverny_claude::usage::{AccountUsage, LimitEntry};
            let week = self.week + jiff::SignedDuration::from_micros(790_221);
            let limit = |kind: &str, percent: u32, resets: jiff::Timestamp| -> LimitEntry {
                serde_json::from_str(&format!(
                    r#"{{"kind":"{kind}","percent":{percent},"severity":"normal",
                         "is_active":true,"resets_at":"{resets}"}}"#
                ))
                .unwrap()
            };
            self.w.accounts[0].usage = Some(AccountUsage {
                fetched_at_ms: (self.now.as_millisecond() - age_min * 60_000) as u64,
                limits: vec![
                    limit("session", session, five),
                    limit("weekly_all", weekly, week),
                ],
            });
        }

        /// One status line refresh from `session`, then a tick's marks.
        fn push(&mut self, session: &str, five: (u32, jiff::Timestamp), weekly: u32) {
            let m = msg(&format!(
                r#"{{"tab_id":"giverny-7","config_dir":"{}",
                    "event":{{"hook_event_name":"GivernyStatusLine","session_id":"{session}",
                             "rate_limits":{{"five_hour":{{"used_percentage":{},"resets_at":{}}},
                                            "seven_day":{{"used_percentage":{weekly},"resets_at":{}}}}}}}}}"#,
                Self::DIR,
                five.0,
                five.1.as_second(),
                self.week.as_second(),
            ));
            feed(&mut self.w, &m, Some(TAB));
        }

        /// What the two bars show, before and after the tick's marks: both
        /// are drawn.
        fn shown(&mut self) -> [(f64, f64); 2] {
            let read = |w: &ClaudeWatch| {
                let acc = &w.accounts[0];
                let limits = &acc.usage.as_ref().unwrap().limits;
                (
                    ClaudeWatch::reading(acc, &limits[0], jiff::Timestamp::now()).percent,
                    ClaudeWatch::reading(acc, &limits[1], jiff::Timestamp::now()).percent,
                )
            };
            let before = read(&self.w);
            self.w.remember_peaks();
            [before, read(&self.w)]
        }
    }

    /// An account's usage was reset early. The five-hour window started
    /// again with a new reset time; the week kept its reset time and went
    /// from 99% to 0. Every session that had not asked the API since kept
    /// relaying its pre-reset numbers every second, between the ones that
    /// had — the bars jumped, and the week, whose mark only ever climbed,
    /// stuck at 99.
    #[test]
    fn an_early_reset_sticks_however_many_idle_sessions_remember_before() {
        // Before the reset: two sessions, the cache saying the same.
        let mut t = EarlyReset::new(60, 99, 10);
        let old = t.old_5h;
        for _ in 0..3 {
            t.push("idle", (60, old), 99);
            t.push("busy", (60, old), 99);
        }
        assert_eq!(t.shown(), [(60.0, 99.0); 2]);

        // The reset. The busy session's next answer says so.
        let new = t.new_5h;
        t.push("busy", (2, new), 0);
        assert_eq!(t.shown(), [(2.0, 0.0); 2]);

        // The idle one keeps saying 60 and 99, between the busy one's
        // refreshes, and the cache still holds what it fetched before.
        let mut seen = Vec::new();
        for _ in 0..5 {
            t.push("idle", (60, old), 99);
            seen.extend(t.shown());
            t.push("busy", (2, new), 0);
            seen.extend(t.shown());
        }
        assert!(seen.iter().all(|&s| s == (2.0, 0.0)), "jumped: {seen:?}");

        // The same windows still climb.
        t.push("busy", (3, new), 1);
        t.push("idle", (60, old), 99);
        assert_eq!(t.shown(), [(3.0, 1.0); 2]);

        // The session that answered first ends. The idle one is all that is
        // left pushing, and its numbers are still from before.
        t.w.pushes.retain(|(_, s), _| s != "busy");
        t.push("idle", (60, old), 99);
        assert_eq!(t.shown(), [(3.0, 1.0); 2]);

        // Once it is asked something, it is current again and agrees.
        t.push("idle", (4, new), 1);
        assert_eq!(t.shown(), [(4.0, 1.0); 2]);
    }

    /// Giverny started after the reset: every session's numbers are of
    /// unknown age, and the stale ones are the highest. The cache, fetched
    /// since the reset, is what says which are current.
    #[test]
    fn after_a_reset_a_fresh_cache_outweighs_sessions_of_unknown_age() {
        let mut t = EarlyReset::new(0, 0, 0);
        let (old, new) = (t.old_5h, t.new_5h);
        t.cache(2, new, 0, 1);
        // Ticks ran before the first push arrived.
        t.w.remember_peaks();
        let mut seen = Vec::new();
        for _ in 0..5 {
            t.push("fresh", (2, new), 3);
            seen.extend(t.shown());
            // Heard last, so the freshest-looking of the two.
            t.push("idle", (60, old), 99);
            seen.extend(t.shown());
        }
        // Never the stale 99; the fresh session's 3 is let in, and holds.
        assert!(
            seen.iter().all(|&(_, week)| week == 0.0 || week == 3.0),
            "went stale: {seen:?}"
        );
        assert!(
            seen.windows(2).all(|p| p[1].1 >= p[0].1),
            "walked back: {seen:?}"
        );
        assert_eq!(seen.last(), Some(&(2.0, 3.0)));
    }

    /// The mark's rules one reading at a time, on the numbers of that early reset.
    #[test]
    fn a_window_mark_takes_what_is_newer_and_nothing_older() {
        let now = jiff::Timestamp::now();
        let at = |mins: i64| now + jiff::Span::new().minutes(mins);
        let read = |percent: f64, resets: jiff::Timestamp| Reading {
            percent,
            live: true,
            critical: false,
            resets: Some(resets),
        };
        let seen = |mins: i64| Sample {
            at: Some(at(mins)),
            how: Taken::Seen,
        };
        let heard = |mins: i64| Sample {
            at: Some(at(mins)),
            how: Taken::Heard,
        };
        let (old_5h, new_5h) = (at(40), at(295));
        let mark = Peak {
            percent: 2.0,
            resets: Some(new_5h),
            since: seen(-1),
        };
        // A reading from the window before never moves the mark, however
        // it was taken.
        assert!(!mark.takes(&read(60.0, old_5h), seen(0), now));
        assert!(!mark.takes(&read(60.0, old_5h), heard(0), now));
        // The same window: a climb is taken, a newer answer below the mark
        // too (a reset), a number heard before the mark was set is not.
        assert!(mark.takes(&read(5.0, new_5h), seen(0), now));
        assert!(mark.takes(&read(5.0, new_5h), heard(0), now));
        assert!(!mark.takes(&read(5.0, new_5h), heard(-2), now));
        let week = Peak {
            percent: 99.0,
            resets: Some(at(4_000)),
            since: seen(-30),
        };
        assert!(week.takes(&read(0.0, at(4_000)), seen(0), now));
        assert!(!week.takes(&read(0.0, at(4_000)), heard(0), now));
        assert!(!week.takes(&read(0.0, at(4_000)), seen(-40), now));
        // A cache far below speaks for a reset only while it is fresh.
        let cache = |mins: i64| Sample {
            at: Some(at(mins)),
            how: Taken::Cache,
        };
        assert!(week.takes(&read(0.0, at(4_000)), cache(-1), now));
        assert!(!week.takes(&read(0.0, at(4_000)), cache(-60), now));
        assert!(!week.takes(&read(90.0, at(4_000)), cache(-1), now));
        // The window rolling over is a new window.
        assert!(mark.takes(&read(1.0, at(600)), heard(0), now));
        let lapsed = Peak {
            percent: 99.0,
            resets: Some(at(-1)),
            since: seen(-30),
        };
        assert!(lapsed.takes(&read(0.0, at(4_000)), heard(0), now));
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
            since: Sample::heard(),
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
