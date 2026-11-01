//! What a tree of processes uses, one process at a time: the pieces the
//! app's one reading ([`crate::use_reading`]) is taken with — `/proc`'s
//! table ([`proc_table`], [`parse_stat`]), a process and everything below
//! it ([`subtree`]), CPU as a share of the machine ([`cpu_pct`]), memory as
//! the proportional set ([`with_real_memory`]), GPU memory from
//! `nvidia-smi` ([`gpu`]) — and the figures as the management panel and the
//! sidebar print them ([`gb`], [`cpu8`]).
//!
//! It once also measured the session for Claude Code's status line, which
//! no longer shows its use.
//!
//! Linux reads `/proc`. macOS has its own table ([`proc_table`] there, from
//! `libproc`), read only for the ceilings Giverny holds itself
//! ([`crate::user_cap`]); nothing is shown from it. Elsewhere
//! there is nothing to show and nothing is spent.

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

/// A size in G, the management panel's way: `0.0G` for nothing, `0.1G` at the
/// least for anything, tenths below ten (`4.2G`), whole above (`12G`), T
/// from a thousand G.
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

/// [`gb`] in exactly four columns, right-aligned: `4.2G`, ` 12G`.
pub fn gb4(mb: u64) -> String {
    format!("{:>4}", gb(mb))
}

/// CPU in exactly eight columns, right-aligned: ` 45% CPU`, `100% CPU`.
pub fn cpu8(pct: u32) -> String {
    format!("{:>3}% CPU", pct.min(100))
}

/// Separates a use figure's pieces in the management panel's use cell
/// (` 45% CPU  ·  4.2G`), and the status line's segments.
pub const SEP: &str = "  ·  ";

/// How many columns Claude Code takes from the terminal's width around the
/// status line: it is drawn two columns in, and a line that reaches the
/// last two columns is cut short with `…` (measured on Claude Code 2.1,
/// whatever `padding` says).
pub const CLAUDE_MARGIN: usize = 4;

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

/// Every process macOS lets this user read, as `/proc`'s
/// table is on Linux, with each one's short name (`p_comm`, at most 16
/// bytes) beside it. From `libproc`: `proc_listallpids`, the short BSD
/// info (parent, name) and `proc_pid_rusage` (`RUSAGE_INFO_V2`):
///
/// - **ticks** are milliseconds (a table read here is diffed at 1000 a
///   second): user + system time, its own and its reaped children's, as on
///   Linux, so a command that ended is still counted in its parent;
/// - **memory** is the physical footprint, what Activity Monitor shows as a
///   process's memory and what the kernel's memory pressure counts: its
///   private pages, compressed ones included, and not the shared libraries
///   every process maps (which a resident set counts once per process);
/// - **start** is the process's start in Mach absolute time: a pid that
///   comes back with another is another process.
///
/// A process whose usage cannot be read (another user's) is listed with
/// none, so the tree above it still holds together.
#[cfg(target_os = "macos")]
pub(crate) fn proc_table() -> (Vec<Proc>, HashMap<u32, String>) {
    use std::mem::{size_of, zeroed};
    // SAFETY: a null buffer asks only for the count.
    let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if n <= 0 {
        return (Vec::new(), HashMap::new());
    }
    // Room for processes started since the count.
    let mut pids: Vec<libc::c_int> = vec![0; n as usize + 256];
    let bytes = (pids.len() * size_of::<libc::c_int>()) as libc::c_int;
    // SAFETY: the buffer holds `bytes` bytes; the call fills at most that.
    let got = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    pids.truncate(got.max(0) as usize);
    let (numer, denom) = mach_timebase();
    let abs_ms =
        |t: u64| (u128::from(t) * u128::from(numer) / u128::from(denom) / 1_000_000) as u64;
    let mut procs = Vec::with_capacity(pids.len());
    let mut comm = HashMap::with_capacity(pids.len());
    for pid in pids.into_iter().filter(|&p| p > 0) {
        // SAFETY: zeroed is a valid value of these plain C structs, and
        // each call writes at most the size it is given.
        let mut info: libc::proc_bsdshortinfo = unsafe { zeroed() };
        let size = size_of::<libc::proc_bsdshortinfo>() as libc::c_int;
        let r = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDT_SHORTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdshortinfo).cast(),
                size,
            )
        };
        if r != size {
            continue;
        }
        let mut ru: libc::rusage_info_v2 = unsafe { zeroed() };
        let ok = unsafe {
            libc::proc_pid_rusage(
                pid,
                libc::RUSAGE_INFO_V2,
                (&mut ru as *mut libc::rusage_info_v2).cast::<libc::rusage_info_t>(),
            )
        } == 0;
        let name: Vec<u8> = info
            .pbsi_comm
            .iter()
            .take_while(|c| **c != 0)
            .map(|c| *c as u8)
            .collect();
        comm.insert(pid as u32, String::from_utf8_lossy(&name).into_owned());
        procs.push(Proc {
            pid: pid as u32,
            ppid: info.pbsi_ppid,
            ticks: if ok {
                abs_ms(
                    ru.ri_user_time
                        + ru.ri_system_time
                        + ru.ri_child_user_time
                        + ru.ri_child_system_time,
                )
            } else {
                0
            },
            mem_kb: if ok { ru.ri_phys_footprint / 1024 } else { 0 },
            start: if ok { ru.ri_proc_start_abstime } else { 0 },
        });
    }
    (procs, comm)
}

/// Mach absolute time's ratio to nanoseconds (1/1 on Intel, 125/3 on
/// Apple Silicon), which `rusage_info`'s times are counted in.
#[cfg(target_os = "macos")]
#[allow(deprecated)] // libc points at `mach2` for this one call.
fn mach_timebase() -> (u32, u32) {
    static TB: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();
    *TB.get_or_init(|| {
        let mut tb = libc::mach_timebase_info { numer: 0, denom: 0 };
        // SAFETY: fills the struct it is given.
        let ok = unsafe { libc::mach_timebase_info(&mut tb) } == 0;
        if ok && tb.numer > 0 && tb.denom > 0 {
            (tb.numer, tb.denom)
        } else {
            (1, 1)
        }
    })
}

/// A process's `argv[0]` on macOS (`sysctl kern.procargs2`): what Claude
/// Code's launcher renames itself to (`process.title = "claude"`), which
/// its short name (`node`) does not show. `None` for another user's.
#[cfg(target_os = "macos")]
pub(crate) fn argv0(pid: u32) -> Option<String> {
    static ARGMAX: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let max = *ARGMAX.get_or_init(|| {
        let mut v: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>();
        let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
        // SAFETY: an int-sized out value for an int-valued name.
        let r = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                2,
                (&mut v as *mut libc::c_int).cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if r == 0 && v > 0 { v as usize } else { 1 << 20 }
    });
    let mut buf = vec![0u8; max];
    let mut len = buf.len();
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
    // SAFETY: `buf` holds `len` bytes; the call writes at most that.
    let r = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if r != 0 {
        return None;
    }
    buf.truncate(len);
    parse_procargs2(&buf)
}

/// `argv[0]` from a `kern.procargs2` answer: `argc` (a native `int`), the
/// executable's path, NUL padding, then `argv` NUL-separated.
pub fn parse_procargs2(buf: &[u8]) -> Option<String> {
    let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?);
    if argc < 1 {
        return None;
    }
    let rest = &buf[4..];
    let path_end = rest.iter().position(|b| *b == 0)?;
    let args = &rest[path_end..];
    let start = args.iter().position(|b| *b != 0)?;
    let args = &args[start..];
    let end = args.iter().position(|b| *b == 0).unwrap_or(args.len());
    Some(String::from_utf8_lossy(&args[..end]).into_owned())
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

    /// Whether the machine has `nvidia-smi`, so a GPU to show: looked for
    /// once.
    #[cfg(target_os = "linux")]
    pub fn present() -> bool {
        static SMI: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
        SMI.get_or_init(|| find_on_path("nvidia-smi")).is_some()
    }

    #[cfg(not(target_os = "linux"))]
    pub fn present() -> bool {
        false
    }

    /// The GPU memory of the processes in `pids`, MiB: `apps` (a
    /// [`parse_compute_apps`] answer) summed over them.
    pub fn of(apps: &HashMap<u32, u64>, pids: &HashSet<u32>) -> u64 {
        apps.iter()
            .filter(|(pid, _)| pids.contains(pid))
            .map(|(_, mb)| mb)
            .sum()
    }

    #[cfg(target_os = "linux")]
    /// `name` in a `$PATH` directory. Windows drives mounted under WSL
    /// (`/mnt/c/…`, a score of them on a usual `$PATH`) are skipped: each
    /// look there costs milliseconds, and a Linux `nvidia-smi` is never
    /// there (WSL's own is in `/usr/lib/wsl/lib`).
    pub(crate) fn find_on_path(name: &str) -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .filter(|d| !d.starts_with("/mnt"))
            .chain(std::iter::once(PathBuf::from("/usr/lib/wsl/lib")))
            .map(|d| d.join(name))
            .find(|p| p.is_file())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_lines_are_read_past_an_awkward_name() {
        let line = "4242 (we ird) (name)) S 20 4242 4242 0 -1 4194560 100 0 0 0 \
                    150 50 7 3 20 0 1 0 12345 104857600 2048 18446744073709551615";
        let got = parse_stat(4242, line, 4).unwrap();
        assert_eq!(got.ppid, 20);
        assert_eq!(got.ticks, 150 + 50 + 7 + 3);
        assert_eq!(got.mem_kb, 2048 * 4);
        assert_eq!(parse_stat(1, "garbage", 4), None);
    }
}
