//! What is running in a tab right now.
//!
//! A restored tab gets a fresh shell, so a full-screen app that was running
//! (btop, lazygit, k9s) is simply gone. Recording the foreground command lets
//! Giverny start it again — the same trick as `claude --resume`, and with the
//! same limits: the program restarts, its internal state does not.

use std::path::PathBuf;

/// Programs Giverny will restart on its own. Deliberately conservative:
/// monitors and browsers that are safe to re-run and pointless to restore
/// half-way. Anything else is recorded but never replayed, because
/// re-executing an arbitrary last command could redeploy, delete or push.
pub const DEFAULT_RESTORE_APPS: &[&str] = &[
    "btop",
    "btop4win",
    "bpytop",
    "htop",
    "top",
    "atop",
    "glances",
    "gotop",
    "s-tui",
    "nvtop",
    "k9s",
    "lazygit",
    "lazydocker",
    "gitui",
    "tig",
    "ranger",
    "yazi",
    "nnn",
    "mc",
    "ncdu",
    "dust",
    "duf",
    "iotop",
    "iftop",
    "bmon",
    "zenith",
];

/// The command running in the foreground of `shell_pid`'s tab, as an argv
/// string. `None` when the shell is just sitting at its prompt.
#[cfg(target_os = "linux")]
pub fn foreground_command(shell_pid: u32) -> Option<String> {
    // The shell's own children are the candidates; take the most recently
    // started, which is the one the user is looking at.
    let mut best: Option<(u64, String)> = None;
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // Fields after the parenthesised comm: state, ppid, …, starttime(22).
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let fields: Vec<&str> = rest.split_whitespace().collect();
        let ppid = fields.get(1).and_then(|p| p.parse::<u32>().ok());
        if ppid != Some(shell_pid) {
            continue;
        }
        let start: u64 = fields.get(19).and_then(|s| s.parse().ok()).unwrap_or(0);
        let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        let argv: Vec<String> = raw
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();
        if argv.is_empty() {
            continue;
        }
        let line = argv.join(" ");
        if best.as_ref().is_none_or(|(t, _)| start >= *t) {
            best = Some((start, line));
        }
    }
    best.map(|(_, line)| line)
}

#[cfg(not(target_os = "linux"))]
pub fn foreground_command(shell_pid: u32) -> Option<String> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_cmd(sysinfo::UpdateKind::Always),
    );
    let parent = Pid::from_u32(shell_pid);
    sys.processes()
        .values()
        .filter(|p| p.parent() == Some(parent))
        .max_by_key(|p| p.start_time())
        .map(|p| {
            let cmd: Vec<String> = p
                .cmd()
                .iter()
                .map(|s| s.to_string_lossy().into_owned())
                .collect();
            if cmd.is_empty() {
                p.name().to_string_lossy().into_owned()
            } else {
                cmd.join(" ")
            }
        })
}

/// Should `command` be restarted automatically? Matches on the program name
/// only — `sudo btop` and `/usr/bin/btop --utf-force` both count, but the
/// arguments are preserved when it is actually run.
pub fn is_restorable(command: &str, allow: &[String]) -> bool {
    let mut words = command.split_whitespace();
    let Some(first) = words.next() else {
        return false;
    };
    // Look through a leading `sudo`/`doas` to the real program.
    let program = match first.rsplit('/').next().unwrap_or(first) {
        "sudo" | "doas" | "env" => words
            .find(|w| !w.contains('='))
            .map(|w| w.rsplit('/').next().unwrap_or(w))
            .unwrap_or(""),
        other => other,
    };
    !program.is_empty() && allow.iter().any(|a| a == program)
}

/// Where a shell is currently sitting, for tabs whose cwd we track.
pub fn shell_cwd(shell_pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{shell_pid}/cwd")).ok()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = shell_pid;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow() -> Vec<String> {
        DEFAULT_RESTORE_APPS.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn restorable_matches_program_not_path_or_args() {
        let a = allow();
        assert!(is_restorable("btop", &a));
        assert!(is_restorable("/usr/bin/btop --utf-force", &a));
        assert!(is_restorable("sudo htop", &a));
        assert!(is_restorable("env TERM=xterm k9s", &a));
        assert!(!is_restorable("", &a));
    }

    #[test]
    fn arbitrary_commands_are_never_replayed() {
        let a = allow();
        // The whole point: a tab's last command is usually not safe to re-run.
        for cmd in [
            "rm -rf build",
            "git push --force",
            "./deploy.sh prod",
            "cargo test",
            "vim src/main.rs",
            "claude --resume abc",
        ] {
            assert!(!is_restorable(cmd, &a), "{cmd} must not auto-run");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn finds_the_command_a_shell_is_running() {
        use std::process::{Command, Stdio};
        // A shell whose child is a long sleep: the child is the foreground.
        let mut shell = Command::new("/bin/sh")
            .args(["-c", "sleep 5"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn shell");
        std::thread::sleep(std::time::Duration::from_millis(400));
        let found = foreground_command(shell.id());
        let _ = shell.kill();
        let _ = shell.wait();
        let found = found.expect("a child command was found");
        assert!(found.starts_with("sleep"), "got {found:?}");
    }
}
