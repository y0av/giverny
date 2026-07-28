//! PTY spawning: environment assembly, shell resolution, and
//! `alacritty_terminal::tty` construction for a Giverny tab.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use alacritty_terminal::event::WindowSize;
use alacritty_terminal::tty::{self, Options, Shell};

/// Everything needed to spawn one tab's PTY.
#[derive(Debug, Clone)]
pub struct SpawnCfg {
    /// Explicit shell override `(program, args)`; `None` = resolve default.
    pub shell: Option<(String, Vec<String>)>,
    /// Working directory (must exist; callers handle fallback-to-`$HOME`).
    pub cwd: PathBuf,
    /// Extra env vars (profile presets etc.), applied last.
    pub env_extra: Vec<(String, String)>,
    /// Stable tab identity, exported as `GIVERNY_TAB_ID` (hooks inherit it).
    pub tab_id: String,
    /// Per-spawn secret for the private OSC channel (`GIVERNY_NONCE`).
    pub nonce: String,
    /// Account profile: sets `CLAUDE_CONFIG_DIR` when present.
    pub claude_config_dir: Option<PathBuf>,
    /// Initial grid + cell geometry.
    pub size: GridSize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridSize {
    pub cols: u16,
    pub rows: u16,
    /// Cell size in physical pixels (integer metrics).
    pub cell_width: u16,
    pub cell_height: u16,
}

impl From<GridSize> for WindowSize {
    fn from(g: GridSize) -> Self {
        WindowSize {
            num_lines: g.rows,
            num_cols: g.cols,
            cell_width: g.cell_width,
            cell_height: g.cell_height,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error("working directory does not exist: {0}")]
    CwdMissing(PathBuf),
    #[error("failed to spawn pty: {0}")]
    Io(#[from] std::io::Error),
}

/// Environment for the child, layered over the app's inherited environment.
pub fn build_env(cfg: &SpawnCfg) -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert("TERM".into(), "xterm-256color".into());
    env.insert("COLORTERM".into(), "truecolor".into());
    env.insert("TERM_PROGRAM".into(), "giverny".into());
    env.insert("TERM_PROGRAM_VERSION".into(), env!("CARGO_PKG_VERSION").into());
    env.insert("GIVERNY_TAB_ID".into(), cfg.tab_id.clone());
    env.insert("GIVERNY_NONCE".into(), cfg.nonce.clone());
    if let Some(dir) = &cfg.claude_config_dir {
        env.insert("CLAUDE_CONFIG_DIR".into(), dir.to_string_lossy().into_owned());
    }
    for (k, v) in &cfg.env_extra {
        env.insert(k.clone(), v.clone());
    }
    env
}

/// Resolve the shell to run: explicit override → `$SHELL` → common fallbacks.
pub fn resolve_shell(cfg: &SpawnCfg) -> Shell {
    if let Some((prog, args)) = &cfg.shell {
        return Shell::new(prog.clone(), args.clone());
    }
    #[cfg(unix)]
    {
        let from_env = std::env::var("SHELL")
            .ok()
            .filter(|s| !s.is_empty() && Path::new(s).exists());
        let prog = from_env.unwrap_or_else(|| {
            ["/bin/zsh", "/usr/bin/zsh", "/bin/bash", "/bin/sh"]
                .iter()
                .find(|p| Path::new(p).exists())
                .map(|s| s.to_string())
                .unwrap_or_else(|| "/bin/sh".into())
        });
        Shell::new(prog, vec![])
    }
    #[cfg(windows)]
    {
        // Tier-2: config → WSL → PowerShell fallback chain lands in M6.
        Shell::new("powershell.exe".into(), vec![])
    }
}

/// Spawn the PTY for a tab. `window_id` feeds `WINDOWID`/utmp bookkeeping.
pub fn spawn(cfg: &SpawnCfg, window_id: u64) -> Result<tty::Pty, SpawnError> {
    if !cfg.cwd.is_dir() {
        return Err(SpawnError::CwdMissing(cfg.cwd.clone()));
    }
    let options = Options {
        shell: Some(resolve_shell(cfg)),
        working_directory: Some(cfg.cwd.clone()),
        env: build_env(cfg),
        drain_on_exit: true,
        #[cfg(target_os = "windows")]
        escape_args: false,
    };
    Ok(tty::new(&options, cfg.size.into(), window_id)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SpawnCfg {
        SpawnCfg {
            shell: None,
            cwd: std::env::temp_dir(),
            env_extra: vec![("FOO".into(), "bar".into())],
            tab_id: "tab-1".into(),
            nonce: "n0nce".into(),
            claude_config_dir: Some(PathBuf::from("/home/u/envs/x/claude")),
            size: GridSize { cols: 80, rows: 24, cell_width: 9, cell_height: 18 },
        }
    }

    #[test]
    fn env_has_identity_and_extras() {
        let env = build_env(&cfg());
        assert_eq!(env.get("TERM").unwrap(), "xterm-256color");
        assert_eq!(env.get("TERM_PROGRAM").unwrap(), "giverny");
        assert_eq!(env.get("GIVERNY_TAB_ID").unwrap(), "tab-1");
        assert_eq!(env.get("GIVERNY_NONCE").unwrap(), "n0nce");
        assert_eq!(env.get("CLAUDE_CONFIG_DIR").unwrap(), "/home/u/envs/x/claude");
        assert_eq!(env.get("FOO").unwrap(), "bar");
    }

    #[test]
    fn no_config_dir_means_no_env_var() {
        let mut c = cfg();
        c.claude_config_dir = None;
        assert!(!build_env(&c).contains_key("CLAUDE_CONFIG_DIR"));
    }

    #[test]
    fn missing_cwd_is_reported() {
        let mut c = cfg();
        c.cwd = PathBuf::from("/definitely/not/a/dir/giverny-test");
        match spawn(&c, 0) {
            Err(SpawnError::CwdMissing(p)) => assert_eq!(p, c.cwd),
            Err(e) => panic!("expected CwdMissing, got {e}"),
            Ok(_) => panic!("expected CwdMissing, got a pty"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn spawns_a_real_pty() {
        let mut c = cfg();
        c.shell = Some(("/bin/sh".into(), vec!["-c".into(), "exit 0".into()]));
        let pty = spawn(&c, 0).expect("pty spawn");
        drop(pty);
    }
}
