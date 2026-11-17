//! Codex integration through documented hooks and app-server RPCs.
//! Never opens credential files or makes HTTP requests itself.
pub mod hooks;
pub mod rpc;
pub mod usage;

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Target {
    pub executable: String,
    pub home: PathBuf,
    pub wsl_distro: Option<String>,
}

impl Target {
    pub fn native(executable: &str, configured_home: &str) -> Self {
        let home = if configured_home.is_empty() {
            std::env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".codex"))
        } else {
            match configured_home.strip_prefix("~/") {
                Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
                None => PathBuf::from(configured_home),
            }
        };
        Self {
            executable: executable.into(),
            home,
            wsl_distro: None,
        }
    }

    /// Discover installations and existing profiles without spawning Codex
    /// or opening credential files. WSL targets are discovered in their distro.
    pub fn is_detected(&self) -> bool {
        if self.wsl_distro.is_some() {
            return true;
        }
        if self.home.join("config.toml").is_file()
            || self.home.join("history.jsonl").is_file()
            || self.home.join("sessions").is_dir()
        {
            return true;
        }
        let executable = PathBuf::from(resolve_executable(&self.executable));
        let executable_file = |path: &std::path::Path| {
            let Ok(metadata) = path.metadata() else {
                return false;
            };
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            }
            #[cfg(not(unix))]
            {
                metadata.is_file()
            }
        };
        if executable.is_absolute() || executable.components().count() > 1 {
            return executable_file(&executable);
        }
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .any(|dir| executable_file(&dir.join(&executable)))
    }

    /// Arguments are passed directly on native platforms, shell-quoted only
    /// at the WSL boundary. Login PATH belongs to that distribution.
    pub fn command(&self, args: &[&str]) -> Command {
        if let Some(distro) = &self.wsl_distro {
            let line = format!(
                "export CODEX_HOME={}; cd {} && exec {} {}",
                quote(&self.home.to_string_lossy()),
                quote(&self.home.to_string_lossy()),
                quote(&self.executable),
                args.iter().map(|s| quote(s)).collect::<Vec<_>>().join(" ")
            );
            let mut cmd = Command::new("wsl.exe");
            cmd.args(["-d", distro, "--", "sh", "-lc", &line]);
            cmd
        } else {
            let mut cmd = Command::new(resolve_executable(&self.executable));
            cmd.args(args).env("CODEX_HOME", &self.home);
            // Absolute npm shims may be selected from Settings when a desktop
            // launcher lacks a version manager's PATH. Node lives beside the
            // shim in standard Node installations.
            if let Some(parent) = PathBuf::from(&self.executable)
                .parent()
                .filter(|p| p.is_absolute())
            {
                let node = if cfg!(windows) { "node.exe" } else { "node" };
                if parent.join(node).is_file() {
                    let mut paths = vec![parent.to_path_buf()];
                    paths.extend(std::env::split_paths(
                        &std::env::var_os("PATH").unwrap_or_default(),
                    ));
                    if let Ok(path) = std::env::join_paths(paths) {
                        cmd.env("PATH", path);
                    }
                }
            }
            if self.home.is_dir() {
                cmd.current_dir(&self.home);
            } else if let Some(home) = dirs::home_dir() {
                cmd.current_dir(home);
            }
            cmd
        }
    }
}

/// Resolve Windows npm shims as well as binaries; never assume a package
/// manager or installation directory.
fn resolve_executable(executable: &str) -> std::ffi::OsString {
    #[cfg(windows)]
    if PathBuf::from(executable).extension().is_none() {
        let paths: Vec<_> = if executable.contains(['/', '\\']) {
            vec![PathBuf::from("")]
        } else {
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect()
        };
        for dir in paths {
            for extension in ["exe", "cmd", "bat"] {
                let candidate = dir.join(executable).with_extension(extension);
                if candidate.is_file() {
                    return candidate.into_os_string();
                }
            }
        }
    }
    executable.into()
}

/// A bounded diagnostic probe; does not authenticate or start a model turn.
pub fn version(target: &Target) -> anyhow::Result<String> {
    use std::{
        io::Read,
        process::Stdio,
        time::{Duration, Instant},
    };
    let mut cmd = target.command(&["--version"]);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000);
    }
    let mut child = cmd.spawn()?;
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                let mut out = String::new();
                if let Some(stdout) = child.stdout.take() {
                    stdout.take(4096).read_to_string(&mut out)?;
                }
                return Ok(out.trim().to_owned());
            }
            Ok(Some(_)) => anyhow::bail!("Codex version probe failed"),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            result => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(anyhow::anyhow!(
                    "Codex version probe timed out or failed: {result:?}"
                ));
            }
        }
    }
}

pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub fn valid_session_id(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

/// Recognize native binaries and the official npm launchers without matching
/// arbitrary commands which merely contain the word "codex".
pub fn is_codex_command(command: &str) -> bool {
    let words: Vec<_> = command.split_whitespace().collect();
    let name = |s: &str| {
        s.rsplit(['/', '\\'])
            .next()
            .unwrap_or(s)
            .to_ascii_lowercase()
    };
    let Some(first) = words.first() else {
        return false;
    };
    match name(first).as_str() {
        "codex" | "codex.exe" | "codex.cmd" => true,
        "node" | "node.exe" => words.get(1).is_some_and(|s| {
            matches!(name(s).as_str(), "codex" | "codex.cmd")
                || s.replace('\\', "/").ends_with("/codex/bin/codex.js")
        }),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn session_ids_cannot_inject_commands() {
        assert!(valid_session_id("01a144ed-3193-79d3-9569-4b9e28f3d9e4"));
        for id in [
            "../session",
            "$(touch x)",
            "00000000-0000-0000-0000-00000000000;",
            "------------------------------------",
        ] {
            assert!(!valid_session_id(id));
        }
        assert_eq!(quote("a'b $c"), "'a'\\''b $c'");
    }
    #[test]
    fn discovery_requires_a_profile_or_executable() {
        let home = std::env::temp_dir().join(format!("giverny-discovery-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let target = Target {
            executable: home.join("missing-codex").to_string_lossy().into_owned(),
            home: home.clone(),
            wsl_distro: None,
        };
        assert!(!target.is_detected());
        std::fs::write(home.join("config.toml"), "").unwrap();
        assert!(target.is_detected());
        std::fs::remove_file(home.join("config.toml")).unwrap();
        std::fs::create_dir(home.join("sessions")).unwrap();
        assert!(target.is_detected());
        std::fs::remove_dir(home.join("sessions")).unwrap();
        std::fs::write(&target.executable, "fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert!(!target.is_detected());
            std::fs::set_permissions(&target.executable, std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        assert!(target.is_detected());
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn recognizes_cli_without_matching_prompts() {
        assert!(is_codex_command("/usr/local/bin/codex resume uuid"));
        assert!(is_codex_command(
            "node /usr/lib/node_modules/@openai/codex/bin/codex.js"
        ));
        assert!(is_codex_command("node /portable/node/bin/codex"));
        assert!(is_codex_command("node.exe C:\\node\\codex.cmd"));
        assert!(!is_codex_command("node /project/codex-helper.js"));
        assert!(!is_codex_command("echo codex"));
    }
}
