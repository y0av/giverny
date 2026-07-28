//! Persistent state: the workspace (tabs/categories) and per-tab scrollback
//! snapshots, written atomically under the user config dir.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::tabs::{TabId, Workspace};

pub const STATE_VERSION: u32 = 1;
/// Snapshot read cap — a corrupt/oversized file must not balloon memory.
const SNAPSHOT_READ_CAP: u64 = 8 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
pub struct SaveState {
    pub version: u32,
    #[serde(default)]
    pub boot_id: String,
    #[serde(default)]
    pub clean_shutdown: bool,
    pub workspace: Workspace,
    #[serde(default = "default_font_size")]
    pub font_size: f32,
}

fn default_font_size() -> f32 {
    13.0
}

/// Filesystem layout for Giverny's persistent data.
#[derive(Debug, Clone)]
pub struct Paths {
    base: PathBuf,
}

impl Paths {
    pub fn default_dirs() -> Self {
        let base = dirs::config_dir()
            .unwrap_or_else(|| {
                dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".config")
            })
            .join("giverny");
        Paths { base }
    }

    /// Custom base (tests).
    pub fn at(base: impl Into<PathBuf>) -> Self {
        Paths { base: base.into() }
    }

    pub fn state_file(&self) -> PathBuf {
        self.base.join("state").join("tabs.json")
    }

    pub fn snapshot_file(&self, tab: TabId) -> PathBuf {
        self.base.join("state").join("snapshots").join(format!("{}.ansi", tab.0))
    }
}

/// Linux boot id (used later to distinguish restart from reboot).
pub fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let dir = path.parent().ok_or_else(|| anyhow::anyhow!("no parent dir"))?;
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn save(paths: &Paths, state: &SaveState) -> anyhow::Result<()> {
    let json = serde_json::to_vec_pretty(state)?;
    atomic_write(&paths.state_file(), &json)
}

/// Load state; tolerant of corruption and future versions (bad files are
/// renamed aside so a broken state never wedges startup).
pub fn load(paths: &Paths) -> Option<SaveState> {
    let path = paths.state_file();
    let bytes = std::fs::read(&path).ok()?;
    match serde_json::from_slice::<SaveState>(&bytes) {
        Ok(state) if state.version <= STATE_VERSION => Some(state),
        Ok(state) => {
            tracing::warn!("state version {} is newer than {}", state.version, STATE_VERSION);
            let _ = std::fs::rename(&path, path.with_extension(format!("v{}.bak", state.version)));
            None
        }
        Err(err) => {
            tracing::warn!("unreadable state file: {err}");
            let _ = std::fs::rename(&path, path.with_extension("corrupt.bak"));
            None
        }
    }
}

pub fn save_snapshot(paths: &Paths, tab: TabId, ansi: &str) -> anyhow::Result<()> {
    atomic_write(&paths.snapshot_file(tab), ansi.as_bytes())
}

pub fn load_snapshot(paths: &Paths, tab: TabId) -> Option<String> {
    let path = paths.snapshot_file(tab);
    let meta = std::fs::metadata(&path).ok()?;
    if meta.len() > SNAPSHOT_READ_CAP {
        return None;
    }
    std::fs::read_to_string(&path).ok().filter(|s| !s.is_empty())
}

pub fn remove_snapshot(paths: &Paths, tab: TabId) {
    let _ = std::fs::remove_file(paths.snapshot_file(tab));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> Paths {
        let dir = std::env::temp_dir().join(format!("giverny-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Paths::at(dir)
    }

    #[test]
    fn roundtrip() {
        let paths = scratch();
        let mut ws = Workspace::default();
        let cat = ws.categories[0].id;
        let tab = ws.add_tab(cat);
        ws.tab_mut(tab).unwrap().custom_title = Some("api".into());

        let state = SaveState {
            version: STATE_VERSION,
            boot_id: boot_id(),
            clean_shutdown: true,
            workspace: ws,
            font_size: 14.5,
        };
        save(&paths, &state).unwrap();
        let back = load(&paths).expect("load");
        assert_eq!(back.font_size, 14.5);
        assert!(back.clean_shutdown);
        assert_eq!(back.workspace.tab(tab).unwrap().title(), "api");
    }

    #[test]
    fn corrupt_state_is_sidelined() {
        let paths = scratch();
        std::fs::create_dir_all(paths.state_file().parent().unwrap()).unwrap();
        std::fs::write(paths.state_file(), b"{ not json").unwrap();
        assert!(load(&paths).is_none());
        assert!(!paths.state_file().exists(), "bad file renamed aside");
    }

    #[test]
    fn snapshot_roundtrip_and_remove() {
        let paths = scratch();
        let tab = TabId(7);
        save_snapshot(&paths, tab, "\x1b[31mhello\x1b[0m\r\n").unwrap();
        assert_eq!(load_snapshot(&paths, tab).unwrap(), "\x1b[31mhello\x1b[0m\r\n");
        remove_snapshot(&paths, tab);
        assert!(load_snapshot(&paths, tab).is_none());
    }
}
