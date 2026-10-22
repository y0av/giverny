//! What happened while a session was away: told to it once, on its next
//! hook call.
//!
//! A person sees the laptop sleep, WSL restart, Giverny relaunch, or the
//! afternoon pass; the session sees none of it. Resumed after a
//! `wsl --shutdown`, a session acts on its picture from before: background
//! commands it thinks are running are gone, its scratchpad under `/tmp` is
//! wiped. So the relay keeps a small record of each session's last hook
//! call and, when the world moved under it since, adds one short paragraph
//! to its context, once per event:
//!
//! - **Rebooted** (the kernel's `boot_id` changed): every process it had is
//!   gone, `/tmp` and its scratchpad with them.
//! - **Restarted** (another `claude` process, same boot: a Giverny relaunch,
//!   a crash, a `--resume`): its background commands and subagents are gone,
//!   `/tmp` is kept.
//! - **Slept** (the wall clock ran ahead of the monotonic one by more than
//!   [`MIN_SLEEP_MS`]): processes survived, connections, timeouts and rate
//!   windows may not have. A sleep in the middle of a tool call lands on
//!   that call's `PostToolUse`.
//! - **Away** (no call for [`LONG_AWAY_MS`], none of the above): the person
//!   may have changed things meanwhile. Told only as the person comes back
//!   — a prompt, a session start — since a gap before a `PostToolUse` is
//!   that tool's own run (a long build), not anyone being away.
//!
//! One event per call, the first that holds in that order: a reboot is also
//! a restart and a long gap, and says so.
//!
//! **The clocks, measured** on WSL2 (kernel 6.6): through a 22-minute sleep
//! of the laptop the wall clock moved 1321 s and `CLOCK_MONOTONIC` 45 s: the
//! VM is frozen, and its monotonic clock with it. `CLOCK_BOOTTIME` stopped
//! too (it equals `CLOCK_MONOTONIC` there), so it cannot tell a sleep from a
//! frozen VM; `MONOTONIC` is what is kept, and on plain Linux, where it also
//! stops in a suspend, it says the same. `/proc/stat`'s `btime` is no boot
//! identity: it is the wall clock less the stopped uptime, so it drifts
//! with every sleep and clock correction (76 s in one boot, 4m38s an hour
//! into the next), and a reboot is told by `/proc/sys/kernel/random/boot_id`.
//! When it was is the birth time of `/run`, the tmpfs mounted as the
//! machine boots (to the second, as the journal says, where `btime` was
//! minutes off), with `btime` behind it.
//!
//! **Only the main session is told.** A subagent runs in its parent's
//! process and lives through the same events, so its calls keep the record
//! fresh and queue what they see for the parent's next call ([`check`]);
//! telling every subagent would say it N times.
//!
//! **How it reaches the session.** The relay hook is `async`, so Claude
//! Code holds its reply and adds it to the context at the session's next
//! step (the next tool call or prompt) rather than the one that ran it.
//!
//! **Cheap.** A call reads the boot id, the monotonic clock, its `claude`'s
//! `/proc/<pid>/stat` up the short walk from the hook, and the session's
//! small record; it writes the record at most once a minute. Only an event
//! takes the record's lock.
//!
//! **Files**, under `<config>/giverny/state/away/` (which a reboot keeps,
//! unlike `/tmp`): `<session>.json`, the last call's clocks and process, and
//! the lines a subagent's call queued for the parent. Claude Code keeps a
//! session's id across `--resume`, so the session id is the key. A record no
//! call touched for [`KEEP_MS`] is removed as a new one is made. Outside
//! Linux, what cannot be read (the boot, the monotonic clock, the `claude`
//! process) is left out, and only a long absence is told.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The directory beside the hook spool the records live in.
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

impl Call {
    /// The call a hook payload is, or `None` for an event that is not
    /// checked (`Stop`, `Notification`, …) or a subagent's start.
    pub fn of(payload: &Value) -> Option<Call> {
        let subagent = payload.get("agent_id").is_some_and(|a| !a.is_null());
        match payload.get("hook_event_name").and_then(Value::as_str)? {
            "SessionStart" if !subagent => Some(Call::Start),
            "UserPromptSubmit" => Some(Call::Prompt),
            "PostToolUse" if subagent => Some(Call::Subagent),
            "PostToolUse" => Some(Call::Tool),
            _ => None,
        }
    }
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

/// The paragraph `event` is told as.
pub fn note(event: Event, now_ms: u64, world: &World) -> String {
    let at = |ms: u64| (world.clock)(ms, now_ms);
    match event {
        Event::Rebooted { last_ms } => {
            let host = if world.wsl { "WSL" } else { "The machine" };
            let when = (world.boot_ms)()
                .filter(|b| *b > last_ms)
                .map(|b| format!(" at about {}", at(b)))
                .unwrap_or_default();
            format!(
                "Giverny: {host} restarted{when}, since this session last worked at {}. \
                 Everything it had running is gone: background commands, subagents, \
                 servers, downloads. /tmp was wiped, your scratchpad with it. Check what \
                 really runs now before acting on what you knew.",
                at(last_ms)
            )
        }
        Event::Restarted { last_ms } => format!(
            "Giverny: this session's Claude Code restarted since it last worked at {} (a \
             Giverny relaunch, a crash or a resume). Its background commands and subagents \
             from before are gone; /tmp and your scratchpad are kept. Check what really \
             runs now before acting on what you knew.",
            at(last_ms)
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
                 connections, timeouts and rate-limit windows may not have: re-check \
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

/// `<dir>/<session>.json`.
pub fn path(dir: &Path, session: &str) -> PathBuf {
    dir.join(format!("{session}.json"))
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

/// Write the record atomically: a temp file named for this process, then
/// a rename, so a parallel call never reads half of it.
fn write(file: &Path, rec: &Record) {
    let Ok(data) = serde_json::to_vec(rec) else {
        return;
    };
    let tmp = file.with_extension(format!("json.{}.tmp", std::process::id()));
    if std::fs::write(&tmp, data).is_ok() && std::fs::rename(&tmp, file).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// A lock beside a record, so two hook calls at once (parallel tool calls)
/// tell an event once. Taken with `create_new`; one a killed call left
/// behind is broken after ten seconds.
struct Lock(PathBuf);

impl Lock {
    fn take(file: &Path) -> Option<Lock> {
        let lock = file.with_extension("json.lock");
        for _ in 0..100 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock)
            {
                Ok(_) => return Some(Lock(lock)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&lock)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| SystemTime::now().duration_since(t).ok())
                        .is_some_and(|age| age > Duration::from_secs(10));
                    if stale {
                        let _ = std::fs::remove_file(&lock);
                    } else {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
                Err(_) => return None,
            }
        }
        None
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The hook call's check: the record in `dir` compared with `now`,
/// refreshed, and what the session is to hear (its subagents' queued lines
/// and this call's event), or nothing. A subagent's call queues its event
/// instead.
pub fn check(
    dir: &Path,
    session: &str,
    now: &Probe,
    call: Call,
    alive: impl Fn(Claude) -> bool,
    world: &World,
) -> Option<String> {
    if !safe(session) {
        return None;
    }
    let file = path(dir, session);
    let Some(rec) = read(&file) else {
        // The session's first call, or the first since the record went: a
        // baseline, nothing to compare.
        let _ = std::fs::create_dir_all(dir);
        prune(dir, now.wall_ms);
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
    let _lock = Lock::take(&file)?;
    let mut rec = read(&file).unwrap_or_default();
    if let Some(e) = detect(&rec.last, now, call, &alive) {
        rec.pending.push(note(e, now.wall_ms, world));
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
fn prune(dir: &Path, now_ms: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
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

/// The kernel's id for this boot, which a reboot changes.
fn boot_id() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|b| !b.is_empty())
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
    // Fields after the parenthesized comm (which may hold spaces or
    // parentheses itself), from field 3 (state) on.
    let (_, rest) = stat.rsplit_once(')')?;
    let f: Vec<&str> = rest.split_whitespace().collect();
    Some((f.get(1)?.parse().ok()?, f.get(19)?.parse().ok()?))
}

/// `claude`, whatever path or `.exe` it was run by.
fn is_claude_name(name: &str) -> bool {
    let name = name.trim();
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    base.strip_suffix(".exe").unwrap_or(base) == "claude"
}

/// `comm` names the program as it was run, and Claude Code also sets it as
/// its process title; `argv[0]` is the second opinion.
#[cfg(target_os = "linux")]
fn is_claude(pid: u32) -> bool {
    if std::fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|c| is_claude_name(&c)) {
        return true;
    }
    std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|cmd| {
        let argv0 = cmd.split(|b| *b == 0).next().unwrap_or_default();
        is_claude_name(&String::from_utf8_lossy(argv0))
    })
}

/// The first `claude` above this hook process (the hook runs under a
/// shell its `claude` started).
#[cfg(target_os = "linux")]
fn claude_of_this_process() -> Option<Claude> {
    let (mut pid, _) = stat_of(std::process::id())?;
    for _ in 0..16 {
        let (parent, start) = stat_of(pid)?;
        if is_claude(pid) {
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

/// The hook reply for one relayed Claude Code `payload`, at wall clock
/// `now_ms`, with the records in `dir`: what happened while the session was
/// away as its `additionalContext`, or nothing to print.
pub fn reply(dir: &Path, payload: &Value, now_ms: u64) -> Option<String> {
    let call = Call::of(payload)?;
    let event = payload.get("hook_event_name").and_then(Value::as_str)?;
    let session = payload.get("session_id").and_then(Value::as_str)?;
    let told = check(dir, session, &probe(now_ms), call, alive, &world())?;
    Some(
        json!({
            "hookSpecificOutput": {
                "hookEventName": event,
                "additionalContext": told
            }
        })
        .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_790_000_000_000;
    const MIN: u64 = 60_000;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("giverny-away-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    /// Minutes after `T0` as `+N`, so the notes read the same in any zone.
    fn clock(ms: u64, _now: u64) -> String {
        format!("+{}", (ms - T0) / MIN)
    }

    /// The new boot, 50 minutes in.
    fn boot() -> Option<u64> {
        Some(T0 + 50 * MIN)
    }

    fn world() -> World<'static> {
        World {
            boot_ms: &boot,
            wsl: true,
            clock: &clock,
        }
    }

    /// A probe `wall` minutes and `mono` minutes after the first, in boot
    /// `boot`, from claude `pid`.
    fn at(wall: u64, mono: u64, boot: &str, pid: u32) -> Probe {
        Probe {
            wall_ms: T0 + wall * MIN,
            mono_ms: Some(1_000 * MIN + mono * MIN),
            boot: Some(boot.into()),
            claude: Some(Claude { pid, start: 7 }),
        }
    }

    /// Run [`check`] on `dir`'s session `s1`: no old claude is alive.
    fn call(dir: &Path, p: &Probe, call: Call) -> Option<String> {
        check(dir, "s1", p, call, |_| false, &world())
    }

    #[test]
    fn the_first_call_is_a_baseline_and_a_normal_next_call_says_nothing() {
        let dir = temp("baseline");
        assert_eq!(call(&dir, &at(0, 0, "b1", 10), Call::Tool), None);
        assert!(path(&dir, "s1").exists());
        // Work goes on: minutes of tool calls, a prompt 20 minutes later.
        for m in 1..5 {
            assert_eq!(call(&dir, &at(m, m, "b1", 10), Call::Tool), None);
        }
        assert_eq!(call(&dir, &at(24, 24, "b1", 10), Call::Prompt), None);
        // A clock correction of a minute is no sleep.
        assert_eq!(call(&dir, &at(26, 25, "b1", 10), Call::Tool), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_reboot_is_told_once() {
        let dir = temp("reboot");
        call(&dir, &at(0, 0, "b1", 10), Call::Tool);
        // A new boot, a new claude, its monotonic clock from zero.
        let mut p = at(60, 0, "b2", 11);
        p.mono_ms = Some(MIN);
        let said = call(&dir, &p, Call::Start).unwrap();
        assert_eq!(
            said,
            "Giverny: WSL restarted at about +50, since this session last worked at +0. \
             Everything it had running is gone: background commands, subagents, \
             servers, downloads. /tmp was wiped, your scratchpad with it. Check what \
             really runs now before acting on what you knew."
        );
        // Told once: the prompt that follows hears nothing.
        let mut q = at(61, 0, "b2", 11);
        q.mono_ms = Some(2 * MIN);
        assert_eq!(call(&dir, &q, Call::Prompt), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_restart_in_the_same_boot_is_told_once() {
        let dir = temp("restart");
        call(&dir, &at(0, 0, "b1", 10), Call::Tool);
        let said = call(&dir, &at(3, 3, "b1", 22), Call::Start).unwrap();
        assert_eq!(
            said,
            "Giverny: this session's Claude Code restarted since it last worked at +0 (a \
             Giverny relaunch, a crash or a resume). Its background commands and subagents \
             from before are gone; /tmp and your scratchpad are kept. Check what really \
             runs now before acting on what you knew."
        );
        assert_eq!(call(&dir, &at(4, 4, "b1", 22), Call::Tool), None);
        // The same pid reused by another process is another claude.
        let mut p = at(5, 5, "b1", 22);
        p.claude = Some(Claude { pid: 22, start: 9 });
        assert!(call(&dir, &p, Call::Tool).unwrap().contains("restarted"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same session resumed in a second tab while the first runs: two
    /// claudes take turns, and neither was restarted.
    #[test]
    fn a_second_live_claude_on_the_session_is_no_restart() {
        let dir = temp("second");
        let quiet = |p: &Probe| check(&dir, "s1", p, Call::Tool, |_| true, &world());
        assert_eq!(quiet(&at(0, 0, "b1", 10)), None);
        assert_eq!(quiet(&at(1, 1, "b1", 11)), None);
        assert_eq!(quiet(&at(2, 2, "b1", 10)), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_sleep_is_told_from_when_to_when_once() {
        let dir = temp("sleep");
        call(&dir, &at(0, 0, "b1", 10), Call::Tool);
        // 22 minutes on the wall, under one on the monotonic clock: a sleep
        // in the middle of a tool call, told on its PostToolUse.
        let said = call(&dir, &at(22, 0, "b1", 10), Call::Tool).unwrap();
        assert_eq!(
            said,
            "Giverny: the computer slept from about +0 to +22. Processes survived, but \
             network connections, timeouts and rate-limit windows may not have: \
             re-check anything that was waiting on the clock."
        );
        assert_eq!(call(&dir, &at(23, 1, "b1", 10), Call::Tool), None);
        // Awake for a while of the span (a long build), then asleep: how long.
        let said = call(&dir, &at(83, 31, "b1", 10), Call::Tool).unwrap();
        assert!(
            said.contains("slept for about 30m between +23 and +83."),
            "{said}"
        );
        // A sleep long enough to be away too is told as the sleep.
        let said = call(&dir, &at(200, 32, "b1", 10), Call::Prompt).unwrap();
        assert!(said.starts_with("Giverny: the computer slept"), "{said}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn long_away_is_told_as_the_person_comes_back_not_after_a_long_tool() {
        let dir = temp("away");
        call(&dir, &at(0, 0, "b1", 10), Call::Tool);
        // A 40-minute build: its PostToolUse is no one coming back.
        assert_eq!(call(&dir, &at(40, 40, "b1", 10), Call::Tool), None);
        // 47 minutes on, the person types.
        let said = call(&dir, &at(87, 87, "b1", 10), Call::Prompt).unwrap();
        assert_eq!(
            said,
            "Giverny: 47m passed since this session last worked at +40. The user may have \
             changed things meanwhile (started or stopped jobs, edited files): check before \
             trusting your last picture."
        );
        assert_eq!(call(&dir, &at(88, 88, "b1", 10), Call::Prompt), None);
        // Twenty-nine minutes is not away.
        assert_eq!(call(&dir, &at(117, 117, "b1", 10), Call::Prompt), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_subagent_queues_what_it_sees_for_its_parent() {
        let dir = temp("subagent");
        call(&dir, &at(0, 0, "b1", 10), Call::Tool);
        // The parent waits on its worker; the worker's call sees the sleep.
        assert_eq!(call(&dir, &at(30, 2, "b1", 10), Call::Subagent), None);
        assert_eq!(call(&dir, &at(31, 3, "b1", 10), Call::Subagent), None);
        let said = call(&dir, &at(32, 4, "b1", 10), Call::Tool).unwrap();
        assert!(said.starts_with("Giverny: the computer slept"), "{said}");
        assert_eq!(said.matches("Giverny:").count(), 1, "{said}");
        assert_eq!(call(&dir, &at(33, 5, "b1", 10), Call::Tool), None);
        // A worker's calls keep the session fresh: no long away for the
        // parent idle beside them.
        for m in (40..100).step_by(5) {
            assert_eq!(call(&dir, &at(m, m - 28, "b1", 10), Call::Subagent), None);
        }
        assert_eq!(call(&dir, &at(101, 73, "b1", 10), Call::Prompt), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn what_cannot_be_read_tells_nothing() {
        let dir = temp("unread");
        let blind = |wall: u64| Probe {
            wall_ms: T0 + wall * MIN,
            ..Probe::default()
        };
        call(&dir, &blind(0), Call::Tool);
        assert_eq!(call(&dir, &blind(25), Call::Tool), None);
        // The gap alone still tells a person away.
        assert!(
            call(&dir, &blind(60), Call::Prompt)
                .unwrap()
                .contains("35m passed")
        );
        // And a session id that is no file name is never one.
        assert_eq!(
            check(&dir, "../x", &blind(0), Call::Tool, |_| false, &world()),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_calls_checked_are_a_start_a_prompt_and_a_tool() {
        let of = |v: Value| Call::of(&v);
        assert_eq!(
            of(json!({"hook_event_name": "SessionStart"})),
            Some(Call::Start)
        );
        assert_eq!(
            of(json!({"hook_event_name": "UserPromptSubmit"})),
            Some(Call::Prompt)
        );
        assert_eq!(
            of(json!({"hook_event_name": "PostToolUse"})),
            Some(Call::Tool)
        );
        assert_eq!(
            of(json!({"hook_event_name": "PostToolUse", "agent_id": "a1"})),
            Some(Call::Subagent)
        );
        assert_eq!(
            of(json!({"hook_event_name": "SessionStart", "agent_id": "a1"})),
            None
        );
        assert_eq!(of(json!({"hook_event_name": "Stop"})), None);
        assert_eq!(of(json!({"hook_event_name": "Notification"})), None);
    }

    /// The whole path the relay takes, on this machine: a record from
    /// another boot is told as a reboot, in the reply Claude Code reads.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_reply_carries_the_note_as_the_events_context() {
        let dir = temp("reply");
        let tool =
            json!({"session_id": "s1", "hook_event_name": "PostToolUse", "tool_name": "Read"});
        // The first call is the baseline.
        assert_eq!(reply(&dir, &tool, T0), None);
        assert_eq!(reply(&dir, &tool, T0 + MIN), None);
        let mut rec = read(&path(&dir, "s1")).unwrap();
        rec.last.boot = Some("another-boot".into());
        write(&path(&dir, "s1"), &rec);
        let prompt = json!({"session_id": "s1", "hook_event_name": "UserPromptSubmit"});
        let out: Value =
            serde_json::from_str(&reply(&dir, &prompt, T0 + 2 * MIN).unwrap()).unwrap();
        let hook = &out["hookSpecificOutput"];
        assert_eq!(hook["hookEventName"], "UserPromptSubmit");
        let said = hook["additionalContext"].as_str().unwrap();
        assert!(said.contains("restarted"), "{said}");
        assert!(said.contains("/tmp was wiped"), "{said}");
        // Told once.
        assert_eq!(reply(&dir, &prompt, T0 + 3 * MIN), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn claude_is_named_by_its_program_whatever_its_path() {
        assert!(is_claude_name("claude\n"));
        assert!(is_claude_name("/home/u/.local/bin/claude"));
        assert!(is_claude_name("C:\\Users\\u\\claude.exe"));
        assert!(!is_claude_name("claude-code"));
        assert!(!is_claude_name("giverny"));
    }

    #[test]
    fn durations_read_short() {
        assert_eq!(duration(47 * MIN), "47m");
        assert_eq!(duration(120 * MIN), "2h");
        assert_eq!(duration(125 * MIN), "2h 5m");
        assert_eq!(duration(3 * 1440 * MIN + 4 * 60 * MIN), "3d 4h");
    }

    #[test]
    fn the_real_probe_reads_this_machine() {
        let p = probe(T0);
        #[cfg(target_os = "linux")]
        {
            assert!(p.mono_ms.is_some());
            assert!(p.boot.as_deref().is_some_and(|b| b.len() > 8), "{p:?}");
            assert!(boot_ms().is_some());
        }
        assert_eq!(p.wall_ms, T0);
    }
}
