//! What happened while a session was away: told to it once,
//! on its next hook call.
//!
//! A person sees the laptop sleep, WSL restart, Giverny relaunch, or the
//! afternoon pass; the session sees none of it. A manager resumed after
//! a `wsl --shutdown` acted on its picture from before: its workers were
//! gone, its scratchpad under `/tmp` wiped (so a brief failed to save), its
//! board and pane rows stale. So the hook keeps a small record of each
//! session's last call and, when the world moved under it since, adds one
//! short paragraph to its context, once per event:
//!
//! - **Rebooted** (the kernel's `boot_id` changed): every process it had is
//!   gone, `/tmp` and its scratchpad with them.
//! - **Restarted** (another `claude` process, same boot: a Giverny relaunch,
//!   a crash, a `--resume`): its background commands and subagents are gone,
//!   `/tmp` is kept.
//!
//! **A reboot or a restart stops a manager's clocks**: every
//! Running row of the session lost its worker with it, so each not paused
//! already is stopped where the session last showed life (the record's last
//! call, or a worker's last write if later) and reads `killed` ([`manage::kill_running`]); the manager hears
//! which, and revives each (a fresh or handed-over worker, then `resume`).
//! Giverny restarts no clock itself. A sleep kills nothing: its frozen span
//! comes off every clock through [`crate::sleeps`].
//! - **Slept** (the wall clock ran ahead of the monotonic one by more than
//!   [`MIN_SLEEP_MS`]): processes survived, connections, timeouts, rate
//!   windows and ETAs may not have. A sleep in the middle of a tool call
//!   lands on that call's `PostToolUse`.
//! - **Away** (no call for [`LONG_AWAY_MS`], none of the above): the person
//!   may have changed things meanwhile. Told only as the person comes back
//!   — a prompt, a session start — since a gap before a `PostToolUse` is
//!   that tool's own run (a long build), not anyone being away.
//!
//! One event per call, the first that holds in that order: a reboot is also
//! a restart and a long gap, and says so.
//!
//! **The clocks, measured** on WSL2 (kernel 6.6, 2026-10-13): through a
//! 22-minute sleep of the laptop the wall clock moved 1321 s and
//! `CLOCK_MONOTONIC` 45 s: the VM is frozen, and its monotonic clock with
//! it. `CLOCK_BOOTTIME` stopped too (it equals `CLOCK_MONOTONIC` there), so
//! it cannot tell a sleep from a frozen VM; `MONOTONIC` is what is kept, and
//! on plain Linux, where it also stops in a suspend, it says the same.
//! `/proc/stat`'s `btime` is no boot identity: it is the wall clock less
//! the stopped uptime, so it drifts with every sleep and clock correction
//! (76 s in one boot, 4m38s an hour into the next), and a reboot is told by
//! `/proc/sys/kernel/random/boot_id`. When it was is the birth time of
//! `/run`, the tmpfs mounted as the machine boots (14:24:45 to the second,
//! as the journal says, where `btime` said 14:29), with `btime` behind it.
//!
//! **Only the main session is told.** A subagent runs in its parent's
//! process and lives through the same events, so its calls keep the record
//! fresh and queue what they see for the parent's next call ([`check`]);
//! telling every worker would say it N times.
//!
//! **Cheap.** A call reads the boot id, the monotonic clock, its `claude`'s
//! `/proc/<pid>/stat` up the short walk from the hook, and the session's
//! small record; it writes the record at most once a minute. Only an event
//! takes the record's lock, and only a reboot or a restart reads a manager
//! session's rows.
//!
//! **Files**, under `<feed dir>/away/` (`~/.config/giverny/feeds`, which a
//! reboot keeps, unlike `/tmp`): `<session>.json`, the last call's clocks
//! and process, and the lines a subagent's call queued for the parent.
//! Claude Code keeps a session's id across `--resume` (one id in a
//! transcript across a restart, Claude Code 2.1.296), so the
//! session id is the key. A record no call touched for [`KEEP_MS`] is
//! removed as a new one is made.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::manage::{self, Lock};

/// The directory under the feed dir the records live in.
pub const DIR: &str = "away";

/// How far the wall clock must run ahead of the monotonic one to be a
/// sleep: more than a clock correction (WSL's run to a minute or so) ever
/// moves it.
pub const MIN_SLEEP_MS: u64 = 2 * 60_000;

/// How long with no call is a person away.
pub const LONG_AWAY_MS: u64 = 30 * 60_000;

/// How often a quiet call writes its record: the clocks it keeps are a pair,
/// so a minute's staleness costs a long-away figure a minute, nothing else.
const REFRESH_MS: u64 = 60_000;

/// How long a record nobody touched is kept: one-shot `claude -p` sessions
/// each leave one.
pub const KEEP_MS: u64 = 30 * 24 * 60 * 60_000;

/// The `claude` process a hook call came from: its pid and its start time
/// (clock ticks after boot, `/proc/<pid>/stat` field 22), so a reused pid is
/// another process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claude {
    pub pid: u32,
    pub start: u64,
}

/// What a hook call sees of the world: the clocks, the boot, its process.
/// A field that cannot be read (another OS, no `claude` found) is `None`,
/// and the event that needs it is never told.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Probe {
    pub wall_ms: u64,
    #[serde(default)]
    pub mono_ms: Option<u64>,
    #[serde(default)]
    pub boot: Option<String>,
    #[serde(default)]
    pub claude: Option<Claude>,
}

/// A session's record: its last call's [`Probe`], and the lines its
/// subagents' calls saw for it to hear.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Record {
    #[serde(flatten)]
    last: Probe,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pending: Vec<String>,
}

/// Which hook call this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Call {
    /// `SessionStart`: a start, a resume, a `/clear`, a compaction.
    Start,
    /// `UserPromptSubmit`: the person typed.
    Prompt,
    /// The main session's `PostToolUse`.
    Tool,
    /// A subagent's `PostToolUse`: it keeps the record and tells nobody.
    Subagent,
}

/// What happened since the last call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Rebooted {
        last_ms: u64,
    },
    Restarted {
        last_ms: u64,
    },
    /// `slept_ms` of the span `from_ms`..`to_ms` the machine ran nothing.
    Slept {
        from_ms: u64,
        to_ms: u64,
        slept_ms: u64,
    },
    Away {
        last_ms: u64,
    },
}

/// What happened between `last` and `now`, the first event that holds.
/// `alive` says whether a `claude` is still running: one that is was not
/// restarted, only joined (the same session resumed in a second tab).
pub fn detect(
    last: &Probe,
    now: &Probe,
    call: Call,
    alive: impl Fn(Claude) -> bool,
) -> Option<Event> {
    let last_ms = last.wall_ms;
    if let (Some(a), Some(b)) = (&last.boot, &now.boot)
        && a != b
    {
        return Some(Event::Rebooted { last_ms });
    }
    if let (Some(a), Some(b)) = (last.claude, now.claude)
        && a != b
        && !alive(a)
    {
        return Some(Event::Restarted { last_ms });
    }
    let wall = now.wall_ms.saturating_sub(last.wall_ms);
    if let (Some(a), Some(b)) = (last.mono_ms, now.mono_ms) {
        let slept_ms = wall.saturating_sub(b.saturating_sub(a));
        if slept_ms > MIN_SLEEP_MS {
            return Some(Event::Slept {
                from_ms: last.wall_ms,
                to_ms: now.wall_ms,
                slept_ms,
            });
        }
    }
    (matches!(call, Call::Start | Call::Prompt) && wall >= LONG_AWAY_MS)
        .then_some(Event::Away { last_ms })
}

/// What the notes need beyond the event: read only when one is told.
pub struct World<'a> {
    /// When the machine booted, in epoch ms ([`boot_ms`]).
    pub boot_ms: &'a dyn Fn() -> Option<u64>,
    /// Is this WSL, whose restart is named so.
    pub wsl: bool,
    /// The local time of an epoch-ms instant, as the note shows it.
    pub clock: &'a dyn Fn(u64, u64) -> String,
}

/// The paragraph `event` is told as. `killed` names the manager session's
/// Running rows a reboot or a restart killed ([`manage::kill_running`]).
pub fn note(event: Event, now_ms: u64, killed: &[String], world: &World) -> String {
    let at = |ms: u64| (world.clock)(ms, now_ms);
    let lost = || {
        if killed.is_empty() {
            return String::new();
        }
        let keys: Vec<String> = killed.iter().map(|k| format!("`{k}`")).collect();
        format!(
            " Your Running rows lost their workers with it, and their clocks stopped there: \
             {} (the pane reads them `killed`). Revive each with a fresh worker (its \
             description naming the task) or an idle one handed it with `giverny-manage \
             start <task> --agent <id>`, then `giverny-manage resume <task>` restarts its \
             clock; or `pause` or `drop` it.",
            keys.join(", ")
        )
    };
    match event {
        Event::Rebooted { last_ms } => {
            let host = if world.wsl { "WSL" } else { "The machine" };
            let when = (world.boot_ms)()
                .filter(|b| *b > last_ms)
                .map(|b| format!(" at about {}", at(b)))
                .unwrap_or_default();
            format!(
                "Giverny: {host} restarted{when}, since this session last worked at {}. \
                 Everything it had running is gone: background commands, subagents and \
                 workers, servers, downloads. /tmp was wiped, your scratchpad with it. \
                 Check what really runs now, and your board and pane rows, before acting \
                 on what you knew.{}",
                at(last_ms),
                lost()
            )
        }
        Event::Restarted { last_ms } => format!(
            "Giverny: this session's Claude Code restarted since it last worked at {} (a \
             Giverny relaunch, a crash or a resume). Its background commands and subagents \
             from before are gone; /tmp and your scratchpad are kept. Check what really \
             runs now before acting on what you knew.{}",
            at(last_ms),
            lost()
        ),
        Event::Slept {
            from_ms,
            to_ms,
            slept_ms,
        } => {
            // Awake for most of the span: say how long it slept, not when.
            let span = if to_ms.saturating_sub(from_ms).saturating_sub(slept_ms) <= MIN_SLEEP_MS {
                format!("from about {} to {}", at(from_ms), at(to_ms))
            } else {
                format!(
                    "for about {} between {} and {}",
                    duration(slept_ms),
                    at(from_ms),
                    at(to_ms)
                )
            };
            format!(
                "Giverny: the computer slept {span}. Processes survived, but network \
                 connections, timeouts, rate-limit windows and ETAs may not have: re-check \
                 anything that was waiting on the clock."
            )
        }
        Event::Away { last_ms } => format!(
            "Giverny: {} passed since this session last worked at {}. The user may have \
             changed things meanwhile (started or stopped jobs, edited files): check before \
             trusting your last picture.",
            duration(now_ms.saturating_sub(last_ms)),
            at(last_ms)
        ),
    }
}

/// `47m`, `2h 5m`, `3d 4h`.
fn duration(ms: u64) -> String {
    let m = (ms + 30_000) / 60_000;
    match m {
        0..60 => format!("{m}m"),
        60..2880 if m.is_multiple_of(60) => format!("{}h", m / 60),
        60..2880 => format!("{}h {}m", m / 60, m % 60),
        _ => format!("{}d {}h", m / 1440, m % 1440 / 60),
    }
}

/// `HH:MM` in local time, with the weekday when it is not `now_ms`'s day.
pub fn local_clock(ms: u64, now_ms: u64) -> String {
    let tz = jiff::tz::TimeZone::system();
    let zoned = |ms: u64| {
        jiff::Timestamp::from_millisecond(ms as i64)
            .ok()
            .map(|t| t.to_zoned(tz.clone()))
    };
    match (zoned(ms), zoned(now_ms)) {
        (Some(t), Some(n)) if t.date() == n.date() => t.strftime("%H:%M").to_string(),
        (Some(t), _) => t.strftime("%a %H:%M").to_string(),
        _ => "?".into(),
    }
}

/// `<feed dir>/away/<session>.json`.
pub fn path(feed_dir: &Path, session: &str) -> PathBuf {
    feed_dir.join(DIR).join(format!("{session}.json"))
}

fn safe(session: &str) -> bool {
    !session.is_empty()
        && session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn read(file: &Path) -> Option<Record> {
    serde_json::from_slice(&std::fs::read(file).ok()?).ok()
}

fn write(file: &Path, rec: &Record) {
    if let Ok(v) = serde_json::to_value(rec) {
        let _ = manage::write(file, &v);
    }
}

/// The hook call's check: the record compared with `now`, refreshed, and
/// what the session is to hear (its subagents' queued lines and this
/// call's event), or nothing. A subagent's call queues its event instead.
/// `kill` stops a manager session's Running rows at the last call's wall
/// clock and gives their keys, asked only on a reboot or a restart.
pub fn check(
    feed_dir: &Path,
    session: &str,
    now: &Probe,
    call: Call,
    alive: impl Fn(Claude) -> bool,
    kill: impl FnOnce(u64) -> Vec<String>,
    world: &World,
) -> Option<String> {
    if !safe(session) {
        return None;
    }
    let file = path(feed_dir, session);
    let Some(rec) = read(&file) else {
        // The session's first call, or the first since the record went: a
        // baseline, nothing to compare.
        let _ = std::fs::create_dir_all(file.parent()?);
        prune(feed_dir, now.wall_ms);
        write(
            &file,
            &Record {
                last: now.clone(),
                pending: Vec::new(),
            },
        );
        return None;
    };
    let quiet = rec.pending.is_empty() || call == Call::Subagent;
    if quiet && detect(&rec.last, now, call, &alive).is_none() {
        if now.wall_ms.saturating_sub(rec.last.wall_ms) >= REFRESH_MS
            || rec.last.claude != now.claude
        {
            write(
                &file,
                &Record {
                    last: now.clone(),
                    ..rec
                },
            );
        }
        return None;
    }
    // Something to tell: under the lock, re-read, as a parallel call may
    // have told it already.
    let _lock = Lock::take(&file).ok()?;
    let mut rec = read(&file).unwrap_or_default();
    let event = detect(&rec.last, now, call, &alive);
    if let Some(e) = event {
        let rows = match e {
            Event::Rebooted { last_ms } | Event::Restarted { last_ms } => kill(last_ms),
            _ => Vec::new(),
        };
        rec.pending.push(note(e, now.wall_ms, &rows, world));
    }
    rec.last = now.clone();
    if call == Call::Subagent {
        write(&file, &rec);
        return None;
    }
    let told = std::mem::take(&mut rec.pending);
    write(&file, &rec);
    (!told.is_empty()).then(|| told.join("\n\n"))
}

/// Remove the records no call touched for [`KEEP_MS`].
fn prune(feed_dir: &Path, now_ms: u64) {
    let Ok(entries) = std::fs::read_dir(feed_dir.join(DIR)) else {
        return;
    };
    for e in entries.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .is_some_and(|t| now_ms.saturating_sub(t.as_millis() as u64) > KEEP_MS);
        if old {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Stop the Running rows of `session`'s manager session, when this writer
/// wrote it, at `last_ms` (the session's last sign of life) and mark them
/// `killed` ([`manage::kill_running`]), under the feed's lock: what a
/// reboot or a restart took the workers of. Their keys.
pub fn kill_rows(feed_dir: &Path, session: &str, last_ms: u64, now_ms: u64) -> Vec<String> {
    let file = manage::file_for(feed_dir, session);
    if !file.exists() {
        return Vec::new();
    }
    let Ok(_lock) = Lock::take(&file) else {
        return Vec::new();
    };
    let Some(mut doc) = std::fs::read(&file)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
    else {
        return Vec::new();
    };
    if manage::writer_of(&doc) != Some(manage::WRITER) {
        return Vec::new();
    }
    let killed = manage::kill_running_in(&mut doc, session, last_ms, now_ms);
    if !killed.is_empty() {
        let _ = manage::write(&file, &doc);
    }
    killed
}

/// This hook call's [`Probe`], at wall clock `now_ms`.
pub fn probe(now_ms: u64) -> Probe {
    Probe {
        wall_ms: now_ms,
        mono_ms: mono_ms(),
        boot: boot_id(),
        claude: claude_of_this_process(),
    }
}

/// `CLOCK_MONOTONIC` in ms: the clock a sleep stops (see the module docs).
#[cfg(unix)]
fn mono_ms() -> Option<u64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable timespec for the call's duration.
    let ok = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } == 0;
    ok.then(|| ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000)
}

#[cfg(not(unix))]
fn mono_ms() -> Option<u64> {
    None
}

fn boot_id() -> Option<String> {
    Some(giverny_core::state::boot_id()).filter(|b| !b.is_empty())
}

/// When the machine booted, in epoch ms: the birth of `/run`, made as it
/// boots, when that is near `/proc/stat`'s `btime` (a `/run` that is no
/// tmpfs was born with the system), else `btime`, which drifts (see the
/// module docs).
pub fn boot_ms() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let line = stat.lines().find_map(|l| l.strip_prefix("btime "))?;
    let btime = line.trim().parse::<u64>().ok()? * 1000;
    let run = std::fs::metadata("/run")
        .and_then(|m| m.created())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64);
    Some(
        run.filter(|r| r.abs_diff(btime) < 30 * 60_000)
            .unwrap_or(btime),
    )
}

/// Is this WSL: what its restart is called.
pub fn is_wsl() -> bool {
    std::env::var_os("WSL_DISTRO_NAME").is_some()
        || Path::new("/proc/sys/fs/binfmt_misc/WSLInterop").exists()
}

/// A `/proc/<pid>/stat`'s parent pid and start time.
#[cfg(target_os = "linux")]
fn stat_of(pid: u32) -> Option<(u32, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the parenthesized comm, from field 3 (state) on.
    let (_, rest) = stat.rsplit_once(')')?;
    let f: Vec<&str> = rest.split_whitespace().collect();
    Some((f.get(1)?.parse().ok()?, f.get(19)?.parse().ok()?))
}

/// The first `claude` above this hook process.
#[cfg(target_os = "linux")]
fn claude_of_this_process() -> Option<Claude> {
    let mut pid = crate::lineage::proc_parent(std::process::id())?;
    for _ in 0..16 {
        let (parent, start) = stat_of(pid)?;
        if crate::lineage::proc_is_claude(pid) {
            return Some(Claude { pid, start });
        }
        if parent == 0 || parent == pid {
            return None;
        }
        pid = parent;
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn claude_of_this_process() -> Option<Claude> {
    None
}

/// Is that `claude` still running: its pid, with the same start time.
#[cfg(target_os = "linux")]
pub fn alive(c: Claude) -> bool {
    stat_of(c.pid).is_some_and(|(_, start)| start == c.start)
}

#[cfg(not(target_os = "linux"))]
pub fn alive(_: Claude) -> bool {
    false
}

/// The real world, for the hook.
pub fn world() -> World<'static> {
    World {
        boot_ms: &boot_ms,
        wsl: is_wsl(),
        clock: &local_clock,
    }
}

/// [`check`] for a hook call as Giverny runs it: the real clocks, process
/// and rows.
pub fn on_call(feed_dir: &Path, session: &str, now_ms: u64, call: Call) -> Option<String> {
    check(
        feed_dir,
        session,
        &probe(now_ms),
        call,
        alive,
        |last_ms| kill_rows(feed_dir, session, last_ms, now_ms),
        &world(),
    )
}
