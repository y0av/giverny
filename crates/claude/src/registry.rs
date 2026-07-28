//! Claude Code's live session registry: `$CONFIG_DIR/sessions/<pid>.json`.
//!
//! Written by Claude Code itself (verified against 2.1.220): gives per-session
//! `status: busy|idle` with zero hook setup. Stale files are never cleaned up
//! upstream, so PID liveness gating is mandatory.

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct SessionEntry {
    pub pid: u32,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(default)]
    pub cwd: PathBuf,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub status: String,
    #[serde(rename = "statusUpdatedAt", default)]
    pub status_updated_at: u64,
}

impl SessionEntry {
    pub fn busy(&self) -> bool {
        self.status == "busy"
    }
}

#[derive(Debug, Clone)]
pub struct LiveSession {
    pub entry: SessionEntry,
    pub config_dir: PathBuf,
}

fn pid_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        // Portable fallback: kill(0).
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
}

/// Scan the registries of every config dir for live sessions.
pub fn scan(config_dirs: impl IntoIterator<Item = PathBuf>) -> Vec<LiveSession> {
    let mut out = Vec::new();
    for dir in config_dirs {
        let sessions = dir.join("sessions");
        let Ok(entries) = std::fs::read_dir(&sessions) else { continue };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else { continue };
            let Ok(entry) = serde_json::from_slice::<SessionEntry>(&bytes) else { continue };
            if pid_alive(entry.pid) {
                out.push(LiveSession { entry, config_dir: dir.clone() });
            }
        }
    }
    out
}

/// Is this claude session currently live in ANY of the given config dirs?
/// (Resuming it twice would interleave two writers into one transcript.)
pub fn session_is_live(config_dirs: impl IntoIterator<Item = PathBuf>, session_id: &str) -> bool {
    scan(config_dirs).iter().any(|s| s.entry.session_id == session_id)
}

/// Walk `/proc/<pid>/stat` parent links; true when `ancestor` is in the chain.
/// Maps a claude process to the Giverny tab whose shell spawned it.
#[cfg(target_os = "linux")]
pub fn has_ancestor(mut pid: u32, ancestor: u32) -> bool {
    for _ in 0..64 {
        if pid == ancestor {
            return true;
        }
        if pid <= 1 {
            return false;
        }
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { return false };
        // Field 4 (ppid) comes after the parenthesized comm, which may itself
        // contain spaces/parens — split after the LAST ')'.
        let Some((_, rest)) = stat.rsplit_once(')') else { return false };
        let mut fields = rest.split_whitespace();
        let _state = fields.next();
        let Some(ppid) = fields.next().and_then(|p| p.parse::<u32>().ok()) else { return false };
        pid = ppid;
    }
    false
}

#[cfg(not(target_os = "linux"))]
pub fn has_ancestor(_pid: u32, _ancestor: u32) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_registry_entry() {
        let json = r#"{ "pid": 1234, "sessionId": "da89-uuid", "cwd": "/home/u/dev",
            "startedAt": 1785250651905, "procStart": "50262977", "version": "2.1.220",
            "kind": "interactive", "entrypoint": "cli", "name": "dev-13",
            "nameSource": "derived", "status": "busy", "updatedAt": 1, "statusUpdatedAt": 2 }"#;
        let e: SessionEntry = serde_json::from_str(json).unwrap();
        assert_eq!(e.pid, 1234);
        assert!(e.busy());
        assert_eq!(e.name.as_deref(), Some("dev-13"));
        assert_eq!(e.cwd, PathBuf::from("/home/u/dev"));
    }

    #[test]
    fn scan_filters_dead_pids() {
        let dir = std::env::temp_dir().join(format!("giverny-reg-{}", std::process::id()));
        let sessions = dir.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        // Our own pid = alive; pid 4194304+1 range = almost surely dead.
        let me = std::process::id();
        std::fs::write(
            sessions.join(format!("{me}.json")),
            format!(r#"{{"pid":{me},"sessionId":"alive","status":"idle"}}"#),
        )
        .unwrap();
        std::fs::write(
            sessions.join("4194301.json"),
            r#"{"pid":4194301,"sessionId":"dead","status":"busy"}"#,
        )
        .unwrap();
        let live = scan([dir.clone()]);
        assert_eq!(live.len(), 1, "{live:?}");
        assert_eq!(live[0].entry.session_id, "alive");
        assert!(session_is_live([dir.clone()], "alive"));
        assert!(!session_is_live([dir.clone()], "dead"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ancestor_chain_finds_self_and_parent() {
        let me = std::process::id();
        assert!(has_ancestor(me, me));
        // Our parent chain reaches pid 1 eventually without panicking.
        assert!(!has_ancestor(me, 4194301));
    }
}
