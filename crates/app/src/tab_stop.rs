//! Stopping a tab and keeping it: the rail's `stop` and
//! `stop all`. What the tab runs ends; the tab stays, back in the state a
//! tab restored after a restart is in, and starts the same way when it is
//! next focused — Claude resuming its conversation.
//!
//! **How it ends.** The way closing a terminal ends what runs in it: the
//! tab's pty is hung up ([`TermSession::shutdown_within`],
//! what closing a tab does), so the shell gets `SIGHUP`, hands it on to its
//! jobs and goes, and Claude Code, which writes its transcript as it goes,
//! exits on it with its conversation on disk. What the tab had started is
//! read before the hangup; whatever of it still runs a moment after is
//! sent `SIGTERM` (with a `SIGCONT` first, for one a duty cycle holds
//! stopped), and only what outlasts that gets `SIGKILL`.

use std::time::{Duration, Instant};

use giverny_term::session::TermSession;

/// How long the shell has to go on the hangup, as closing a tab allows.
const HANGUP_WAIT: Duration = Duration::from_secs(10);
/// How long what the tab started has after the hangup before `SIGTERM`.
const TERM_AFTER: Duration = Duration::from_secs(3);
/// How long after `SIGTERM` before `SIGKILL`.
const KILL_AFTER: Duration = Duration::from_secs(3);

/// One process the tab started: its pid and when it started (clock ticks
/// after boot), so a pid taken by another process since is left alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Proc {
    pub pid: u32,
    pub start: u64,
}

/// The tab's shell and everything below it, as it is now: read before the
/// hangup, since what outlives its parent is handed to init and no longer
/// reads as the tab's.
#[cfg(target_os = "linux")]
pub fn tree(shell: u32) -> Vec<Proc> {
    let procs = table();
    let below = giverny_claude::session_use::subtree(&procs, shell);
    procs
        .iter()
        .filter(|p| below.contains(&p.pid))
        .map(|p| Proc {
            pid: p.pid,
            start: p.start,
        })
        .collect()
}

#[cfg(not(target_os = "linux"))]
pub fn tree(_shell: u32) -> Vec<Proc> {
    Vec::new()
}

#[cfg(target_os = "linux")]
fn table() -> Vec<giverny_claude::session_use::Proc> {
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    rd.flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(|pid| {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            giverny_claude::session_use::parse_stat(pid, &stat, 4)
        })
        .collect()
}

/// `p` still runs: the same pid with the same start, and not a zombie.
#[cfg(target_os = "linux")]
fn alive(p: &Proc) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{}/stat", p.pid)) else {
        return false;
    };
    let zombie = stat
        .rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .is_some_and(|state| state == "Z" || state == "X");
    !zombie
        && giverny_claude::session_use::parse_stat(p.pid, &stat, 4)
            .is_some_and(|now| now.start == p.start)
}

#[cfg(not(target_os = "linux"))]
fn alive(_p: &Proc) -> bool {
    false
}

#[cfg(unix)]
fn signal(procs: &[Proc], sig: libc::c_int) {
    for p in procs {
        // SAFETY: kill only sends a signal.
        unsafe {
            libc::kill(p.pid as libc::pid_t, sig);
        }
    }
}

/// End `session` and what it started (`tree`, read by [`tree`] before),
/// off the UI thread: hang up, then `SIGTERM`, then `SIGKILL`, each only
/// for what the step before left. `done` runs once it is over.
pub fn end(session: TermSession, tree: Vec<Proc>, done: impl FnOnce() + Send + 'static) {
    let spawned = std::thread::Builder::new()
        .name("giverny tab stop".into())
        .spawn(move || {
            let gone = session.shutdown_within(HANGUP_WAIT);
            let left = wait_out(&tree, TERM_AFTER);
            if !left.is_empty() {
                tracing::info!(
                    "stop: {} process(es) outlived the hangup{}; SIGTERM",
                    left.len(),
                    if gone { "" } else { " (the shell too)" }
                );
                #[cfg(unix)]
                {
                    signal(&left, libc::SIGCONT);
                    signal(&left, libc::SIGTERM);
                }
                let left = wait_out(&left, KILL_AFTER);
                if !left.is_empty() {
                    tracing::warn!("stop: {} process(es) outlived SIGTERM; SIGKILL", left.len());
                    #[cfg(unix)]
                    signal(&left, libc::SIGKILL);
                }
            }
            done();
        });
    if let Err(err) = spawned {
        tracing::error!("stop: no thread to end the tab on: {err}");
    }
}

/// Wait up to `wait` for `procs` to end; what still runs then.
fn wait_out(procs: &[Proc], wait: Duration) -> Vec<Proc> {
    let until = Instant::now() + wait;
    loop {
        let left: Vec<Proc> = procs.iter().copied().filter(alive).collect();
        if left.is_empty() || Instant::now() >= until {
            return left;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// How long something has run, the way the confirm says it: `40s`, `3m`,
/// `2h05m`.
pub fn took(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        _ => format!("{}h{:02}m", s / 3600, s / 60 % 60),
    }
}

/// A command as the confirm names it: its words, cut to `max` characters.
pub fn command(line: &str, max: usize) -> String {
    let words = line.split_whitespace().collect::<Vec<_>>().join(" ");
    if words.chars().count() <= max {
        return words;
    }
    let mut cut: String = words.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// The program `line` runs is an agent Giverny reads on its own (Claude
/// Code, Codex), whose work the confirm names from that reading instead.
pub fn is_agent(line: &str) -> bool {
    matches!(giverny_core::procs::program_name(line), "claude" | "codex")
}

/// The confirm's lines for one tab: `what` runs, prefixed by the tab's
/// `title` when the stop is for several tabs.
pub fn lines_for(title: Option<&str>, what: Vec<String>) -> Vec<String> {
    what.into_iter()
        .map(|w| match title {
            Some(t) => format!("{t}: {w}"),
            None => w,
        })
        .collect()
}
