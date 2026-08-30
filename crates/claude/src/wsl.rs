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

    /// CREATE_NO_WINDOW: no console flashes up in front of the app.
    const NO_WINDOW: u32 = 0x0800_0000;

    pub fn command(distro: &str) -> Command {
        let mut cmd = Command::new("wsl.exe");
        cmd.arg("-d")
            .arg(distro)
            .creation_flags(NO_WINDOW)
            .stdin(Stdio::null());
        cmd
    }

    /// Run something inside `distro` and return its stdout, trimmed.
    /// `None` when the distribution, or the command, is not there.
    fn capture(distro: &str, args: &[&str]) -> Option<String> {
        let out = command(distro).arg("--").args(args).output().ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!text.is_empty()).then_some(text)
    }

    /// Installed distributions, asked once. `wsl.exe -l -q` answers in
    /// UTF-16 (it is wsl.exe talking, not a program inside a distribution)
    /// and fails outright when nothing is installed.
    pub fn distros() -> &'static [String] {
        static DISTROS: OnceLock<Vec<String>> = OnceLock::new();
        DISTROS.get_or_init(|| {
            let Ok(out) = Command::new("wsl.exe")
                .args(["-l", "-q"])
                .creation_flags(NO_WINDOW)
                .stdin(Stdio::null())
                .output()
            else {
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
                let out = Command::new("wsl.exe")
                    .args(["--", "sh", "-c", "printf %s \"$WSL_DISTRO_NAME\""])
                    .creation_flags(NO_WINDOW)
                    .stdin(Stdio::null())
                    .output()
                    .ok()?;
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

    #[test]
    fn ordinary_paths_are_not_wsl() {
        assert!(!is_wsl_path(Path::new(r"C:\Users\itay\.claude")));
        assert!(!is_wsl_path(Path::new("/home/itay/.claude")));
        assert!(!is_wsl_path(Path::new(r"\\server\share\.claude")));
    }
}
