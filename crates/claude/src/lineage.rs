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
