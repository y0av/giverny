//! What a tree of processes uses, one process at a time: the pieces the
//! app's one reading ([`crate::use_reading`]) is taken with — `/proc`'s
//! table ([`proc_table`], [`parse_stat`]), a process and everything below
//! it ([`subtree`]), CPU as a share of the machine ([`cpu_pct`]), memory as
//! the proportional set ([`with_real_memory`]), GPU memory from
//! `nvidia-smi` ([`gpu`]) — and a size as the rail prints it ([`gb`]).
//!
//! Linux reads `/proc`. Elsewhere there is nothing to show and nothing is
//! spent.

use std::collections::{HashMap, HashSet};
#[cfg(target_os = "linux")]
use std::path::PathBuf;

/// One process as the tree sum needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    /// CPU time in clock ticks: its own and its reaped children's
    /// (`utime + stime + cutime + cstime`), so a command that ended is
    /// still counted, in its parent.
    pub ticks: u64,
    /// Memory, KiB: the stat line's resident set at first, the
    /// proportional set ([`with_real_memory`]) once that is read.
    pub mem_kb: u64,
    /// When it started, clock ticks after boot: a pid that comes back
    /// with another start is another process.
    pub start: u64,
}

/// `root` and every process below it.
pub fn subtree(procs: &[Proc], root: u32) -> HashSet<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for p in procs {
        if p.pid != p.ppid {
            children.entry(p.ppid).or_default().push(p.pid);
        }
    }
    let mut seen = HashSet::new();
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        if seen.insert(pid) {
            stack.extend(children.get(&pid).into_iter().flatten().copied());
        }
    }
    seen.retain(|pid| procs.iter().any(|p| p.pid == *pid));
    seen
}

/// CPU spent between two readings, percent of `cores` cores, rounded and
/// at most 100. A total that went down (a process left the tree with its
/// time) reads as nothing rather than wrapping.
pub fn cpu_pct(ticks_then: u64, ticks_now: u64, ticks_per_s: u64, over_ms: u64, cores: u32) -> u32 {
    if over_ms == 0 || cores == 0 || ticks_per_s == 0 {
        return 0;
    }
    let used_ms = ticks_now.saturating_sub(ticks_then) as f64 * 1000.0 / ticks_per_s as f64;
    (used_ms * 100.0 / (over_ms as f64 * cores as f64))
        .round()
        .clamp(0.0, 100.0) as u32
}

/// A size in G: `0.0G` for nothing, `0.1G` at the least for anything,
/// tenths below ten (`4.2G`), whole above (`12G`), T from a thousand G.
pub fn gb(mb: u64) -> String {
    if mb == 0 {
        return "0.0G".into();
    }
    let mut v = mb as f64 / 1024.0;
    let mut unit = 'G';
    if v >= 999.5 {
        v /= 1024.0;
        unit = 'T';
    }
    if v < 9.95 {
        format!("{:.1}{unit}", v.max(0.1))
    } else {
        format!("{:.0}{unit}", v.min(999.0))
    }
}

/// This machine's RAM, MiB (`MemTotal` in `/proc/meminfo`); 0 where it is
/// not known.
pub fn ram_mb() -> u64 {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|m| parse_mem_total(&m))
            .unwrap_or(0)
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

/// The `MemTotal:` line of `/proc/meminfo`, MiB.
pub fn parse_mem_total(meminfo: &str) -> Option<u64> {
    meminfo
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))
        .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
        .map(|kb| kb / 1024)
}

#[cfg(target_os = "linux")]
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(target_os = "linux")]
pub(crate) fn sysconf(name: libc::c_int) -> Option<u64> {
    // SAFETY: sysconf only reads a configuration value.
    let v = unsafe { libc::sysconf(name) };
    u64::try_from(v).ok().filter(|v| *v > 0)
}

/// Every process in `/proc`.
#[cfg(target_os = "linux")]
pub(crate) fn proc_table(page_kb: u64) -> Vec<Proc> {
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    rd.flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(|pid| {
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            parse_stat(pid, &stat, page_kb)
        })
        .collect()
}

/// One `/proc/<pid>/stat` line. The fields after the parenthesized name
/// (which may hold spaces or parentheses itself) count from `state`
/// (field 3): `ppid` is field 4, `utime`…`cstime` 14–17, `starttime` 22,
/// `rss` (pages) 24.
pub fn parse_stat(pid: u32, stat: &str, page_kb: u64) -> Option<Proc> {
    let (_, rest) = stat.rsplit_once(')')?;
    let f: Vec<&str> = rest.split_whitespace().collect();
    let n = |field: usize| f.get(field - 3)?.parse::<i64>().ok();
    let ticks = (14..=17).map(|i| n(i).unwrap_or(0).max(0) as u64).sum();
    Some(Proc {
        pid,
        ppid: n(4)? as u32,
        ticks,
        mem_kb: n(24).unwrap_or(0).max(0) as u64 * page_kb,
        start: n(22).unwrap_or(0).max(0) as u64,
    })
}

/// Is a process name Claude Code's? `claude`, by its short name or its
/// `argv[0]`'s last part.
pub fn is_claude_name(name: &str) -> bool {
    let name = name.trim();
    name.rsplit('/').next().unwrap_or(name) == "claude"
}

/// Whether `pid` is a Claude Code process: its short name (`comm`) or its
/// `argv[0]` is `claude`.
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

/// The `Pss:` line of a `/proc/<pid>/smaps_rollup`, KiB.
pub fn parse_pss(smaps_rollup: &str) -> Option<u64> {
    smaps_rollup
        .lines()
        .find_map(|l| l.strip_prefix("Pss:"))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
}

/// Give each process in `pids` the memory it really uses: its
/// proportional set (`Pss` in `/proc/<pid>/smaps_rollup`), where every
/// page shared with other processes is split between them. The resident
/// set counts a shared page once per sharer — the code of every `node`,
/// `rustc` or `bash`, shared libraries — so a tree's resident sets add up
/// to more than it uses; its proportional sets add up to what it holds
/// (pages shared with processes outside the tree count only by its
/// share). A process whose file cannot be read (gone, or another user's)
/// keeps its resident set.
#[cfg(target_os = "linux")]
pub(crate) fn with_real_memory(procs: &mut [Proc], pids: &HashSet<u32>) {
    for p in procs.iter_mut().filter(|p| pids.contains(&p.pid)) {
        if let Some(kb) = std::fs::read_to_string(format!("/proc/{}/smaps_rollup", p.pid))
            .ok()
            .and_then(|t| parse_pss(&t))
        {
            p.mem_kb = kb;
        }
    }
}

/// GPU memory per process, from `nvidia-smi --query-compute-apps`, which the
/// app's sampler asks now and then ([`crate::use_reading::Sampler`]).
pub mod gpu {
    use super::*;

    /// `pid, used_memory` lines (MiB, `--format=csv,noheader,nounits`);
    /// a line that does not read is skipped.
    pub fn parse_compute_apps(out: &str) -> HashMap<u32, u64> {
        let mut m = HashMap::new();
        for l in out.lines() {
            let mut f = l.split(',').map(str::trim);
            let (Some(pid), Some(mb)) = (f.next(), f.next()) else {
                continue;
            };
            if let (Ok(pid), Ok(mb)) = (pid.parse::<u32>(), mb.parse::<u64>()) {
                *m.entry(pid).or_default() += mb;
            }
        }
        m
    }

    /// The GPU memory of the processes in `pids`, MiB: `apps` (a
    /// [`parse_compute_apps`] answer) summed over them.
    pub fn of(apps: &HashMap<u32, u64>, pids: &HashSet<u32>) -> u64 {
        apps.iter()
            .filter(|(pid, _)| pids.contains(pid))
            .map(|(_, mb)| mb)
            .sum()
    }

    /// `nvidia-smi`, looked for once: in a `$PATH` directory, or WSL's
    /// own in `/usr/lib/wsl/lib`. Windows drives mounted under WSL
    /// (`/mnt/c/…`, a score of them on a usual `$PATH`) are skipped: each
    /// look there costs milliseconds, and a Linux `nvidia-smi` is never
    /// there.
    #[cfg(target_os = "linux")]
    pub(crate) fn nvidia_smi() -> Option<PathBuf> {
        static SMI: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
        SMI.get_or_init(|| {
            let path = std::env::var_os("PATH")?;
            std::env::split_paths(&path)
                .filter(|d| !d.starts_with("/mnt"))
                .chain(std::iter::once(PathBuf::from("/usr/lib/wsl/lib")))
                .map(|d| d.join("nvidia-smi"))
                .find(|p| p.is_file())
        })
        .clone()
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
            start: 0,
        }
    }

    #[test]
    fn a_subtree_is_its_root_and_everything_below_it() {
        // init → shell → claude → {bash → cargo → rustc, sh → statusline},
        // and a neighbour claude with its own child.
        let procs = [
            p(1, 0, 5, 10),
            p(10, 1, 1, 100),
            p(20, 10, 100, 400_000),
            p(30, 20, 2, 4_000),
            p(31, 30, 50, 100_000),
            p(32, 31, 400, 900_000),
            p(40, 20, 0, 1_000),
            p(41, 40, 1, 2_000),
            p(50, 10, 999, 999_999),
            p(51, 50, 999, 999_999),
        ];
        let tree = subtree(&procs, 20);
        let mut got: Vec<u32> = tree.iter().copied().collect();
        got.sort();
        assert_eq!(got, [20, 30, 31, 32, 40, 41]);
        // A root that is gone is nothing.
        assert!(subtree(&procs, 77).is_empty());
        // A parent loop does not hang the walk.
        let looped = [p(5, 6, 1, 1), p(6, 5, 1, 1)];
        assert_eq!(subtree(&looped, 5).len(), 2);
    }

    #[test]
    fn stat_lines_are_read_past_an_awkward_name() {
        let line = "4242 (we ird) (name)) S 20 4242 4242 0 -1 4194560 100 0 0 0 \
                    150 50 7 3 20 0 1 0 12345 104857600 2048 18446744073709551615";
        let got = parse_stat(4242, line, 4).unwrap();
        assert_eq!(got.ppid, 20);
        assert_eq!(got.ticks, 150 + 50 + 7 + 3);
        assert_eq!(got.mem_kb, 2048 * 4);
        assert_eq!(got.start, 12345);
        assert_eq!(parse_stat(1, "garbage", 4), None);
    }

    #[test]
    fn claude_is_told_by_its_name() {
        assert!(is_claude_name("claude\n"));
        assert!(is_claude_name("/home/u/.local/bin/claude"));
        assert!(!is_claude_name("node"));
        assert!(!is_claude_name("claude-code-helper"));
    }

    #[test]
    fn real_memory_is_the_proportional_set() {
        let rollup = "55d0c0de0000-7ffd00000000 ---p 00000000 00:00 0  [rollup]\n\
                      Rss:              460000 kB\n\
                      Pss:              362123 kB\n\
                      Pss_Anon:         300000 kB\n";
        assert_eq!(parse_pss(rollup), Some(362_123));
        assert_eq!(parse_pss("Rss: 5 kB\n"), None);
    }

    #[test]
    fn the_machines_ram_is_mem_total() {
        let meminfo = "MemTotal:       32768000 kB\nMemFree:  100 kB\n";
        assert_eq!(parse_mem_total(meminfo), Some(32_000));
        assert_eq!(parse_mem_total("MemFree: 100 kB\n"), None);
    }

    /// This process's proportional set is read, and it is at most its
    /// resident set.
    #[cfg(target_os = "linux")]
    #[test]
    fn this_process_gets_its_proportional_set() {
        let mut procs = proc_table(4);
        let me = std::process::id();
        let rss = procs.iter().find(|p| p.pid == me).unwrap().mem_kb;
        with_real_memory(&mut procs, &HashSet::from([me]));
        let pss = procs.iter().find(|p| p.pid == me).unwrap().mem_kb;
        assert!(pss > 0 && pss <= rss + 1024, "pss {pss} rss {rss}");
        assert!(ram_mb() > 0, "this machine's RAM");
    }

    #[test]
    fn cpu_is_a_share_of_the_machine() {
        // 100 ticks a second; two cores' worth of a second on eight cores.
        assert_eq!(cpu_pct(0, 200, 100, 1_000, 8), 25);
        assert_eq!(cpu_pct(0, 99_999, 100, 1_000, 8), 100);
        // A total that fell (a process left the tree) is nothing, not a wrap.
        assert_eq!(cpu_pct(500, 100, 100, 1_000, 8), 0);
        assert_eq!(cpu_pct(0, 100, 100, 0, 8), 0);
    }

    #[test]
    fn sizes_read_in_g() {
        assert_eq!(gb(0), "0.0G");
        assert_eq!(gb(1), "0.1G");
        assert_eq!(gb(4300), "4.2G");
        assert_eq!(gb(12 * 1024), "12G");
        assert_eq!(gb(1536 * 1024), "1.5T");
    }

    #[test]
    fn nvidia_smi_compute_apps_are_summed_per_pid() {
        let m = gpu::parse_compute_apps("4242, 1200\n4242, 24\n77, 300\n[N/A], 5\n\n");
        assert_eq!(m.get(&4242), Some(&1224));
        assert_eq!(m.get(&77), Some(&300));
        assert_eq!(m.len(), 2);
        assert_eq!(gpu::of(&m, &HashSet::from([4242, 9])), 1224);
        assert_eq!(gpu::of(&m, &HashSet::new()), 0);
    }

    /// The real table: this test's own process is in it, with a parent.
    #[cfg(target_os = "linux")]
    #[test]
    fn this_process_is_in_its_parents_subtree() {
        let procs = proc_table(4);
        let me = std::process::id();
        let mine = procs.iter().find(|p| p.pid == me).expect("in /proc");
        let tree = subtree(&procs, mine.ppid);
        assert!(tree.contains(&me));
        assert!(mine.mem_kb > 0, "some memory is resident");
    }
}
