//! Every claude in a Giverny tab, held by the kernel inside the machine's
//! budget, which it cannot forget or opt out of.
//!
//! A tab is a plain shell: Giverny cannot know at spawn which tab will run
//! Claude Code, and a shell tab must stay as it is. So the claude itself is
//! taken, as soon as the app's sampler sees it under a tab (its pass is
//! once a second): the claude, and whatever it has started by then, is
//! moved into a transient systemd scope of its own,
//! `giverny-claude-<pid>.scope`, inside one slice, [`SLICE`]. Everything it
//! starts after that — its subagents (in-process), every Bash command,
//! every build — is born in that scope, inside the slice.
//!
//! - **The slice is the one ceiling**: hard `CPUQuota` and `MemoryMax`
//!   (with no swap) at `[manager.limits]` (cores − 2 and 70 % of RAM unless
//!   set), for all Claude tabs and every `giverny manage run` scope
//!   together ([`crate::manage_run`] puts its scopes here too). It is what
//!   keeps the desktop responsive.
//! - **A tab's scope holds guarantees, not ceilings**: `CPUWeight`
//!   [`TAB_CPU_WEIGHT`], above every `giverny manage run` scope's
//!   ([`crate::manage_run::RUN_CPU_WEIGHT`]), so the person's interactive
//!   claude wins contention against worker builds; and `MemoryLow` at the
//!   default lease's RAM (`[management_panel.lease]`). CPU is shared and
//!   measured, never reserved: no lease's cores weigh
//!   anything. Idle cores and free memory are anyone's; under contention
//!   the CPU splits by weight, and when the slice is at its ceiling the
//!   kernel reclaims from scopes above their protection first. (The
//!   protection counts against the slice's own limit; under pressure on
//!   the whole machine it does not, as the slices above ours protect
//!   nothing.)
//! - **Commands die before claude.** When the slice runs out of memory the
//!   kernel kills the process with the highest badness in it. Every
//!   process under a claude in the slice, but no claude, gets
//!   `oom_score_adj` [`COMMAND_OOM_ADJ`] (raising it needs no privilege,
//!   and what they start inherits it), so the runaway command is the one
//!   killed, not a session. `OOMPolicy=continue` keeps systemd from
//!   stopping the scope, and the claude in it, after a kill.
//! - **Plain shells are untouched**: only a process the sampler recognises
//!   as claude is moved, and the shell that started it stays where it is.
//!
//! The move is systemd's `StartTransientUnit` with the pids (`busctl
//! --user`): `systemd-run` can only start a new command, not adopt one.
//! A claude already inside the slice (one started by a capped claude, or
//! adopted on an earlier pass) is left alone. Where there is no user
//! systemd (or `$GIVERNY_RUN_NO_SYSTEMD` is set) nothing is moved, and the
//! log says so once. On macOS, which has no cgroups, the same sampler
//! holds the same limits itself instead ([`crate::user_cap`]):
//! it kills the biggest command over the RAM limit and
//! duty-cycles the busy ones over the CPU limit, from what it measures,
//! with the gaps a sampler has.
//!
//! **Before the first command**. A pass comes up to a second
//! after the claude appears, and every command used to start at the
//! claude's own `oom_score_adj` (0) until the next pass raised it. So the
//! plugin's `SessionStart` hook does the same move itself, before the
//! session runs anything ([`on_session_start`]), and adds a line to the
//! session's `$CLAUDE_ENV_FILE`, which Claude Code sources in every Bash
//! command's shell, raising that shell's `oom_score_adj` before the command
//! starts. A claude with no plugin hook (one started before the plugin
//! was written) still waits for the pass.
//!
//! What it does not hold: anything a command reaches outside its own
//! process tree (a daemon it talks to, a Windows program through WSL
//! interop), and a process that is not a Bash command (an MCP server) in
//! the second before the pass raises it. Leaving it is not refused:
//! the plugin's Bash guard ([`crate::bash_guard`]) reminds
//! the session of the cap once per kind of escape, and each pass's sweep
//! ([`crate::escapes::Sweep`]) finds what left anyway and tells its
//! session, without moving it back. The memory cap is the floor.

use std::collections::{HashMap, HashSet};

use giverny_core::config::DefaultLease;
use giverny_core::limits::Resolved;

use crate::session_use::Proc;

/// The slice every Claude tab's scope and every `giverny manage run` scope
/// sits in. systemd reads the dash as nesting: it is
/// `giverny.slice/giverny-claude.slice` under the user's manager.
pub const SLICE: &str = "giverny-claude.slice";

/// The name of a claude's scope.
pub fn scope_name(pid: u32) -> String {
    format!("giverny-claude-{pid}.scope")
}

/// Is a `/proc/<pid>/cgroup` text inside [`SLICE`]?
pub fn in_slice(cgroup: &str) -> bool {
    cgroup
        .lines()
        .filter_map(|l| l.strip_prefix("0::"))
        .any(|path| path.split('/').any(|part| part == SLICE))
}

/// `systemctl --user set-property` arguments that cap the slice at the
/// limits.
pub fn slice_props(limits: &Resolved) -> Vec<String> {
    let mut a: Vec<String> = ["--user", "set-property", "--runtime", SLICE]
        .map(String::from)
        .into();
    if limits.cpu_cores > 0 {
        a.push(format!("CPUQuota={}%", limits.cpu_cores * 100));
    }
    if limits.ram.0 > 0 {
        a.push(format!("MemoryMax={}M", limits.ram.0));
        a.push("MemorySwapMax=0".into());
    }
    a
}

/// Every Claude tab's scope's `CPUWeight`: four times a
/// `run` scope's ([`crate::manage_run::RUN_CPU_WEIGHT`]), so when the cores
/// are contended the person's interactive claude (and the commands it runs
/// itself) goes ahead of the workers' builds, which still get every idle
/// core.
pub const TAB_CPU_WEIGHT: u64 = 400;
// The tabs win contention against the runs, or this does not build.
const _: () = assert!(TAB_CPU_WEIGHT > crate::manage_run::RUN_CPU_WEIGHT);

/// What a claude's commands get in `oom_score_adj`: when the slice runs
/// out of memory the kernel kills the process with the highest badness
/// (its share of memory, plus this in thousandths), so a command goes
/// before any claude (left at its own, 0). Raising it needs no privilege.
pub const COMMAND_OOM_ADJ: i32 = 500;

/// `busctl` arguments that start `unit`, a scope in [`SLICE`] holding
/// `pids`, weighted at [`TAB_CPU_WEIGHT`] and protected at `lease`'s RAM.
pub fn adopt_args(unit: &str, pids: &[u32], lease: &DefaultLease) -> Vec<String> {
    let mut a: Vec<String> = [
        "--user",
        "call",
        "org.freedesktop.systemd1",
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
        "StartTransientUnit",
        "ssa(sv)a(sa(sv))",
        unit,
        "fail",
    ]
    .map(String::from)
    .into();
    let mut props: Vec<Vec<String>> = Vec::new();
    let mut pid_prop: Vec<String> = vec!["PIDs".into(), "au".into(), pids.len().to_string()];
    pid_prop.extend(pids.iter().map(u32::to_string));
    props.push(pid_prop);
    props.push(vec!["Slice".into(), "s".into(), SLICE.into()]);
    props.push(vec![
        "Description".into(),
        "s".into(),
        format!(
            "Giverny: claude {}, {} protected",
            pids.first().copied().unwrap_or(0),
            lease.ram
        ),
    ]);
    // Guarantees, not ceilings: the slice is the only ceiling.
    props.push(vec![
        "CPUWeight".into(),
        "t".into(),
        TAB_CPU_WEIGHT.to_string(),
    ]);
    if lease.ram.0 > 0 {
        props.push(vec![
            "MemoryLow".into(),
            "t".into(),
            (lease.ram.0 * 1024 * 1024).to_string(),
        ]);
    }
    props.push(vec!["OOMPolicy".into(), "s".into(), "continue".into()]);
    props.push(vec![
        "CollectMode".into(),
        "s".into(),
        "inactive-or-failed".into(),
    ]);
    a.push(props.len().to_string());
    a.extend(props.into_iter().flatten());
    a.push("0".into()); // no auxiliary units
    a
}

/// The claudes to adopt this pass: each claude in `claudes` with no other
/// of them above it (a claude inside a claude rides with the outer one),
/// not seen before (`seen`, by pid and start time).
pub fn outermost(procs: &[Proc], claudes: &HashSet<u32>, seen: &HashMap<u32, u64>) -> Vec<u32> {
    let parent: HashMap<u32, u32> = procs.iter().map(|p| (p.pid, p.ppid)).collect();
    let start: HashMap<u32, u64> = procs.iter().map(|p| (p.pid, p.start)).collect();
    let mut out: Vec<u32> = claudes
        .iter()
        .copied()
        .filter(|pid| seen.get(pid) != start.get(pid))
        .filter(|&pid| {
            let mut p = pid;
            for _ in 0..64 {
                match parent.get(&p) {
                    Some(&pp) if pp != p && pp > 1 => {
                        if claudes.contains(&pp) {
                            return false;
                        }
                        p = pp;
                    }
                    _ => break,
                }
            }
            true
        })
        .collect();
    out.sort_unstable();
    out
}

/// `root` and every process below it whose cgroup is `root`'s
/// (`cgroup_of`): what is moved with it. A child already in a scope of its
/// own (a `giverny manage run`) stays in it.
pub fn moved_with(
    procs: &[Proc],
    root: u32,
    cgroup_of: impl Fn(u32) -> Option<String>,
) -> Vec<u32> {
    let Some(home) = cgroup_of(root) else {
        return vec![root];
    };
    let mut out: Vec<u32> = crate::session_use::subtree(procs, root)
        .into_iter()
        .filter(|&p| p == root || cgroup_of(p).as_deref() == Some(home.as_str()))
        .collect();
    out.sort_unstable();
    // The claude first: the scope's description names it.
    if let Some(i) = out.iter().position(|&p| p == root) {
        out.swap(0, i);
    }
    out
}

/// Whether systemd was asked yet, and what it said.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum State {
    #[default]
    Unasked,
    Usable,
    Off,
}

/// The processes under the capped claudes `roots` that are not claudes
/// themselves and not handled before (`done`, by pid → start time): the
/// ones whose `oom_score_adj` is raised to [`COMMAND_OOM_ADJ`].
pub fn commands_under(
    procs: &[Proc],
    roots: &HashSet<u32>,
    claudes: &HashSet<u32>,
    done: &HashMap<u32, u64>,
) -> Vec<u32> {
    let start: HashMap<u32, u64> = procs.iter().map(|p| (p.pid, p.start)).collect();
    let mut out: Vec<u32> = roots
        .iter()
        .flat_map(|&r| crate::session_use::subtree(procs, r))
        .filter(|p| !claudes.contains(p))
        .filter(|p| done.get(p) != start.get(p))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// The claude a hook process runs for, when it is a tab's own: walking up
/// from `start` (the hook's parent) through `parent`, the first process
/// `is_claude` knows, with no second claude between it and `app`. `None`
/// for a claude inside a claude (it rides with the outer one), one whose
/// parents never reach the app (a multiplexer's server, a background
/// job's daemon: the sampler, which walks down from the app, never sees
/// those either), or no claude at all.
pub fn tab_claude(
    start: u32,
    app: u32,
    parent: impl Fn(u32) -> Option<u32>,
    is_claude: impl Fn(u32) -> bool,
) -> Option<u32> {
    let mut found = None;
    let mut pid = start;
    for _ in 0..64 {
        if pid == app {
            return found;
        }
        if is_claude(pid) {
            if found.is_some() {
                return None;
            }
            found = Some(pid);
        }
        match parent(pid) {
            Some(p) if p != pid && p != 0 => pid = p,
            _ => return None,
        }
    }
    None
}

/// What a capped tab's session start adds to `$CLAUDE_ENV_FILE`, which
/// Claude Code sources in the shell of every Bash command it runs: the
/// shell puts itself first in line for an OOM kill
/// ([`COMMAND_OOM_ADJ`]) before the command starts, and the command
/// inherits it, where the sampler's raise comes up to a second later.
/// Silent and never failing, so a shell where `/proc`
/// cannot be written runs the command as before.
pub fn env_file_line() -> String {
    format!(
        "{{ echo {COMMAND_OOM_ADJ} > /proc/self/oom_score_adj; }} 2>/dev/null || :  \
         # Giverny: commands die before claude\n"
    )
}

/// Add [`env_file_line`] to `text` (an env file's contents), unless it is
/// there already (a resume or compact of the same session).
pub fn with_env_line(text: &str) -> Option<String> {
    let line = env_file_line();
    if text.contains(line.trim_end()) {
        return None;
    }
    let mut out = text.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&line);
    Some(out)
}

/// The session start's half of the cap, run by the plugin's
/// `SessionStart` hook: the sampler adopts a claude on its next pass, up
/// to a second after it appears, and what the claude starts and detaches
/// before then, or the first second of every command before its
/// `oom_score_adj` is raised, is outside. So the hook does both itself,
/// before the session's first command:
///
/// - a tab's own claude ([`tab_claude`]) not yet in [`SLICE`] is moved
///   into its scope there, with what shares its cgroup, as the sampler
///   would (whose later pass finds it in the slice and leaves it);
/// - a claude in the slice (moved now or before) has
///   [`env_file_line`] added to its `$CLAUDE_ENV_FILE` (`env_file`), so
///   every Bash command starts first in line for an OOM kill.
///
/// Nothing for a session that is no tab's own, where there is no user
/// systemd (or `$GIVERNY_RUN_NO_SYSTEMD` is set), or anywhere but Linux.
/// Quiet whatever happens: a failure leaves it to the sampler.
#[cfg(target_os = "linux")]
pub fn on_session_start(env_file: Option<&std::path::Path>) {
    use crate::lineage;
    let Some(app) = std::env::var(lineage::APP_PID_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
    else {
        return;
    };
    if lineage::in_bg_job() || lineage::giverny_var("GIVERNY_TAB_ID").is_none() {
        return;
    }
    let Some(start) = lineage::proc_parent(std::process::id()) else {
        return;
    };
    let Some(claude) = tab_claude(start, app, lineage::proc_parent, lineage::proc_is_claude) else {
        return;
    };
    let cgroup_of = |pid: u32| std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok();
    let Some(cg) = cgroup_of(claude) else { return };
    let capped = in_slice(&cg) || adopt_now(claude, cgroup_of);
    if capped && let Some(f) = env_file {
        let now = std::fs::read_to_string(f).unwrap_or_default();
        if let Some(text) = with_env_line(&now) {
            let _ = std::fs::write(f, text);
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn on_session_start(_env_file: Option<&std::path::Path>) {}

/// Move `claude`, with what shares its cgroup, into its scope in [`SLICE`]
/// now, the slice capped first: what the sampler's pass does
/// ([`Adopter::tick`]). Whether it is in the slice after.
#[cfg(target_os = "linux")]
fn adopt_now(claude: u32, cgroup_of: impl Fn(u32) -> Option<String>) -> bool {
    if std::env::var_os(crate::manage_run::NO_SYSTEMD_ENV).is_some_and(|v| !v.is_empty()) {
        return false;
    }
    // A slice already there was capped by whoever made it (the sampler, a
    // `run`), and is capped again by the sampler when the limits change:
    // no systemd round trip for it on every session's start.
    if !slice_dir(&cgroup_of(claude).unwrap_or_default()).is_some_and(|d| d.is_dir()) {
        let _ = cap_slice_now();
    }
    let page_kb = crate::session_use::sysconf(libc::_SC_PAGESIZE).unwrap_or(4096) / 1024;
    let procs = crate::session_use::proc_table(page_kb);
    let pids = moved_with(&procs, claude, &cgroup_of);
    let lease = DefaultLease::load();
    run_quiet("busctl", &adopt_args(&scope_name(claude), &pids, &lease)).is_ok()
        // The sampler's pass may have taken it a moment before.
        || cgroup_of(claude).is_some_and(|cg| in_slice(&cg))
}

/// Where [`SLICE`]'s cgroup is, under the user manager a process with
/// `cgroup` (its `/proc/<pid>/cgroup` text) runs under; `None` outside one.
pub fn slice_dir(cgroup: &str) -> Option<std::path::PathBuf> {
    let path = cgroup.lines().find_map(|l| l.strip_prefix("0::"))?;
    let end = path.find(".service/").or_else(|| {
        path.trim_end()
            .ends_with(".service")
            .then(|| path.trim_end().len() - ".service".len())
    })?;
    let manager = &path[..end + ".service".len()];
    manager
        .rsplit('/')
        .next()
        .is_some_and(|m| m.starts_with("user@"))
        .then(|| std::path::PathBuf::from(format!("/sys/fs/cgroup{manager}/giverny.slice/{SLICE}")))
}

/// The app's sampler's half of the cap: [`Adopter::tick`] each pass.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Debug, Default)]
pub struct Adopter {
    state: State,
    /// Claudes handled, by pid → start time (a pid that comes back with
    /// another start is another process).
    seen: HashMap<u32, u64>,
    /// The outermost claudes that are in the slice: whose commands are
    /// put first in line for an OOM kill.
    capped: HashSet<u32>,
    /// Commands whose `oom_score_adj` was raised, by pid → start time.
    raised: HashMap<u32, u64>,
    /// The slice properties last set, so they are set again only when the
    /// limits change.
    slice_set: Option<Vec<String>>,
    failures: u32,
    /// What left the slice since: told to its session,
    /// never moved back.
    sweep: crate::escapes::Sweep,
    look: Option<crate::escapes::System>,
}

impl Adopter {
    /// Move each claude not yet in the slice into a scope of its own
    /// there, and put every command under a capped claude first in line
    /// for an OOM kill. `procs` is the pass's process table; `claudes` the
    /// claudes under the app.
    #[cfg(target_os = "linux")]
    pub fn tick(&mut self, procs: &[Proc], claudes: &HashSet<u32>) {
        if self.state == State::Off {
            return;
        }
        self.seen.retain(|pid, _| claudes.contains(pid));
        self.capped.retain(|pid| claudes.contains(pid));
        let start: HashMap<u32, u64> = procs.iter().map(|p| (p.pid, p.start)).collect();
        self.raised.retain(|pid, _| start.contains_key(pid));
        let cgroup_of = |pid: u32| std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok();
        for pid in outermost(procs, claudes, &self.seen) {
            self.seen
                .insert(pid, start.get(&pid).copied().unwrap_or_default());
            let Some(cg) = cgroup_of(pid) else { continue };
            if in_slice(&cg) {
                self.capped.insert(pid);
                continue;
            }
            if self.state == State::Unasked {
                self.state = match crate::manage_run::detect_mode() {
                    crate::manage_run::Mode::Systemd { .. } => State::Usable,
                    crate::manage_run::Mode::Plain => {
                        tracing::warn!(
                            "Claude tabs run uncapped: no user systemd here (or ${} set), \
                             so their claude is not put in {SLICE}",
                            crate::manage_run::NO_SYSTEMD_ENV
                        );
                        State::Off
                    }
                };
                if self.state == State::Off {
                    return;
                }
            }
            self.cap_slice();
            let lease = DefaultLease::load();
            let pids = moved_with(procs, pid, cgroup_of);
            let unit = scope_name(pid);
            match run_quiet("busctl", &adopt_args(&unit, &pids, &lease)) {
                Ok(()) => {
                    self.capped.insert(pid);
                    tracing::info!(
                        "claude {pid} in {SLICE}/{unit}: weight {TAB_CPU_WEIGHT}, {} protected \
                         ({} process(es) moved)",
                        lease.ram,
                        pids.len()
                    );
                }
                // The claude's session-start hook took it a moment before:
                // its scope is there already.
                Err(_) if cgroup_of(pid).is_some_and(|cg| in_slice(&cg)) => {
                    self.capped.insert(pid);
                }
                Err(err) => {
                    self.failures += 1;
                    if self.failures == 1 {
                        tracing::warn!("claude {pid} runs uncapped: {unit} not started: {err}");
                    } else {
                        tracing::debug!("claude {pid} runs uncapped: {unit} not started: {err}");
                    }
                }
            }
        }
        for pid in commands_under(procs, &self.capped, claudes, &self.raised) {
            self.raised
                .insert(pid, start.get(&pid).copied().unwrap_or_default());
            raise_oom_adj(pid);
        }
        let look = self.look.get_or_insert_with(crate::escapes::System::new);
        let found = self.sweep.tick(
            procs,
            &self.capped,
            claudes,
            look,
            crate::session_use::now_ms(),
        );
        if !found.is_empty() {
            for (session, f) in &found {
                tracing::info!(
                    "pid {} of session {session} runs outside {SLICE} (in {}): told on its next call",
                    f.pid,
                    f.cgroup
                );
            }
            crate::escapes::record(&crate::feed::feed_dir(), &found);
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub fn tick(&mut self, _procs: &[Proc], _claudes: &HashSet<u32>) {}

    /// Cap the slice at the limits, when they are not what was set last.
    #[cfg(target_os = "linux")]
    fn cap_slice(&mut self) {
        let args = slice_args_now();
        if self.slice_set.as_ref() == Some(&args) {
            return;
        }
        match run_quiet("systemctl", &args) {
            Ok(()) => {
                tracing::info!("{SLICE} capped: {}", args[4..].join(" "));
                self.slice_set = Some(args);
            }
            Err(err) => tracing::warn!("{SLICE}: not capped: {err}"),
        }
    }
}

/// [`slice_props`] for this machine under the limits in Giverny's
/// `config.toml` (`auto` where unreadable).
#[cfg(target_os = "linux")]
pub fn slice_args_now() -> Vec<String> {
    let limits = giverny_core::limits::Limits::load().unwrap_or_default();
    // Cores and RAM only: the GPUs (`nvidia-smi`, slow) are no cgroup's.
    // The machine's cores, not this process's cgroup share.
    let cores = crate::session_use::sysconf(libc::_SC_NPROCESSORS_ONLN).unwrap_or(1) as u32;
    let ram_kb = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|t| {
            t.lines()
                .find_map(|l| l.strip_prefix("MemTotal:"))
                .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
        })
        .unwrap_or(0);
    let machine = giverny_core::limits::Machine {
        cores,
        ram: giverny_core::limits::Mem(ram_kb / 1024),
        gpus: Vec::new(),
    };
    slice_props(&limits.resolve(&machine))
}

/// Cap the slice at the limits now (`giverny manage run`, before it puts a
/// scope in it, so the slice is capped even where no Giverny adopted a
/// claude yet).
#[cfg(target_os = "linux")]
pub fn cap_slice_now() -> Result<(), String> {
    run_quiet("systemctl", &slice_args_now())
}

#[cfg(not(target_os = "linux"))]
pub fn cap_slice_now() -> Result<(), String> {
    Ok(())
}

/// Put `pid` first in line for an OOM kill ([`COMMAND_OOM_ADJ`]), unless
/// it is there already. A process that is gone, or not ours, is left.
#[cfg(target_os = "linux")]
fn raise_oom_adj(pid: u32) {
    let path = format!("/proc/{pid}/oom_score_adj");
    let now = std::fs::read_to_string(&path)
        .ok()
        .and_then(|v| v.trim().parse::<i32>().ok());
    if now.is_some_and(|v| v < COMMAND_OOM_ADJ) {
        let _ = std::fs::write(&path, COMMAND_OOM_ADJ.to_string());
    }
}

/// Run `prog args`, its stderr the error when it fails.
#[cfg(target_os = "linux")]
fn run_quiet(prog: &str, args: &[String]) -> Result<(), String> {
    let out = std::process::Command::new(prog)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("{prog}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{prog} exit {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use giverny_core::limits::Mem;

    #[test]
    fn the_scope_is_weighted_above_the_runs_and_protected_never_capped() {
        // The lease's cores weigh nothing.
        let lease = DefaultLease {
            cpu_cores: 1,
            ram: Mem::gb(3),
            ..Default::default()
        };
        let a = adopt_args("giverny-claude-7.scope", &[7, 9], &lease);
        let s = a.join(" ");
        assert!(
            s.starts_with(
                "--user call org.freedesktop.systemd1 /org/freedesktop/systemd1 \
                 org.freedesktop.systemd1.Manager StartTransientUnit ssa(sv)a(sa(sv)) \
                 giverny-claude-7.scope fail 7 PIDs au 2 7 9 Slice s giverny-claude.slice \
                 Description s "
            ),
            "{s}"
        );
        assert!(
            s.ends_with(
                "CPUWeight t 400 MemoryLow t 3221225472 \
                 OOMPolicy s continue CollectMode s inactive-or-failed 0"
            ),
            "{s}"
        );
        assert!(!s.contains("MemoryMax") && !s.contains("CPUQuota"), "{s}");
        // The description is one argument, however many words it has.
        let d = a.iter().position(|x| x == "Description").unwrap();
        assert_eq!(a[d + 2], "Giverny: claude 7, 3G protected");
    }
}
