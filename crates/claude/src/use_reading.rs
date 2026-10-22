//! One reading of what Giverny runs, at one moment, measured one way.
//!
//! The app reads `/proc` once a second ([`Sampler`]) and derives every
//! figure it shows from that one pass:
//!
//! * the **total**: the app and everything under it, every tab, Claude or
//!   not (the line under the account bars);
//! * each **tab**: its shell and everything under it (the rail's tab rows
//!   and collapsed group headers).
//!
//! Every group is a set of processes, and every figure is a sum over its
//! processes of the same per-process figures from the same pass: CPU
//! clock ticks spent since the last pass ([`pid_ticks`]), as a share of
//! the machine; memory as the proportional set (shared pages split
//! between their sharers); GPU memory and compute per process from
//! `nvidia-smi`. So a tab never reads more than the total.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::session_use::{Proc, subtree};

/// One group's use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Use {
    /// CPU since the last pass, percent of the whole machine.
    pub cpu_pct: u32,
    /// Proportional set summed, MiB.
    pub mem_mb: u64,
    /// GPU memory, MiB; `None` without an NVIDIA GPU.
    pub gpu_mb: Option<u64>,
    /// Share of the GPUs' compute, percent; `None` without a GPU that
    /// reports it per process.
    pub gpu_pct: Option<u32>,
}

/// One pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reading {
    /// When it was taken, ms since the epoch.
    pub at_ms: u64,
    /// The app's pid, the root of [`Reading::total`].
    pub app: u32,
    pub total: Use,
    /// Each tab, by its shell's pid (a child of the app): the shell and
    /// everything under it, Claude Code or not.
    pub tabs: BTreeMap<u32, Use>,
    /// How many Claude Code processes are under the app.
    pub claudes: usize,
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

    /// The reading: `app`'s tree, each of its tabs, and how many of its
    /// processes are claudes (`is_claude`).
    pub fn reading(&self, at_ms: u64, app: u32, is_claude: impl Fn(u32) -> bool) -> Reading {
        let tree = subtree(&self.procs, app);
        let tabs = self.tabs(app, &tree);
        Reading {
            at_ms,
            app,
            total: self.use_of(&tree),
            tabs: tabs
                .iter()
                .map(|(&tab, pids)| (tab, self.use_of(pids)))
                .collect(),
            claudes: tree.iter().filter(|&&p| is_claude(p)).count(),
        }
    }

    /// [`Reading::tabs`]' processes: each child of `app` (a tab's shell)
    /// with every process under it, from `tree` (`app`'s subtree), in one
    /// sweep.
    fn tabs(&self, app: u32, tree: &HashSet<u32>) -> BTreeMap<u32, HashSet<u32>> {
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
        by_tab
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
// Only Linux takes a reading.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct Sampler {
    prev: Option<(Seen, std::time::Instant)>,
    gpu: Option<(HashMap<u32, u64>, std::time::Instant)>,
    pmon: Option<Pmon>,
    pmon_child: Option<std::process::Child>,
    pmon_at: Option<std::time::Instant>,
}

impl Sampler {
    #[cfg(target_os = "linux")]
    const GPU_EVERY: std::time::Duration = std::time::Duration::from_secs(6);

    /// One pass for the app `app`. `None` where there is no `/proc`. The
    /// first pass has no CPU yet.
    #[cfg(target_os = "linux")]
    pub fn sample(&mut self, app: u32) -> Option<Reading> {
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
        // Memory is read for what is shown: the app's tree.
        let shown = subtree(&procs, app);
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
        Some(table.reading(su::now_ms(), app, su::proc_is_claude))
    }

    #[cfg(not(target_os = "linux"))]
    pub fn sample(&mut self, _app: u32) -> Option<Reading> {
        None
    }

    /// Ask `nvidia-smi` again when due; collect a finished `pmon`.
    #[cfg(target_os = "linux")]
    fn poll_gpu(&mut self, now: std::time::Instant) {
        use std::io::Read;
        use std::process::{Command, Stdio};
        let Some(smi) = crate::session_use::gpu::nvidia_smi() else {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: u32, ppid: u32, ticks: u64, mem_kb: u64) -> Proc {
        Proc {
            pid,
            ppid,
            ticks,
            mem_kb,
            start: pid as u64,
        }
    }

    /// app 100 → shell 110 → claude 120 → bash 130 → cargo 140 → rustc 141,
    /// app → shell 210 → claude 220; init 1 and a process outside the app.
    fn machine() -> Vec<Proc> {
        vec![
            p(1, 0, 0, 0),
            p(100, 1, 300, 200_000),
            p(110, 100, 10, 4_000),
            p(120, 110, 500, 300_000),
            p(130, 120, 20, 3_000),
            p(140, 130, 50, 50_000),
            p(141, 140, 900, 900_000),
            p(210, 100, 5, 4_000),
            p(220, 210, 100, 250_000),
            p(900, 1, 4_000, 2_000_000),
        ]
    }

    fn table(procs: Vec<Proc>, prev: Option<&Seen>, gpu: Option<HashMap<u32, u64>>) -> Table {
        Table {
            spent: pid_ticks(&procs, prev),
            procs,
            gpu_mb: gpu,
            pmon: None,
            over_ms: 1_000,
            ticks_per_s: 100,
            cores: 4,
        }
    }

    #[test]
    fn each_tab_is_within_the_total() {
        let before = machine();
        let mut after = machine();
        for q in &mut after {
            q.ticks += match q.pid {
                141 => 150, // the build: a core and a half
                120 => 40,
                220 => 100,
                900 => 400, // outside: never counted
                _ => 5,
            };
        }
        let gpu = HashMap::from([(141, 2048), (220, 512), (900, 4096)]);
        let t = table(after, Some(&seen(&before)), Some(gpu));
        let r = t.reading(42, 100, |p| p == 120 || p == 220);
        assert_eq!(r.claudes, 2);
        // CPU, of 400 ticks a second: shell A 5+40+5+5+150, B 5+100, and
        // the app's own 5 in the total.
        let (a, b) = (r.tabs[&110], r.tabs[&210]);
        assert_eq!(a.cpu_pct, 51);
        assert_eq!(b.cpu_pct, 26);
        assert_eq!(r.total.cpu_pct, 79, "the app and both tabs");
        assert_eq!(a.mem_mb, 1_257_000u64.div_ceil(1024));
        assert_eq!(r.total.mem_mb, 1_711_000u64.div_ceil(1024));
        assert_eq!(
            (a.gpu_mb, b.gpu_mb, r.total.gpu_mb),
            (Some(2048), Some(512), Some(2560))
        );
        for tab in [a, b] {
            assert!(tab.cpu_pct <= r.total.cpu_pct, "{tab:?} {:?}", r.total);
            assert!(tab.mem_mb <= r.total.mem_mb, "{tab:?} {:?}", r.total);
        }
    }

    /// Every tab reads, Claude Code in it or not: its shell and every
    /// process under it, each counted once.
    #[test]
    fn every_tab_is_its_whole_shell_claude_or_not() {
        let before = machine();
        let mut after = machine();
        for q in &mut after {
            q.ticks += 10;
        }
        let t = table(after, Some(&seen(&before)), None);
        let none = t.reading(42, 100, |_| false);
        assert_eq!(none.claudes, 0);
        assert_eq!(
            none.tabs.keys().copied().collect::<Vec<_>>(),
            vec![110, 210]
        );
        for shell in [110, 210] {
            assert_eq!(none.tabs[&shell], t.use_of(&subtree(&t.procs, shell)));
        }
        // Whichever claudes are where, the same tabs.
        for claudes in [&[120, 141][..], &[130, 220], &[220]] {
            let r = t.reading(42, 100, |p| claudes.contains(&p));
            assert_eq!(r.tabs, none.tabs, "{claudes:?}");
            assert_eq!(r.claudes, claudes.len());
        }
        // The app itself and processes outside it are in no tab.
        assert!(!none.tabs.contains_key(&100) && !none.tabs.contains_key(&900));
    }

    #[test]
    fn the_first_pass_has_no_cpu() {
        let t = table(machine(), None, None);
        let r = t.reading(1, 100, |p| p == 120);
        assert_eq!(r.total.cpu_pct, 0);
        assert!(r.total.mem_mb > 0);
        assert_eq!(r.total.gpu_mb, None);
    }

    #[test]
    fn a_process_that_ended_is_counted_once() {
        // 141 (rustc) ran 900 ticks by the last pass, then 100 more and
        // ended; cargo (140) reaped it, its ticks now hold 1000 of rustc's.
        let before = machine();
        let after: Vec<Proc> = machine()
            .into_iter()
            .filter(|q| q.pid != 141)
            .map(|mut q| {
                if q.pid == 140 {
                    q.ticks += 1_000 + 10;
                }
                q
            })
            .collect();
        let spent = pid_ticks(&after, Some(&seen(&before)));
        assert_eq!(spent[&140], 110, "its own 10 and rustc's last 100");
        // A new process spent all it has; a reused pid is new.
        let mut again = machine();
        again.push(p(150, 130, 30, 1));
        again[9].start = 7;
        let spent = pid_ticks(&again, Some(&seen(&before)));
        assert_eq!(spent[&150], 30);
        assert_eq!(spent[&900], 4_000);
        // A total that fell (time left with a process) is nothing.
        let mut fell = machine();
        fell[1].ticks = 0;
        assert_eq!(pid_ticks(&fell, Some(&seen(&before)))[&100], 0);
    }

    #[test]
    fn gpu_utilisation_is_the_share_of_every_gpu() {
        let out = "# gpu        pid  type    sm   mem   enc   dec   command\n\
                   # Idx          #   C/G     %     %     %     %   name\n\
                   \x20   0      4242     C    60    10     -     -   python\n\
                   \x20   0      5000     C    30     5     -     -   other\n\
                   \x20   1      4243     C    20     1     -     -   python\n\
                   \x20   1      6000     G     -     -     -     -   Xorg\n";
        let p = parse_pmon(out);
        assert_eq!(p.gpus, 2);
        assert_eq!(p.sm.get(&6000), None, "`-` is no figure");
        assert_eq!(gpu_pct(&p, &HashSet::from([4242, 4243])), 40);
        let idle = parse_pmon("    0          -     -     -     -     -     -   -\n");
        assert_eq!((idle.gpus, idle.sm.len()), (1, 0));
        assert_eq!(gpu_pct(&Pmon::default(), &HashSet::from([1])), 0);
    }

    /// A real pass over this machine: the test process's own tree.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_real_pass_reads_this_process() {
        let mut s = Sampler::default();
        let me = std::process::id();
        let first = s.sample(me).expect("a /proc");
        assert_eq!(first.total.cpu_pct, 0);
        assert!(first.total.mem_mb > 0);
    }
}
