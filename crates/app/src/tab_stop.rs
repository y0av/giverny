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
    below(&table(), shell)
}

#[cfg(not(target_os = "linux"))]
pub fn tree(_shell: u32) -> Vec<Proc> {
    Vec::new()
}

/// One `/proc` entry as [`tree`] reads it: the process, its parent, and
/// whether it has already ended (a zombie waiting to be reaped).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Clone, Copy)]
struct Entry {
    proc: Proc,
    ppid: u32,
    ended: bool,
}

/// `root` and every process below it in `table`, by parent links.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn below(table: &[Entry], root: u32) -> Vec<Proc> {
    let mut out: Vec<Proc> = table
        .iter()
        .filter(|e| e.proc.pid == root)
        .map(|e| e.proc)
        .collect();
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for e in table.iter().filter(|e| e.ppid == parent) {
            if !out.iter().any(|p| p.pid == e.proc.pid) {
                out.push(e.proc);
                frontier.push(e.proc.pid);
            }
        }
    }
    out
}

/// A `/proc/<pid>/stat` line read: its parent, start and state come after
/// the last `)`, since the name before it may hold spaces and parentheses.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_stat(pid: u32, stat: &str) -> Option<Entry> {
    let (_, rest) = stat.rsplit_once(')')?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let state = *fields.first()?;
    let ppid = fields.get(1)?.parse().ok()?;
    let start = giverny_claude::registry::stat_start_ticks(stat)?;
    Some(Entry {
        proc: Proc { pid, start },
        ppid,
        ended: state == "Z" || state == "X",
    })
}

#[cfg(target_os = "linux")]
fn read(pid: u32) -> Option<Entry> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parse_stat(pid, &stat)
}

#[cfg(target_os = "linux")]
fn table() -> Vec<Entry> {
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    rd.flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(read)
        .collect()
}

/// `p` still runs: the same pid with the same start, and not a zombie.
#[cfg(target_os = "linux")]
fn alive(p: &Proc) -> bool {
    read(p.pid).is_some_and(|now| !now.ended && now.proc.start == p.start)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_read_short() {
        assert_eq!(took(Duration::from_secs(40)), "40s");
        assert_eq!(took(Duration::from_secs(185)), "3m");
        assert_eq!(took(Duration::from_secs(2 * 3600 + 5 * 60)), "2h05m");
    }

    #[test]
    fn a_long_command_is_cut() {
        assert_eq!(
            command("cargo   build  --release", 40),
            "cargo build --release"
        );
        assert_eq!(command("sleep 100000", 8), "sleep 1…");
    }

    #[test]
    fn agents_are_told_apart_from_commands() {
        assert!(is_agent("claude --resume abc"));
        assert!(is_agent("/home/u/.local/bin/claude"));
        assert!(is_agent("codex"));
        assert!(!is_agent("sleep 100000"));
        assert!(!is_agent("cargo build"));
    }

    #[test]
    fn several_tabs_name_theirs() {
        assert_eq!(
            lines_for(Some("shell"), vec!["sleep 9 (3m)".into()]),
            ["shell: sleep 9 (3m)"]
        );
        assert_eq!(
            lines_for(None, vec!["sleep 9 (3m)".into()]),
            ["sleep 9 (3m)"]
        );
    }

    #[test]
    fn a_tree_is_its_root_and_what_is_below() {
        let e = |pid, ppid| Entry {
            proc: Proc { pid, start: 7 },
            ppid,
            ended: false,
        };
        let table = [e(1, 0), e(10, 1), e(11, 10), e(12, 11), e(20, 1), e(13, 10)];
        let mut pids: Vec<u32> = below(&table, 10).iter().map(|p| p.pid).collect();
        pids.sort_unstable();
        assert_eq!(pids, [10, 11, 12, 13]);
    }

    #[test]
    fn a_stat_line_is_read_past_its_name() {
        let line = "42 (a) b (c) Z 7 42 42 0 -1 4194304 0 0 0 0 0 0 0 0 20 0 1 0 98765 0 0";
        let e = parse_stat(42, line).expect("parsed");
        assert_eq!(e.ppid, 7);
        assert_eq!(e.proc.start, 98765);
        assert!(e.ended, "Z is a zombie");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_tree_is_read_and_its_end_seen() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("sleep");
        let me = std::process::id();
        let procs = tree(me);
        let p = *procs
            .iter()
            .find(|p| p.pid == child.id())
            .expect("the child is below us");
        assert!(alive(&p));
        signal(&[p], libc::SIGTERM);
        let _ = child.wait();
        assert!(!alive(&p), "reaped, it has ended");
        assert!(
            !alive(&Proc {
                pid: me,
                start: p.start + 1_000_000
            }),
            "a pid with another start is another process"
        );
    }
}
