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
    #[serde(default)]
    pub layout: Layout,
}

fn default_font_size() -> f32 {
    13.0
}

/// Window geometry and rail width, in logical points.
///
/// Window *position* is deliberately absent: Wayland gives clients no way to
/// place their own windows, so a saved position would restore on X11 and
/// silently do nothing on the user's actual session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    /// Inner size of the window when it was last *not* maximized.
    #[serde(default)]
    pub window: Option<[f32; 2]>,
    #[serde(default)]
    pub maximized: bool,
    #[serde(default)]
    pub rail_width: Option<f32>,
}

impl Layout {
    /// Sanitized window size: absurd or corrupt values fall back to `None`
    /// rather than opening a 3-pixel window that can't be resized back.
    pub fn window_size(&self) -> Option<[f32; 2]> {
        let [w, h] = self.window?;
        (w.is_finite()
            && h.is_finite()
            && w >= 640.0
            && h >= 400.0
            && w <= 20_000.0
            && h <= 20_000.0)
            .then_some([w, h])
    }

    pub fn rail_width_in(&self, range: std::ops::RangeInclusive<f32>) -> Option<f32> {
        let w = self.rail_width?;
        w.is_finite().then(|| w.clamp(*range.start(), *range.end()))
    }
}

/// Read just the geometry out of the state file, before the window exists.
///
/// Cheaper than a full load and, more importantly, tolerant: a state file
/// this build can't fully parse still shouldn't cost the user their window
/// size.
pub fn load_layout(paths: &Paths) -> Layout {
    #[derive(Deserialize)]
    struct Just {
        #[serde(default)]
        layout: Layout,
    }
    std::fs::read(paths.state_file())
        .ok()
        .and_then(|b| serde_json::from_slice::<Just>(&b).ok())
        .map(|j| j.layout)
        .unwrap_or_default()
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
                dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join(".config")
            })
            .join("giverny");
        Paths { base }
    }

    /// Custom base (tests).
    pub fn at(base: impl Into<PathBuf>) -> Self {
        Paths { base: base.into() }
    }

    /// Root of Giverny's config/state directory.
    pub fn base(&self) -> &Path {
        &self.base
    }

    pub fn state_file(&self) -> PathBuf {
        self.base.join("state").join("tabs.json")
    }

    pub fn snapshot_file(&self, tab: TabId) -> PathBuf {
        self.base
            .join("state")
            .join("snapshots")
            .join(format!("{}.ansi", tab.0))
    }

    /// Hook events spooled while the app is closed (drained at launch).
    pub fn hook_spool(&self) -> PathBuf {
        self.base.join("state").join("hook-spool.jsonl")
    }
}

/// Linux boot id (used later to distinguish restart from reboot).
pub fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("no parent dir"))?;
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
            tracing::warn!(
                "state version {} is newer than {}",
                state.version,
                STATE_VERSION
            );
            let _ = std::fs::rename(
                &path,
                path.with_extension(format!("v{}.bak", state.version)),
            );
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
    std::fs::read_to_string(&path)
        .ok()
        .filter(|s| !s.is_empty())
}

pub fn remove_snapshot(paths: &Paths, tab: TabId) {
    let _ = std::fs::remove_file(paths.snapshot_file(tab));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> Paths {
        let dir = std::env::temp_dir().join(format!("giverny-state-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Paths::at(dir)
    }

    #[test]
    fn roundtrip() {
        let paths = scratch("roundtrip");
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
            layout: Layout {
                window: Some([1000.0, 700.0]),
                maximized: false,
                rail_width: Some(300.0),
            },
        };
        save(&paths, &state).unwrap();
        let back = load(&paths).expect("load");
        assert_eq!(back.font_size, 14.5);
        assert!(back.clean_shutdown);
        assert_eq!(back.workspace.tab(tab).unwrap().title(), "api");
        assert_eq!(back.layout.window_size(), Some([1000.0, 700.0]));
        // The pre-window read sees the same thing without a full load.
        assert_eq!(load_layout(&paths), back.layout);
    }

    #[test]
    fn nonsense_geometry_is_ignored() {
        let bad = |w, h| Layout {
            window: Some([w, h]),
            ..Default::default()
        };
        assert_eq!(bad(f32::NAN, 700.0).window_size(), None);
        assert_eq!(bad(3.0, 700.0).window_size(), None, "unrecoverably small");
        assert_eq!(bad(1e9, 700.0).window_size(), None);
        assert_eq!(Layout::default().window_size(), None);

        // A rail width from a build with a different range is clamped, not lost.
        let rail = |w| Layout {
            rail_width: Some(w),
            ..Default::default()
        };
        assert_eq!(rail(9000.0).rail_width_in(180.0..=420.0), Some(420.0));
        assert_eq!(rail(0.0).rail_width_in(180.0..=420.0), Some(180.0));
        assert_eq!(rail(300.0).rail_width_in(180.0..=420.0), Some(300.0));
    }

    #[test]
    fn layout_survives_a_state_file_this_build_cannot_parse() {
        // Geometry is read on its own path precisely so a schema change or a
        // truncated workspace doesn't cost the user their window size.
        let paths = scratch("layout-only");
        std::fs::create_dir_all(paths.state_file().parent().unwrap()).unwrap();
        std::fs::write(
            paths.state_file(),
            br#"{"version":99,"layout":{"window":[900.0,600.0],"rail_width":260.0},
                 "workspace":{"unexpected":true}}"#,
        )
        .unwrap();
        let layout = load_layout(&paths);
        assert_eq!(layout.window_size(), Some([900.0, 600.0]));
        assert_eq!(layout.rail_width, Some(260.0));
    }

    #[test]
    fn corrupt_state_is_sidelined() {
        let paths = scratch("corrupt");
        std::fs::create_dir_all(paths.state_file().parent().unwrap()).unwrap();
        std::fs::write(paths.state_file(), b"{ not json").unwrap();
        assert!(load(&paths).is_none());
        assert!(!paths.state_file().exists(), "bad file renamed aside");
    }

    #[test]
    fn snapshot_roundtrip_and_remove() {
        let paths = scratch("snapshot");
        let tab = TabId(7);
        save_snapshot(&paths, tab, "\x1b[31mhello\x1b[0m\r\n").unwrap();
        assert_eq!(
            load_snapshot(&paths, tab).unwrap(),
            "\x1b[31mhello\x1b[0m\r\n"
        );
        remove_snapshot(&paths, tab);
        assert!(load_snapshot(&paths, tab).is_none());
    }
}
