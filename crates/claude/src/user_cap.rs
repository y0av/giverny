//! The ceilings Giverny holds itself where the kernel holds none: macOS.
//!
//! On Linux with a user systemd every Claude tab's claude and every
//! `giverny manage run` command sit in `giverny-claude.slice`, whose hard
//! `CPUQuota` and `MemoryMax` at `[manager.limits]` the kernel holds
//! ([`crate::tab_cap`], [`crate::manage_run`]). macOS has no cgroups, so
//! Giverny holds the same limits in userspace, from what it measures, with
//! no privilege:
//!
//! - **The app** ([`Holder`], each pass of its once-a-second sampler,
//!   [`crate::use_reading::Sampler`]) holds what the slice holds on Linux:
//!   every claude under a tab with everything it started, and every plain
//!   `giverny manage run` command, together ([`held`]).
//!   - **RAM.** When they together use more than the limit's RAM (physical
//!     footprints, [`crate::session_use::proc_table`]), the biggest
//!     command among them is killed (`SIGKILL`), never a claude and never a
//!     `run` process (which has to report it): the rule the slice's OOM
//!     killer follows through `oom_score_adj` ([`ram_victim`]). One kill a
//!     pass; the next pass sees what it freed.
//!   - **CPU.** Every command under a claude runs at low priority (`nice`
//!     [`COMMAND_NICE`], which what it starts inherits) so the person's own
//!     work goes first, as a `run` command already starts under
//!     `taskpolicy -c utility`. When they together use more
//!     than the limit's cores, the command trees that are busy
//!     ([`command_trees`]) are held to what is left by a duty cycle
//!     ([`Throttle`]): every [`PERIOD`] they run for a share of it and are
//!     stopped (`SIGSTOP`) for the rest, then continued (`SIGCONT`), as
//!     `cpulimit` does; the share follows their measured demand
//!     ([`Governor`]) and comes back to all of it as soon as they fit.
//!     Claudes are never stopped.
//! - **`giverny manage run`** ([`RunGuard`]) holds its own command, four
//!   times a second: killed when its tree passes [`Ceiling::for_run`] —
//!   twice its lease (where a Linux scope's `MemoryHigh` starts slowing it;
//!   with no reclaim to slow it by, it is stopped there instead) or the
//!   limit's RAM, whichever is less — and reported as an OOM kill; and,
//!   when no Giverny app is holding it (none in `$GIVERNY_PID`), its CPU
//!   held to the limit's cores the same way.
//!
//! **Where.** macOS. On Linux only with `$GIVERNY_RUN_NO_SYSTEMD` set, the
//! stand-in for macOS in tests ([`active`]). Linux without systemd and
//! native Windows are not supported for this; Linux with a
//! user systemd (WSL too) keeps the slice and none of this runs.
//!
//! **The gap.** A sampler is not the kernel. The app looks once a second
//! and a `run` four times a second, so a command that allocates faster
//! than that passes the limit before it is seen (the slice would refuse
//! the page); a process forked and exited between two looks is never seen;
//! a new process runs unthrottled and at normal priority until the next
//! look; and memory is a footprint read, not a page the kernel refuses. A
//! process stopped by the duty cycle when Giverny itself is killed
//! (`SIGKILL`, a crash) stays stopped until something sends it `SIGCONT`
//! (`kill -CONT <pid>`); a normal exit continues everything first
//! ([`continue_all_at_exit`]). The Bash guard's refusals ([`crate::bash_guard`])
//! are the same everywhere.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crate::session_use::{Proc, subtree};
use crate::use_reading::Table;

/// Whether Giverny holds the ceilings itself here: on macOS always; on
/// Linux only when `$GIVERNY_RUN_NO_SYSTEMD` stands in for macOS.
pub fn active() -> bool {
    if cfg!(target_os = "macos") {
        return true;
    }
    cfg!(target_os = "linux")
        && std::env::var_os(crate::manage_run::NO_SYSTEMD_ENV).is_some_and(|v| !v.is_empty())
}

/// The `nice` every command under a claude is given: below the person's
/// own programs (0), above nothing that matters.
pub const COMMAND_NICE: i32 = 10;

/// One duty cycle: the throttled trees run for a share of it and are
/// stopped for the rest. Short, so a stopped command is never stopped for
/// long and a shell's prompt still feels alive.
pub const PERIOD: Duration = Duration::from_millis(100);

/// The least share of a [`PERIOD`] a throttled tree still runs: it is held
/// back, never starved.
pub const MIN_FRAC: f64 = 0.05;

/// A command tree is throttled only when it wants at least this much of a
/// core: an idle MCP server or a waiting shell is left alone.
pub const BUSY_TREE_CORES: f64 = 0.1;

/// `1 core`, `6 cores`.
pub fn cores_text(n: u32) -> String {
    match n {
        1 => "1 core".into(),
        n => format!("{n} cores"),
    }
}

/// The limits Giverny holds: RAM (MiB, 0 for none) and cores (0 for none).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ceiling {
    pub ram_mb: u64,
    pub cpu_cores: u32,
}

impl Ceiling {
    /// `[manager.limits]` on this machine (`auto` where unreadable).
    pub fn now() -> Ceiling {
        let limits = giverny_core::limits::Limits::load().unwrap_or_default();
        let r = limits.resolve(&giverny_core::limits::Machine::detect_cpu_ram());
        Ceiling {
            ram_mb: r.ram.0,
            cpu_cores: r.cpu_cores,
        }
    }

    /// A plain `run`'s own ceiling under a lease of `lease_mb`: twice the
    /// lease — where a Linux scope's `MemoryHigh` would start slowing it,
    /// which needs a kernel; here it is stopped there — and never above
    /// the limit (`limit`), whose cores are its CPU ceiling. A lease of no
    /// RAM is held to the limit alone.
    pub fn for_run(lease_mb: u64, limit: Ceiling) -> Ceiling {
        let ram_mb = match (lease_mb, limit.ram_mb) {
            (0, l) => l,
            (lease, 0) => lease * 2,
            (lease, l) => (lease * 2).min(l),
        };
        Ceiling {
            ram_mb,
            cpu_cores: limit.cpu_cores,
        }
    }
}

/// What the ceiling holds, as the slice does on Linux: every claude in
/// `claudes` with everything below it, and every plain `run` process in
/// `runs` with its command.
pub fn held(procs: &[Proc], claudes: &HashSet<u32>, runs: &HashSet<u32>) -> HashSet<u32> {
    claudes
        .iter()
        .chain(runs)
        .flat_map(|&p| subtree(procs, p))
        .collect()
}

/// Every process ever held, by pid → start, so one stays held while it
/// lives: a command that detaches after it was seen (`nohup … &`, a
/// daemon's double fork, its shell gone) is reparented to `launchd` or
/// `init`, out of its claude's tree, as it could not leave a cgroup.
#[derive(Debug, Default)]
pub struct Members {
    seen: HashMap<u32, u64>,
}

impl Members {
    /// `now` (what the trees hold this look) with every earlier member
    /// still alive and all below it; remembered for the next look.
    pub fn hold(&mut self, procs: &[Proc], mut now: HashSet<u32>) -> HashSet<u32> {
        let start: HashMap<u32, u64> = procs.iter().map(|p| (p.pid, p.start)).collect();
        self.seen.retain(|p, s| start.get(p) == Some(s));
        for &pid in self.seen.keys() {
            if !now.contains(&pid) {
                now.extend(subtree(procs, pid));
            }
        }
        for &pid in &now {
            self.seen
                .entry(pid)
                .or_insert_with(|| start.get(&pid).copied().unwrap_or_default());
        }
        now
    }
}

/// The process to kill for memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Victim {
    pub pid: u32,
    /// Its memory, MiB.
    pub mem_mb: u64,
    /// What `held` used together, MiB.
    pub total_mb: u64,
}

/// When the processes in `held` together use more than `limit_mb`, the
/// biggest of them that is not `spared` (the claudes, the `run`
/// processes): the slice's OOM rule, commands before claudes. `None` under
/// the limit, with no limit, or with nothing but spared processes.
pub fn ram_victim(
    procs: &[Proc],
    held: &HashSet<u32>,
    spared: &HashSet<u32>,
    limit_mb: u64,
) -> Option<Victim> {
    if limit_mb == 0 {
        return None;
    }
    let mine = || procs.iter().filter(|p| held.contains(&p.pid));
    let total_kb: u64 = mine().map(|p| p.mem_kb).sum();
    if total_kb <= limit_mb * 1024 {
        return None;
    }
    mine()
        .filter(|p| !spared.contains(&p.pid) && p.mem_kb > 0)
        .max_by_key(|p| (p.mem_kb, p.pid))
        .map(|p| Victim {
            pid: p.pid,
            mem_mb: p.mem_kb.div_ceil(1024),
            total_mb: total_kb.div_ceil(1024),
        })
}

/// The commands in `held`, each under its command: the process right
/// below the nearest of `roots` (the claudes and `run` processes) above
/// it, or the topmost held process above it when none is. The `roots`
/// themselves are in no command. Keyed by the command's process.
pub fn command_trees(
    procs: &[Proc],
    held: &HashSet<u32>,
    roots: &HashSet<u32>,
) -> HashMap<u32, Vec<u32>> {
    let parent: HashMap<u32, u32> = procs.iter().map(|p| (p.pid, p.ppid)).collect();
    let mut top_of: HashMap<u32, u32> = HashMap::new();
    let mut trees: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut path = Vec::new();
    let mut pids: Vec<u32> = held
        .iter()
        .copied()
        .filter(|p| !roots.contains(p))
        .collect();
    pids.sort_unstable();
    for pid in pids {
        path.clear();
        let mut at = pid;
        let top = loop {
            if let Some(&t) = top_of.get(&at) {
                break t;
            }
            path.push(at);
            match parent.get(&at) {
                Some(&up) if roots.contains(&up) || !held.contains(&up) => break at,
                Some(&up) if up != at && path.len() < 256 => at = up,
                _ => break at,
            }
        };
        for &p in &path {
            top_of.insert(p, top);
        }
        trees.entry(top).or_default().push(pid);
    }
    trees
}

/// Cores the processes in `pids` used over the table's span.
pub fn cores_of(table: &Table, pids: impl IntoIterator<Item = u32>) -> f64 {
    if table.over_ms == 0 || table.ticks_per_s == 0 {
        return 0.0;
    }
    let ticks: u64 = pids.into_iter().filter_map(|p| table.spent.get(&p)).sum();
    ticks as f64 / table.ticks_per_s as f64 * 1000.0 / table.over_ms as f64
}

/// The share of each [`PERIOD`] the throttled trees run: 1 when they are
/// not throttled.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Governor {
    frac: f64,
}

impl Default for Governor {
    fn default() -> Self {
        Governor { frac: 1.0 }
    }
}

impl Governor {
    pub fn frac(&self) -> f64 {
        self.frac
    }

    /// Whether the trees are held back now.
    pub fn engaged(&self) -> bool {
        self.frac < 1.0
    }

    /// The next share, from the last span: `demand` the cores the trees it
    /// holds would use running all the time (what they used over the
    /// share they ran), `others` the cores used by what it does not hold
    /// (the claudes, the idle trees), under `limit` cores.
    ///
    /// The trees get what is left of the limit after the others, as a
    /// share of their demand, at least [`MIN_FRAC`]. Down at once; up
    /// halfway each look, so a build that was held does not burst past the
    /// limit on release; within 5 % of all of it, released.
    pub fn update(&mut self, demand: f64, others: f64, limit: f64) -> f64 {
        if limit <= 0.0 {
            self.frac = 1.0;
            return self.frac;
        }
        if !self.engaged() && demand + others <= limit {
            return self.frac;
        }
        let want = if demand <= 0.0 {
            1.0
        } else {
            ((limit - others).max(0.0) / demand).clamp(MIN_FRAC, 1.0)
        };
        let next = if want > self.frac {
            (self.frac + want) / 2.0
        } else {
            want
        };
        self.frac = if next >= 0.95 { 1.0 } else { next };
        self.frac
    }
}

/// What one pass of the [`Holder`] decided.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    /// Commands not seen before, to start at [`COMMAND_NICE`].
    pub renice: Vec<u32>,
    pub kill: Option<Victim>,
    /// The processes the duty cycle holds, and their share.
    pub throttle: Vec<u32>,
    pub frac: f64,
}

/// The app's half: [`Holder::tick`] each pass of its sampler.
#[derive(Debug, Default)]
pub struct Holder {
    ceiling: Option<(Ceiling, std::time::Instant)>,
    /// Commands reniced, by pid → start.
    reniced: HashMap<u32, u64>,
    /// Commands killed, by pid → start: not counted again while they go.
    killed: HashMap<u32, u64>,
    /// Every process held once, by pid → start: still held while it lives
    /// ([`Members`]).
    members: Members,
    governor: Governor,
    /// The command trees held last pass, by their command's pid.
    throttled: HashSet<u32>,
    throttle: Throttle,
    said: bool,
}

impl Holder {
    /// How often the limits are read again from `config.toml`.
    const CEILING_EVERY: Duration = Duration::from_secs(30);

    /// What to do this pass, under `ceiling`: `table` the pass, `claudes`
    /// the claudes under the app, `runs` the plain `run` processes.
    pub fn decide(
        &mut self,
        table: &Table,
        claudes: &HashSet<u32>,
        runs: &HashSet<u32>,
        ceiling: Ceiling,
    ) -> Plan {
        let start: HashMap<u32, u64> = table.procs.iter().map(|p| (p.pid, p.start)).collect();
        let same = |pid: &u32, s: &u64| start.get(pid) == Some(s);
        self.reniced.retain(|p, s| same(p, s));
        self.killed.retain(|p, s| same(p, s));
        let mut held = self
            .members
            .hold(&table.procs, held(&table.procs, claudes, runs));
        held.retain(|p| !self.killed.contains_key(p));
        let roots: HashSet<u32> = claudes.union(runs).copied().collect();
        let mut plan = Plan {
            frac: 1.0,
            ..Plan::default()
        };
        for &pid in held.iter().filter(|p| !roots.contains(p)) {
            if let std::collections::hash_map::Entry::Vacant(e) = self.reniced.entry(pid) {
                e.insert(start.get(&pid).copied().unwrap_or_default());
                plan.renice.push(pid);
            }
        }
        plan.renice.sort_unstable();
        plan.kill = ram_victim(&table.procs, &held, &roots, ceiling.ram_mb);
        if let Some(v) = plan.kill {
            self.killed
                .insert(v.pid, start.get(&v.pid).copied().unwrap_or_default());
        }
        if table.over_ms == 0 || ceiling.cpu_cores == 0 {
            return plan;
        }
        // The busy trees: what each wants is its use over the share it
        // ran (all of the span, for a tree not held last pass).
        let frac = self.governor.frac();
        let trees = command_trees(&table.procs, &held, &roots);
        let wants = |top: &u32| {
            let used = cores_of(table, trees[top].iter().copied());
            if self.throttled.contains(top) {
                used / frac
            } else {
                used
            }
        };
        let mut busy: Vec<u32> = trees
            .keys()
            .copied()
            .filter(|t| wants(t) >= BUSY_TREE_CORES)
            .collect();
        busy.sort_unstable();
        let demand: f64 = busy.iter().map(wants).sum();
        let governed = cores_of(table, busy.iter().flat_map(|t| trees[t].iter().copied()));
        let others = (cores_of(table, held.iter().copied()) - governed).max(0.0);
        let next = self
            .governor
            .update(demand, others, f64::from(ceiling.cpu_cores));
        if next < 1.0 {
            self.throttled = busy.iter().copied().collect();
            plan.throttle = busy.iter().flat_map(|t| trees[t].iter().copied()).collect();
            plan.throttle.sort_unstable();
        } else {
            self.throttled.clear();
        }
        plan.frac = next;
        plan
    }

    /// One pass: decide under the limits and act on it.
    pub fn tick(&mut self, table: &Table, claudes: &HashSet<u32>, runs: &HashSet<u32>) {
        let now = std::time::Instant::now();
        let ceiling = match self.ceiling {
            Some((c, at)) if now.duration_since(at) < Self::CEILING_EVERY => c,
            _ => {
                let c = Ceiling::now();
                if self.ceiling.map(|(old, _)| old) != Some(c) {
                    tracing::info!(
                        "Giverny holds the Claude tabs' limits itself (no cgroups here): \
                         {} MiB RAM, {}",
                        c.ram_mb,
                        cores_text(c.cpu_cores)
                    );
                }
                self.ceiling = Some((c, now));
                c
            }
        };
        let was = self.governor.engaged();
        let plan = self.decide(table, claudes, runs, ceiling);
        for &pid in &plan.renice {
            renice(pid, COMMAND_NICE);
        }
        if let Some(v) = plan.kill {
            kill(v.pid);
            tracing::warn!(
                "the Claude tabs used {} MiB, over the limit's {} MiB: killed command {} ({} MiB), \
                 the biggest under them (a claude never is)",
                v.total_mb,
                ceiling.ram_mb,
                v.pid,
                v.mem_mb
            );
        }
        if plan.frac < 1.0 && !was {
            tracing::info!(
                "the Claude tabs' commands use more than the limit's {}: held to {:.0}% \
                 of the time ({} process(es))",
                cores_text(ceiling.cpu_cores),
                plan.frac * 100.0,
                plan.throttle.len()
            );
        } else if plan.frac >= 1.0 && was {
            tracing::info!("the Claude tabs' commands fit the limit again: released");
        }
        if !self.said {
            self.said = true;
            tracing::debug!("user_cap: holding {} claude(s)", claudes.len());
        }
        self.throttle.set(plan.throttle, plan.frac);
    }
}

// ---- acting ----------------------------------------------------------------

/// `SIGKILL` to `pid`.
pub fn kill(pid: u32) {
    #[cfg(unix)]
    // SAFETY: kill only sends a signal.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// `pid` to `nice` `to`. One the person put lower already stays there:
/// moving a process back up needs privilege, so that call fails, and
/// harmlessly.
pub fn renice(pid: u32, to: i32) {
    #[cfg(unix)]
    // SAFETY: setpriority only sets a number of `pid`'s.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS as _, pid as libc::id_t, to);
    }
    #[cfg(not(unix))]
    let _ = (pid, to);
}

/// Every process the duty cycles hold stopped right now, so an exit can
/// continue them ([`continue_all_at_exit`]).
#[cfg(unix)]
static STOPPED: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

/// `sig` to each of `pids`.
#[cfg(unix)]
fn signal_all(pids: &[u32], sig: libc::c_int) {
    for &p in pids {
        // SAFETY: kill only sends a signal.
        unsafe {
            libc::kill(p as libc::pid_t, sig);
        }
    }
}

/// On a normal exit (`exit`, a return from `main`), continue whatever a
/// duty cycle holds stopped, so no command is left stopped by Giverny
/// going. Registered once.
#[cfg(unix)]
pub fn continue_all_at_exit() {
    extern "C" fn cont() {
        if let Ok(s) = STOPPED.try_lock() {
            signal_all(&s, libc::SIGCONT);
        }
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: registers a plain function with no state of its own.
        unsafe {
            libc::atexit(cont);
        }
    });
}

#[cfg(not(unix))]
pub fn continue_all_at_exit() {}

/// The duty cycle ([`PERIOD`]): a thread that lets the processes it is
/// given run for their share of each period and stops them for the rest,
/// started on the first throttle and continuing everything when dropped.
#[derive(Debug, Default)]
pub struct Throttle {
    #[cfg(unix)]
    shared: Option<std::sync::Arc<(std::sync::Mutex<Duty>, std::sync::Condvar)>>,
    #[cfg(unix)]
    handle: Option<std::thread::JoinHandle<()>>,
}

#[derive(Debug, Clone, Default)]
#[cfg_attr(not(unix), allow(dead_code))]
struct Duty {
    pids: Vec<u32>,
    frac: f64,
    quit: bool,
}

impl Throttle {
    /// Hold `pids` to `frac` of each period from now on (1, or none: let
    /// everything run).
    pub fn set(&mut self, pids: Vec<u32>, frac: f64) {
        #[cfg(unix)]
        {
            let idle = frac >= 1.0 || pids.is_empty();
            if self.shared.is_none() {
                if idle {
                    return;
                }
                continue_all_at_exit();
                let shared = std::sync::Arc::new((
                    std::sync::Mutex::new(Duty::default()),
                    std::sync::Condvar::new(),
                ));
                let s = shared.clone();
                self.handle = std::thread::Builder::new()
                    .name("giverny-duty-cycle".into())
                    .spawn(move || duty_cycle(&s))
                    .ok();
                self.shared = Some(shared);
            }
            if let Some(s) = &self.shared {
                let mut d = s.0.lock().unwrap_or_else(|p| p.into_inner());
                d.pids = if idle { Vec::new() } else { pids };
                d.frac = if idle { 1.0 } else { frac.max(MIN_FRAC) };
                s.1.notify_all();
            }
        }
        #[cfg(not(unix))]
        let _ = (pids, frac);
    }
}

impl Drop for Throttle {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            if let Some(s) = &self.shared {
                s.0.lock().unwrap_or_else(|p| p.into_inner()).quit = true;
                s.1.notify_all();
            }
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }
}

/// The thread: each period, continue what the last one stopped, let the
/// current set run its share, stop it for the rest. What it stopped is
/// always continued at the start of the next period — the set may have
/// changed — and when it quits.
#[cfg(unix)]
fn duty_cycle(s: &(std::sync::Mutex<Duty>, std::sync::Condvar)) {
    let mut stopped: Vec<u32> = Vec::new();
    loop {
        if !stopped.is_empty() {
            signal_all(&stopped, libc::SIGCONT);
            let mut all = STOPPED.lock().unwrap_or_else(|p| p.into_inner());
            all.retain(|p| !stopped.contains(p));
            stopped.clear();
        }
        let d = {
            let mut d = s.0.lock().unwrap_or_else(|p| p.into_inner());
            while !d.quit && (d.frac >= 1.0 || d.pids.is_empty()) {
                d = s.1.wait(d).unwrap_or_else(|p| p.into_inner());
            }
            d.clone()
        };
        if d.quit {
            return;
        }
        std::thread::sleep(PERIOD.mul_f64(d.frac));
        {
            // Recorded before they are stopped, so an exit in between
            // still continues them.
            let mut all = STOPPED.lock().unwrap_or_else(|p| p.into_inner());
            all.extend(&d.pids);
        }
        signal_all(&d.pids, libc::SIGSTOP);
        stopped = d.pids;
        std::thread::sleep(PERIOD.mul_f64(1.0 - d.frac));
    }
}

// ---- reading -----------------------------------------------------------------

/// One look at the machine's processes, the platform's way: Linux's
/// `/proc` (resident sets, proportional where [`Snapshot::real_memory`]
/// reads them), macOS's `libproc` (physical footprints).
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub procs: Vec<Proc>,
    /// The table's clock: what its ticks count a second.
    pub ticks_per_s: u64,
    /// Each process's short name (macOS; empty on Linux, which reads
    /// `/proc/<pid>/comm` instead).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    comm: HashMap<u32, String>,
}

impl Snapshot {
    /// The table now; `None` where there is none to read.
    pub fn take() -> Option<Snapshot> {
        #[cfg(target_os = "linux")]
        {
            let su = crate::session_use::sysconf;
            let page_kb = su(libc::_SC_PAGESIZE).unwrap_or(4096) / 1024;
            let procs = crate::session_use::proc_table(page_kb);
            (!procs.is_empty()).then(|| Snapshot {
                procs,
                ticks_per_s: su(libc::_SC_CLK_TCK).unwrap_or(100),
                comm: HashMap::new(),
            })
        }
        #[cfg(target_os = "macos")]
        {
            let (procs, comm) = crate::session_use::proc_table();
            (!procs.is_empty()).then_some(Snapshot {
                procs,
                ticks_per_s: 1000,
                comm,
            })
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            None
        }
    }

    /// The memory the processes in `pids` really use: their proportional
    /// sets on Linux; macOS's footprints are that already.
    pub fn real_memory(&mut self, pids: &HashSet<u32>) {
        #[cfg(target_os = "linux")]
        crate::session_use::with_real_memory(&mut self.procs, pids);
        #[cfg(not(target_os = "linux"))]
        let _ = pids;
    }
}

/// Which processes are claudes, each asked once (by pid and start): on
/// macOS the name is the short one or, for Claude Code's launcher (a
/// `node` that calls itself `claude`), `argv[0]`.
#[derive(Debug, Default)]
pub struct Claudes {
    known: HashMap<u32, (u64, bool)>,
}

impl Claudes {
    /// The claudes among `pids` in `snap`.
    pub fn among(&mut self, snap: &Snapshot, pids: &HashSet<u32>) -> HashSet<u32> {
        let start: HashMap<u32, u64> = snap.procs.iter().map(|p| (p.pid, p.start)).collect();
        self.known
            .retain(|p, (s, _)| start.get(p) == Some(s) && pids.contains(p));
        pids.iter()
            .copied()
            .filter(|&pid| {
                let s = start.get(&pid).copied().unwrap_or_default();
                match self.known.get(&pid) {
                    Some(&(ks, is)) if ks == s => is,
                    _ => {
                        let is = Self::ask(snap, pid);
                        self.known.insert(pid, (s, is));
                        is
                    }
                }
            })
            .collect()
    }

    fn ask(snap: &Snapshot, pid: u32) -> bool {
        #[cfg(target_os = "macos")]
        {
            use crate::lineage::is_claude_name;
            snap.comm.get(&pid).is_some_and(|c| is_claude_name(c))
                || crate::session_use::argv0(pid).is_some_and(|a| is_claude_name(&a))
        }
        #[cfg(target_os = "linux")]
        {
            let _ = snap;
            crate::lineage::proc_is_claude(pid)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = (snap, pid);
            false
        }
    }
}

// ---- `giverny manage run` ------------------------------------------------------

/// What a [`RunGuard`] saw and did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunReport {
    /// Processes it killed for memory.
    pub kills: u64,
    /// The most the command's tree held at one look, MiB.
    pub peak_mb: Option<u64>,
    /// What the tree held when it last killed, MiB.
    pub killed_at_mb: Option<u64>,
    /// It held the command's CPU back at some point.
    pub throttled: bool,
}

/// A plain `run`'s watch over its command ([`RunGuard::start`]).
pub struct RunGuard {
    quit: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<RunReport>>,
}

impl RunGuard {
    /// How often the command's tree is looked at.
    pub const EVERY: Duration = Duration::from_millis(250);
    /// Every how many looks its CPU is weighed (a second's worth).
    const CPU_EVERY: u32 = 4;

    /// Watch the tree under `child` (the command `run` started) until
    /// [`RunGuard::finish`]: kill the biggest process in it (never a
    /// claude) while it holds more than `ceiling`'s RAM, and when
    /// `hold_cpu`, hold it to `ceiling`'s cores. `say` tells the worker.
    pub fn start(
        child: u32,
        ceiling: Ceiling,
        hold_cpu: bool,
        say: impl Fn(&str) + Send + 'static,
    ) -> RunGuard {
        let quit = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let q = quit.clone();
        let handle = std::thread::Builder::new()
            .name("giverny-run-guard".into())
            .spawn(move || guard(child, ceiling, hold_cpu, &q, &say))
            .ok();
        RunGuard { quit, handle }
    }

    /// Stop watching (the command has ended) and say what it saw.
    pub fn finish(mut self) -> RunReport {
        self.quit.store(true, std::sync::atomic::Ordering::Relaxed);
        self.handle
            .take()
            .and_then(|h| h.join().ok())
            .unwrap_or_default()
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        self.quit.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The guard's thread.
fn guard(
    child: u32,
    ceiling: Ceiling,
    hold_cpu: bool,
    quit: &std::sync::atomic::AtomicBool,
    say: &dyn Fn(&str),
) -> RunReport {
    use std::sync::atomic::Ordering;
    let mut report = RunReport::default();
    let mut claudes = Claudes::default();
    let mut governor = Governor::default();
    let mut throttle = Throttle::default();
    let mut killed: HashMap<u32, u64> = HashMap::new();
    let mut members = Members::default();
    let mut prev: Option<(crate::use_reading::Seen, std::time::Instant)> = None;
    let mut look = 0u32;
    while !quit.load(Ordering::Relaxed) {
        look = look.wrapping_add(1);
        if let Some(mut snap) = Snapshot::take() {
            let start: HashMap<u32, u64> = snap.procs.iter().map(|p| (p.pid, p.start)).collect();
            killed.retain(|p, s| start.get(p) == Some(s));
            let mut tree = members.hold(&snap.procs, subtree(&snap.procs, child));
            tree.retain(|p| !killed.contains_key(p));
            snap.real_memory(&tree);
            let mem_kb: u64 = snap
                .procs
                .iter()
                .filter(|p| tree.contains(&p.pid))
                .map(|p| p.mem_kb)
                .sum();
            let mem_mb = mem_kb.div_ceil(1024);
            if !tree.is_empty() {
                report.peak_mb = Some(report.peak_mb.unwrap_or(0).max(mem_mb));
            }
            let spared = claudes.among(&snap, &tree);
            if let Some(v) = ram_victim(&snap.procs, &tree, &spared, ceiling.ram_mb) {
                kill(v.pid);
                killed.insert(v.pid, start.get(&v.pid).copied().unwrap_or_default());
                report.kills += 1;
                report.killed_at_mb = Some(v.total_mb);
                say(&format!(
                    "its command used {} MiB, over the {} MiB Giverny holds it to here (no \
                     cgroups): killed process {} ({} MiB), the biggest in it",
                    v.total_mb, ceiling.ram_mb, v.pid, v.mem_mb
                ));
            }
            // The first look starts the clock, so the first weighing is a
            // second in.
            if hold_cpu
                && ceiling.cpu_cores > 0
                && (prev.is_none() || look.is_multiple_of(RunGuard::CPU_EVERY))
            {
                let now = std::time::Instant::now();
                let table = Table {
                    spent: crate::use_reading::pid_ticks(&snap.procs, prev.as_ref().map(|p| &p.0)),
                    over_ms: prev
                        .as_ref()
                        .map_or(0, |(_, at)| now.duration_since(*at).as_millis() as u64),
                    ticks_per_s: snap.ticks_per_s,
                    cores: 1,
                    procs: std::mem::take(&mut snap.procs),
                    ..Table::default()
                };
                prev = Some((crate::use_reading::seen(&table.procs), now));
                if table.over_ms > 0 {
                    let mine: Vec<u32> = tree
                        .iter()
                        .copied()
                        .filter(|p| !spared.contains(p))
                        .collect();
                    let used = cores_of(&table, mine.iter().copied());
                    let others = cores_of(&table, spared.iter().copied());
                    let was = governor.engaged();
                    let frac = governor.update(
                        used / governor.frac(),
                        others,
                        f64::from(ceiling.cpu_cores),
                    );
                    if frac < 1.0 && !was {
                        report.throttled = true;
                        say(&format!(
                            "its command uses more than the limit's {}: held to {:.0}% of \
                             the time (no cgroups here)",
                            cores_text(ceiling.cpu_cores),
                            frac * 100.0
                        ));
                    }
                    let mut mine = mine;
                    mine.sort_unstable();
                    throttle.set(mine, frac);
                }
            }
        }
        std::thread::sleep(RunGuard::EVERY);
    }
    drop(throttle);
    report
}

/// Whether a Giverny app holds this process's work already: one is named
/// in `$GIVERNY_PID` and alive. Its [`Holder`] then holds the CPU of every
/// `run` too, and two duty cycles on one process would undo each other.
pub fn app_holds() -> bool {
    let Some(pid) = std::env::var(crate::lineage::APP_PID_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
    else {
        return false;
    };
    #[cfg(unix)]
    {
        // SAFETY: signal 0 sends nothing; it only checks the pid exists.
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: u32, ppid: u32, mem_mb: u64) -> Proc {
        Proc {
            pid,
            ppid,
            ticks: 0,
            mem_kb: mem_mb * 1024,
            start: u64::from(pid),
        }
    }

    /// app 1 → shell 10 → claude 20 → bash 30 → cargo 31 → rustc 32, 33;
    /// claude 20 → mcp 40; shell 11 → claude 21 → bash 50 → hog 51;
    /// claude 21 → bash 52 → `giverny manage run` 60 → cmd 61;
    /// and a plain shell 12 → vim 70 (no claude).
    fn machine() -> Vec<Proc> {
        vec![
            p(1, 0, 100),
            p(10, 1, 5),
            p(20, 10, 400),
            p(30, 20, 5),
            p(31, 30, 200),
            p(32, 31, 900),
            p(33, 31, 700),
            p(40, 20, 80),
            p(11, 1, 5),
            p(21, 11, 600),
            p(50, 21, 5),
            p(51, 50, 1500),
            p(52, 21, 5),
            p(60, 52, 10),
            p(61, 60, 300),
            p(12, 1, 5),
            p(70, 12, 3000),
        ]
    }

    fn claudes() -> HashSet<u32> {
        [20, 21].into()
    }

    #[test]
    fn over_the_ram_limit_the_biggest_command_goes_and_never_a_claude() {
        let procs = machine();
        let runs: HashSet<u32> = [60].into();
        let h = held(&procs, &claudes(), &runs);
        let spared: HashSet<u32> = claudes().union(&runs).copied().collect();
        // Held: 400+5+200+900+700+80+600+5+1500+5+10+300 = 4705 MiB.
        assert_eq!(ram_victim(&procs, &h, &spared, 4800), None, "under");
        assert_eq!(ram_victim(&procs, &h, &spared, 0), None, "no limit");
        assert_eq!(
            ram_victim(&procs, &h, &spared, 4096),
            Some(Victim {
                pid: 51,
                mem_mb: 1500,
                total_mb: 4705
            }),
            "the hog, not vim (no claude's) and not a claude"
        );
        // A claude that is the biggest is still spared: the biggest command.
        let mut fat = procs.clone();
        fat[9].mem_kb = 9000 * 1024; // claude 21
        assert_eq!(ram_victim(&fat, &h, &spared, 4096).unwrap().pid, 51);
        // Only claudes and runs over the limit: nothing to kill.
        let only: HashSet<u32> = [20, 21, 60].into();
        assert_eq!(ram_victim(&procs, &only, &spared, 100), None);
    }
}
