//! One reading of what Giverny runs, at one moment, measured one way.
//!
//! The app reads `/proc` once a second ([`Sampler`]) and
//! derives every figure it shows from that one pass:
//!
//! * the **total**: the app and everything under it, every tab, Claude or
//!   not (the sidebar's line);
//! * each **tab**: its shell and everything under it (the sidebar's tab
//!   rows and category headers);
//! * each **session**: a claude process and everything under it, keyed by
//!   the claude's pid;
//! * each **run**: the processes in a `giverny manage run` scope's cgroup
//!   (the management panel's rows). A scope's command is usually a
//!   descendant of the claude that ran it (`systemd-run --scope` execs it in
//!   place), but not always: one started detached (`setsid`, `nohup`, a
//!   shell whose parent died) is reparented outside the app. So each run's
//!   processes are added, once, to its session (the claude whose
//!   `sessions/<pid>.json` names the run's session id), that claude's tab
//!   and the total; a run with no claude here, to the total alone.
//!   A run is part of its session, a session part of its
//!   tab, a tab part of the total, by construction.
//!
//! Every group is a set of processes, and every figure is a sum over its
//! processes of the same per-process figures from the same pass: CPU
//! clock ticks spent since the last pass ([`pid_ticks`]), as a share of
//! the machine; memory as the proportional set (shared pages split
//! between their sharers); GPU memory and compute per process from
//! `nvidia-smi`. So a run never reads more than its session, a session more
//! than its tab, nor a tab more than the total.
//!
//! Claude Code's status line shows none of it: the app draws
//! every figure from its newest pass.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::session_use::{Proc, subtree};

/// One group's use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Use {
    /// CPU since the last pass, percent of the whole machine.
    pub cpu_pct: u32,
    /// Proportional set summed, MiB.
    pub mem_mb: u64,
    /// GPU memory, MiB; `None` without an NVIDIA GPU.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_mb: Option<u64>,
    /// Share of the GPUs' compute, percent; `None` without a GPU that
    /// reports it per process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_pct: Option<u32>,
}

/// One `giverny manage run` task's commands' use (its scopes summed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunUse {
    pub session: String,
    pub task: String,
    #[serde(rename = "use")]
    pub used: Use,
    /// The worker whose commands the run's processes are under, when one
    /// is ([`crate::worker_pids`]): that worker's figure already holds it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

/// One pass.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reading {
    /// When it was taken, ms since the epoch.
    pub at_ms: u64,
    /// The app's pid, the root of [`Reading::total`].
    pub app: u32,
    pub total: Use,
    /// Every claude under the app, by pid.
    pub sessions: BTreeMap<u32, Use>,
    /// Each tab, by its shell's pid (a child of the app): the shell and
    /// everything under it, Claude Code or not, its `manage run` scopes
    /// too.
    #[serde(default)]
    pub tabs: BTreeMap<u32, Use>,
    pub runs: Vec<RunUse>,
    /// Each worker's processes (the Bash commands it started, with all
    /// under them), by agent id; part of its session's.
    #[serde(default)]
    pub agents: BTreeMap<String, Use>,
}

/// What one pass knows about every process, and the GPU.
#[derive(Debug, Clone, Default)]
pub struct Table {
    pub procs: Vec<Proc>,
    /// CPU ticks each process spent since the last pass ([`pid_ticks`]).
    pub spent: HashMap<u32, u64>,
    /// GPU memory per pid, MiB; `None` without a GPU.
    pub gpu_mb: Option<HashMap<u32, u64>>,
    /// `pmon`'s compute per pid; `None` where it is not reported.
    pub pmon: Option<Pmon>,
    /// The span the ticks were spent over, ms, and the machine's clock
    /// rate and cores, to make a share of them.
    pub over_ms: u64,
    pub ticks_per_s: u64,
    pub cores: u32,
}

impl Table {
    /// The use of the processes in `pids`.
    pub fn use_of(&self, pids: &HashSet<u32>) -> Use {
        let ticks: u64 = pids.iter().filter_map(|p| self.spent.get(p)).sum();
        let mem_kb: u64 = self
            .procs
            .iter()
            .filter(|p| pids.contains(&p.pid))
            .map(|p| p.mem_kb)
            .sum();
        Use {
            cpu_pct: crate::session_use::cpu_pct(
                0,
                ticks,
                self.ticks_per_s,
                self.over_ms,
                self.cores,
            ),
            mem_mb: mem_kb.div_ceil(1024),
            gpu_mb: self
                .gpu_mb
                .as_ref()
                .map(|g| crate::session_use::gpu::of(g, pids)),
            gpu_pct: self
                .pmon
                .as_ref()
                .filter(|p| p.gpus > 0)
                .map(|p| gpu_pct(p, pids)),
        }
    }

    /// The reading: `app`'s tree, each claude in it (`is_claude`), each
    /// run's processes (`runs`: session, task, its scope's pids; several
    /// scopes of one task are summed), and each worker's (`agents`).
    ///
    /// `claude_of` names each session's claude (session id → pid). A run's
    /// processes count in its session's claude, every claude around that
    /// one, the claude's tab and the total, whether they descend from the
    /// claude or not: a run started detached (`setsid`, `nohup`, a shell
    /// whose parent died) has left the tree, and still is its session's.
    /// A run whose session has no claude here counts in the
    /// total alone. Each set is a set, so a run that is under its claude is
    /// counted once.
    pub fn reading(
        &self,
        at_ms: u64,
        app: u32,
        is_claude: impl Fn(u32) -> bool,
        runs: &[(String, String, HashSet<u32>)],
        agents: &HashMap<String, HashSet<u32>>,
        claude_of: &HashMap<String, u32>,
    ) -> Reading {
        let tree = subtree(&self.procs, app);
        let mut claudes: BTreeMap<u32, HashSet<u32>> = tree
            .iter()
            .copied()
            .filter(|&p| is_claude(p))
            .map(|p| (p, subtree(&self.procs, p)))
            .collect();
        let (mut tabs, tab_of) = self.tabs(app, &tree);
        let mut total = tree;
        for (session, _, pids) in runs {
            total.extend(pids);
            let Some(&claude) = claude_of.get(session) else {
                continue;
            };
            if !claudes.contains_key(&claude) {
                continue;
            }
            for set in claudes.values_mut().filter(|s| s.contains(&claude)) {
                set.extend(pids);
            }
            if let Some(tab) = tab_of.get(&claude) {
                tabs.entry(*tab).or_default().extend(pids);
            }
        }
        let mut by_task: BTreeMap<(String, String), HashSet<u32>> = BTreeMap::new();
        for (session, task, pids) in runs {
            by_task
                .entry((session.clone(), task.clone()))
                .or_default()
                .extend(pids);
        }
        let runs = by_task
            .into_iter()
            .map(|((session, task), pids)| RunUse {
                agent: agents
                    .iter()
                    .find(|(_, a)| !a.is_disjoint(&pids))
                    .map(|(id, _)| id.clone()),
                session,
                task,
                used: self.use_of(&pids),
            })
            .collect();
        Reading {
            at_ms,
            app,
            total: self.use_of(&total),
            sessions: claudes
                .iter()
                .map(|(&p, pids)| (p, self.use_of(pids)))
                .collect(),
            tabs: tabs
                .iter()
                .map(|(&tab, pids)| (tab, self.use_of(pids)))
                .collect(),
            runs,
            agents: agents
                .iter()
                .map(|(id, pids)| (id.clone(), self.use_of(pids)))
                .collect(),
        }
    }
}

impl Table {
    /// [`Reading::tabs`]' processes: each child of `app` (a tab's shell)
    /// with every process under it, from `tree` (`app`'s subtree), in one
    /// sweep; and each process's tab.
    fn tabs(
        &self,
        app: u32,
        tree: &HashSet<u32>,
    ) -> (BTreeMap<u32, HashSet<u32>>, HashMap<u32, u32>) {
        let parent: HashMap<u32, u32> = self.procs.iter().map(|p| (p.pid, p.ppid)).collect();
        // Each process's tab, as found: a walk up stops at the first
        // process whose tab is known.
        let mut tab_of: HashMap<u32, u32> = HashMap::new();
        let mut by_tab: BTreeMap<u32, HashSet<u32>> = BTreeMap::new();
        let mut path = Vec::new();
        for &pid in tree {
            if pid == app {
                continue;
            }
            path.clear();
            let mut at = pid;
            let tab = loop {
                if let Some(&t) = tab_of.get(&at) {
                    break Some(t);
                }
                path.push(at);
                match parent.get(&at) {
                    Some(&up) if up == app => break Some(at),
                    Some(&up) if up != at && path.len() < 256 => at = up,
                    _ => break None,
                }
            };
            let Some(tab) = tab else { continue };
            for &p in &path {
                tab_of.insert(p, tab);
            }
            by_tab.entry(tab).or_default().insert(pid);
        }
        (by_tab, tab_of)
    }
}

/// What a process was at the last pass: its parent, start and ticks.
pub type Seen = HashMap<u32, (u32, u64, u64)>;

/// Each live process's CPU ticks since the last pass (`prev`), from
/// [`Proc::ticks`] — its own time and its reaped children's.
///
/// A process seen last time (same pid, same start) spent the difference.
/// One new since spent all it has (with no last pass at all, nothing is
/// known yet: zero). A process that ended since had its time, up to the
/// last pass already counted, added into whichever process reaped it: that
/// much is taken back off the nearest of its last-pass ancestors still
/// alive, so only its last stretch is counted, once. Never below zero.
pub fn pid_ticks(procs: &[Proc], prev: Option<&Seen>) -> HashMap<u32, u64> {
    let Some(prev) = prev else {
        return procs.iter().map(|p| (p.pid, 0)).collect();
    };
    let alive: HashMap<u32, u64> = procs.iter().map(|p| (p.pid, p.start)).collect();
    let same = |pid: u32, start: u64| alive.get(&pid) == Some(&start);
    let mut back: HashMap<u32, u64> = HashMap::new();
    for (&pid, &(ppid, start, ticks)) in prev {
        if same(pid, start) {
            continue;
        }
        let mut up = ppid;
        for _ in 0..64 {
            match prev.get(&up) {
                _ if alive.contains_key(&up) => {
                    *back.entry(up).or_default() += ticks;
                    break;
                }
                Some(&(pp, _, _)) if pp != up => up = pp,
                _ => break,
            }
        }
    }
    procs
        .iter()
        .map(|p| {
            let spent = match prev.get(&p.pid) {
                Some(&(_, start, then)) if start == p.start => p.ticks.saturating_sub(then),
                _ => p.ticks,
            };
            (
                p.pid,
                spent.saturating_sub(back.get(&p.pid).copied().unwrap_or(0)),
            )
        })
        .collect()
}

/// This pass's processes, as the next pass needs them.
pub fn seen(procs: &[Proc]) -> Seen {
    procs
        .iter()
        .map(|p| (p.pid, (p.ppid, p.start, p.ticks)))
        .collect()
}

/// One `nvidia-smi pmon -c 1 -s u` reading: each process's `sm` percent
/// (summed over GPUs), and how many GPUs answered.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pmon {
    pub sm: HashMap<u32, u32>,
    pub gpus: usize,
}

/// `pmon`'s table: `# …` headers, then `<gpu> <pid> <type> <sm> <mem> …`
/// rows, `-` for nothing (a GPU with no process has a row of `-`).
pub fn parse_pmon(out: &str) -> Pmon {
    let mut gpus = HashSet::new();
    let mut sm: HashMap<u32, u32> = HashMap::new();
    for l in out.lines().map(str::trim).filter(|l| !l.starts_with('#')) {
        let f: Vec<&str> = l.split_whitespace().collect();
        let Some(gpu) = f.first().and_then(|g| g.parse::<u32>().ok()) else {
            continue;
        };
        gpus.insert(gpu);
        if let (Some(pid), Some(pct)) = (
            f.get(1).and_then(|p| p.parse::<u32>().ok()),
            f.get(3).and_then(|p| p.parse::<u32>().ok()),
        ) {
            *sm.entry(pid).or_default() += pct;
        }
    }
    Pmon {
        sm,
        gpus: gpus.len(),
    }
}

/// The share of all the GPUs' compute the processes in `pids` use,
/// percent, at most 100.
pub fn gpu_pct(pmon: &Pmon, pids: &HashSet<u32>) -> u32 {
    if pmon.gpus == 0 {
        return 0;
    }
    let used: u32 = pmon
        .sm
        .iter()
        .filter(|(pid, _)| pids.contains(pid))
        .map(|(_, pct)| pct)
        .sum();
    (used / pmon.gpus as u32).min(100)
}

// ---- the sampler ----------------------------------------------------------

/// Takes the passes, keeping what the next one diffs against. GPU answers
/// are slow: `nvidia-smi`'s memory per process is asked every
/// [`Sampler::GPU_EVERY`], its compute (`pmon`, which watches for a second)
/// started then and collected on a later pass, never waited for.
#[derive(Debug, Default)]
// macOS reads only the table (for its ceilings), none of the GPU or
// worker figures.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct Sampler {
    prev: Option<(Seen, std::time::Instant)>,
    gpu: Option<(HashMap<u32, u64>, std::time::Instant)>,
    pmon: Option<Pmon>,
    pmon_child: Option<std::process::Child>,
    pmon_at: Option<std::time::Instant>,
    workers: crate::worker_pids::Attributor,
    /// Puts each tab's claude under its cap ([`crate::tab_cap`]); only the
    /// app's own sampler ([`Sampler::capping_tabs`]).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    caps: Option<crate::tab_cap::Adopter>,
    /// Where the kernel holds no ceiling (macOS), Giverny's own hold on
    /// the tabs' claudes and the plain runs ([`crate::user_cap`]);
    /// only the app's sampler, only there.
    holder: Option<crate::user_cap::Holder>,
    /// Which processes are claudes, asked once each (macOS).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    claudes: crate::user_cap::Claudes,
    /// Where the claudes' registry files are, found once (Linux).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    config_dirs: Option<Vec<std::path::PathBuf>>,
}

impl Sampler {
    /// The app's sampler: each pass also puts every claude it finds under
    /// a tab into its capped scope ([`crate::tab_cap`]), or, where there
    /// are no cgroups (macOS), holds the limits itself
    /// ([`crate::user_cap`]).
    pub fn capping_tabs() -> Sampler {
        Sampler {
            caps: Some(crate::tab_cap::Adopter::default()),
            holder: crate::user_cap::active().then(crate::user_cap::Holder::default),
            ..Sampler::default()
        }
    }

    #[cfg(target_os = "linux")]
    const GPU_EVERY: std::time::Duration = std::time::Duration::from_secs(6);

    /// One pass for the app `app`, with the runs under `runs_dir`. `None`
    /// where there is no `/proc`. The first pass has no CPU yet.
    #[cfg(target_os = "linux")]
    pub fn sample(&mut self, app: u32, runs_dir: &Path) -> Option<Reading> {
        use crate::session_use as su;
        let ticks_per_s = su::sysconf(libc::_SC_CLK_TCK).unwrap_or(100);
        let page_kb = su::sysconf(libc::_SC_PAGESIZE).unwrap_or(4096) / 1024;
        let cores = std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(1);
        let mut procs = su::proc_table(page_kb);
        if procs.is_empty() {
            return None;
        }
        let now = std::time::Instant::now();
        let plain = crate::run_live::running_plain(runs_dir);
        let runs: Vec<(String, String, HashSet<u32>)> =
            crate::run_live::running(runs_dir)
                .into_iter()
                .map(|(session, task, cg)| (session, task, crate::run_live::cgroup_pids(&cg)))
                // A plain run's command: the `run` process and all below it.
                .chain(plain.iter().map(|(session, task, pid)| {
                    (session.clone(), task.clone(), subtree(&procs, *pid))
                }))
                .collect();
        // Memory is read for what is shown: the app's tree and the runs.
        let mut shown = subtree(&procs, app);
        shown.extend(runs.iter().flat_map(|r| r.2.iter().copied()));
        su::with_real_memory(&mut procs, &shown);
        let spent = pid_ticks(&procs, self.prev.as_ref().map(|p| &p.0));
        let over_ms = self
            .prev
            .as_ref()
            .map_or(0, |(_, at)| now.duration_since(*at).as_millis() as u64);
        self.poll_gpu(now);
        let table = Table {
            spent,
            gpu_mb: self.gpu.as_ref().map(|g| g.0.clone()),
            pmon: self.pmon.clone(),
            over_ms,
            ticks_per_s,
            cores,
            procs,
        };
        self.prev = Some((seen(&table.procs), now));
        let claudes: HashSet<u32> = shown
            .iter()
            .copied()
            .filter(|&p| crate::lineage::proc_is_claude(p))
            .collect();
        if let Some(caps) = &mut self.caps {
            caps.tick(&table.procs, &claudes);
        }
        if let Some(holder) = &mut self.holder {
            let runs: HashSet<u32> = plain.iter().map(|r| r.2).collect();
            holder.tick(&table, &claudes, &runs);
        }
        let agents = self
            .workers
            .attribute(&table.procs, &claudes, &crate::worker_pids::Machine);
        let claude_of = self.claude_of(&table.procs, &claudes, &runs);
        Some(table.reading(
            su::now_ms(),
            app,
            |p| claudes.contains(&p),
            &runs,
            &agents,
            &claude_of,
        ))
    }

    /// Each running run's session's claude, among `claudes`: the claude
    /// whose registry file names it ([`crate::registry::session_of_pid`]).
    /// Read only while something runs.
    #[cfg(target_os = "linux")]
    fn claude_of(
        &mut self,
        procs: &[Proc],
        claudes: &HashSet<u32>,
        runs: &[(String, String, HashSet<u32>)],
    ) -> HashMap<String, u32> {
        if runs.is_empty() {
            return HashMap::new();
        }
        let dirs = self
            .config_dirs
            .get_or_insert_with(|| crate::registry::ambient_config_dirs(None));
        let wanted: HashSet<&str> = runs.iter().map(|r| r.0.as_str()).collect();
        procs
            .iter()
            .filter(|p| claudes.contains(&p.pid))
            .filter_map(|p| {
                let session = crate::registry::session_of_pid(dirs, p.pid, p.start)?;
                wanted
                    .contains(session.as_str())
                    .then_some((session, p.pid))
            })
            .collect()
    }

    /// macOS: no figures are shown (the reading is Linux's), but the app's
    /// sampler reads the table each pass to hold the limits itself
    /// ([`crate::user_cap`]): every claude under the app and
    /// every plain `run` under `runs_dir`.
    #[cfg(target_os = "macos")]
    pub fn sample(&mut self, app: u32, runs_dir: &Path) -> Option<Reading> {
        let holder = self.holder.as_mut()?;
        let snap = crate::user_cap::Snapshot::take()?;
        let now = std::time::Instant::now();
        let tree = subtree(&snap.procs, app);
        let claudes = self.claudes.among(&snap, &tree);
        let table = Table {
            spent: pid_ticks(&snap.procs, self.prev.as_ref().map(|p| &p.0)),
            over_ms: self
                .prev
                .as_ref()
                .map_or(0, |(_, at)| now.duration_since(*at).as_millis() as u64),
            ticks_per_s: snap.ticks_per_s,
            cores: std::thread::available_parallelism()
                .map(|n| n.get() as u32)
                .unwrap_or(1),
            procs: snap.procs,
            ..Table::default()
        };
        self.prev = Some((seen(&table.procs), now));
        let runs: HashSet<u32> = crate::run_live::running_plain(runs_dir)
            .into_iter()
            .map(|r| r.2)
            .collect();
        holder.tick(&table, &claudes, &runs);
        None
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub fn sample(&mut self, _app: u32, _runs_dir: &Path) -> Option<Reading> {
        None
    }

    /// Ask `nvidia-smi` again when due; collect a finished `pmon`.
    #[cfg(target_os = "linux")]
    fn poll_gpu(&mut self, now: std::time::Instant) {
        use std::io::Read;
        use std::process::{Command, Stdio};
        let Some(smi) = crate::session_use::gpu::find_on_path("nvidia-smi") else {
            return;
        };
        if let Some(child) = &mut self.pmon_child
            && let Ok(Some(status)) = child.try_wait()
        {
            let mut out = String::new();
            if let Some(so) = child.stdout.as_mut() {
                let _ = so.read_to_string(&mut out);
            }
            // Unsupported here (some drivers, WSL): no figure.
            self.pmon = status.success().then(|| parse_pmon(&out));
            self.pmon_child = None;
        }
        if self
            .pmon_at
            .is_some_and(|at| now.duration_since(at) < Self::GPU_EVERY)
        {
            return;
        }
        self.pmon_at = Some(now);
        self.gpu = Command::new(&smi)
            .args([
                "--query-compute-apps=pid,used_memory",
                "--format=csv,noheader,nounits",
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| {
                (
                    crate::session_use::gpu::parse_compute_apps(&String::from_utf8_lossy(
                        &o.stdout,
                    )),
                    now,
                )
            });
        if self.pmon_child.is_none() {
            self.pmon_child = Command::new(&smi)
                .args(["pmon", "-c", "1", "-s", "u"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .ok();
        }
    }
}

/// `GIVERNY_USE_DUMP=<app pid> cargo test -p giverny-claude -- --ignored
/// dump_a_live_tree --nocapture`: take passes over a running app's tree,
/// read-only, and print what each costs and what it credits.
#[cfg(all(test, target_os = "linux"))]
mod dump {}
