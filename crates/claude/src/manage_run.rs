//! `giverny manage run <task> -- <cmd…>`: a worker's heavy command, given
//! its task's granted lease and measured.
//!
//! **The cap.** On Linux with a user systemd the command runs in a scope of
//! its own, in the slice every Claude tab's claude sits in
//! ([`crate::tab_cap::SLICE`]): `systemd-run --user --scope
//! --slice=giverny-claude.slice -p MemoryLow=<ram> -p MemoryHigh=<2×ram>
//! -p CPUWeight=`[`RUN_CPU_WEIGHT`] (and `OOMPolicy=continue`, so the scope
//! outlives a kill and can be read), visible in `systemctl --user status
//! giverny-run-…` while it runs. The grant is a guarantee, not a ceiling:
//! the slice's hard `CPUQuota`/`MemoryMax` at `[manager.limits]` is the one
//! ceiling, and inside it the command may use idle cores and free memory;
//! its grant's RAM is protected from reclaim, and above twice its grant it
//! is slowed (reclaimed), never killed. When the slice does run out, the
//! command (`oom_score_adj` 500) is killed before any claude.
//!
//! **CPU is shared, not reserved**: a lease holds no cores.
//! Every `run` scope gets the same `CPUWeight`, below the Claude tabs'
//! ([`crate::tab_cap::TAB_CPU_WEIGHT`]), so when the cores are contended
//! the builds split them evenly and the person's interactive claude goes first;
//! the ledger holds a claim back only while the CPU is measured busy.
//! `CARGO_BUILD_JOBS` follows the lease's RAM ([`jobs_for`], a job a GiB,
//! at most the limit's cores) unless set, so more idle cores never push a
//! build past its memory.
//!
//! **Anywhere else** (no user systemd, macOS, Windows,
//! `$GIVERNY_RUN_NO_SYSTEMD`) it runs plain, registered as running like a
//! scope ([`run_live`]) so the app and the ledger see it. On macOS, one
//! of the supported platforms (with Linux and WSL), the command starts at
//! low priority, `taskpolicy -c utility`, so the person's own work goes
//! first ([`low_priority`]), and Giverny holds it itself, with no cgroups
//! ([`crate::user_cap::RunGuard`]): four times a second its
//! tree is measured, and killed — the biggest process in it, reported as
//! an OOM kill — once it passes twice its lease (or the limit's RAM, if
//! less); its CPU is held to the limit's cores by a duty cycle, unless a
//! Giverny app holds it with the tabs' already. Linux without systemd and
//! Windows run it as they always have, the lease advisory
//! (`$GIVERNY_RUN_NO_SYSTEMD` on Linux stands in for macOS, for tests).
//!
//! **The lease.** The task's lease in the ledger ([`resources`]). With none,
//! `run` claims one itself — the default lease (`[management_panel.lease]`,
//! Settings → Management panel; 3G unless changed there) unless `--ram`
//! says —
//! waits while that is queued (the row waiting, as `eta --why wait`), and
//! releases it when the command ends. Refused (larger than the limits): the
//! command does not run, exit 5.
//!
//! **Slots.** For the command's life `run` holds an exclusive `flock` on
//! each slot its lease holds (`<ledger dir>/slots/<name>.lock`), so two
//! `run`s under one lease take a slot in turn; the second says so and marks
//! its row waiting until it has it.
//!
//! **Heartbeat.** Leases expire 20 minutes after their session's last
//! `giverny manage` command; while the command runs, `run` beats the
//! session's leases every [`HEARTBEAT_EVERY`].
//!
//! **Measured.** Peak memory: the scope cgroup's `memory.peak` (read by a
//! small `sh` shim inside the scope after the command exits, while the
//! cgroup still exists, along with `memory.events`' `oom_kill`), else the
//! largest process's RSS (`wait4`'s `ru_maxrss`). CPU time: `wait4`'s user +
//! system time of the command and every descendant it waited for. Both go
//! on the task's feed row (`usage`), and from there into the history when
//! the task lands, so a later `claim` can say what tasks like it peaked at.
//! A command the memory cap killed is reported as such.
//!
//! The exit code is the command's (128 + the signal when one killed it).
//!
//! **Killed, it takes its command with it**. Under a scope,
//! a `run` that gets SIGTERM, SIGHUP or SIGINT stops its own scope
//! ([`ScopeStop`]: SIGTERM to its processes, [`STOP_GRACE`] later
//! `cgroup.kill`), records and releases as usual, and exits 128 + the
//! signal. One that dies before it can (SIGKILL) is covered by the shim's
//! watcher, which stops the scope once `run`'s pid is gone. Run plain, it
//! stops its command's process tree the same way ([`TreeStop`]):
//! every process below the command, and any that detached
//! from it while `run` watched. A command that ends on its own is
//! unchanged: what it left running on purpose stays.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use giverny_core::limits::Mem;
use serde_json::{Map, Value, json};

use crate::manage::{self, Flags};
use crate::{manage_history, resources, run_live, user_cap};

/// How often a running command's session leases are beaten (the TTL is 20m).
pub const HEARTBEAT_EVERY: Duration = Duration::from_secs(5 * 60);
/// How often a queued claim is asked again.
const QUEUED_POLL: Duration = Duration::from_secs(15);
/// Set (non-empty): never use systemd; run plain.
pub const NO_SYSTEMD_ENV: &str = "GIVERNY_RUN_NO_SYSTEMD";
/// Where the shim writes what it read of the scope.
const STATS_ENV: &str = "GIVERNY_RUN_STATS";
/// The scope's unit, for the shim's watcher.
const UNIT_ENV: &str = "GIVERNY_RUN_UNIT";
/// `run`'s pid: the shim's watcher stops the scope once it is gone.
const PID_ENV: &str = "GIVERNY_RUN_PID";
/// Every `run` scope's `CPUWeight`: systemd's default, the
/// same for every worker's command — CPU is shared, not leased — and below
/// the Claude tabs' ([`crate::tab_cap::TAB_CPU_WEIGHT`]).
pub const RUN_CPU_WEIGHT: u64 = 100;

/// Runs the command inside its scope, then reads the scope's cgroup while
/// it still exists. `started` first, so an empty file means the scope never
/// ran (systemd-run failed) and the command can be run plain instead.
const SHIM: &str = r#"# giverny manage run: run the command in its systemd scope, then
# record the scope's peak memory and OOM kills while the scope still exists.
s="$GIVERNY_RUN_STATS"
cg=$(sed -n 's/^0:://p' /proc/self/cgroup 2>/dev/null)
[ -n "$cg" ] || cg=/nonexistent
d="/sys/fs/cgroup$cg"
[ -n "$s" ] && printf 'started\ncgroup %s\n' "$d" > "$s" 2>/dev/null
# A `run` that dies before its command (SIGKILL, say) must not
# leave the command running here. A watcher, in a session of its own so a
# signal to run's group does not take it, stops this scope once run's pid
# is gone: SIGTERM to its processes, then cgroup.kill. Only this scope, and
# only when it is the unit run named; any doubt and there is no watcher.
w=
u="$GIVERNY_RUN_UNIT"
case "$d" in
  */"$u".scope)
    if [ -n "$u" ] && [ -n "$GIVERNY_RUN_PID" ] && [ -e "$d/cgroup.procs" ]; then
      ss=; command -v setsid >/dev/null 2>&1 && ss=setsid
      $ss sh -c 'trap "" TERM HUP INT QUIT
while kill -0 "$1" 2>/dev/null; do sleep 1; done
for p in $(cat "$2/cgroup.procs" 2>/dev/null); do kill -TERM "$p" 2>/dev/null; done
sleep 3
echo 1 > "$2/cgroup.kill" 2>/dev/null || systemctl --user kill --signal=KILL "$3.scope" >/dev/null 2>&1' \
        sh "$GIVERNY_RUN_PID" "$d" "$u" </dev/null >/dev/null 2>&1 &
      w=$!
    fi;;
esac
# The command goes first when the slice runs out of memory, before any
# claude and before this shim, which still has to read the cgroup.
sh -c 'echo 500 > /proc/self/oom_score_adj 2>/dev/null; exec "$@"' sh "$@"
rc=$?
# Ended on its own: the watcher goes (with its group, when setsid made one),
# and whatever the command left running on purpose stays.
[ -n "$w" ] && { kill -9 -"$w" 2>/dev/null || kill -9 "$w" 2>/dev/null; }
if [ -n "$s" ]; then
  {
    echo started
    [ -r "$d/memory.peak" ] && echo "peak $(cat "$d/memory.peak")"
    grep '^oom_kill ' "$d/memory.events" 2>/dev/null
  } > "$s" 2>/dev/null
fi
exit $rc
"#;

/// How the command is run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// In a `systemd-run --user --scope`; `expand_flag` when systemd is new
    /// enough (254) to take `--expand-environment=no`, so a `$` in the
    /// command is never expanded by systemd.
    Systemd { expand_flag: bool },
    /// Plain, uncapped: the lease is advisory.
    Plain,
}

/// A user systemd that `systemd-run --user --scope` can use, else plain.
pub fn detect_mode() -> Mode {
    if std::env::var_os(NO_SYSTEMD_ENV).is_some_and(|v| !v.is_empty()) {
        return Mode::Plain;
    }
    #[cfg(target_os = "linux")]
    {
        use std::process::Stdio;
        let quiet = |c: &mut Command| {
            c.stdin(Stdio::null()).stderr(Stdio::null());
        };
        let mut show = Command::new("systemctl");
        show.args(["--user", "show", "-p", "Version", "--value"]);
        quiet(&mut show);
        let major = show
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .trim()
                    .split(|c: char| !c.is_ascii_digit())
                    .next()
                    .and_then(|n| n.parse::<u32>().ok())
            });
        let mut run = Command::new("systemd-run");
        run.arg("--version").stdout(Stdio::null());
        quiet(&mut run);
        if let Some(major) = major
            && run.status().is_ok_and(|s| s.success())
        {
            return Mode::Systemd {
                expand_flag: major >= 254,
            };
        }
    }
    Mode::Plain
}

/// Does `run` hold a command to its lease's bound here
/// ([`resources::lease_bound_mb`])? Under a systemd scope, or held by
/// Giverny itself ([`user_cap::active`]) — the probe `run` makes; else the
/// lease is advisory.
pub fn caps_here() -> bool {
    matches!(detect_mode(), Mode::Systemd { .. }) || user_cap::active()
}

/// Say beside the ledger whether this `run` held its command
/// ([`resources::run_caps_path`]), so the ledger counts leases by it. Only
/// when that changed; a failure costs only that.
fn record_caps(ledger: &Path, capped: bool) {
    let path = resources::run_caps_path(ledger);
    let word = if capped { "capped" } else { "advisory" };
    if std::fs::read_to_string(&path).is_ok_and(|t| t.trim() == word) {
        return;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&tmp, format!("{word}\n")).is_ok() && std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// What the command is held to: the lease's RAM (0 = no memory cap), and
/// the build jobs that RAM pays for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cap {
    pub ram_mb: u64,
    /// `CARGO_BUILD_JOBS` unless set ([`jobs_for`]).
    pub jobs: u32,
}

impl Cap {
    /// `3G, 3 jobs`.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if self.ram_mb > 0 {
            parts.push(Mem(self.ram_mb).to_string());
        }
        parts.push(match self.jobs {
            1 => "1 job".to_string(),
            n => format!("{n} jobs"),
        });
        parts.join(", ")
    }
}

/// Build jobs for a lease of `ram_mb`: about one a GiB —
/// what a rustc job peaks at — at least one, at most `cores` (the limit's).
/// From RAM, not from cores: more idle cores must never push a build past
/// the memory it holds. A lease of no RAM builds by `default_mb`.
pub fn jobs_for(ram_mb: u64, default_mb: u64, cores: u32) -> u32 {
    let mb = if ram_mb > 0 { ram_mb } else { default_mb };
    let jobs = u32::try_from(mb / 1024).unwrap_or(u32::MAX);
    jobs.min(cores.max(1)).max(1)
}

/// `cmd` as a plain run starts it: on macOS, which has no
/// systemd scope to weigh it, at low priority — `taskpolicy -c utility`,
/// when it is there — so the person's own work goes first and a build
/// still gets every idle core. As given anywhere else: Linux has its
/// scope, and other platforms are not supported for this.
pub fn low_priority(cmd: &[String]) -> Vec<String> {
    if cfg!(target_os = "macos") && Path::new(TASKPOLICY).exists() {
        return [TASKPOLICY, "-c", "utility"]
            .iter()
            .map(|s| s.to_string())
            .chain(cmd.iter().cloned())
            .collect();
    }
    cmd.to_vec()
}

/// macOS's tool for a process's QoS clamp.
const TASKPOLICY: &str = "/usr/sbin/taskpolicy";

/// A unit name from `task`: `giverny-run-giverny_161-4242`.
pub fn unit_name(task: &str, pid: u32) -> String {
    let t: String = task
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    format!("giverny-run-{t}-{pid}")
}

/// `systemd-run`'s arguments for `cmd` in a capped scope, through `shim`.
pub fn scope_args(
    expand_flag: bool,
    unit: &str,
    task: &str,
    cap: &Cap,
    shim: &Path,
    cmd: &[String],
) -> Vec<String> {
    let mut a: Vec<String> = ["--user", "--scope", "--quiet", "--collect"]
        .map(String::from)
        .into();
    if expand_flag {
        a.push("--expand-environment=no".into());
    }
    // Beside the Claude tabs, inside the machine's budget.
    a.push(format!("--slice={}", crate::tab_cap::SLICE));
    a.push(format!("--unit={unit}"));
    a.push(format!(
        "--description=giverny manage run {}",
        task.replace('%', "%%")
    ));
    let mut prop = |p: String| {
        a.push("-p".into());
        a.push(p);
    };
    // Guarantees from the grant; the slice is the hard ceiling. A soft
    // `MemoryHigh` at twice the grant keeps one runaway from taking the
    // whole slice from its neighbours: above it the kernel reclaims and
    // slows the command, and never kills it.
    if cap.ram_mb > 0 {
        prop(format!("MemoryLow={}M", cap.ram_mb));
        prop(format!("MemoryHigh={}M", high_mb(cap)));
    }
    // One weight for every run, below the tabs': CPU is shared.
    prop(format!("CPUWeight={RUN_CPU_WEIGHT}"));
    // A kill when the slice runs out stops only the command: the shim lives
    // on to read the cgroup, and the scope is not torn down under it.
    prop("OOMPolicy=continue".into());
    a.push("--".into());
    a.push("sh".into());
    a.push(shim.display().to_string());
    a.extend(cmd.iter().cloned());
    a
}

/// Where a run's memory is throttled: twice its grant.
pub fn high_mb(cap: &Cap) -> u64 {
    resources::lease_bound_mb(cap.ram_mb)
}

/// What the shim read of the scope.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScopeStats {
    /// The shim ran: the scope was made.
    pub started: bool,
    pub peak_bytes: Option<u64>,
    pub oom_kills: u64,
}

pub fn parse_stats(text: &str) -> ScopeStats {
    let mut s = ScopeStats::default();
    for line in text.lines() {
        let mut w = line.split_whitespace();
        match (w.next(), w.next().and_then(|n| n.parse::<u64>().ok())) {
            (Some("started"), _) => s.started = true,
            (Some("peak"), Some(n)) => s.peak_bytes = Some(n),
            (Some("oom_kill"), Some(n)) => s.oom_kills = n,
            _ => {}
        }
    }
    s
}

/// One run, measured.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Measured {
    pub exit: i32,
    /// The signal that killed the command, when `wait4` saw one.
    pub signal: Option<i32>,
    pub peak_mb: Option<u64>,
    pub cpu_ms: Option<u64>,
    pub wall_ms: u64,
    pub oom_kills: u64,
    /// Held: by a systemd scope, or by Giverny itself ([`Measured::held_mb`]).
    pub capped: bool,
    /// Held by Giverny itself, with no cgroups here (macOS;
    /// [`crate::user_cap::RunGuard`]): the memory it was held to, MiB.
    pub held_mb: Option<u64>,
    /// `run` itself got this signal and stopped the command's scope;
    /// `exit` is then 128 + it.
    pub stopped_by: Option<i32>,
}

/// Knobs a test turns.
#[derive(Debug, Clone, Copy)]
pub struct Opts {
    pub mode: Mode,
    pub heartbeat_every: Duration,
    /// Ignore Ctrl-C and Ctrl-\ while the command runs, as a shell does, so
    /// the command's usage is still recorded (the CLI only: it is
    /// process-wide).
    pub ignore_signals: bool,
    /// A plain command is held by Giverny itself ([`crate::user_cap`]):
    /// [`user_cap::active`] for the CLI (macOS, or the Linux stand-in);
    /// said outright by a test, so the suite means the same whatever its
    /// environment.
    pub hold: bool,
    /// Where a signal `run` caught is set, and the scope —
    /// run plain, the command's tree — is stopped once it is. The CLI's is [`CAUGHT`], set by the
    /// SIGTERM/SIGHUP/SIGINT handlers it takes while the command runs; a
    /// test sets its own.
    pub stop: Option<&'static AtomicI32>,
    /// How long the scope's processes have between SIGTERM and SIGKILL.
    pub stop_grace: Duration,
}

/// The signal `run` caught while its scope ran (0: none).
pub static CAUGHT: AtomicI32 = AtomicI32::new(0);
/// Between a stopped scope's SIGTERM and its SIGKILL.
pub const STOP_GRACE: Duration = Duration::from_secs(3);

/// `giverny manage run`, from [`manage::run_in_code`].
pub fn run(
    dir: &Path,
    ledger: &Path,
    session: &str,
    task: &str,
    flags: &Flags,
    cap: Option<&resources::Capacity>,
) -> Result<(String, i32), String> {
    let opts = Opts {
        mode: detect_mode(),
        heartbeat_every: HEARTBEAT_EVERY,
        ignore_signals: true,
        hold: user_cap::active(),
        stop: Some(&CAUGHT),
        stop_grace: STOP_GRACE,
    };
    run_with(dir, ledger, session, task, flags, cap, opts)
}

fn say(task: &str, s: &str) {
    eprintln!("giverny manage run {task}: {s}");
}

/// [`run`] with its knobs.
pub fn run_with(
    dir: &Path,
    ledger: &Path,
    session: &str,
    task: &str,
    flags: &Flags,
    cap: Option<&resources::Capacity>,
    opts: Opts,
) -> Result<(String, i32), String> {
    if let Some(n) = flags.cpu_note() {
        say(task, n);
    }
    let ledger_cap = cap;
    let Some((lease, claimed)) = lease_for(dir, ledger, session, task, flags, cap)? else {
        return Ok((String::new(), resources::exit::REFUSED));
    };
    // The limits (the jobs' cores; the ceiling where Giverny holds it
    // itself) and the default lease.
    let (limit, default_mb) = match cap {
        Some(c) => (
            user_cap::Ceiling {
                ram_mb: c.limits.ram.0,
                cpu_cores: c.limits.cpu_cores,
            },
            c.default_lease.ram.0,
        ),
        None => (
            user_cap::Ceiling::now(),
            giverny_core::config::DefaultLease::load().ram.0,
        ),
    };
    let cores = limit.cpu_cores;
    let held = (|| {
        let _slots = lock_slots(dir, ledger, session, task, &lease.slots)?;
        let cap = Cap {
            ram_mb: lease.ram_mb,
            jobs: jobs_for(lease.ram_mb, default_mb, cores),
        };
        let hb = Heartbeat::start(
            ledger.to_path_buf(),
            session.to_string(),
            opts.heartbeat_every,
        );
        let m = execute(ledger, session, task, &cap, limit, &flags.command, opts);
        drop(hb);
        m.map(|m| (m, cap))
    })();
    // Whatever happened, a lease `run` claimed goes back.
    if claimed {
        let now = manage::now_ms();
        match resources::release_at(ledger, Some(dir), session, task, now, ledger_cap) {
            Ok(_) => resources::annotate_row(dir, session, task, None),
            Err(e) => say(
                task,
                &format!("could not release the lease it claimed: {e}"),
            ),
        }
    }
    let (m, cap) = held?;
    let _ = resources::heartbeat(ledger, Some(dir), session, manage::now_ms(), ledger_cap);
    let recorded = manage::edit_row(dir, session, task, |row| {
        record_usage(row, &m, &cap, &flags.command, manage::now_ms())
    });
    say(task, &summary(&m, &cap));
    let more = Mem((cap.ram_mb * 2).max(1024));
    if m.oom_kills > 0 && m.held_mb.is_some() {
        say(
            task,
            &format!(
                "killed (OOM): its command passed the memory Giverny holds it to on this \
                 machine, which has no cgroups ({}: twice its lease, within the limits). Its \
                 lease was {}. Ask for more: `giverny manage release {task}` then `giverny \
                 manage claim {task} --ram {more}` (or have the manager do it), and run it again",
                Mem(m.held_mb.unwrap_or(0)),
                Mem(cap.ram_mb)
            ),
        );
    } else if m.oom_kills > 0 {
        say(
            task,
            &format!(
                "killed (OOM): the Claude work on this machine together ran out of the memory \
                 the limits allow ({}), and this command was the one to go. Its lease was {}. \
                 Ask for more: `giverny manage release {task}` then `giverny manage claim \
                 {task} --ram {more}` (or have the manager do it), and run it again",
                crate::tab_cap::SLICE,
                Mem(cap.ram_mb)
            ),
        );
    } else if m.held_mb.is_some() && m.signal == Some(9) {
        say(
            task,
            "killed by SIGKILL: perhaps the Giverny app's memory ceiling (the Claude work on \
             this machine together passed the limits), perhaps someone else",
        );
    } else if m.capped && m.signal == Some(9) && cap.ram_mb > 0 {
        say(
            task,
            "killed by SIGKILL: perhaps the slice's memory ceiling, perhaps someone else",
        );
    }
    if !recorded {
        say(
            task,
            "no row for it in this manager session, so the usage is on no row and will not reach the history",
        );
    }
    Ok((String::new(), m.exit))
}

/// `exit 0; peak 1.2G of 3G, 45.1s CPU in 30.2s`.
pub fn summary(m: &Measured, cap: &Cap) -> String {
    let secs = |ms: u64| format!("{:.1}s", ms as f64 / 1000.0);
    let mut s = match (m.stopped_by, m.signal) {
        (Some(sig), _) => format!("exit {} (stopped: run got {})", m.exit, signal_name(sig)),
        (None, Some(sig)) => format!("exit {} (signal {sig})", m.exit),
        (None, None) => format!("exit {}", m.exit),
    };
    let peak = m
        .peak_mb
        .map(|p| Mem(p).to_string())
        .unwrap_or_else(|| "?".into());
    if m.capped && cap.ram_mb > 0 {
        s.push_str(&format!("; peak {peak} of {}", Mem(cap.ram_mb)));
    } else {
        s.push_str(&format!("; peak {peak}"));
    }
    if let Some(c) = m.cpu_ms {
        s.push_str(&format!(", {} CPU", secs(c)));
    }
    s.push_str(&format!(" in {}", secs(m.wall_ms)));
    if !m.capped {
        s.push_str(" (uncapped)");
    }
    s
}

/// The task's lease, or one claimed now (true: `run` claimed it). `None`:
/// refused, already said.
fn lease_for(
    dir: &Path,
    ledger: &Path,
    session: &str,
    task: &str,
    flags: &Flags,
    cap: Option<&resources::Capacity>,
) -> Result<Option<(resources::Lease, bool)>, String> {
    let now = manage::now_ms();
    if let Some(l) = resources::with_ledger(ledger, now, |l| l.lease(session, task).cloned())? {
        return Ok(Some((l, false)));
    }
    let default = match cap {
        Some(c) => c.default_lease,
        None => giverny_core::config::DefaultLease::load(),
    };
    let repo = flags.repo.clone().or_else(|| {
        let cwd = std::env::current_dir().unwrap_or_default();
        manage_history::repo_of(task, &cwd)
    });
    let peaks = manage::Peaks::load(dir);
    let req = resources::Request {
        ram_mb: flags.ram_mb.unwrap_or(default.ram.0),
        peak_mb: peaks.expected(session, task, repo.as_deref()),
        ..flags.request()
    };
    let mut waiting = false;
    let mut said = false;
    loop {
        let capacity = match cap {
            Some(c) => c.clone(),
            None => resources::Capacity::detect(ledger)?,
        };
        let now = manage::now_ms();
        let out = resources::with_ledger(ledger, now, |l| {
            peaks.fill(l);
            let left = |s: &str, t: &str| resources::eta_or_estimate_s(dir, s, t, now);
            l.claim_timed(&capacity, session, task, repo.as_deref(), &req, now, &left)
        })?;
        resources::annotate_row(dir, session, task, resources::row_lease(&out, &req));
        let line =
            resources::outcome_line(task, &out, &|s, t| resources::eta_left_s(dir, s, t, now));
        let got = match out {
            resources::Outcome::Granted(l)
            | resources::Outcome::GrantedSmaller { lease: l, .. }
            | resources::Outcome::Backfilled { lease: l, .. } => {
                say(task, &format!("held no lease, so claimed one: {line}"));
                Some((l, true))
            }
            resources::Outcome::Held(l) => Some((l, false)),
            resources::Outcome::Refused(why) => {
                say(task, &format!("held no lease and cannot claim one: {why}"));
                if waiting {
                    manage::mark_waiting(dir, session, task, None, now);
                }
                return Ok(None);
            }
            resources::Outcome::Queued { .. } => {
                if !said {
                    say(
                        task,
                        &format!("held no lease, claimed one: {line}; waiting"),
                    );
                    said = true;
                }
                if !waiting {
                    waiting = manage::mark_waiting(
                        dir,
                        session,
                        task,
                        Some("giverny manage run: queued for a lease"),
                        now,
                    );
                }
                None
            }
        };
        if let Some(got) = got {
            if waiting {
                manage::mark_waiting(dir, session, task, None, manage::now_ms());
            }
            return Ok(Some(got));
        }
        std::thread::sleep(QUEUED_POLL);
    }
}

/// `cargo:/x/target` → `cargo_3a_2fx_2ftarget.lock`: every character a
/// file name may not safely hold, escaped, so two slots never share a file.
pub fn slot_file(slot: &str) -> String {
    let mut s = String::new();
    for c in slot.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '.') {
            s.push(c);
        } else {
            let mut b = [0u8; 4];
            for byte in c.encode_utf8(&mut b).bytes() {
                s.push_str(&format!("_{byte:02x}"));
            }
        }
    }
    format!("{s}.lock")
}

/// An exclusive hold on one slot's lock file, let go when dropped (or when
/// the process dies: `flock`).
pub struct SlotLock {
    #[cfg(unix)]
    _file: std::fs::File,
}

/// Take `slot`'s lock under `ledger`'s directory: `Ok(Some)` at once,
/// `Ok(None)` when another holds it and `wait` is false.
pub fn lock_slot(ledger: &Path, slot: &str, wait: bool) -> Result<Option<SlotLock>, String> {
    let dir = ledger.parent().unwrap_or(Path::new(".")).join("slots");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(slot_file(slot));
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let op = if wait {
            libc::LOCK_EX
        } else {
            libc::LOCK_EX | libc::LOCK_NB
        };
        loop {
            // SAFETY: a valid open descriptor; flock only blocks.
            if unsafe { libc::flock(file.as_raw_fd(), op) } == 0 {
                return Ok(Some(SlotLock { _file: file }));
            }
            let e = std::io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::EWOULDBLOCK) if !wait => return Ok(None),
                _ => return Err(format!("{}: {e}", path.display())),
            }
        }
    }
    #[cfg(not(unix))]
    {
        // No flock: the ledger's own exclusivity is all there is.
        let _ = (path, wait);
        Ok(Some(SlotLock {}))
    }
}

/// Every slot of the lease, in name order (so two runs never deadlock),
/// waiting for each that another `run` holds — the row waiting meanwhile.
fn lock_slots(
    dir: &Path,
    ledger: &Path,
    session: &str,
    task: &str,
    slots: &[String],
) -> Result<Vec<SlotLock>, String> {
    let mut names: Vec<&String> = slots.iter().collect();
    names.sort();
    names.dedup();
    let mut held = Vec::new();
    let mut waiting = false;
    for s in names {
        let lock = match lock_slot(ledger, s, false)? {
            Some(l) => l,
            None => {
                say(
                    task,
                    &format!("slot {s} is held by another `giverny manage run`; waiting for it"),
                );
                if !waiting {
                    waiting = manage::mark_waiting(
                        dir,
                        session,
                        task,
                        Some(&format!("giverny manage run: waiting for slot {s}")),
                        manage::now_ms(),
                    );
                }
                lock_slot(ledger, s, true)?.ok_or("slot lock not taken")?
            }
        };
        held.push(lock);
    }
    if waiting {
        manage::mark_waiting(dir, session, task, None, manage::now_ms());
    }
    Ok(held)
}

/// Beats `session`'s leases every `every` until dropped.
struct Heartbeat {
    stop: Option<std::sync::mpsc::Sender<()>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Heartbeat {
    fn start(ledger: PathBuf, session: String, every: Duration) -> Heartbeat {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let handle = std::thread::spawn(move || {
            while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(every) {
                if let Err(e) =
                    resources::heartbeat(&ledger, None, &session, manage::now_ms(), None)
                {
                    eprintln!("giverny manage run: heartbeat: {e}");
                }
            }
        });
        Heartbeat {
            stop: Some(tx),
            handle: Some(handle),
        }
    }
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Write the shim beside the ledger (only when its bytes differ).
fn shim_path(ledger: &Path) -> Result<PathBuf, String> {
    let dir = ledger.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join("run-shim.sh");
    if std::fs::read(&path).is_ok_and(|b| b == SHIM.as_bytes()) {
        return Ok(path);
    }
    let tmp = dir.join(format!("run-shim.sh.{}.tmp", std::process::id()));
    std::fs::write(&tmp, SHIM)
        .and_then(|_| std::fs::rename(&tmp, &path))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

fn stats_path(ledger: &Path) -> Result<PathBuf, String> {
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = run_live::runs_dir(ledger);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir.join(format!(
        "{}-{}.stats",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )))
}

/// Run `cmd` under `cap` and measure it. A scope systemd would not make
/// falls back to running plain, held by Giverny itself where it holds the
/// limits (macOS: [`crate::user_cap`]) under `limit`.
fn execute(
    ledger: &Path,
    session: &str,
    task: &str,
    cap: &Cap,
    limit: user_cap::Ceiling,
    cmd: &[String],
    opts: Opts,
) -> Result<Measured, String> {
    if let Mode::Systemd { expand_flag } = opts.mode {
        let shim = shim_path(ledger)?;
        let stats = stats_path(ledger)?;
        let unit = unit_name(task, std::process::id());
        if let Err(err) = crate::tab_cap::cap_slice_now() {
            say(
                task,
                &format!("{} not capped: {err}", crate::tab_cap::SLICE),
            );
        }
        let mut c = Command::new("systemd-run");
        c.args(scope_args(expand_flag, &unit, task, cap, &shim, cmd))
            .env(STATS_ENV, &stats)
            // The shim's watcher stops the scope if `run` dies first.
            .env(UNIT_ENV, &unit)
            .env(PID_ENV, std::process::id().to_string());
        jobs_env(&mut c, cap);
        say(
            task,
            &format!(
                "leased {} in {unit}.scope, in {} (`systemctl --user status {unit}.scope`)",
                cap.describe(),
                crate::tab_cap::SLICE
            ),
        );
        // Seen by the management panel while it runs; the ledger counts its
        // lease at least at its `MemoryHigh` until it ends.
        let bound = (cap.ram_mb > 0).then(|| high_mb(cap));
        let live = run_live::register_bound(&stats, task, session, manage::now_ms(), bound, cmd);
        // A signal to `run` stops its scope, as Ctrl-C would
        // stop the command, and `run` still releases and records. Taken
        // before the spawn: a caught signal is the default again after exec.
        let mut stop = opts.stop.map(|caught| {
            caught.store(0, Ordering::Relaxed);
            ScopeStop::new(caught, &unit, opts.stop_grace)
        });
        #[cfg(unix)]
        let catching = (opts.ignore_signals && stop.is_some()).then(CatchSignals::new);
        let w = spawn_wait(
            &mut c,
            "systemd-run",
            opts.ignore_signals,
            stop.as_mut(),
            |_| (),
        )
        .map(|w| w.0);
        #[cfg(unix)]
        drop(catching);
        drop(live);
        let stopped_by = stop.as_ref().and_then(ScopeStop::signal);
        if let Some(sig) = stopped_by {
            say(
                task,
                &format!(
                    "got {}: stopped its own scope {unit}.scope (SIGTERM, SIGKILL after {}s)",
                    signal_name(sig),
                    opts.stop_grace.as_secs()
                ),
            );
        }
        let st = std::fs::read_to_string(&stats)
            .map(|t| parse_stats(&t))
            .unwrap_or_default();
        let _ = std::fs::remove_file(&stats);
        let w = w?;
        if st.started {
            record_caps(ledger, true);
        }
        // Stopped, it never falls back to a plain run.
        if st.started || stopped_by.is_some() {
            let cg_mb = st.peak_bytes.map(|b| b.div_ceil(1024 * 1024));
            return Ok(Measured {
                // Stopped: `run` exits as the signal it got would have.
                exit: stopped_by.map_or(w.exit, |s| 128 + s),
                signal: w.signal,
                stopped_by,
                // What the cap measures; RSS only where the kernel has no
                // `memory.peak`.
                peak_mb: cg_mb.or(w.maxrss_mb),
                cpu_ms: w.cpu_ms,
                wall_ms: w.wall_ms,
                oom_kills: st.oom_kills,
                capped: st.started,
                held_mb: None,
            });
        }
        say(
            task,
            &format!(
                "systemd-run did not make the scope (exit {}); running it plain, uncapped",
                w.exit
            ),
        );
    }
    // No cgroups (macOS): Giverny holds the command itself.
    let held = (opts.mode == Mode::Plain && opts.hold)
        .then(|| user_cap::Ceiling::for_run(cap.ram_mb, limit));
    // The app, when one holds this work, holds its CPU with every other
    // claude's: two duty cycles on one process would undo each other.
    let hold_cpu = held.is_some() && !user_cap::app_holds();
    match held {
        Some(ceiling) => say(
            task,
            &format!(
                "no cgroups here (macOS, or ${NO_SYSTEMD_ENV} set): leased {}, held by Giverny \
                 itself: killed above {} (twice its lease, within the limits), its CPU held to \
                 {} {}",
                cap.describe(),
                Mem(ceiling.ram_mb),
                user_cap::cores_text(ceiling.cpu_cores),
                if hold_cpu {
                    "by this run"
                } else {
                    "by the Giverny app with the tabs'"
                }
            ),
        ),
        None if opts.mode == Mode::Plain => say(
            task,
            &format!(
                "not capped (no user systemd here, or ${NO_SYSTEMD_ENV} set): \
                 running it plain, the lease ({}) advisory",
                cap.describe()
            ),
        ),
        None => {}
    }
    // No scope weighs it: on macOS, low priority instead, so the person's
    // work goes first. The `.live` file names the command
    // as given.
    let given = cmd;
    let cmd = low_priority(cmd);
    let mut c = Command::new(&cmd[0]);
    c.args(&cmd[1..]);
    jobs_env(&mut c, cap);
    // Seen by the app (which holds it with the tabs where there are no
    // cgroups) and by the ledger's CPU-busy check, as a scope is.
    record_caps(ledger, held.is_some());
    let live = stats_path(ledger).ok().and_then(|stats| {
        run_live::register_bound(
            &stats,
            task,
            session,
            manage::now_ms(),
            held.map(|c| c.ram_mb),
            given,
        )
    });
    // A signal to `run` stops the command's whole tree, as
    // under a scope; taken before the spawn, as there.
    let mut stop = opts.stop.map(|caught| {
        caught.store(0, Ordering::Relaxed);
        TreeStop::new(caught, opts.stop_grace)
    });
    #[cfg(unix)]
    let catching = (opts.ignore_signals && stop.is_some()).then(CatchSignals::new);
    let t = task.to_string();
    let waited = spawn_wait(&mut c, &cmd[0], opts.ignore_signals, stop.as_mut(), |pid| {
        held.map(|ceiling| user_cap::RunGuard::start(pid, ceiling, hold_cpu, move |s| say(&t, s)))
    });
    #[cfg(unix)]
    drop(catching);
    let (w, guard) = waited?;
    drop(live);
    let seen = guard.map(user_cap::RunGuard::finish).unwrap_or_default();
    let stopped_by = stop.as_ref().and_then(TreeStop::signal);
    if let Some(sig) = stopped_by {
        say(
            task,
            &format!(
                "got {}: stopped its command and everything under it (SIGTERM, SIGKILL after {}s)",
                signal_name(sig),
                opts.stop_grace.as_secs()
            ),
        );
    }
    Ok(Measured {
        exit: stopped_by.map_or(w.exit, |s| 128 + s),
        signal: w.signal,
        // The whole tree's, where the guard watched it.
        peak_mb: match (seen.peak_mb, w.maxrss_mb) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        },
        cpu_ms: w.cpu_ms,
        wall_ms: w.wall_ms,
        oom_kills: seen.kills,
        capped: held.is_some(),
        held_mb: held.map(|c| c.ram_mb),
        stopped_by,
    })
}

/// `CARGO_BUILD_JOBS=<jobs>` (from the lease's RAM) unless it is set
/// already.
fn jobs_env(c: &mut Command, cap: &Cap) {
    if std::env::var_os("CARGO_BUILD_JOBS").is_none() {
        c.env("CARGO_BUILD_JOBS", cap.jobs.max(1).to_string());
    }
}

struct Waited {
    exit: i32,
    signal: Option<i32>,
    cpu_ms: Option<u64>,
    maxrss_mb: Option<u64>,
    wall_ms: u64,
}

/// Spawn and reap `c`, with its rusage on Unix; `watch` is handed the
/// child's pid as it starts, and what it returns is held until the child
/// is reaped.
/// With `stop`, the wait polls it, and once it has stopped the scope (or
/// the command's tree) it sees that empty (or killed) before returning.
fn spawn_wait<G, S: Stop>(
    c: &mut Command,
    what: &str,
    ignore_signals: bool,
    stop: Option<&mut S>,
    watch: impl FnOnce(u32) -> G,
) -> Result<(Waited, G), String> {
    let t0 = Instant::now();
    let mut child = c.spawn().map_err(|e| format!("{what}: {e}"))?;
    let watching = watch(child.id());
    #[cfg(unix)]
    {
        let mut stop = stop;
        let _ = &mut child;
        // SIGINT is the scope stopper's when there is one; SIGQUIT is left
        // to the command either way.
        let _quiet = ignore_signals.then(|| IgnoreSignals::new(stop.is_none()));
        let pid = child.id() as libc::pid_t;
        let mut status: libc::c_int = 0;
        // SAFETY: zeroed rusage is a valid value; wait4 fills it.
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        let flags = if stop.is_some() { libc::WNOHANG } else { 0 };
        loop {
            // SAFETY: our own child's pid, valid out-pointers.
            let r = unsafe { libc::wait4(pid, &mut status, flags, &mut ru) };
            if r == pid {
                break;
            }
            if r == 0 {
                if let Some(s) = stop.as_deref_mut() {
                    s.tick(child.id());
                }
                std::thread::sleep(STOP_POLL);
                continue;
            }
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EINTR) {
                return Err(format!("{what}: wait: {e}"));
            }
        }
        if let Some(s) = stop {
            s.finish();
        }
        let wall_ms = t0.elapsed().as_millis() as u64;
        let (exit, signal) = if libc::WIFEXITED(status) {
            (libc::WEXITSTATUS(status), None)
        } else if libc::WIFSIGNALED(status) {
            let s = libc::WTERMSIG(status);
            (128 + s, Some(s))
        } else {
            (1, None)
        };
        let tv = |t: libc::timeval| t.tv_sec as u64 * 1000 + t.tv_usec as u64 / 1000;
        let cpu_ms = tv(ru.ru_utime) + tv(ru.ru_stime);
        // Linux counts KiB, macOS bytes.
        let rss = ru.ru_maxrss.max(0) as u64;
        let maxrss_mb = if cfg!(target_os = "macos") {
            rss.div_ceil(1024 * 1024)
        } else {
            rss.div_ceil(1024)
        };
        Ok((
            Waited {
                exit,
                signal,
                cpu_ms: Some(cpu_ms),
                maxrss_mb: (maxrss_mb > 0).then_some(maxrss_mb),
                wall_ms,
            },
            watching,
        ))
    }
    #[cfg(not(unix))]
    {
        let _ = (ignore_signals, stop);
        let st = child.wait().map_err(|e| format!("{what}: wait: {e}"))?;
        Ok((
            Waited {
                exit: st.code().unwrap_or(1),
                signal: None,
                cpu_ms: None,
                maxrss_mb: None,
                wall_ms: t0.elapsed().as_millis() as u64,
            },
            watching,
        ))
    }
}

/// SIGQUIT, and SIGINT when `int`, ignored in this process until dropped.
/// Taken after the spawn, so the command keeps its own defaults.
#[cfg(unix)]
struct IgnoreSignals {
    int: Option<libc::sighandler_t>,
    quit: libc::sighandler_t,
}

#[cfg(unix)]
impl IgnoreSignals {
    fn new(int: bool) -> IgnoreSignals {
        // SAFETY: setting a disposition to SIG_IGN is always sound.
        unsafe {
            IgnoreSignals {
                int: int.then(|| libc::signal(libc::SIGINT, libc::SIG_IGN)),
                quit: libc::signal(libc::SIGQUIT, libc::SIG_IGN),
            }
        }
    }
}

#[cfg(unix)]
impl Drop for IgnoreSignals {
    fn drop(&mut self) {
        // SAFETY: restoring the dispositions `new` replaced.
        unsafe {
            if let Some(int) = self.int {
                libc::signal(libc::SIGINT, int);
            }
            libc::signal(libc::SIGQUIT, self.quit);
        }
    }
}

/// SIGTERM, SIGHUP and SIGINT caught into [`CAUGHT`] until dropped,
/// so a `run` told to go stops its scope first. One this
/// process ignored (`nohup`) stays ignored.
#[cfg(unix)]
struct CatchSignals {
    old: Vec<(libc::c_int, libc::sighandler_t)>,
}

#[cfg(unix)]
extern "C" fn on_signal(sig: libc::c_int) {
    // Async-signal-safe: one atomic store.
    CAUGHT.store(sig, Ordering::Relaxed);
}

#[cfg(unix)]
impl CatchSignals {
    fn new() -> CatchSignals {
        let handler = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        let mut old = Vec::new();
        for sig in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
            // SAFETY: a handler that only stores an atomic.
            let prev = unsafe { libc::signal(sig, handler) };
            if prev == libc::SIG_IGN {
                // SAFETY: putting back the ignore it replaced.
                unsafe { libc::signal(sig, libc::SIG_IGN) };
            } else if prev != libc::SIG_ERR {
                old.push((sig, prev));
            }
        }
        CatchSignals { old }
    }
}

#[cfg(unix)]
impl Drop for CatchSignals {
    fn drop(&mut self) {
        for &(sig, prev) in &self.old {
            // SAFETY: restoring the disposition `new` replaced.
            unsafe { libc::signal(sig, prev) };
        }
    }
}

/// How often a wait with a [`ScopeStop`] looks again.
const STOP_POLL: Duration = Duration::from_millis(100);
/// How long a stopped scope is still watched after its SIGKILL.
const STOP_AFTER_KILL: Duration = Duration::from_secs(2);

#[cfg(unix)]
const SIGTERM: i32 = libc::SIGTERM;
#[cfg(unix)]
const SIGKILL: i32 = libc::SIGKILL;
#[cfg(not(unix))]
const SIGTERM: i32 = 15;
#[cfg(not(unix))]
const SIGKILL: i32 = 9;

/// `SIGTERM`, `SIGHUP`, `SIGINT`, else `signal <n>`.
fn signal_name(sig: i32) -> String {
    match sig {
        15 => "SIGTERM".into(),
        1 => "SIGHUP".into(),
        2 => "SIGINT".into(),
        n => format!("signal {n}"),
    }
}

/// Stops the run's own scope once `run` has caught a signal:
/// SIGTERM to every process in it, and after the grace `cgroup.kill` (or,
/// where that cannot be written, SIGKILL to each process still listed).
/// The scope is found from the child's own cgroup, and only when that is
/// the unit `run` named; else only the child itself (systemd-run, not yet
/// in its scope) is signalled. Doubt falls on the side of killing nothing.
pub struct ScopeStop {
    caught: &'static AtomicI32,
    unit: String,
    grace: Duration,
    /// Where `/proc` and the cgroup tree are (a test's fakes).
    proc_root: PathBuf,
    cgroup_root: PathBuf,
    /// The scope's directory, learnt from the child while it lives (so a
    /// child that dies with the signal still leaves it known).
    scope: Option<PathBuf>,
    stopping: Option<Stopping>,
}

/// A stop under way.
struct Stopping {
    signal: i32,
    at: Instant,
    /// The child, signalled itself when the scope is not known.
    child: Option<u32>,
    killed: bool,
}

impl ScopeStop {
    pub fn new(caught: &'static AtomicI32, unit: &str, grace: Duration) -> ScopeStop {
        ScopeStop::with_roots(caught, unit, grace, "/proc", CGROUP_ROOT)
    }

    pub fn with_roots(
        caught: &'static AtomicI32,
        unit: &str,
        grace: Duration,
        proc_root: impl Into<PathBuf>,
        cgroup_root: impl Into<PathBuf>,
    ) -> ScopeStop {
        ScopeStop {
            caught,
            unit: unit.to_string(),
            grace,
            proc_root: proc_root.into(),
            cgroup_root: cgroup_root.into(),
            scope: None,
            stopping: None,
        }
    }

    /// The signal it stopped the scope for.
    pub fn signal(&self) -> Option<i32> {
        self.stopping.as_ref().map(|s| s.signal)
    }

    /// While the child (not yet reaped, so its pid is its own) runs: learns
    /// the scope, starts the stop once a signal is in, kills past the grace.
    pub fn tick(&mut self, child: u32) {
        if self.scope.is_none() {
            self.scope =
                std::fs::read_to_string(self.proc_root.join(child.to_string()).join("cgroup"))
                    .ok()
                    .and_then(|text| scope_dir(&text, &self.unit, &self.cgroup_root));
        }
        self.begin(Some(child));
        self.escalate();
    }

    /// A caught signal starts the stop: SIGTERM to the scope's processes,
    /// else to the child alone.
    fn begin(&mut self, child: Option<u32>) {
        if self.stopping.is_some() {
            return;
        }
        let sig = self.caught.load(Ordering::Relaxed);
        if sig == 0 {
            return;
        }
        match (&self.scope, child) {
            (Some(dir), _) => signal_procs(dir, SIGTERM),
            (None, Some(pid)) => send(pid, SIGTERM),
            (None, None) => {}
        }
        self.stopping = Some(Stopping {
            signal: sig,
            at: Instant::now(),
            child,
            killed: false,
        });
    }

    /// Past the grace: SIGKILL, once.
    fn escalate(&mut self) {
        let Some(s) = self.stopping.as_mut() else {
            return;
        };
        if s.killed || s.at.elapsed() < self.grace {
            return;
        }
        s.killed = true;
        match (&self.scope, s.child) {
            (Some(dir), _) => {
                if std::fs::write(dir.join("cgroup.kill"), "1").is_err() {
                    signal_procs(dir, SIGKILL);
                }
            }
            (None, Some(pid)) => send(pid, SIGKILL),
            (None, None) => {}
        }
    }

    /// After the child is reaped (its pid no longer ours): a signal caught
    /// meanwhile still stops the scope, which is watched until it is empty,
    /// killed past the grace, and let go a little after that.
    pub fn finish(&mut self) {
        if let Some(s) = self.stopping.as_mut() {
            s.child = None;
        }
        self.begin(None);
        let Some(dir) = self.scope.clone() else {
            return;
        };
        while let Some(s) = self.stopping.as_ref() {
            if live_procs(&dir).is_empty() || s.at.elapsed() >= self.grace + STOP_AFTER_KILL {
                return;
            }
            self.escalate();
            std::thread::sleep(STOP_POLL);
        }
    }
}

/// What a `run` told to go stops, polled by [`spawn_wait`]: its scope
/// ([`ScopeStop`]) or, run plain, its command's process tree
/// ([`TreeStop`]).
pub trait Stop {
    /// While the child (not yet reaped) runs.
    fn tick(&mut self, child: u32);
    /// After the child is reaped.
    fn finish(&mut self);
}

impl Stop for ScopeStop {
    fn tick(&mut self, child: u32) {
        ScopeStop::tick(self, child)
    }
    fn finish(&mut self) {
        ScopeStop::finish(self)
    }
}

/// How often a [`TreeStop`] looks at the command's tree before it is told
/// to go.
const TREE_EVERY: Duration = Duration::from_secs(1);

/// Stops a plain run's command once `run` has caught a signal,
/// as [`ScopeStop`] stops a scope: SIGTERM to every process
/// of the command's tree, and after the grace SIGKILL to each still there.
/// With no cgroup to list them, the tree is the child and everything below
/// it, looked at every [`TREE_EVERY`] and remembered by pid and start time
/// ([`user_cap::Members`]), so a process that detached from it (`nohup …
/// &`, a double fork) is still stopped, and a pid reused by another
/// program is never signalled.
pub struct TreeStop {
    caught: &'static AtomicI32,
    grace: Duration,
    members: user_cap::Members,
    /// The tree at the last look.
    tree: std::collections::HashSet<u32>,
    looked: Option<Instant>,
    stopping: Option<Stopping>,
}

impl TreeStop {
    pub fn new(caught: &'static AtomicI32, grace: Duration) -> TreeStop {
        TreeStop {
            caught,
            grace,
            members: user_cap::Members::default(),
            tree: Default::default(),
            looked: None,
            stopping: None,
        }
    }

    /// The signal it stopped the command for.
    pub fn signal(&self) -> Option<i32> {
        self.stopping.as_ref().map(|s| s.signal)
    }

    /// The tree now: below `child` while it is ours, and every earlier
    /// member still alive with all below it.
    fn look(&mut self, child: Option<u32>) {
        self.looked = Some(Instant::now());
        let Some(snap) = user_cap::Snapshot::take() else {
            return;
        };
        let now = child
            .map(|c| crate::session_use::subtree(&snap.procs, c))
            .unwrap_or_default();
        self.tree = self.members.hold(&snap.procs, now);
        self.tree.remove(&std::process::id());
    }

    fn signal_tree(&self, sig: i32) {
        for &pid in &self.tree {
            send(pid, sig);
        }
    }

    /// A caught signal starts the stop: SIGTERM to the whole tree.
    fn begin(&mut self, child: Option<u32>) {
        if self.stopping.is_some() {
            return;
        }
        let sig = self.caught.load(Ordering::Relaxed);
        if sig == 0 {
            return;
        }
        self.look(child);
        self.signal_tree(SIGTERM);
        self.stopping = Some(Stopping {
            signal: sig,
            at: Instant::now(),
            child,
            killed: false,
        });
    }

    /// Past the grace: SIGKILL to what is left, once.
    fn escalate(&mut self) {
        let Some(s) = self.stopping.as_ref() else {
            return;
        };
        if s.killed || s.at.elapsed() < self.grace {
            return;
        }
        let child = s.child;
        self.look(child);
        self.signal_tree(SIGKILL);
        if let Some(s) = self.stopping.as_mut() {
            s.killed = true;
        }
    }
}

impl Stop for TreeStop {
    fn tick(&mut self, child: u32) {
        if self.stopping.is_none() && self.looked.is_none_or(|t| t.elapsed() >= TREE_EVERY) {
            self.look(Some(child));
        }
        self.begin(Some(child));
        self.escalate();
    }

    /// After the child is reaped: a signal caught meanwhile still stops
    /// the tree, which is watched until it is gone, killed past the grace,
    /// and let go a little after that. A command that ended on its own
    /// leaves what it meant to running.
    fn finish(&mut self) {
        if let Some(s) = self.stopping.as_mut() {
            s.child = None;
        }
        self.begin(None);
        while let Some(s) = self.stopping.as_ref() {
            if s.at.elapsed() >= self.grace + STOP_AFTER_KILL {
                return;
            }
            self.look(None);
            if self.tree.is_empty() {
                return;
            }
            self.escalate();
            std::thread::sleep(STOP_POLL);
        }
    }
}

/// The cgroup v2 tree.
const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// The directory of the scope `cgroup_text` (a `/proc/<pid>/cgroup`) puts
/// its process in, under `cgroup_root`, when that scope is `<unit>.scope`;
/// else None.
pub fn scope_dir(cgroup_text: &str, unit: &str, cgroup_root: &Path) -> Option<PathBuf> {
    let path = cgroup_text
        .lines()
        .find_map(|l| l.strip_prefix("0::"))?
        .trim();
    let name = path.rsplit('/').next()?;
    if unit.is_empty() || name != format!("{unit}.scope") || path.split('/').any(|p| p == "..") {
        return None;
    }
    Some(cgroup_root.join(path.trim_start_matches('/')))
}

/// The pids `dir`'s `cgroup.procs` lists that are still there.
fn live_procs(dir: &Path) -> Vec<u32> {
    std::fs::read_to_string(dir.join("cgroup.procs"))
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|p| p.parse::<u32>().ok())
        .filter(|&p| p > 1 && alive(p))
        .collect()
}

fn signal_procs(dir: &Path, sig: i32) {
    for pid in live_procs(dir) {
        send(pid, sig);
    }
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(unix)]
fn send(pid: u32, sig: i32) {
    if pid > 1 {
        // SAFETY: a plain kill(2) of one positive pid.
        unsafe { libc::kill(pid as libc::pid_t, sig) };
    }
}

#[cfg(not(unix))]
fn alive(_: u32) -> bool {
    false
}

#[cfg(not(unix))]
fn send(_: u32, _: i32) {}

/// Add one run to the row's `usage` (see `docs/management-panel.md`, **Row**).
pub fn record_usage(
    row: &mut Map<String, Value>,
    m: &Measured,
    cap: &Cap,
    cmd: &[String],
    now: u64,
) {
    let mut u = row
        .get("usage")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let get = |u: &Map<String, Value>, k: &str| manage::u64_of(u, k).unwrap_or(0);
    let runs = get(&u, "runs") + 1;
    u.insert("runs".into(), json!(runs));
    match m.peak_mb {
        Some(p) => {
            let peak = p.max(get(&u, "peak_mb"));
            u.insert("peak_mb".into(), json!(peak));
            u.insert("last_peak_mb".into(), json!(p));
        }
        None => {
            u.remove("last_peak_mb");
        }
    }
    let cpu_s = get(&u, "cpu_s") + m.cpu_ms.unwrap_or(0).div_ceil(1000);
    u.insert("cpu_s".into(), json!(cpu_s));
    let wall_s = get(&u, "wall_s") + m.wall_ms.div_ceil(1000);
    u.insert("wall_s".into(), json!(wall_s));
    u.insert(
        "oom_kills".into(),
        json!(get(&u, "oom_kills") + m.oom_kills),
    );
    // The mark follows the latest run alone: a clean run closes it.
    u.insert("oom_open".into(), json!(m.oom_kills > 0));
    let mut line = cmd.join(" ");
    if line.chars().count() > 200 {
        line = line.chars().take(199).collect::<String>() + "…";
    }
    u.insert("last_cmd".into(), json!(line));
    u.insert("last_exit".into(), json!(m.exit));
    u.insert("capped".into(), json!(m.capped));
    // A lease holds no cores: an old `cap_cpu` goes.
    u.remove("cap_cpu");
    if m.capped && cap.ram_mb > 0 {
        u.insert("cap_ram_mb".into(), json!(cap.ram_mb));
    } else {
        u.remove("cap_ram_mb");
    }
    u.insert("last_at".into(), json!(manage::stamp(now)));
    row.insert("usage".into(), Value::Object(u));
}

/// A fresh grant for the row's task: an open OOM closes once
/// the task is granted, after that run, more RAM than the run was held to,
/// which is the release-and-claim-bigger the OOM line asks for. A lease
/// held from before the run, or one no bigger, leaves it open. `oom_kills`
/// stays: the history counts every kill.
pub fn close_oom_on_grant(row: &mut Map<String, Value>, lease: &Value) {
    let Some(lease) = lease.as_object() else {
        return;
    };
    if !matches!(
        lease.get("state").and_then(Value::as_str),
        Some("granted" | "smaller")
    ) {
        return;
    }
    let Some(usage) = row.get_mut("usage") else {
        return;
    };
    if !crate::feed::oom_open(usage) {
        return;
    }
    let Some(u) = usage.as_object_mut() else {
        return;
    };
    let after = match (
        manage::ms_of(lease, "granted_at"),
        manage::ms_of(u, "last_at"),
    ) {
        (Some(granted), Some(ran)) => granted >= ran,
        // Nothing says when the run was: a grant now is after it.
        (Some(_), None) => true,
        _ => false,
    };
    let ram = manage::u64_of(lease, "ram_mb").unwrap_or(0);
    let bigger = manage::u64_of(u, "cap_ram_mb").is_none_or(|cap| ram > cap);
    if after && bigger {
        u.insert("oom_open".into(), json!(false));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::manage::{Cmd, parse_args};

    #[test]
    fn the_command_is_everything_after_the_double_dash() {
        let a: Vec<String> = "run t --cpu 2 -- cargo test --release -p x -- --nocapture"
            .split_whitespace()
            .map(String::from)
            .collect();
        let (c, f) = parse_args(&a).unwrap();
        assert_eq!(c, Cmd::Run("t".into()));
        assert!(f.cpu_ignored, "an old --cpu is taken, and ignored");
        assert_eq!(
            f.command,
            ["cargo", "test", "--release", "-p", "x", "--", "--nocapture"]
        );
        let none: Vec<String> = ["run", "t"].map(String::from).into();
        assert!(parse_args(&none).unwrap_err().contains("needs a command"));
        let no_task: Vec<String> = ["run", "--", "ls"].map(String::from).into();
        assert!(parse_args(&no_task).is_err());
    }

    #[test]
    fn the_scope_carries_the_cap() {
        let cap = Cap {
            ram_mb: 3072,
            jobs: 3,
        };
        let cmd: Vec<String> = ["cargo", "test", "$HOME"].map(String::from).into();
        let a = scope_args(
            true,
            &unit_name("demo#161", 42),
            "demo#161",
            &cap,
            Path::new("/l/run-shim.sh"),
            &cmd,
        );
        let s = a.join(" ");
        assert!(s.starts_with("--user --scope --quiet --collect --expand-environment=no"));
        assert!(s.contains("--unit=giverny-run-demo_161-42"), "{s}");
        assert!(s.contains("--slice=giverny-claude.slice"), "{s}");
        assert!(
            s.contains("-p MemoryLow=3072M -p MemoryHigh=6144M -p CPUWeight=100"),
            "the same weight for every run: {s}"
        );
        assert!(s.contains("-p OOMPolicy=continue"), "{s}");
        assert!(s.ends_with("-- sh /l/run-shim.sh cargo test $HOME"), "{s}");
        // A lease of no RAM sets no memory cap; old systemd gets no flag.
        let a = scope_args(
            false,
            "u",
            "t",
            &Cap { ram_mb: 0, jobs: 1 },
            Path::new("/s"),
            &cmd,
        )
        .join(" ");
        assert!(!a.contains("Memory") && !a.contains("expand"), "{a}");
        assert!(a.contains("-p CPUWeight=100"), "{a}");
        // The slice is the one hard ceiling.
        assert!(!s.contains("MemoryMax") && !s.contains("CPUQuota"), "{s}");
        assert_eq!(slot_file("cargo:/x/t"), "cargo_3a_2fx_2ft.lock");
        assert_eq!(
            parse_stats("started\npeak 3221225472\noom_kill 2\n"),
            ScopeStats {
                started: true,
                peak_bytes: Some(3 << 30),
                oom_kills: 2
            }
        );
        assert!(!parse_stats("").started);
    }
}
