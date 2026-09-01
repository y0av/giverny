//! Claude accounts that live inside WSL, seen from a Windows Giverny.
//!
//! On Windows, Giverny opens tabs in a WSL login shell by default, because
//! that is where Claude Code and unix tooling usually live. Everything that
//! reads an account then has the wrong home: `~/.claude` on the Windows side
//! is a different directory from `~/.claude` in the distribution, so the
//! usage cache is empty, the identity is unknown, and `claude -p /usage`
//! finds no program to run.
//!
//! A distribution's filesystem is reachable from Windows as a UNC path, so
//! *reading* needs nothing special once the path is known — the mapping in
//! this module is what makes it known. Anything that has to *run* (refreshing
//! usage, the hook relay) goes back through `wsl.exe`, which is the only part
//! that needs a live distribution.

use std::path::{Path, PathBuf};

/// Windows' two spellings for the share that exposes a distribution's
/// filesystem. `\\wsl.localhost\` is current; `\\wsl$\` is the older one,
/// still in use and still what plenty of config files say.
const UNC_PREFIXES: [&str; 2] = [r"\\wsl.localhost\", r"\\wsl$\"];

/// Split a Windows path into the distribution it lives in and the unix path
/// inside it. `None` for any path that is not inside WSL.
pub fn split_unc(path: &Path) -> Option<(String, String)> {
    let text = path.to_str()?;
    let rest = UNC_PREFIXES
        .iter()
        .find_map(|prefix| text.strip_prefix(prefix))?;
    let (distro, tail) = match rest.split_once('\\') {
        Some((distro, tail)) => (distro, tail),
        // `\\wsl.localhost\Ubuntu` — the root of the distribution.
        None => (rest, ""),
    };
    if distro.is_empty() {
        return None;
    }
    let unix = format!("/{}", tail.replace('\\', "/"));
    Some((distro.to_string(), unix.trim_end_matches('/').to_string()))
}

/// The Windows path for `unix_path` inside `distro`.
pub fn unc_path(distro: &str, unix_path: &str) -> PathBuf {
    let tail = unix_path.trim_start_matches('/').replace('/', "\\");
    PathBuf::from(format!("{}{distro}\\{tail}", UNC_PREFIXES[0]))
}

/// Does this path live inside a WSL distribution?
pub fn is_wsl_path(path: &Path) -> bool {
    split_unc(path).is_some()
}

/// Claude account directories inside every installed distribution, as
/// Windows paths. Empty off Windows, where a unix home is just the home.
pub fn account_dirs() -> Vec<PathBuf> {
    distros()
        .iter()
        .filter_map(|distro| {
            let dir = account_dir(distro)?;
            dir.is_dir().then_some(dir)
        })
        .collect()
}

/// Where one distribution keeps the account Claude Code uses by default.
pub fn account_dir(distro: &str) -> Option<PathBuf> {
    let home = home(distro)?;
    Some(unc_path(distro, &format!("{home}/.claude")))
}

/// Is `config_dir` the account that distribution's Claude Code uses when
/// nothing names one? Then a command typed into its shell needs no
/// `CLAUDE_CONFIG_DIR` in front of it.
pub fn is_default_account(distro: &str, unix_dir: &str) -> bool {
    home(distro).is_some_and(|h| format!("{h}/.claude") == unix_dir)
}

/// The profile directory a config dir reported from inside a distribution
/// belongs to.
///
/// Profiles are keyed by the path Windows can open; a session running inside
/// WSL reports the path *it* can open. Everything the app stores — which
/// account a tab is on, which account a session resumes under — uses the
/// Windows-side name, so the translation happens once, at the boundary.
pub fn canonical_config_dir(reported: &Path, known: &[PathBuf]) -> Option<PathBuf> {
    if known.iter().any(|k| k == reported) {
        return Some(reported.to_path_buf());
    }
    let text = reported.to_str()?;
    if !text.starts_with('/') {
        return None;
    }
    // Two distributions can hold the same unix path; the first match is the
    // best available answer, and only a machine running the same account in
    // two distributions at once can tell the difference.
    known
        .iter()
        .find(|k| split_unc(k).is_some_and(|(_, unix)| unix == text))
        .cloned()
}

/// Where each Giverny tab's shell is, inside every distribution:
/// `(tab id, distribution, unix path)`.
///
/// A tab inside WSL had no source for this at all. Windows can see `wsl.exe`'s
/// own working directory and nothing about the shell it started; `/proc` is on
/// the other side; and a WSL bash emits no OSC 7 unless something configured
/// it to. The distribution can answer for itself: every process Giverny
/// started carries `GIVERNY_TAB_ID` in its environment, and `/proc` has the
/// rest. One `wsl.exe` per distribution answers for every tab at once.
pub fn tab_cwds() -> Vec<(String, String, String)> {
    #[cfg(windows)]
    {
        let mut out = Vec::new();
        for distro in distros() {
            out.extend(
                tab_cwds_in(&distro)
                    .into_iter()
                    .map(|(tab, cwd)| (tab, distro.clone(), cwd)),
            );
        }
        out
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// One distribution's answer, or nothing.
///
/// The script travels as a *file* over the share rather than as an argument
/// to `wsl.exe`. A command line crossing that boundary is rebuilt on the way
/// through, and this one is quotes, newlines and `$` from end to end — the
/// version that passed it as an argument came back empty every time, on a
/// machine where running the same script by hand worked. Rewritten each
/// sweep, since a distribution that restarted took `/tmp` with it.
#[cfg(windows)]
pub fn tab_cwds_in(distro: &str) -> Vec<(String, String)> {
    const AT: &str = "/tmp/.giverny-probe.sh";
    if std::fs::write(unc_path(distro, AT), TAB_CWD_SCRIPT).is_err() {
        return Vec::new();
    }
    let Some(text) = imp::capture(distro, &["sh", AT]) else {
        return Vec::new();
    };
    publish_live_pids(distro, parse_live_pids(&text));
    parse_tab_cwds(&text)
}

/// Nothing to probe where there is no distribution.
#[cfg(not(windows))]
pub fn tab_cwds_in(distro: &str) -> Vec<(String, String)> {
    let _ = distro;
    Vec::new()
}

/// Every process that belongs to a tab, with its parent and its directory.
/// `sh`, not `bash`: this runs in whatever the distribution has.
#[cfg(any(windows, test))]
const TAB_CWD_SCRIPT: &str = r#"for d in /proc/[0-9]*; do
id=$(tr '\0' '\n' < "$d/environ" 2>/dev/null | sed -n 's/^GIVERNY_TAB_ID=//p' | head -n1)
[ -n "$id" ] || continue
cwd=$(readlink "$d/cwd" 2>/dev/null) || continue
[ -n "$cwd" ] || continue
ppid=$(sed -n 's/^PPid:[[:space:]]*//p' "$d/status" 2>/dev/null)
printf '%s\t%s\t%s\t%s\n' "$id" "${d#/proc/}" "$ppid" "$cwd"
done
printf 'pids'
for d in /proc/[0-9]*; do printf ' %s' "${d#/proc/}"; done
printf '\n'"#;

/// The `pids` line: every process id the distribution had when asked.
#[cfg(any(windows, test))]
fn parse_live_pids(text: &str) -> Vec<u32> {
    text.lines()
        .find_map(|line| line.strip_prefix("pids "))
        .map(|list| {
            list.split_whitespace()
                .filter_map(|p| p.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// What the last sweep saw running inside each distribution.
///
/// Read by the session registry, which cannot ask for itself: it is consulted
/// from the UI thread, and a `wsl.exe` launch there is a frozen window. So the
/// sweep — already off-thread, already walking `/proc` — leaves the answer
/// here, and the registry only ever reads it.
static LIVE_PIDS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, std::collections::HashSet<u32>>>,
> = std::sync::LazyLock::new(Default::default);

/// Publish one distribution's live pids. Called only by the sweep.
pub fn publish_live_pids(distro: &str, pids: Vec<u32>) {
    if pids.is_empty() {
        return;
    }
    if let Ok(mut map) = LIVE_PIDS.lock() {
        map.insert(distro.to_string(), pids.into_iter().collect());
    }
}

/// Is `pid` running inside the distribution holding `config_dir`?
///
/// `None` when no sweep has reported that distribution yet — at startup, or
/// where Giverny has no tab in it. The caller decides what to do with not
/// knowing; treating it as "still running" would refuse to restore a
/// conversation on exactly the launch that wants to.
pub fn pid_alive_in(config_dir: &Path, pid: u32) -> Option<bool> {
    let (distro, _) = split_unc(config_dir)?;
    let map = LIVE_PIDS.lock().ok()?;
    let pids = map.get(&distro)?;
    Some(pids.contains(&pid))
}

/// One directory per tab, from the script's per-process lines.
///
/// A tab holds a tree — the login shell, claude, whatever claude ran — and
/// they are in different directories. The shell is the one to report: it is
/// the root of the tree, the process whose parent is not also in it.
#[cfg(any(windows, test))]
fn parse_tab_cwds(text: &str) -> Vec<(String, String)> {
    struct Row<'a> {
        tab: &'a str,
        pid: &'a str,
        ppid: &'a str,
        cwd: &'a str,
    }
    let rows: Vec<Row> = text
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(4, '\t');
            Some(Row {
                tab: parts.next()?,
                pid: parts.next()?,
                ppid: parts.next()?,
                // A directory may contain anything but a tab, which is why it
                // is last and why the split is bounded.
                cwd: parts.next()?,
            })
        })
        .filter(|r| !r.tab.is_empty() && r.cwd.starts_with('/'))
        .collect();

    let mut out: Vec<(String, String)> = Vec::new();
    for row in &rows {
        if out.iter().any(|(tab, _)| tab == row.tab) {
            continue;
        }
        let root = rows
            .iter()
            .filter(|r| r.tab == row.tab)
            .find(|r| {
                !rows
                    .iter()
                    .any(|other| other.tab == r.tab && other.pid == r.ppid)
            })
            .unwrap_or(row);
        out.push((root.tab.to_string(), root.cwd.to_string()));
    }
    out
}

/// Installed distributions. Empty off Windows.
pub fn distros() -> Vec<String> {
    #[cfg(windows)]
    {
        imp::distros().to_vec()
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}

/// The distribution `wsl.exe` opens when no `-d` names one.
pub fn default_distro() -> Option<String> {
    #[cfg(windows)]
    {
        imp::default_distro()
    }
    #[cfg(not(windows))]
    {
        None
    }
}

/// The unix home of a distribution's default user.
pub fn home(distro: &str) -> Option<String> {
    #[cfg(windows)]
    {
        imp::home(distro)
    }
    #[cfg(not(windows))]
    {
        let _ = distro;
        None
    }
}

/// Where `claude` is inside a distribution.
pub fn claude_bin(distro: &str) -> Option<String> {
    #[cfg(windows)]
    {
        imp::claude_bin(distro)
    }
    #[cfg(not(windows))]
    {
        let _ = distro;
        None
    }
}

/// A Windows path as the distribution sees it.
pub fn to_wsl_path(distro: &str, windows_path: &Path) -> Option<String> {
    #[cfg(windows)]
    {
        imp::to_wsl_path(distro, windows_path)
    }
    #[cfg(not(windows))]
    {
        let (_, _) = (distro, windows_path);
        None
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::collections::HashMap;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::sync::{LazyLock, Mutex, OnceLock};
    use std::time::Duration;

    /// CREATE_NO_WINDOW: no console flashes up in front of the app.
    const NO_WINDOW: u32 = 0x0800_0000;

    /// Distributions that belong to another program rather than to a person.
    ///
    /// Docker Desktop installs two, and they hold no home, no shell worth
    /// speaking of and certainly no Claude account. Asking one of them about
    /// itself while Docker is not running is where the startup hang came
    /// from, and there was never a reason to ask.
    fn is_infrastructure(name: &str) -> bool {
        name.starts_with("docker-desktop")
    }

    pub fn command(distro: &str) -> Command {
        let mut cmd = Command::new("wsl.exe");
        cmd.arg("-d")
            .arg(distro)
            .creation_flags(NO_WINDOW)
            .stdin(Stdio::null());
        cmd
    }

    /// How long any one `wsl.exe` may take before it is abandoned.
    ///
    /// Every call here can land on a distribution that has to be started
    /// first, and some never answer at all. Discovery runs during startup, so
    /// "never answers" means a window that never opens — which is exactly what
    /// a machine with Docker Desktop's distributions installed saw: no output,
    /// no crash, no window.
    const CAP: Duration = Duration::from_secs(6);

    /// Run something inside `distro` and return its stdout, trimmed.
    /// `None` when the distribution, or the command, is not there — or when it
    /// takes longer than anyone can wait for a terminal to open.
    pub fn capture(distro: &str, args: &[&str]) -> Option<String> {
        let mut cmd = command(distro);
        cmd.arg("--").args(args);
        let out = run_capped(cmd)?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!text.is_empty()).then_some(text)
    }

    /// `Command::output`, with a deadline and a kill behind it.
    pub fn run_capped(mut cmd: Command) -> Option<std::process::Output> {
        let mut child = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let deadline = std::time::Instant::now() + CAP;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return child.wait_with_output().ok(),
                Ok(None) if std::time::Instant::now() >= deadline => {
                    let _ = child.kill();
                    return None;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(_) => return None,
            }
        }
    }

    /// Installed distributions, asked once. `wsl.exe -l -q` answers in
    /// UTF-16 (it is wsl.exe talking, not a program inside a distribution)
    /// and fails outright when nothing is installed.
    pub fn distros() -> &'static [String] {
        static DISTROS: OnceLock<Vec<String>> = OnceLock::new();
        DISTROS.get_or_init(|| {
            let mut cmd = Command::new("wsl.exe");
            cmd.args(["-l", "-q"])
                .creation_flags(NO_WINDOW)
                .stdin(Stdio::null());
            let Some(out) = run_capped(cmd) else {
                return Vec::new();
            };
            if !out.status.success() {
                return Vec::new();
            }
            let utf16: Vec<u16> = out
                .stdout
                .as_chunks::<2>()
                .0
                .iter()
                .map(|b| u16::from_le_bytes(*b))
                .collect();
            String::from_utf16_lossy(&utf16)
                .lines()
                .map(|line| line.trim().trim_matches('\u{feff}').to_string())
                .filter(|line| !line.is_empty())
                .filter(|name| !is_infrastructure(name))
                .collect()
        })
    }

    /// One answer per distribution, kept for the life of the process: these
    /// are asked on the UI thread and each one costs a `wsl.exe` launch.
    fn cached(
        store: &'static LazyLock<Mutex<HashMap<String, Option<String>>>>,
        distro: &str,
        ask: impl FnOnce() -> Option<String>,
    ) -> Option<String> {
        if let Some(hit) = store.lock().ok()?.get(distro) {
            return hit.clone();
        }
        let answer = ask();
        if let Ok(mut map) = store.lock() {
            map.insert(distro.to_string(), answer.clone());
        }
        answer
    }

    static HOMES: LazyLock<Mutex<HashMap<String, Option<String>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    static BINS: LazyLock<Mutex<HashMap<String, Option<String>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    static EXES: LazyLock<Mutex<HashMap<String, Option<String>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    /// The distribution `wsl.exe` opens with no `-d`: the one a tab lands in
    /// unless something says otherwise. Asked, because the listing order is
    /// not it.
    pub fn default_distro() -> Option<String> {
        static DEFAULT: OnceLock<Option<String>> = OnceLock::new();
        DEFAULT
            .get_or_init(|| {
                let mut cmd = Command::new("wsl.exe");
                cmd.args(["--", "sh", "-c", "printf %s \"$WSL_DISTRO_NAME\""])
                    .creation_flags(NO_WINDOW)
                    .stdin(Stdio::null());
                let out = run_capped(cmd)?;
                out.status
                    .success()
                    .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())?
                    .into()
            })
            .clone()
            .filter(|name: &String| !name.is_empty())
    }

    /// The unix home of the distribution's default user.
    pub fn home(distro: &str) -> Option<String> {
        cached(&HOMES, distro, || {
            capture(distro, &["sh", "-c", "cd ~ && pwd"])
        })
    }

    /// Where `claude` is inside the distribution.
    ///
    /// A login shell first, since Claude Code's own installer puts it in
    /// `~/.local/bin` and leaves the `PATH` line in `~/.profile`; then an
    /// interactive bash, which is the only place an nvm-managed npm install
    /// exists. Resolved to a path rather than run by name, so the answer can
    /// be printed by `giverny doctor` and a failure says which step failed.
    pub fn claude_bin(distro: &str) -> Option<String> {
        cached(&BINS, distro, || {
            capture(distro, &["sh", "-lc", "command -v claude"])
                .or_else(|| capture(distro, &["bash", "-lic", "command -v claude"]))
                .and_then(|out| out.lines().last().map(|l| l.trim().to_string()))
                .filter(|path| path.starts_with('/'))
        })
    }

    /// `windows_path` as the distribution sees it. Asked of `wslpath`, which
    /// knows where the drives are mounted; assuming `/mnt/c` is a guess that
    /// is wrong on any machine with a custom automount root.
    pub fn to_wsl_path(distro: &str, windows_path: &Path) -> Option<String> {
        let key = format!("{distro}\u{0}{}", windows_path.display());
        let store: &'static LazyLock<Mutex<HashMap<String, Option<String>>>> = &EXES;
        if let Some(hit) = store.lock().ok()?.get(&key) {
            return hit.clone();
        }
        let answer = capture(
            distro,
            &["wslpath", "-a", "-u", &windows_path.to_string_lossy()],
        )
        .filter(|path| path.starts_with('/'));
        if let Ok(mut map) = store.lock() {
            map.insert(key, answer.clone());
        }
        answer
    }
}

#[cfg(windows)]
pub use imp::command;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unc_paths_map_both_ways() {
        let dir = Path::new(r"\\wsl.localhost\Ubuntu\home\itay\.claude");
        let (distro, unix) = split_unc(dir).expect("a WSL path");
        assert_eq!(distro, "Ubuntu");
        assert_eq!(unix, "/home/itay/.claude");
        assert_eq!(unc_path(&distro, &unix), dir);
    }

    /// The older spelling is still what plenty of machines use.
    #[test]
    fn the_legacy_share_name_is_understood() {
        let (distro, unix) =
            split_unc(Path::new(r"\\wsl$\Debian\home\x\.claude.json")).expect("a WSL path");
        assert_eq!(distro, "Debian");
        assert_eq!(unix, "/home/x/.claude.json");
    }

    #[test]
    fn a_distribution_name_may_have_spaces() {
        let (distro, unix) =
            split_unc(Path::new(r"\\wsl.localhost\Ubuntu 22.04\root\.claude")).unwrap();
        assert_eq!(distro, "Ubuntu 22.04");
        assert_eq!(unix, "/root/.claude");
    }

    /// What a session inside a distribution reports, mapped back to the name
    /// the app keys everything by.
    #[test]
    fn a_unix_config_dir_maps_back_to_the_windows_one() {
        let known = vec![
            PathBuf::from(r"C:\Users\itay\.claude"),
            PathBuf::from(r"\\wsl.localhost\Ubuntu\home\itay\.claude"),
        ];
        assert_eq!(
            canonical_config_dir(Path::new("/home/itay/.claude"), &known),
            Some(known[1].clone())
        );
        // A path already in Windows terms is already canonical.
        assert_eq!(
            canonical_config_dir(&known[0], &known),
            Some(known[0].clone())
        );
        // An account nobody knows about stays unknown rather than being
        // attributed to the nearest match.
        assert_eq!(
            canonical_config_dir(Path::new("/home/itay/.claude-work"), &known),
            None
        );
    }

    /// A tab is a tree of processes in different directories; the shell at
    /// the root of it is the tab's place.
    #[test]
    fn a_tabs_directory_is_its_shells() {
        let text = "giverny-3\t900\t1\t/home/ita/proj\n\
                    giverny-3\t901\t900\t/home/ita/proj/src\n\
                    giverny-3\t902\t901\t/tmp\n\
                    giverny-7\t950\t1\t/home/ita\n";
        let mut found = parse_tab_cwds(text);
        found.sort();
        assert_eq!(
            found,
            vec![
                ("giverny-3".to_string(), "/home/ita/proj".to_string()),
                ("giverny-7".to_string(), "/home/ita".to_string()),
            ]
        );
    }

    /// The script is only ever run by a distribution, which means a syntax
    /// error in it is invisible until someone with WSL hits it. Any unix can
    /// run it: what it finds here does not matter, that it runs does.
    #[cfg(unix)]
    #[test]
    fn the_probe_script_runs() {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(TAB_CWD_SCRIPT)
            .output()
            .expect("sh");
        assert!(
            out.status.success(),
            "script failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let text = String::from_utf8_lossy(&out.stdout);
        for (tab, cwd) in parse_tab_cwds(&text) {
            assert!(!tab.is_empty());
            assert!(cwd.starts_with('/'), "{cwd}");
        }
    }

    /// The pid list rides along with the same sweep, and is not mistaken for
    /// a tab.
    #[test]
    fn the_pid_line_is_read_and_not_confused_for_a_tab() {
        let text = "giverny-3\t900\t1\t/home/ita/proj\npids 1 2 900 1043\n";
        assert_eq!(parse_live_pids(text), vec![1, 2, 900, 1043]);
        assert_eq!(
            parse_tab_cwds(text),
            vec![("giverny-3".to_string(), "/home/ita/proj".to_string())]
        );
        assert!(parse_live_pids("giverny-3\t900\t1\t/home/x\n").is_empty());
    }

    /// A directory with a space in it is still one field.
    #[test]
    fn directories_with_spaces_survive_parsing() {
        let found = parse_tab_cwds("giverny-1\t5\t1\t/home/ita/my proj\n");
        assert_eq!(
            found,
            vec![("giverny-1".into(), "/home/ita/my proj".into())]
        );
        assert!(parse_tab_cwds("nonsense\n").is_empty());
    }

    #[test]
    fn ordinary_paths_are_not_wsl() {
        assert!(!is_wsl_path(Path::new(r"C:\Users\itay\.claude")));
        assert!(!is_wsl_path(Path::new("/home/itay/.claude")));
        assert!(!is_wsl_path(Path::new(r"\\server\share\.claude")));
    }
}
