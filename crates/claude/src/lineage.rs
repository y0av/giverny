//! Is the Claude Code session running a hook the tab's own?
//!
//! Everything started in a tab inherits `GIVERNY_TAB_ID`, including a
//! `claude` the tab's claude starts in its shell and one started inside a
//! terminal multiplexer the tab launched. Those are not the tab's session:
//! their hooks must not move the tab's state or name its resume target, and
//! their workers must not land in the tab's management panel or lose their own
//! subagent panel to it.
//!
//! The tab exports the app's process id ([`APP_PID_ENV`]). The tab's own
//! claude is the one that reaches the app through its parents with no other
//! claude on the way: a claude inside a claude passes a second one, and a
//! multiplexer's server detaches from the tab, so its sessions never reach
//! the app at all.
//!
//! Why process ancestry and not the session id: the relay has to answer at
//! once — the subagent panel is drawn from what it prints — and the session
//! id alone says nothing about where a session runs. The app learns a tab's
//! session from the very hooks in question, so a nested session would
//! claim the tab before anything could tell the two apart.
//!
//! Where the walk cannot see a claude at all — a relay that runs as a
//! Windows program on behalf of a session inside WSL, whose parents are not
//! that session's — it has nothing to judge by and trusts the tab id, as
//! before.

use std::path::PathBuf;

/// The app's process id, exported to every tab's shell.
pub const APP_PID_ENV: &str = "GIVERNY_PID";

/// How far up the walk goes before giving up.
const MAX_DEPTH: usize = 64;

/// What the walk from a hook process up to the app found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lineage {
    /// Exactly one claude between the hook and the app: the tab's own.
    Own,
    /// A second claude on the way, or a claude whose parents never reach
    /// the app: some other session that inherited the tab's identity.
    Nested,
    /// No claude seen on the way: nothing to judge by.
    Unknown,
}

impl Lineage {
    /// Should this session be treated as the tab's? Only a [`Lineage::Nested`]
    /// one is turned away; an unknown one is given the benefit of the doubt.
    pub fn is_tabs(self) -> bool {
        self != Lineage::Nested
    }
}

/// Walk from `start` (the hook's parent) up through `parent` links to `app`,
/// counting the processes `is_claude` recognises.
pub fn lineage(
    start: u32,
    app: u32,
    parent: impl Fn(u32) -> Option<u32>,
    is_claude: impl Fn(u32) -> bool,
) -> Lineage {
    let mut claudes = 0usize;
    let mut pid = start;
    for _ in 0..MAX_DEPTH {
        if pid == app {
            return match claudes {
                0 => Lineage::Unknown,
                1 => Lineage::Own,
                _ => Lineage::Nested,
            };
        }
        if is_claude(pid) {
            claudes += 1;
            if claudes > 1 {
                return Lineage::Nested;
            }
        }
        match parent(pid) {
            Some(p) if p != pid && p != 0 => pid = p,
            _ => break,
        }
    }
    if claudes == 0 {
        Lineage::Unknown
    } else {
        Lineage::Nested
    }
}

/// The environment variable Claude Code gives a session its background
/// daemon hosts: that job's directory, `<config>/jobs/<short id>`.
pub const JOB_DIR_ENV: &str = "CLAUDE_JOB_DIR";

/// The directory of the background job whose process tree this runs in
/// (`$CLAUDE_JOB_DIR`), as `get` reads the environment. Set for the job's
/// own session and inherited by everything it starts.
pub fn job_dir_in(get: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    get(JOB_DIR_ENV)
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
}

/// `$name`, when this process may take it from Giverny: set, not blank, and
/// not inside a background job.
///
/// The daemon that runs background jobs is detached, and keeps the
/// environment of whatever started it: a tab of some Giverny, a Windows
/// build's, one long closed. Its `GIVERNY_TAB_ID` names that tab, its
/// `GIVERNY_FEED_DIR` that Giverny's feeds (`/mnt/c/…` for a Windows one),
/// its `GIVERNY_PROFILE_DIR` that Giverny's view of the account. Nothing in a
/// job's process tree was told any of it by the Giverny showing the job, so
/// none of it is used there: the job is addressed by its id, and the feeds
/// go to this platform's own directory.
pub fn giverny_var_in(name: &str, get: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    if job_dir_in(get).is_some() {
        return None;
    }
    get(name).filter(|v| !v.trim().is_empty())
}

/// The account a background job runs under: its directory is
/// `<config>/jobs/<id>`.
pub fn job_account_in(get: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    Some(job_dir_in(get)?.parent()?.parent()?.to_path_buf())
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// Does this process run inside a background job ([`job_dir_in`])?
pub fn in_bg_job() -> bool {
    job_dir_in(&env).is_some()
}

/// [`giverny_var_in`] for this process.
pub fn giverny_var(name: &str) -> Option<String> {
    giverny_var_in(name, &env)
}

/// [`job_account_in`] for this process.
pub fn job_account() -> Option<PathBuf> {
    job_account_in(&env)
}

/// The background job whose own session ran this process, by its short id
/// (the job directory's name), or `None` outside one.
///
/// The daemon is detached — its parent is init — so the walk to the app
/// never ends for such a session, and the tab id it carries is whichever
/// tab first started the daemon, not the tab showing the job.
/// The job id is what names it; the app finds the tab parked on it.
///
/// A claude started inside the job's shell inherits `CLAUDE_JOB_DIR` too.
/// The job's own session runs directly under the daemon's `bg-pty-host`; a
/// nested one runs under a shell, and is not the job.
pub fn bg_job() -> Option<String> {
    let dir = std::env::var(JOB_DIR_ENV).ok()?;
    let id = std::path::Path::new(dir.trim())
        .file_name()?
        .to_string_lossy()
        .into_owned();
    if id.is_empty() || !runs_as_bg_worker(std::process::id()) {
        return None;
    }
    Some(id)
}

/// Walk up from `start` to the first claude: is its parent the daemon's
/// `bg-pty-host`? `None` when no claude is found on the way.
pub fn under_pty_host(
    start: u32,
    parent: impl Fn(u32) -> Option<u32>,
    is_claude: impl Fn(u32) -> bool,
    is_pty_host: impl Fn(u32) -> bool,
) -> Option<bool> {
    let mut pid = start;
    for _ in 0..MAX_DEPTH {
        if is_claude(pid) && !is_pty_host(pid) {
            return Some(parent(pid).is_some_and(&is_pty_host));
        }
        match parent(pid) {
            Some(p) if p != pid && p != 0 => pid = p,
            _ => break,
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn runs_as_bg_worker(pid: u32) -> bool {
    let Some(start) = proc_parent(pid) else {
        return false;
    };
    under_pty_host(start, proc_parent, proc_is_claude, |p| {
        std::fs::read(format!("/proc/{p}/cmdline")).is_ok_and(|cmd| is_pty_host_cmdline(&cmd))
    })
    .unwrap_or(false)
}

/// The daemon's pty host, by its command line: it runs with
/// `--bg-pty-host`, under the process title `claude bg-pty-host` (one
/// `argv[0]`, as Claude Code 2.1.292 sets it).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn is_pty_host_cmdline(cmd: &[u8]) -> bool {
    cmd.split(|b| *b == 0)
        .enumerate()
        .any(|(i, arg)| arg == b"--bg-pty-host" || (i == 0 && arg.ends_with(b" bg-pty-host")))
}

/// Claude Code's background daemon is a Unix one; elsewhere a job directory
/// is taken at its word.
#[cfg(not(target_os = "linux"))]
fn runs_as_bg_worker(_pid: u32) -> bool {
    true
}

/// Is a process name Claude Code's? `claude`, or `claude.exe` on Windows.
pub fn is_claude_name(name: &str) -> bool {
    let name = name.trim();
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let base = base
        .strip_suffix(".exe")
        .or_else(|| base.strip_suffix(".EXE"))
        .unwrap_or(base);
    base == "claude"
}

/// The lineage of the session that ran this process, judged against the
/// app named in [`APP_PID_ENV`]. `Unknown` when the variable is missing or
/// unreadable — a tab spawned by a Giverny that did not set it.
pub fn of_this_process() -> Lineage {
    let Some(app) = std::env::var(APP_PID_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
    else {
        return Lineage::Unknown;
    };
    of_process(std::process::id(), app)
}

/// The lineage of the session above `pid`, judged against `app`.
#[cfg(target_os = "linux")]
fn of_process(pid: u32, app: u32) -> Lineage {
    let Some(start) = proc_parent(pid) else {
        return Lineage::Unknown;
    };
    lineage(start, app, proc_parent, proc_is_claude)
}

#[cfg(target_os = "linux")]
pub(crate) fn proc_parent(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Field 4 (ppid) follows the parenthesized comm, which may itself hold
    // spaces or parentheses — split after the last `)`.
    let (_, rest) = stat.rsplit_once(')')?;
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// `comm` names the program as it was run (`claude`, whatever the binary's
/// versioned file is called) and Claude Code also sets it as its process
/// title; `argv[0]` is the second opinion.
#[cfg(target_os = "linux")]
pub(crate) fn proc_is_claude(pid: u32) -> bool {
    if std::fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|c| is_claude_name(&c)) {
        return true;
    }
    std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|cmd| {
        let argv0 = cmd.split(|b| *b == 0).next().unwrap_or_default();
        is_claude_name(&String::from_utf8_lossy(argv0))
    })
}

/// Same walk on other platforms, via `sysinfo`'s process table.
#[cfg(not(target_os = "linux"))]
fn of_process(pid: u32, app: u32) -> Lineage {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_exe(UpdateKind::OnlyIfNotSet),
    );
    let parent = |p: u32| {
        sys.process(Pid::from_u32(p))
            .and_then(|p| p.parent())
            .map(|p| p.as_u32())
    };
    let is_claude = |p: u32| {
        sys.process(Pid::from_u32(p)).is_some_and(|proc| {
            is_claude_name(&proc.name().to_string_lossy())
                || proc
                    .exe()
                    .and_then(|e| e.file_name())
                    .is_some_and(|n| is_claude_name(&n.to_string_lossy()))
        })
    };
    let Some(start) = parent(pid) else {
        return Lineage::Unknown;
    };
    lineage(start, app, parent, is_claude)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A process tree as `(pid, parent, is claude)`.
    fn walk(tree: &[(u32, u32, bool)], start: u32, app: u32) -> Lineage {
        let map: HashMap<u32, (u32, bool)> = tree.iter().map(|&(p, pp, c)| (p, (pp, c))).collect();
        lineage(
            start,
            app,
            |p| map.get(&p).map(|e| e.0),
            |p| map.get(&p).is_some_and(|e| e.1),
        )
    }

    const APP: u32 = 100;

    #[test]
    fn a_claude_inside_the_tabs_claude_is_not() {
        // app → shell → claude → bash → claude → sh → relay
        let tree = [
            (200, APP, false),
            (300, 200, true),
            (310, 300, false),
            (320, 310, true),
            (400, 320, false),
        ];
        assert_eq!(walk(&tree, 400, APP), Lineage::Nested);
    }
}
