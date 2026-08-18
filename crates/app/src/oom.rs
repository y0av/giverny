//! Surviving an out-of-memory kill that lands on a child process.
//!
//! Everything a tab starts — a shell, Claude Code, a dev server, a headless
//! browser — runs inside the systemd scope the desktop launcher created for
//! Giverny. Scopes default to `OOMPolicy=stop`, which means that when the
//! kernel's OOM killer picks off *any* process in the scope, systemd stops the
//! whole unit: Giverny is sent `SIGTERM` and every other tab goes with it. A
//! `next-server` eating the machine is enough to close the terminal.
//!
//! `OOMPolicy=continue` leaves the rest of the scope alone — the process the
//! kernel chose dies, nothing else does. It cannot be set from inside the
//! process, because the scope belongs to whoever launched us, so it goes in a
//! drop-in that systemd reads when the scope is created.
//!
//! Linux-only, and a no-op where there is no systemd to read it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// systemd reads drop-ins from every dash-truncated prefix of a unit name, so
/// `app-gnome-giverny-4121988.scope` picks up `app-gnome-giverny-.scope.d/`.
/// GNOME builds that name with a `gnome-` infix; KDE and most other launchers
/// leave it out. Both are ours alone — no other application's scope matches.
const SCOPE_DIRS: &[&str] = &["app-giverny-.scope.d", "app-gnome-giverny-.scope.d"];

const FILE: &str = "giverny-oom.conf";

const DROP_IN: &str = "\
# Written by `giverny install-desktop`. Remove it with `--remove`.
#
# An app scope defaults to OOMPolicy=stop: if the kernel's OOM killer takes any
# process inside it — a dev server in one tab, a headless browser in another —
# systemd stops the entire scope, and every other tab dies with it. Giverny
# would rather lose only the process the kernel chose.
[Scope]
OOMPolicy=continue
";

fn config_home() -> Option<PathBuf> {
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")),
    }
}

fn paths_in(config: &Path) -> Vec<PathBuf> {
    SCOPE_DIRS
        .iter()
        .map(|dir| config.join("systemd/user").join(dir).join(FILE))
        .collect()
}

fn paths() -> Vec<PathBuf> {
    config_home().map(|c| paths_in(&c)).unwrap_or_default()
}

/// systemd caches what it has loaded; without a reload the drop-in would only
/// take effect at the next login rather than the next launch.
fn reload() {
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
}

/// The scope this process is in, read from its cgroup. `doctor` runs in a tab,
/// which makes it a child of the app and so a member of the same scope.
fn scope_from_cgroup(contents: &str) -> Option<String> {
    contents
        .lines()
        .filter_map(|line| line.rsplit('/').next())
        .find(|unit| unit.ends_with(".scope"))
        .map(str::to_string)
}

fn enclosing_scope() -> Option<String> {
    scope_from_cgroup(&fs::read_to_string("/proc/self/cgroup").ok()?)
}

/// What systemd will do to the rest of the scope when one process is OOM-killed.
/// `None` when there is no systemd to ask, or we are not in a scope at all.
#[cfg(target_os = "linux")]
fn effective_policy() -> Option<String> {
    let scope = enclosing_scope()?;
    let out = Command::new("systemctl")
        .args(["--user", "show", &scope, "-p", "OOMPolicy", "--value"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

/// Best effort throughout: a machine with no systemd has nothing to fix, and
/// a failure here costs the OOM behaviour, not the install.
pub fn install() {
    if !cfg!(target_os = "linux") {
        return;
    }
    let mut written = Vec::new();
    for path in paths() {
        if let Some(parent) = path.parent()
            && let Err(err) = fs::create_dir_all(parent)
        {
            eprintln!("warning: cannot create {}: {err}", parent.display());
            continue;
        }
        match fs::write(&path, DROP_IN) {
            Ok(()) => written.push(path),
            Err(err) => eprintln!("warning: cannot write {}: {err}", path.display()),
        }
    }
    if written.is_empty() {
        return;
    }
    reload();
    println!(
        "installed the OOM drop-in ({})",
        written
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!("  a tab's process being OOM-killed no longer closes the whole terminal");
}

/// Removes the drop-in, and any directory it left empty.
pub fn remove() -> usize {
    let mut gone = 0;
    for path in paths() {
        if fs::remove_file(&path).is_ok() {
            gone += 1;
            if let Some(parent) = path.parent() {
                let _ = fs::remove_dir(parent);
            }
        }
    }
    if gone > 0 {
        reload();
    }
    gone
}

/// For `doctor`: whether the drop-in is in place, and what the scope around us
/// is actually set to — the second is the one that decides what happens.
#[cfg(target_os = "linux")]
pub struct Status {
    pub installed: bool,
    pub policy: Option<String>,
}

#[cfg(target_os = "linux")]
pub fn status() -> Status {
    Status {
        installed: {
            let paths = paths();
            !paths.is_empty() && paths.iter().all(|p| p.is_file())
        },
        policy: effective_policy(),
    }
}

/// Whether this process sits in a scope that would take the app down with a
/// single OOM-killed child. Cheap enough for startup: no subprocess, and no
/// answer at all unless we are in one of our own scopes.
pub fn at_risk() -> bool {
    if !cfg!(target_os = "linux") {
        return false;
    }
    enclosing_scope().is_some_and(|s| s.contains("giverny")) && paths().iter().any(|p| !p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_in_sets_the_policy() {
        assert!(DROP_IN.contains("[Scope]"));
        assert!(DROP_IN.contains("OOMPolicy=continue"));
    }

    #[test]
    fn one_drop_in_per_launcher_naming() {
        let paths = paths_in(Path::new("/home/x/.config"));
        assert_eq!(
            paths,
            vec![
                PathBuf::from("/home/x/.config/systemd/user/app-giverny-.scope.d/giverny-oom.conf"),
                PathBuf::from(
                    "/home/x/.config/systemd/user/app-gnome-giverny-.scope.d/giverny-oom.conf"
                ),
            ],
            "GNOME names the scope app-gnome-giverny-<pid>, others app-giverny-<pid>"
        );
    }

    #[test]
    fn scope_read_from_cgroup() {
        let cgroup = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/\
                      app-gnome-giverny-4121988.scope\n";
        assert_eq!(
            scope_from_cgroup(cgroup).as_deref(),
            Some("app-gnome-giverny-4121988.scope")
        );
        assert_eq!(
            scope_from_cgroup("0::/user.slice/user-1000.slice/session-2.scope\n").as_deref(),
            Some("session-2.scope"),
            "a scope we do not own is still a scope"
        );
        assert_eq!(
            scope_from_cgroup("0::/user.slice/user-1000.slice/user@1000.service/init.scope-ish\n"),
            None,
            "only a real .scope counts"
        );
        assert_eq!(scope_from_cgroup(""), None);
    }
}
