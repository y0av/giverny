//! What a task's `giverny manage run` commands use right now.
//!
//! While a command runs, `run` keeps a small JSON file beside its stats
//! file, `<ledger dir>/runs/<pid>-<n>.live` (`task`, `session`, `pid`,
//! `stats`, `started_ms`, `cwd`: the worktree it works in, which the
//! reach guard keeps other sessions out of, [`crate::reach_guard`], and
//! `bound_mb`: the memory the command is held to, null when nothing holds
//! it; `cmd`: the command, which `pause` names),
//! and removes it when the command ends. The shim
//! inside the command's systemd scope adds `cgroup <dir>` to the stats file
//! as it starts, so a reader finds the scope's cgroup without asking
//! systemd. The app's one sampler ([`crate::use_reading`]) lists them
//! ([`running`]) every couple of seconds and measures each scope's
//! processes (`cgroup.procs`) the way it measures everything else it
//! shows.
//!
//! A command run plain (no user systemd: macOS) registers the same file,
//! with no cgroup in its stats: its processes are the `run` process's and
//! everything below it ([`running_plain`]), which the app holds to the
//! limits itself ([`crate::user_cap`]) and the ledger counts
//! as building. A `.live` file whose `run` is gone (killed) is skipped by
//! readers and swept by the next `run`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// A task's commands' use, summed over its running commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RunLive {
    /// CPU over the last interval, percent of the whole machine (every
    /// core busy is 100).
    pub cpu_pct: u32,
    /// The memory its processes really use (proportional sets), MiB.
    pub mem_mb: u64,
    /// GPU memory of its processes, MiB; `None` when the machine
    /// has no NVIDIA GPU (or `nvidia-smi`'s answer is not in yet).
    pub gpu_mb: Option<u64>,
}

/// One task's use, keyed as the ledger keys leases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskLive {
    pub session: String,
    pub task: String,
    pub live: RunLive,
    /// The worker whose commands the run is under, when one is: that
    /// worker's own figure already holds the run's.
    pub agent: Option<String>,
}

impl RunLive {
    /// Two disjoint sets of processes' use together.
    pub fn plus(self, other: RunLive) -> RunLive {
        RunLive {
            cpu_pct: (self.cpu_pct + other.cpu_pct).min(100),
            mem_mb: self.mem_mb + other.mem_mb,
            gpu_mb: match (self.gpu_mb, other.gpu_mb) {
                (Some(a), Some(b)) => Some(a + b),
                (a, b) => a.or(b),
            },
        }
    }
}

/// `<ledger dir>/runs`.
pub fn runs_dir(ledger: &Path) -> PathBuf {
    ledger.parent().unwrap_or(Path::new(".")).join("runs")
}

/// The `.live` file beside `stats` (`…/runs/4242-0.stats` →
/// `…/runs/4242-0.live`).
pub fn live_path(stats: &Path) -> PathBuf {
    stats.with_extension("live")
}

/// Removes its `.live` file when dropped.
pub struct Registered(PathBuf);

impl Drop for Registered {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Say that `task`'s command, measured through `stats`, is running, held
/// to nothing. Sweeps `.live` files whose `run` is gone first. A failure
/// only costs the pane its live figure, so it is `None`, not an error.
pub fn register(stats: &Path, task: &str, session: &str, now_ms: u64) -> Option<Registered> {
    register_bound(stats, task, session, now_ms, None, &[])
}

/// [`register`], for a command held to `bound_mb` of memory (a scope's
/// `MemoryHigh`, Giverny's own kill), which the ledger counts its lease at
/// until it ends ([`crate::resources::RunBounds`]). `cmd` is
/// kept for `pause` to name it ([`task_runs`]).
pub fn register_bound(
    stats: &Path,
    task: &str,
    session: &str,
    now_ms: u64,
    bound_mb: Option<u64>,
    cmd: &[String],
) -> Option<Registered> {
    if let Some(dir) = stats.parent() {
        sweep(dir);
    }
    let path = live_path(stats);
    let body = json!({
        "task": task,
        "session": session,
        "pid": std::process::id(),
        "stats": stats.display().to_string(),
        "started_ms": now_ms,
        "cwd": std::env::current_dir().ok().map(|d| d.display().to_string()),
        "bound_mb": bound_mb,
        "cmd": (!cmd.is_empty()).then(|| cmd.join(" ")),
    });
    let tmp = path.with_extension("live.tmp");
    std::fs::write(&tmp, body.to_string()).ok()?;
    std::fs::rename(&tmp, &path).ok()?;
    Some(Registered(path))
}

/// Remove `.live` files whose `run` process is gone.
fn sweep(dir: &Path) {
    for (path, l) in read_dir(dir) {
        if !alive(l.pid) {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[derive(Debug, Clone)]
struct LiveFile {
    task: String,
    session: String,
    pid: u32,
    stats: PathBuf,
    started_ms: Option<u64>,
    cwd: Option<PathBuf>,
    /// Held to that much memory; `None` when nothing holds it (or the file
    /// is an older version's and does not say).
    bound_mb: Option<u64>,
    /// The command, when the file says.
    cmd: Option<String>,
}

fn parse_live(bytes: &[u8]) -> Option<LiveFile> {
    let v: Value = serde_json::from_slice(bytes).ok()?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    Some(LiveFile {
        task: s("task")?,
        session: s("session")?,
        pid: v.get("pid").and_then(Value::as_u64)? as u32,
        stats: PathBuf::from(s("stats")?),
        started_ms: v.get("started_ms").and_then(Value::as_u64),
        cwd: s("cwd").map(PathBuf::from),
        bound_mb: v.get("bound_mb").and_then(Value::as_u64),
        cmd: s("cmd"),
    })
}

fn read_dir(dir: &Path) -> Vec<(PathBuf, LiveFile)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "live"))
        .filter_map(|p| {
            let l = parse_live(&std::fs::read(&p).ok()?)?;
            Some((p, l))
        })
        .collect()
}

/// The process is there (Linux: `/proc/<pid>`; other Unix: `kill(pid,
/// 0)`, which only asks; elsewhere assumed).
pub(crate) fn alive(pid: u32) -> bool {
    if cfg!(target_os = "linux") {
        return Path::new(&format!("/proc/{pid}")).exists();
    }
    #[cfg(unix)]
    {
        // SAFETY: signal 0 sends nothing; it only checks the pid exists.
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// The leases (`<session>:<task>`) with a `giverny manage run` command
/// going under `runs_dir`, scope or not: who is building now, for the
/// ledger's CPU-busy check, which must work where there is
/// no cgroup (macOS) too.
pub fn running_leases(runs_dir: &Path) -> HashSet<String> {
    read_dir(runs_dir)
        .into_iter()
        .filter(|(_, l)| alive(l.pid))
        .map(|(_, l)| crate::resources::lease_id(&l.session, &l.task))
        .collect()
}

/// Each lease (`<session>:<task>`) with a `run` command going under
/// `runs_dir`: the most any of them is held to, `None` when one is held to
/// nothing.
pub fn running_bounds(runs_dir: &Path) -> HashMap<String, Option<u64>> {
    let mut out: HashMap<String, Option<u64>> = HashMap::new();
    for (_, l) in read_dir(runs_dir).into_iter().filter(|(_, l)| alive(l.pid)) {
        let id = crate::resources::lease_id(&l.session, &l.task);
        let b = out.entry(id).or_insert(Some(0));
        *b = match (*b, l.bound_mb) {
            (Some(a), Some(n)) => Some(a.max(n)),
            _ => None,
        };
    }
    out
}

/// One `giverny manage run` command's record, whose `run` may be gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRecord {
    /// The `run` process, whose scope is `giverny-run-<task>-<pid>`.
    pub pid: u32,
    pub session: String,
    pub task: String,
    /// Where it was started, when it said.
    pub cwd: Option<PathBuf>,
}

/// Every `.live` file under `runs_dir`, its `run` alive or not (one that is
/// gone stays until the next `run` sweeps it): who a `giverny-run-…` scope
/// belongs to ([`crate::reach_guard`]).
pub fn records(runs_dir: &Path) -> Vec<RunRecord> {
    read_dir(runs_dir)
        .into_iter()
        .map(|(_, l)| RunRecord {
            pid: l.pid,
            session: l.session,
            task: l.task,
            cwd: l.cwd,
        })
        .collect()
}

/// One of a task's `run` commands still going: what `pause` names,
/// for the manager to stop or keep. Giverny stops nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveRun {
    /// The `run` process. A SIGTERM to it stops the command's scope, or
    /// run plain its whole tree, and still records and releases:
    /// `kill <pid>`.
    pub pid: u32,
    /// The command, when its `.live` file says (older ones do not).
    pub cmd: Option<String>,
    /// When it started, epoch ms.
    pub started_ms: Option<u64>,
}

/// `session`'s `run` commands for `task` that are still going under
/// `runs_dir`, oldest first.
pub fn task_runs(runs_dir: &Path, session: &str, task: &str) -> Vec<LiveRun> {
    session_runs(runs_dir, session)
        .into_iter()
        .filter(|(t, _)| t == task)
        .map(|(_, r)| r)
        .collect()
}

/// `session`'s `run` commands still going under `runs_dir`, each with its
/// task, oldest first.
pub fn session_runs(runs_dir: &Path, session: &str) -> Vec<(String, LiveRun)> {
    let mut out: Vec<(String, LiveRun)> = read_dir(runs_dir)
        .into_iter()
        .filter(|(_, l)| l.session == session && alive(l.pid))
        .map(|(_, l)| {
            let r = LiveRun {
                pid: l.pid,
                cmd: l.cmd,
                started_ms: l.started_ms,
            };
            (l.task, r)
        })
        .collect();
    out.sort_by_key(|(_, r)| (r.started_ms, r.pid));
    out
}

/// The `cgroup <dir>` line the shim wrote into a stats file.
pub fn cgroup_of(stats_text: &str) -> Option<PathBuf> {
    stats_text
        .lines()
        .find_map(|l| l.strip_prefix("cgroup "))
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
}

/// The processes in a cgroup (`cgroup.procs`).
pub fn cgroup_pids(dir: &Path) -> HashSet<u32> {
    std::fs::read_to_string(dir.join("cgroup.procs"))
        .map(|t| t.lines().filter_map(|l| l.trim().parse().ok()).collect())
        .unwrap_or_default()
}

/// Every running command under `runs_dir` with a scope of its own:
/// `(session, task, its cgroup)`. A `.live` file whose `run` is gone, or
/// whose command runs plain (no cgroup), is left out.
pub fn running(runs_dir: &Path) -> Vec<(String, String, PathBuf)> {
    running_since(runs_dir)
        .into_iter()
        .map(|(s, t, cg, _)| (s, t, cg))
        .collect()
}

/// [`running`], with when each command started (epoch ms, `None` from a
/// `.live` file that does not say).
pub fn running_since(runs_dir: &Path) -> Vec<(String, String, PathBuf, Option<u64>)> {
    read_dir(runs_dir)
        .into_iter()
        .filter(|(_, l)| alive(l.pid))
        .filter_map(|(_, l)| {
            let cg = std::fs::read_to_string(&l.stats)
                .ok()
                .and_then(|t| cgroup_of(&t))?;
            Some((l.session, l.task, cg, l.started_ms))
        })
        .collect()
}

/// Every running command under `runs_dir` with no scope of its own (run
/// plain): `(session, task, the run process)`, whose command is below it.
pub fn running_plain(runs_dir: &Path) -> Vec<(String, String, u32)> {
    read_dir(runs_dir)
        .into_iter()
        .filter(|(_, l)| alive(l.pid))
        .filter(|(_, l)| {
            std::fs::read_to_string(&l.stats)
                .ok()
                .and_then(|t| cgroup_of(&t))
                .is_none()
        })
        .map(|(_, l)| (l.session, l.task, l.pid))
        .collect()
}
